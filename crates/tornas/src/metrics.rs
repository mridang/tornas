//! Prometheus metrics.
//!
//! Two sources feed `/metrics`:
//! * Event counters and the HTTP histogram go through the `metrics` facade, so
//!   they accumulate between scrapes. librqbit's uTP code reports through the same
//!   recorder once uTP sockets are in use.
//! * Everything describing current state (budget, library, per-torrent, session,
//!   DHT, tracker feed, process) is rendered fresh from typed engine data on each
//!   scrape.
//!
//! librqbit's own `SessionStatsSnapshot::as_prometheus` is deliberately not used:
//! it emits `rqbit_peers_queued` twice (the second is really the peers-seen
//! count), and Prometheus rejects any scrape containing a duplicate series.
//!
//! Peer addresses are never used as label values; they would create unbounded
//! series. Per-torrent series are labelled by `imdb_id` only, and descriptive
//! fields live on `tornas_torrent_info` for joins.

use std::{collections::BTreeMap, fmt::Write, sync::OnceLock, time::Instant};

use metrics::{counter, describe_counter, describe_histogram, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

use crate::engine::Engine;

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

const HTTP_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// Install the global recorder once per process. Safe to call repeatedly.
pub fn install() -> Option<&'static PrometheusHandle> {
    if let Some(h) = HANDLE.get() {
        return Some(h);
    }
    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("tornas_http_request_duration_seconds".into()),
            HTTP_BUCKETS,
        )
        .ok()?
        .install_recorder()
        .ok()?;
    describe_counter!(
        "tornas_adds_total",
        "Movies added, by result (ok, refused, error)"
    );
    describe_counter!(
        "tornas_evictions_total",
        "Movies evicted to stay under the disk budget"
    );
    describe_counter!("tornas_evicted_bytes_total", "Bytes freed by eviction");
    describe_counter!("tornas_tmdb_errors_total", "Failed TMDB lookups");
    describe_counter!(
        "tornas_streams_total",
        "Video stream requests, by kind (range or full)"
    );
    describe_counter!(
        "tornas_stream_bytes_total",
        "Bytes requested by video stream clients"
    );
    describe_counter!("tornas_removals_total", "Movies removed through the API");
    describe_counter!(
        "tornas_seeding_paused_total",
        "Downloads that finished and were paused"
    );
    describe_counter!(
        "tornas_updates_installed_total",
        "Releases installed by auto-update"
    );
    describe_counter!(
        "tornas_stalled_evictions_total",
        "Downloads evicted after making no progress"
    );
    describe_counter!(
        "tornas_unauthorized_total",
        "Requests refused for a missing or wrong API token"
    );
    describe_counter!(
        "tornas_forbidden_source_total",
        "Requests refused because the source address is not allowed"
    );
    describe_counter!(
        "tornas_http_requests_total",
        "HTTP requests served, by route template, method and status"
    );
    describe_histogram!(
        "tornas_http_request_duration_seconds",
        "Time until response headers, by route template. For video this is time to first byte, not the whole stream."
    );
    // Register every counter at zero so the first scrape already shows the full set.
    for r in ["ok", "refused", "error"] {
        counter!("tornas_adds_total", "result" => r).absolute(0);
    }
    for k in ["range", "full"] {
        counter!("tornas_streams_total", "kind" => k).absolute(0);
    }
    for name in [
        "tornas_evictions_total",
        "tornas_evicted_bytes_total",
        "tornas_tmdb_errors_total",
        "tornas_stream_bytes_total",
        "tornas_removals_total",
        "tornas_seeding_paused_total",
        "tornas_updates_installed_total",
        "tornas_stalled_evictions_total",
        "tornas_unauthorized_total",
        "tornas_forbidden_source_total",
    ] {
        counter!(name).absolute(0);
    }
    let _ = HANDLE.set(handle);
    HANDLE.get()
}

pub fn add(result: &'static str) {
    counter!("tornas_adds_total", "result" => result).increment(1);
}
pub fn eviction(bytes: u64) {
    counter!("tornas_evictions_total").increment(1);
    counter!("tornas_evicted_bytes_total").increment(bytes);
}
pub fn tmdb_error() {
    counter!("tornas_tmdb_errors_total").increment(1);
}
pub fn stream(kind: &'static str, bytes: u64) {
    counter!("tornas_streams_total", "kind" => kind).increment(1);
    counter!("tornas_stream_bytes_total").increment(bytes);
}
pub fn seeding_paused() {
    counter!("tornas_seeding_paused_total").increment(1);
}
pub fn update_installed() {
    counter!("tornas_updates_installed_total").increment(1);
}
pub fn stalled_eviction() {
    counter!("tornas_stalled_evictions_total").increment(1);
}
pub fn forbidden_source() {
    counter!("tornas_forbidden_source_total").increment(1);
}
pub fn unauthorized() {
    counter!("tornas_unauthorized_total").increment(1);
}
pub fn removal() {
    counter!("tornas_removals_total").increment(1);
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
    // Static strings, so the label does not borrow the request that is moved below.
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
    counter!(
        "tornas_http_requests_total",
        "route" => route.clone(),
        "method" => method,
        "status" => status
    )
    .increment(1);
    histogram!("tornas_http_request_duration_seconds", "route" => route)
        .record(start.elapsed().as_secs_f64());
    resp
}

// ---- text exposition helpers ----------------------------------------------

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

fn sample(out: &mut String, name: &str, labels: &[(&str, &str)], value: impl std::fmt::Display) {
    if labels.is_empty() {
        let _ = writeln!(out, "{name} {value}");
    } else {
        let l: Vec<String> = labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", esc(v)))
            .collect();
        let _ = writeln!(out, "{name}{{{}}} {value}", l.join(","));
    }
}

fn gauge(out: &mut String, name: &str, help: &str, value: impl std::fmt::Display) {
    family(out, name, "gauge", help);
    sample(out, name, &[], value);
}

/// Process stats from /proc (Linux only; absent elsewhere).
fn process(out: &mut String) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return;
    };
    let field = |k: &str| -> Option<u64> {
        status
            .lines()
            .find(|l| l.starts_with(k))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
    };
    if let Some(kb) = field("VmRSS:") {
        gauge(
            out,
            "tornas_process_resident_memory_bytes",
            "Resident memory",
            kb * 1024,
        );
    }
    if let Some(n) = field("Threads:") {
        gauge(out, "tornas_process_threads", "OS threads", n);
    }
    if let Ok(rd) = std::fs::read_dir("/proc/self/fd") {
        gauge(
            out,
            "tornas_process_open_fds",
            "Open file descriptors (peer sockets count here)",
            rd.count(),
        );
    }
}

/// Full scrape body.
pub fn render(engine: &Engine) -> anyhow::Result<String> {
    let status = engine.status()?;
    let facts = engine.torrent_facts()?;
    let mut out = HANDLE.get().map(|h| h.render()).unwrap_or_default();
    out.push('\n');

    // ---- build and process
    family(
        &mut out,
        "tornas_build_info",
        "gauge",
        "Build information; always 1",
    );
    sample(
        &mut out,
        "tornas_build_info",
        &[
            ("version", env!("CARGO_PKG_VERSION")),
            ("os", std::env::consts::OS),
            ("arch", std::env::consts::ARCH),
        ],
        1,
    );
    gauge(
        &mut out,
        "tornas_uptime_seconds",
        "Seconds since the server started",
        status.session.uptime_secs,
    );
    process(&mut out);

    // ---- budget and disk
    let b = &status.budget;
    gauge(
        &mut out,
        "tornas_budget_limit_bytes",
        "Configured disk budget",
        b.limit,
    );
    gauge(
        &mut out,
        "tornas_budget_used_bytes",
        "Bytes charged to the budget",
        b.used,
    );
    gauge(
        &mut out,
        "tornas_budget_min_free_bytes",
        "Configured minimum free disk",
        b.min_free,
    );
    gauge(
        &mut out,
        "tornas_disk_free_bytes",
        "Free bytes on the torrents filesystem",
        b.disk_free,
    );
    gauge(
        &mut out,
        "tornas_disk_total_bytes",
        "Size of the torrents filesystem",
        b.disk_total,
    );
    gauge(
        &mut out,
        "tornas_disk_below_min_free",
        "1 when free disk is under the configured minimum",
        u8::from(b.disk_free < b.min_free),
    );
    gauge(
        &mut out,
        "tornas_warnings",
        "Active operational warnings",
        status.warnings.len(),
    );

    // ---- library aggregates
    gauge(
        &mut out,
        "tornas_movies",
        "Movies in the library",
        status.movies.len(),
    );
    gauge(
        &mut out,
        "tornas_movies_protected",
        "Movies inside the stream grace window, not evictable",
        status.movies.iter().filter(|m| m.protected).count(),
    );
    let mut by_private: BTreeMap<&str, (u64, u64)> =
        [("true", (0, 0)), ("false", (0, 0))].into_iter().collect();
    let mut by_state: BTreeMap<&str, u64> = [
        "checking",
        "downloading",
        "seeding",
        "done",
        "paused",
        "error",
    ]
    .into_iter()
    .map(|s| (s, 0))
    .collect();
    for f in &facts {
        let e = by_private
            .entry(if f.private { "true" } else { "false" })
            .or_default();
        e.0 += 1;
        e.1 += f.size_bytes;
        *by_state.entry(f.state).or_default() += 1;
    }
    family(
        &mut out,
        "tornas_torrents",
        "gauge",
        "Loaded torrents, by BEP 27 private flag",
    );
    for (p, (n, _)) in &by_private {
        sample(&mut out, "tornas_torrents", &[("private", p)], n);
    }
    family(
        &mut out,
        "tornas_torrents_size_bytes",
        "gauge",
        "Whole-torrent size of loaded torrents, by private flag",
    );
    for (p, (_, bytes)) in &by_private {
        sample(
            &mut out,
            "tornas_torrents_size_bytes",
            &[("private", p)],
            bytes,
        );
    }
    family(
        &mut out,
        "tornas_torrents_by_state",
        "gauge",
        "Loaded torrents by state",
    );
    for (s, n) in &by_state {
        sample(&mut out, "tornas_torrents_by_state", &[("state", s)], n);
    }

    // ---- per torrent
    family(
        &mut out,
        "tornas_torrent_info",
        "gauge",
        "Descriptive labels for a torrent; always 1. Join on imdb_id.",
    );
    for f in &facts {
        sample(
            &mut out,
            "tornas_torrent_info",
            &[
                ("imdb_id", &f.imdb_id),
                ("info_hash", &f.info_hash),
                ("title", &f.title),
                ("state", f.state),
                ("private", if f.private { "true" } else { "false" }),
            ],
            1,
        );
    }
    type Getter = fn(&crate::engine::TorrentFacts) -> f64;
    let per: &[(&str, &str, &str, Getter)] = &[
        (
            "tornas_torrent_size_bytes",
            "gauge",
            "Size of all files in the torrent",
            |f| f.size_bytes as f64,
        ),
        (
            "tornas_torrent_selected_bytes",
            "gauge",
            "Bytes selected for download (the video file)",
            |f| f.selected_bytes as f64,
        ),
        (
            "tornas_torrent_progress_bytes",
            "gauge",
            "Verified bytes of the selection on disk",
            |f| f.progress_bytes as f64,
        ),
        (
            "tornas_torrent_progress_ratio",
            "gauge",
            "Download progress of the selection, 0..1",
            |f| {
                if f.selected_bytes == 0 {
                    0.0
                } else {
                    f.progress_bytes as f64 / f.selected_bytes as f64
                }
            },
        ),
        (
            "tornas_torrent_piece_length_bytes",
            "gauge",
            "Piece size",
            |f| f.piece_length as f64,
        ),
        (
            "tornas_torrent_pieces",
            "gauge",
            "Pieces in the whole torrent",
            |f| f.pieces as f64,
        ),
        (
            "tornas_torrent_pieces_verified",
            "gauge",
            "Pieces downloaded and hash-checked this session",
            |f| f.pieces_verified as f64,
        ),
        (
            "tornas_torrent_files",
            "gauge",
            "Files in the torrent",
            |f| f.files as f64,
        ),
        (
            "tornas_torrent_fetched_bytes_total",
            "counter",
            "Bytes received from peers this session, including discarded",
            |f| f.fetched_bytes as f64,
        ),
        (
            "tornas_torrent_uploaded_bytes_total",
            "counter",
            "Bytes sent to peers this session",
            |f| f.uploaded_bytes as f64,
        ),
        (
            "tornas_torrent_download_bytes_per_second",
            "gauge",
            "Current download rate",
            |f| f.download_bps as f64,
        ),
        (
            "tornas_torrent_upload_bytes_per_second",
            "gauge",
            "Current upload rate",
            |f| f.upload_bps as f64,
        ),
        (
            "tornas_torrent_idle_seconds",
            "gauge",
            "Seconds since last streamed or added; eviction is least recently used first",
            |f| f.idle_secs as f64,
        ),
    ];
    for (name, kind, help, get) in per {
        family(&mut out, name, kind, help);
        for f in &facts {
            sample(&mut out, name, &[("imdb_id", &f.imdb_id)], get(f));
        }
    }
    family(
        &mut out,
        "tornas_torrent_piece_download_seconds",
        "gauge",
        "Average time to download one piece",
    );
    for f in &facts {
        if let Some(s) = f.piece_download_secs_avg {
            sample(
                &mut out,
                "tornas_torrent_piece_download_seconds",
                &[("imdb_id", &f.imdb_id)],
                format!("{s:.4}"),
            );
        }
    }
    family(
        &mut out,
        "tornas_torrent_eta_seconds",
        "gauge",
        "Estimated seconds to finish at the current rate",
    );
    for f in &facts {
        if let Some(s) = f.eta_secs {
            sample(
                &mut out,
                "tornas_torrent_eta_seconds",
                &[("imdb_id", &f.imdb_id)],
                s,
            );
        }
    }
    family(
        &mut out,
        "tornas_torrent_peers",
        "gauge",
        "Peers known to a torrent, by state",
    );
    for f in &facts {
        for (state, n) in f.peers {
            sample(
                &mut out,
                "tornas_torrent_peers",
                &[("imdb_id", &f.imdb_id), ("state", state)],
                n,
            );
        }
    }
    family(
        &mut out,
        "tornas_torrent_peers_live",
        "gauge",
        "Connected peers of a torrent, by transport",
    );
    for f in &facts {
        for (transport, n) in f.peers_live_by_transport {
            sample(
                &mut out,
                "tornas_torrent_peers_live",
                &[("imdb_id", &f.imdb_id), ("transport", transport)],
                n,
            );
        }
    }
    family(
        &mut out,
        "tornas_torrent_trackers",
        "gauge",
        "Trackers attached to a torrent, by URL scheme",
    );
    for f in &facts {
        for (scheme, n) in &f.trackers_by_scheme {
            sample(
                &mut out,
                "tornas_torrent_trackers",
                &[("imdb_id", &f.imdb_id), ("scheme", scheme)],
                n,
            );
        }
    }

    // ---- session, from the typed snapshot
    let snap = engine.session.stats_snapshot();
    family(
        &mut out,
        "tornas_fetched_bytes_total",
        "counter",
        "Bytes received from peers across all torrents",
    );
    sample(
        &mut out,
        "tornas_fetched_bytes_total",
        &[],
        snap.counters.fetched_bytes,
    );
    family(
        &mut out,
        "tornas_uploaded_bytes_total",
        "counter",
        "Bytes sent to peers across all torrents",
    );
    sample(
        &mut out,
        "tornas_uploaded_bytes_total",
        &[],
        snap.counters.uploaded_bytes,
    );
    family(
        &mut out,
        "tornas_blocked_connections_total",
        "counter",
        "Peer connections refused by the blocklist or allowlist",
    );
    sample(
        &mut out,
        "tornas_blocked_connections_total",
        &[("direction", "incoming")],
        snap.counters.blocked_incoming,
    );
    sample(
        &mut out,
        "tornas_blocked_connections_total",
        &[("direction", "outgoing")],
        snap.counters.blocked_outgoing,
    );
    gauge(
        &mut out,
        "tornas_download_bytes_per_second",
        "Aggregate download rate",
        snap.download_speed.as_bytes(),
    );
    gauge(
        &mut out,
        "tornas_upload_bytes_per_second",
        "Aggregate upload rate",
        snap.upload_speed.as_bytes(),
    );
    let p = &snap.peers;
    family(
        &mut out,
        "tornas_peers",
        "gauge",
        "Peers across all torrents, by state",
    );
    for (state, n) in [
        ("queued", p.queued),
        ("connecting", p.connecting),
        ("live", p.live),
        ("seen", p.seen),
        ("dead", p.dead),
        ("not_needed", p.not_needed),
    ] {
        sample(&mut out, "tornas_peers", &[("state", state)], n);
    }
    family(
        &mut out,
        "tornas_peers_live",
        "gauge",
        "Connected peers across all torrents, by transport",
    );
    for (transport, n) in [
        ("tcp", p.live_tcp),
        ("utp", p.live_utp),
        ("socks", p.live_socks),
    ] {
        sample(
            &mut out,
            "tornas_peers_live",
            &[("transport", transport)],
            n,
        );
    }
    family(
        &mut out,
        "tornas_peer_steals_total",
        "counter",
        "Pieces reassigned from a slow peer to a faster one",
    );
    sample(&mut out, "tornas_peer_steals_total", &[], p.steals);
    family(
        &mut out,
        "tornas_peer_connections_total",
        "counter",
        "Outgoing peer connection attempts and their outcome, by transport and address family",
    );
    let c = &snap.connections;
    for (transport, fam) in [("tcp", &c.tcp), ("utp", &c.utp), ("socks", &c.socks)] {
        for (family_name, st) in [("v4", &fam.v4), ("v6", &fam.v6)] {
            for (outcome, n) in [
                ("attempt", st.attempts),
                ("success", st.successes),
                ("error", st.errors),
            ] {
                sample(
                    &mut out,
                    "tornas_peer_connections_total",
                    &[
                        ("transport", transport),
                        ("family", family_name),
                        ("outcome", outcome),
                    ],
                    n,
                );
            }
        }
    }

    // ---- DHT (UDP)
    let dht = engine.dht_stats();
    gauge(
        &mut out,
        "tornas_dht_enabled",
        "1 when the DHT is running",
        u8::from(dht.is_some()),
    );
    if let Some((v4, v6, outstanding)) = dht {
        family(
            &mut out,
            "tornas_dht_nodes",
            "gauge",
            "Nodes in the DHT routing table, by address family",
        );
        sample(&mut out, "tornas_dht_nodes", &[("family", "v4")], v4);
        sample(&mut out, "tornas_dht_nodes", &[("family", "v6")], v6);
        gauge(
            &mut out,
            "tornas_dht_outstanding_requests",
            "DHT queries awaiting a reply",
            outstanding,
        );
    }

    // ---- public tracker feed
    let feed = engine.trackers.state();
    gauge(
        &mut out,
        "tornas_trackers_enabled",
        "1 when the public tracker feed is on",
        u8::from(engine.trackers.config.read().enabled),
    );
    let mut schemes: BTreeMap<String, u64> = BTreeMap::new();
    for t in engine.trackers.current() {
        if let Some(s) = t.split("://").next() {
            *schemes.entry(s.to_owned()).or_default() += 1;
        }
    }
    family(
        &mut out,
        "tornas_trackers_active",
        "gauge",
        "Public trackers currently handed to new torrents, by URL scheme",
    );
    for (scheme, n) in &schemes {
        sample(&mut out, "tornas_trackers_active", &[("scheme", scheme)], n);
    }
    if let Some(ts) = feed.updated_at {
        gauge(
            &mut out,
            "tornas_tracker_list_age_seconds",
            "Seconds since the tracker list was last refreshed successfully",
            crate::units::now_secs() - ts,
        );
    }
    gauge(
        &mut out,
        "tornas_tracker_list_rejected",
        "Entries dropped by the scheme filter, block list or IP rule in the last refresh",
        feed.rejected,
    );
    gauge(
        &mut out,
        "tornas_tracker_list_deduplicated",
        "Duplicate entries collapsed in the last refresh",
        feed.deduplicated,
    );
    family(
        &mut out,
        "tornas_tracker_source_up",
        "gauge",
        "1 when the last fetch of a tracker list source succeeded",
    );
    for s in &feed.sources {
        let up = s.enabled && s.last_error.is_none() && s.last_ok_at.is_some();
        sample(
            &mut out,
            "tornas_tracker_source_up",
            &[("source", &s.name)],
            u8::from(up),
        );
    }
    family(
        &mut out,
        "tornas_tracker_source_accepted",
        "gauge",
        "Trackers a source contributed after filtering",
    );
    for s in &feed.sources {
        sample(
            &mut out,
            "tornas_tracker_source_accepted",
            &[("source", &s.name)],
            s.accepted,
        );
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_label_values() {
        let mut out = String::new();
        sample(&mut out, "m", &[("t", "a \"b\" \\c\nd")], 1);
        assert_eq!(out, "m{t=\"a \\\"b\\\" \\\\c\\nd\"} 1\n");
    }

    #[test]
    fn no_labels() {
        let mut out = String::new();
        sample(&mut out, "m", &[], 2.5);
        assert_eq!(out, "m 2.5\n");
    }
}
