//! Metrics, as OpenTelemetry instruments.
//!
//! Event counters and the HTTP histogram are synchronous instruments that
//! accumulate between scrapes. Everything describing current state (budget,
//! library, per-torrent, session, DHT, trackers, process) is an **observable**
//! instrument whose callback reads typed engine data at collection time — there is
//! no hand-written text exposition any more.
//!
//! The meter provider and its Prometheus reader live in [`telemetry`](crate::telemetry);
//! `/metrics` encodes that reader's registry, and when an OTLP endpoint is
//! configured the same instruments are pushed to the collector.
//!
//! Peer addresses are never label values (they would be unbounded); per-torrent
//! series are labelled by `imdb_id`, with descriptive fields on `tornas_torrent_info`.

use std::{
    any::Any,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::Context;
use opentelemetry::{
    KeyValue,
    metrics::{AsyncInstrument, Counter, Histogram, Meter, ObservableCounter, ObservableGauge},
};

use crate::engine::{Engine, StatusView, TorrentFacts};

fn meter() -> Meter {
    opentelemetry::global::meter("tornas")
}

// ---- synchronous instruments ----------------------------------------------

struct Instruments {
    adds: Counter<u64>,
    evictions: Counter<u64>,
    evicted_bytes: Counter<u64>,
    tmdb_errors: Counter<u64>,
    streams: Counter<u64>,
    stream_bytes: Counter<u64>,
    removals: Counter<u64>,
    seeding_paused: Counter<u64>,
    stalled_evictions: Counter<u64>,
    unauthorized: Counter<u64>,
    forbidden_source: Counter<u64>,
    pauses: Counter<u64>,
    resumes: Counter<u64>,
    http_requests: Counter<u64>,
    http_duration: Histogram<f64>,
}

static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

fn instruments() -> &'static Instruments {
    INSTRUMENTS.get_or_init(|| {
        let m = meter();
        let counter = |name: &'static str, help: &'static str| {
            m.u64_counter(name).with_description(help).build()
        };
        Instruments {
            adds: counter(
                "tornas_adds_total",
                "Movies added, by result (ok, refused, error)",
            ),
            evictions: counter(
                "tornas_evictions_total",
                "Movies evicted to stay under the disk budget",
            ),
            evicted_bytes: counter("tornas_evicted_bytes_total", "Bytes freed by eviction"),
            tmdb_errors: counter("tornas_tmdb_errors_total", "Failed TMDB lookups"),
            streams: counter(
                "tornas_streams_total",
                "Video stream requests, by kind (range or full)",
            ),
            stream_bytes: counter(
                "tornas_stream_bytes_total",
                "Bytes requested by video stream clients",
            ),
            removals: counter("tornas_removals_total", "Movies removed through the API"),
            seeding_paused: counter(
                "tornas_seeding_paused_total",
                "Downloads that finished and were paused",
            ),
            stalled_evictions: counter(
                "tornas_stalled_evictions_total",
                "Downloads evicted after making no progress",
            ),
            unauthorized: counter(
                "tornas_unauthorized_total",
                "Requests refused for a missing or wrong API token",
            ),
            forbidden_source: counter(
                "tornas_forbidden_source_total",
                "Requests refused because the source address is not allowed",
            ),
            pauses: counter("tornas_pauses_total", "Times everything was paused"),
            resumes: counter(
                "tornas_resumes_total",
                "Times a pause ended, by trigger (manual or auto)",
            ),
            http_requests: counter(
                "tornas_http_requests_total",
                "HTTP requests served, by route template, method and status",
            ),
            http_duration: m
                .f64_histogram("tornas_http_request_duration_seconds")
                .with_description(
                    "Time until response headers, by route template. For video this is time to \
                     first byte, not the whole stream.",
                )
                .build(),
        }
    })
}

/// Build the instruments and seed the labelled ones at zero, so the first scrape
/// already carries the full set of event counters. Call once, after the meter
/// provider is installed.
pub fn install() {
    let i = instruments();
    for r in ["ok", "refused", "error"] {
        i.adds.add(0, &[KeyValue::new("result", r)]);
    }
    for k in ["range", "full"] {
        i.streams.add(0, &[KeyValue::new("kind", k)]);
    }
    for t in ["manual", "auto"] {
        i.resumes.add(0, &[KeyValue::new("trigger", t)]);
    }
    for c in [
        &i.evictions,
        &i.evicted_bytes,
        &i.tmdb_errors,
        &i.stream_bytes,
        &i.removals,
        &i.seeding_paused,
        &i.stalled_evictions,
        &i.unauthorized,
        &i.forbidden_source,
        &i.pauses,
    ] {
        c.add(0, &[]);
    }
}

pub fn add(result: &'static str) {
    instruments()
        .adds
        .add(1, &[KeyValue::new("result", result)]);
}
pub fn eviction(bytes: u64) {
    instruments().evictions.add(1, &[]);
    instruments().evicted_bytes.add(bytes, &[]);
}
pub fn tmdb_error() {
    instruments().tmdb_errors.add(1, &[]);
}
pub fn stream(kind: &'static str, bytes: u64) {
    instruments().streams.add(1, &[KeyValue::new("kind", kind)]);
    instruments().stream_bytes.add(bytes, &[]);
}
pub fn seeding_paused() {
    instruments().seeding_paused.add(1, &[]);
}
pub fn stalled_eviction() {
    instruments().stalled_evictions.add(1, &[]);
}
pub fn forbidden_source() {
    instruments().forbidden_source.add(1, &[]);
}
pub fn unauthorized() {
    instruments().unauthorized.add(1, &[]);
}
pub fn paused() {
    instruments().pauses.add(1, &[]);
}
pub fn resumed(trigger: &'static str) {
    instruments()
        .resumes
        .add(1, &[KeyValue::new("trigger", trigger)]);
}
pub fn removal() {
    instruments().removals.add(1, &[]);
}

/// Middleware: count requests and time them by route *template* (never the raw
/// path, which would be unbounded).
pub async fn track_http(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_owned());
    let method: &'static str = match req.method().as_str() {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "OPTIONS" => "OPTIONS",
        _ => "OTHER",
    };
    let start = Instant::now();
    let resp = next.run(req).await;
    let status = resp.status().as_u16().to_string();
    let i = instruments();
    i.http_requests.add(
        1,
        &[
            KeyValue::new("route", route.clone()),
            KeyValue::new("method", method),
            KeyValue::new("status", status),
        ],
    );
    i.http_duration.record(
        start.elapsed().as_secs_f64(),
        &[KeyValue::new("route", route)],
    );
    resp
}

// ---- the /metrics scrape ---------------------------------------------------

static REGISTRY: OnceLock<prometheus::Registry> = OnceLock::new();

/// Hand `/metrics` the Prometheus registry the meter provider gathers into. Called
/// once from the telemetry wiring.
pub fn set_registry(registry: prometheus::Registry) {
    let _ = REGISTRY.set(registry);
}

/// Encode the current metrics as Prometheus text.
pub fn render() -> anyhow::Result<String> {
    let registry = REGISTRY.get().context("metrics registry not initialised")?;
    let mut buf = String::new();
    prometheus::TextEncoder::new().encode_utf8(&registry.gather(), &mut buf)?;
    Ok(buf)
}

// ---- observable state ------------------------------------------------------

/// The engine reads a scrape needs, computed once and shared by every observable
/// callback in that scrape (they fire back to back). A short TTL means one scrape
/// does the work once.
struct Cached {
    status: StatusView,
    facts: Vec<TorrentFacts>,
    fetched_bytes: u64,
    uploaded_bytes: u64,
    blocked_incoming: u64,
    blocked_outgoing: u64,
    download_bps: u64,
    upload_bps: u64,
    peers: Vec<(&'static str, u64)>,
    peers_live: Vec<(&'static str, u64)>,
    steals: u64,
    connections: Vec<(&'static str, &'static str, &'static str, u64)>,
    dht: Option<(u64, u64, u64)>,
    trackers_enabled: bool,
    trackers_active: Vec<(String, u64)>,
    tracker_list_age: Option<i64>,
    tracker_rejected: u64,
    tracker_deduplicated: u64,
    tracker_sources: Vec<(String, bool, u64)>,
}

struct Snapshot {
    engine: Arc<Engine>,
    cache: Mutex<Option<(Instant, Arc<Cached>)>>,
}

const SNAPSHOT_TTL: Duration = Duration::from_millis(250);

impl Snapshot {
    fn get(&self) -> Arc<Cached> {
        let mut guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, cached)) = guard.as_ref()
            && at.elapsed() < SNAPSHOT_TTL
        {
            return cached.clone();
        }
        let cached = Arc::new(self.compute());
        *guard = Some((Instant::now(), cached.clone()));
        cached
    }

    fn compute(&self) -> Cached {
        let e = &self.engine;
        let status = e.status().unwrap_or_else(|_| StatusView {
            pause: e.pause_view(),
            version: env!("CARGO_PKG_VERSION"),
            hostname: String::new(),
            warnings: Vec::new(),
            budget: Default::default(),
            session: Default::default(),
            movies: Vec::new(),
            events: Vec::new(),
        });
        let facts = e.torrent_facts().unwrap_or_default();
        let snap = e.session.stats_snapshot();
        let p = &snap.peers;
        let c = &snap.connections;
        let connections = [("tcp", &c.tcp), ("utp", &c.utp), ("socks", &c.socks)]
            .into_iter()
            .flat_map(|(t, cf)| {
                [("v4", &cf.v4), ("v6", &cf.v6)]
                    .into_iter()
                    .flat_map(move |(fam, st)| {
                        [
                            (t, fam, "attempt", st.attempts),
                            (t, fam, "success", st.successes),
                            (t, fam, "error", st.errors),
                        ]
                    })
            })
            .collect();

        let mut schemes: std::collections::BTreeMap<String, u64> = Default::default();
        for t in e.trackers.current() {
            if let Some(s) = t.split("://").next() {
                *schemes.entry(s.to_owned()).or_default() += 1;
            }
        }
        let feed = e.trackers.state();
        let tracker_sources = feed
            .sources
            .iter()
            .map(|s| {
                let up = s.enabled && s.last_error.is_none() && s.last_ok_at.is_some();
                (s.name.clone(), up, s.accepted as u64)
            })
            .collect();

        Cached {
            fetched_bytes: snap.counters.fetched_bytes,
            uploaded_bytes: snap.counters.uploaded_bytes,
            blocked_incoming: snap.counters.blocked_incoming,
            blocked_outgoing: snap.counters.blocked_outgoing,
            download_bps: snap.download_speed.as_bytes(),
            upload_bps: snap.upload_speed.as_bytes(),
            peers: vec![
                ("queued", u64::from(p.queued)),
                ("connecting", u64::from(p.connecting)),
                ("live", u64::from(p.live)),
                ("seen", u64::from(p.seen)),
                ("dead", u64::from(p.dead)),
                ("not_needed", u64::from(p.not_needed)),
            ],
            peers_live: vec![
                ("tcp", u64::from(p.live_tcp)),
                ("utp", u64::from(p.live_utp)),
                ("socks", u64::from(p.live_socks)),
            ],
            steals: u64::from(p.steals),
            connections,
            dht: e.dht_stats(),
            trackers_enabled: e.trackers.config.read().enabled,
            trackers_active: schemes.into_iter().collect(),
            tracker_list_age: feed.updated_at.map(|ts| crate::utils::now_secs() - ts),
            tracker_rejected: feed.rejected as u64,
            tracker_deduplicated: feed.deduplicated as u64,
            tracker_sources,
            status,
            facts,
        }
    }
}

/// Observable instruments must be retained for their callbacks to keep firing;
/// these live for the life of the process.
static OBSERVERS: OnceLock<Vec<Box<dyn Any + Send + Sync>>> = OnceLock::new();

type Kept = Vec<Box<dyn Any + Send + Sync>>;
type GaugeFn = Box<dyn Fn(&Cached, &dyn AsyncInstrument<f64>) + Send + Sync>;
type CounterFn = Box<dyn Fn(&Cached, &dyn AsyncInstrument<u64>) + Send + Sync>;
type PerTorrent = (&'static str, &'static str, fn(&TorrentFacts) -> f64);

/// Register a gauge whose callback observes from the cached snapshot.
fn push_gauge(
    kept: &mut Kept,
    snap: &Arc<Snapshot>,
    name: &'static str,
    help: &'static str,
    f: GaugeFn,
) {
    let snap = snap.clone();
    let g: ObservableGauge<f64> = meter()
        .f64_observable_gauge(name)
        .with_description(help)
        .with_callback(move |inst| f(&snap.get(), inst))
        .build();
    kept.push(Box::new(g));
}

/// Register an observable counter (a monotonic session total read from the engine).
fn push_counter(
    kept: &mut Kept,
    snap: &Arc<Snapshot>,
    name: &'static str,
    help: &'static str,
    f: CounterFn,
) {
    let snap = snap.clone();
    let c: ObservableCounter<u64> = meter()
        .u64_observable_counter(name)
        .with_description(help)
        .with_callback(move |inst| f(&snap.get(), inst))
        .build();
    kept.push(Box::new(c));
}

/// Register the observable instruments that read engine state. Call once, after the
/// engine has started.
pub fn observe(engine: Arc<Engine>) {
    let snap = Arc::new(Snapshot {
        engine: engine.clone(),
        cache: Mutex::new(None),
    });
    let mut kept: Kept = Vec::new();
    let g = |kept: &mut Kept, name, help, f: GaugeFn| push_gauge(kept, &snap, name, help, f);
    let cnt = |kept: &mut Kept, name, help, f: CounterFn| push_counter(kept, &snap, name, help, f);

    // Build info: a constant 1 with descriptive labels.
    {
        let build: ObservableGauge<u64> = meter()
            .u64_observable_gauge("tornas_build_info")
            .with_description("Build information; always 1")
            .with_callback(|inst| {
                inst.observe(
                    1,
                    &[
                        KeyValue::new("version", env!("CARGO_PKG_VERSION")),
                        KeyValue::new("os", std::env::consts::OS),
                        KeyValue::new("arch", std::env::consts::ARCH),
                    ],
                )
            })
            .build();
        kept.push(Box::new(build));
    }

    g(
        &mut kept,
        "tornas_uptime_seconds",
        "Seconds since the server started",
        Box::new(|d, o| o.observe(d.status.session.uptime_secs as f64, &[])),
    );

    // ---- global state
    let opts = engine.opts.clone();
    let tuning = engine.tuning.clone();
    let engine_disk = engine.clone();
    g(
        &mut kept,
        "tornas_paused",
        "1 while everything is paused",
        Box::new(|d, o| o.observe(f64::from(d.status.pause.paused), &[])),
    );
    g(
        &mut kept,
        "tornas_paused_for_missing_disk",
        "1 while paused because the data disk is missing",
        Box::new(|d, o| {
            o.observe(
                f64::from(d.status.pause.reason == Some(crate::engine::PauseReason::DiskMissing)),
                &[],
            )
        }),
    );
    g(
        &mut kept,
        "tornas_data_disk_mounted",
        "0 while the data disk is missing (only watched with --require-mount)",
        Box::new(move |_d, o| o.observe(f64::from(!engine_disk.is_disk_missing()), &[])),
    );
    g(
        &mut kept,
        "tornas_queued_downloads",
        "Downloads held back by --max-active-downloads",
        Box::new(|d, o| o.observe(d.status.session.queued as f64, &[])),
    );
    if let Some(max) = opts.max_active_downloads {
        g(
            &mut kept,
            "tornas_max_active_downloads",
            "Configured download queue size",
            Box::new(move |_d, o| o.observe(f64::from(max), &[])),
        );
    }
    g(
        &mut kept,
        "tornas_ratelimit_download_bytes_per_second",
        "Global download limit in force; 0 = unlimited",
        Box::new(|d, o| o.observe(d.status.session.download_limit.unwrap_or(0) as f64, &[])),
    );
    g(
        &mut kept,
        "tornas_ratelimit_upload_bytes_per_second",
        "Global upload limit in force; 0 = unlimited",
        Box::new(|d, o| o.observe(d.status.session.upload_limit.unwrap_or(0) as f64, &[])),
    );
    g(
        &mut kept,
        "tornas_bandwidth_window",
        "Index of the [[bandwidth.schedule]] window in force, or -1",
        Box::new(|d, o| {
            o.observe(
                d.status
                    .session
                    .schedule_window
                    .map(|w| w as f64)
                    .unwrap_or(-1.0),
                &[],
            )
        }),
    );
    let (peer_limit, checks) = (tuning.peer_limit, tuning.concurrent_checks);
    g(
        &mut kept,
        "tornas_peer_limit",
        "Default peers per torrent",
        Box::new(move |_d, o| o.observe(f64::from(peer_limit), &[])),
    );
    g(
        &mut kept,
        "tornas_concurrent_checks",
        "Torrents allowed to hash-check at once",
        Box::new(move |_d, o| o.observe(f64::from(checks), &[])),
    );
    g(
        &mut kept,
        "tornas_pause_remaining_seconds",
        "Seconds until the pause lifts on its own, or -1",
        Box::new(|d, o| o.observe(d.status.pause.remaining_secs.unwrap_or(-1) as f64, &[])),
    );

    // ---- budget and disk
    g(
        &mut kept,
        "tornas_budget_limit_bytes",
        "Configured disk budget",
        Box::new(|d, o| o.observe(d.status.budget.limit as f64, &[])),
    );
    g(
        &mut kept,
        "tornas_budget_used_bytes",
        "Bytes charged to the budget",
        Box::new(|d, o| o.observe(d.status.budget.used as f64, &[])),
    );
    g(
        &mut kept,
        "tornas_budget_min_free_bytes",
        "Configured minimum free disk",
        Box::new(|d, o| o.observe(d.status.budget.min_free as f64, &[])),
    );
    g(
        &mut kept,
        "tornas_disk_free_bytes",
        "Free bytes on the torrents filesystem",
        Box::new(|d, o| o.observe(d.status.budget.disk_free as f64, &[])),
    );
    g(
        &mut kept,
        "tornas_disk_total_bytes",
        "Size of the torrents filesystem",
        Box::new(|d, o| o.observe(d.status.budget.disk_total as f64, &[])),
    );
    g(
        &mut kept,
        "tornas_disk_below_min_free",
        "1 when free disk is under the configured minimum",
        Box::new(|d, o| {
            o.observe(
                f64::from(d.status.budget.disk_free < d.status.budget.min_free),
                &[],
            )
        }),
    );
    g(
        &mut kept,
        "tornas_warnings",
        "Active operational warnings",
        Box::new(|d, o| o.observe(d.status.warnings.len() as f64, &[])),
    );

    // ---- library aggregates
    g(
        &mut kept,
        "tornas_movies",
        "Movies in the library",
        Box::new(|d, o| o.observe(d.status.movies.len() as f64, &[])),
    );
    g(
        &mut kept,
        "tornas_movies_protected",
        "Movies inside the stream grace window, not evictable",
        Box::new(|d, o| {
            o.observe(
                d.status.movies.iter().filter(|m| m.protected).count() as f64,
                &[],
            )
        }),
    );
    g(
        &mut kept,
        "tornas_torrents",
        "Loaded torrents, by BEP 27 private flag",
        Box::new(|d, o| {
            for private in [true, false] {
                let n = d.facts.iter().filter(|f| f.private == private).count();
                o.observe(n as f64, &[KeyValue::new("private", private.to_string())]);
            }
        }),
    );
    g(
        &mut kept,
        "tornas_torrents_size_bytes",
        "Whole-torrent size of loaded torrents, by private flag",
        Box::new(|d, o| {
            for private in [true, false] {
                let bytes: u64 = d
                    .facts
                    .iter()
                    .filter(|f| f.private == private)
                    .map(|f| f.size_bytes)
                    .sum();
                o.observe(
                    bytes as f64,
                    &[KeyValue::new("private", private.to_string())],
                );
            }
        }),
    );
    g(
        &mut kept,
        "tornas_torrents_by_state",
        "Loaded torrents by state",
        Box::new(|d, o| {
            for state in [
                "checking",
                "downloading",
                "seeding",
                "done",
                "paused",
                "error",
            ] {
                let n = d.facts.iter().filter(|f| f.state == state).count();
                o.observe(n as f64, &[KeyValue::new("state", state)]);
            }
        }),
    );

    // ---- per torrent
    g(
        &mut kept,
        "tornas_torrent_info",
        "Descriptive labels for a torrent; always 1. Join on imdb_id.",
        Box::new(|d, o| {
            for f in &d.facts {
                o.observe(
                    1.0,
                    &[
                        KeyValue::new("imdb_id", f.imdb_id.clone()),
                        KeyValue::new("info_hash", f.info_hash.clone()),
                        KeyValue::new("title", f.title.clone()),
                        KeyValue::new("state", f.state),
                        KeyValue::new("private", f.private.to_string()),
                    ],
                );
            }
        }),
    );
    let per: &[PerTorrent] = &[
        (
            "tornas_torrent_size_bytes",
            "Size of all files in the torrent",
            |f| f.size_bytes as f64,
        ),
        (
            "tornas_torrent_selected_bytes",
            "Bytes selected for download",
            |f| f.selected_bytes as f64,
        ),
        (
            "tornas_torrent_progress_bytes",
            "Verified bytes of the selection on disk",
            |f| f.progress_bytes as f64,
        ),
        (
            "tornas_torrent_progress_ratio",
            "Download progress of the selection, 0..1",
            |f| {
                if f.selected_bytes == 0 {
                    0.0
                } else {
                    f.progress_bytes as f64 / f.selected_bytes as f64
                }
            },
        ),
        ("tornas_torrent_piece_length_bytes", "Piece size", |f| {
            f.piece_length as f64
        }),
        (
            "tornas_torrent_pieces",
            "Pieces in the whole torrent",
            |f| f.pieces as f64,
        ),
        (
            "tornas_torrent_pieces_verified",
            "Pieces downloaded and hash-checked this session",
            |f| f.pieces_verified as f64,
        ),
        ("tornas_torrent_files", "Files in the torrent", |f| {
            f.files as f64
        }),
        (
            "tornas_torrent_download_bytes_per_second",
            "Current download rate",
            |f| f.download_bps as f64,
        ),
        (
            "tornas_torrent_upload_bytes_per_second",
            "Current upload rate",
            |f| f.upload_bps as f64,
        ),
        (
            "tornas_torrent_idle_seconds",
            "Seconds since last streamed or added",
            |f| f.idle_secs as f64,
        ),
    ];
    for (name, help, get) in per {
        let get = *get;
        g(
            &mut kept,
            name,
            help,
            Box::new(move |d, o| {
                for f in &d.facts {
                    o.observe(get(f), &[KeyValue::new("imdb_id", f.imdb_id.clone())]);
                }
            }),
        );
    }
    cnt(
        &mut kept,
        "tornas_torrent_fetched_bytes_total",
        "Bytes received from peers this session, including discarded",
        Box::new(|d, o| {
            for f in &d.facts {
                o.observe(
                    f.fetched_bytes,
                    &[KeyValue::new("imdb_id", f.imdb_id.clone())],
                );
            }
        }),
    );
    cnt(
        &mut kept,
        "tornas_torrent_uploaded_bytes_total",
        "Bytes sent to peers this session",
        Box::new(|d, o| {
            for f in &d.facts {
                o.observe(
                    f.uploaded_bytes,
                    &[KeyValue::new("imdb_id", f.imdb_id.clone())],
                );
            }
        }),
    );
    g(
        &mut kept,
        "tornas_torrent_piece_download_seconds",
        "Average time to download one piece",
        Box::new(|d, o| {
            for f in &d.facts {
                if let Some(s) = f.piece_download_secs_avg {
                    o.observe(s, &[KeyValue::new("imdb_id", f.imdb_id.clone())]);
                }
            }
        }),
    );
    g(
        &mut kept,
        "tornas_torrent_eta_seconds",
        "Estimated seconds to finish at the current rate",
        Box::new(|d, o| {
            for f in &d.facts {
                if let Some(s) = f.eta_secs {
                    o.observe(s as f64, &[KeyValue::new("imdb_id", f.imdb_id.clone())]);
                }
            }
        }),
    );
    g(
        &mut kept,
        "tornas_torrent_peers",
        "Peers known to a torrent, by state",
        Box::new(|d, o| {
            for f in &d.facts {
                for (state, n) in f.peers {
                    o.observe(
                        n as f64,
                        &[
                            KeyValue::new("imdb_id", f.imdb_id.clone()),
                            KeyValue::new("state", state),
                        ],
                    );
                }
            }
        }),
    );
    g(
        &mut kept,
        "tornas_torrent_peers_live",
        "Connected peers of a torrent, by transport",
        Box::new(|d, o| {
            for f in &d.facts {
                for (transport, n) in f.peers_live_by_transport {
                    o.observe(
                        n as f64,
                        &[
                            KeyValue::new("imdb_id", f.imdb_id.clone()),
                            KeyValue::new("transport", transport),
                        ],
                    );
                }
            }
        }),
    );
    g(
        &mut kept,
        "tornas_torrent_trackers",
        "Trackers attached to a torrent, by URL scheme",
        Box::new(|d, o| {
            for f in &d.facts {
                for (scheme, n) in &f.trackers_by_scheme {
                    o.observe(
                        *n as f64,
                        &[
                            KeyValue::new("imdb_id", f.imdb_id.clone()),
                            KeyValue::new("scheme", scheme.clone()),
                        ],
                    );
                }
            }
        }),
    );

    // ---- session aggregates
    cnt(
        &mut kept,
        "tornas_fetched_bytes_total",
        "Bytes received from peers across all torrents",
        Box::new(|d, o| o.observe(d.fetched_bytes, &[])),
    );
    cnt(
        &mut kept,
        "tornas_uploaded_bytes_total",
        "Bytes sent to peers across all torrents",
        Box::new(|d, o| o.observe(d.uploaded_bytes, &[])),
    );
    cnt(
        &mut kept,
        "tornas_blocked_connections_total",
        "Peer connections refused by the blocklist or allowlist",
        Box::new(|d, o| {
            o.observe(
                d.blocked_incoming,
                &[KeyValue::new("direction", "incoming")],
            );
            o.observe(
                d.blocked_outgoing,
                &[KeyValue::new("direction", "outgoing")],
            );
        }),
    );
    g(
        &mut kept,
        "tornas_download_bytes_per_second",
        "Aggregate download rate",
        Box::new(|d, o| o.observe(d.download_bps as f64, &[])),
    );
    g(
        &mut kept,
        "tornas_upload_bytes_per_second",
        "Aggregate upload rate",
        Box::new(|d, o| o.observe(d.upload_bps as f64, &[])),
    );
    g(
        &mut kept,
        "tornas_peers",
        "Peers across all torrents, by state",
        Box::new(|d, o| {
            for (state, n) in &d.peers {
                o.observe(*n as f64, &[KeyValue::new("state", *state)]);
            }
        }),
    );
    g(
        &mut kept,
        "tornas_peers_live",
        "Connected peers across all torrents, by transport",
        Box::new(|d, o| {
            for (transport, n) in &d.peers_live {
                o.observe(*n as f64, &[KeyValue::new("transport", *transport)]);
            }
        }),
    );
    cnt(
        &mut kept,
        "tornas_peer_steals_total",
        "Pieces reassigned from a slow peer to a faster one",
        Box::new(|d, o| o.observe(d.steals, &[])),
    );
    cnt(
        &mut kept,
        "tornas_peer_connections_total",
        "Outgoing peer connection attempts and outcome, by transport and address family",
        Box::new(|d, o| {
            for (transport, family, outcome, n) in &d.connections {
                o.observe(
                    *n,
                    &[
                        KeyValue::new("transport", *transport),
                        KeyValue::new("family", *family),
                        KeyValue::new("outcome", *outcome),
                    ],
                );
            }
        }),
    );

    // ---- DHT
    g(
        &mut kept,
        "tornas_dht_enabled",
        "1 when the DHT is running",
        Box::new(|d, o| o.observe(f64::from(d.dht.is_some()), &[])),
    );
    g(
        &mut kept,
        "tornas_dht_nodes",
        "Nodes in the DHT routing table, by address family",
        Box::new(|d, o| {
            if let Some((v4, v6, _)) = d.dht {
                o.observe(v4 as f64, &[KeyValue::new("family", "v4")]);
                o.observe(v6 as f64, &[KeyValue::new("family", "v6")]);
            }
        }),
    );
    g(
        &mut kept,
        "tornas_dht_outstanding_requests",
        "DHT queries awaiting a reply",
        Box::new(|d, o| {
            if let Some((_, _, out)) = d.dht {
                o.observe(out as f64, &[]);
            }
        }),
    );

    // ---- public tracker feed
    g(
        &mut kept,
        "tornas_trackers_enabled",
        "1 when the public tracker feed is on",
        Box::new(|d, o| o.observe(f64::from(d.trackers_enabled), &[])),
    );
    g(
        &mut kept,
        "tornas_trackers_active",
        "Public trackers currently handed to new torrents, by URL scheme",
        Box::new(|d, o| {
            for (scheme, n) in &d.trackers_active {
                o.observe(*n as f64, &[KeyValue::new("scheme", scheme.clone())]);
            }
        }),
    );
    g(
        &mut kept,
        "tornas_tracker_list_age_seconds",
        "Seconds since the tracker list was last refreshed successfully, or -1",
        Box::new(|d, o| o.observe(d.tracker_list_age.unwrap_or(-1) as f64, &[])),
    );
    g(
        &mut kept,
        "tornas_tracker_list_rejected",
        "Entries dropped in the last refresh",
        Box::new(|d, o| o.observe(d.tracker_rejected as f64, &[])),
    );
    g(
        &mut kept,
        "tornas_tracker_list_deduplicated",
        "Duplicates collapsed in the last refresh",
        Box::new(|d, o| o.observe(d.tracker_deduplicated as f64, &[])),
    );
    g(
        &mut kept,
        "tornas_tracker_source_up",
        "1 when the last fetch of a tracker source succeeded",
        Box::new(|d, o| {
            for (name, up, _) in &d.tracker_sources {
                o.observe(f64::from(*up), &[KeyValue::new("source", name.clone())]);
            }
        }),
    );
    g(
        &mut kept,
        "tornas_tracker_source_accepted",
        "Trackers a source contributed after filtering",
        Box::new(|d, o| {
            for (name, _, accepted) in &d.tracker_sources {
                o.observe(*accepted as f64, &[KeyValue::new("source", name.clone())]);
            }
        }),
    );

    // ---- process (Linux)
    g(
        &mut kept,
        "tornas_process_resident_memory_bytes",
        "Resident memory",
        Box::new(|_d, o| {
            if let Some(kb) = proc_field("VmRSS:") {
                o.observe((kb * 1024) as f64, &[]);
            }
        }),
    );
    g(
        &mut kept,
        "tornas_process_threads",
        "OS threads",
        Box::new(|_d, o| {
            if let Some(n) = proc_field("Threads:") {
                o.observe(n as f64, &[]);
            }
        }),
    );
    g(
        &mut kept,
        "tornas_process_open_fds",
        "Open file descriptors (peer sockets count here)",
        Box::new(|_d, o| {
            if let Ok(rd) = std::fs::read_dir("/proc/self/fd") {
                o.observe(rd.count() as f64, &[]);
            }
        }),
    );

    let _ = OBSERVERS.set(kept);
}

fn proc_field(key: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}
