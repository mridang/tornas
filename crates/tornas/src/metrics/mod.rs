//! Metrics, as OpenTelemetry instruments.
//!
//! Event counters and the HTTP histogram are synchronous instruments that
//! accumulate between scrapes (defined here). Everything describing current state
//! (budget, library, per-torrent, session, DHT, trackers, process) is an
//! **observable** instrument whose callback reads typed engine data at collection
//! time; those live in [`observe`], registered once the engine has started.
//!
//! The meter provider and its Prometheus reader live in [`telemetry`](crate::telemetry);
//! `/metrics` encodes that reader's registry, and when an OTLP endpoint is
//! configured the same instruments are pushed to the collector.

use std::{sync::OnceLock, time::Instant};

use anyhow::Context;
use opentelemetry::{
    KeyValue,
    metrics::{Counter, Histogram, Meter},
};

mod observe;
pub use observe::observe;

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
