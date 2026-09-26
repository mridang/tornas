//! Helpers shared by the integration tests: a fake TMDB, server options built
//! through the real argument parser, and polling helpers.
#![allow(dead_code)]

use std::{net::SocketAddr, path::Path, time::Duration};

use axum::{Router, extract::Path as AxPath, routing::get};
use clap::Parser;
use serde_json::json;
use tornas::{config::ServerOpts, engine::Engine};

pub async fn fake_tmdb() -> SocketAddr {
    let app = Router::new()
        .route(
            "/3/find/{imdb}",
            get(|AxPath(imdb): AxPath<String>| async move {
                let n: i64 = imdb.trim_start_matches("tt").parse().unwrap_or(1);
                axum::Json(json!({ "movie_results": [{ "id": n }] }))
            }),
        )
        .route(
            "/3/movie/{id}",
            get(|AxPath(id): AxPath<i64>| async move {
                axum::Json(json!({
                    "title": format!("Movie {id}"), "release_date": "2001-01-01", "overview": "dummy",
                    "poster_path": "/p.jpg", "runtime": 90, "genres": [{"name": "Test"}], "vote_average": 7.0
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

pub fn server_opts(data_dir: &Path, tmdb: SocketAddr, budget: u64) -> ServerOpts {
    // Built through the real argument parser, so tests get production defaults and
    // new settings never break this helper.
    let args = [
        "tornas".to_owned(),
        "server".to_owned(),
        format!("--data-dir={}", data_dir.display()),
        format!("--disk-budget={budget}"),
        "--min-free=0".to_owned(),
        "--stream-grace=0s".to_owned(),
        "--pause-duration=2s".to_owned(),
        "--stall-timeout=0s".to_owned(),
        "--sweep-interval=1h".to_owned(),
        "--http-listen=127.0.0.1:0".to_owned(),
        "--mdns-name=test".to_owned(),
        "--disable-mdns".to_owned(),
        "--disable-trackers".to_owned(),
        "--disable-dlna".to_owned(),
        "--disable-dht".to_owned(),
        "--disable-upnp-port-forward".to_owned(),
        "--tmdb-token=test".to_owned(),
        format!("--tmdb-base-url=http://{tmdb}/3"),
    ];
    match tornas::config::Cli::try_parse_from(args)
        .expect("test options parse")
        .cmd
    {
        tornas::config::Command::Server(o) => o,
        _ => unreachable!(),
    }
}

pub async fn wait_finished(engine: &Engine, imdb: &str) {
    for i in 0..600 {
        let v = engine.get_movie(imdb).unwrap();
        if v.as_ref().map(|m| m.finished).unwrap_or(false) {
            return;
        }
        if i % 20 == 0 {
            eprintln!(
                "waiting {imdb}: {:?}",
                v.map(|m| (m.state, m.progress_bytes, m.total_bytes, m.peers))
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{imdb} never finished downloading");
}

pub fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let m = e.metadata().unwrap();
            total += if m.is_dir() {
                dir_size(&e.path())
            } else {
                m.len()
            };
        }
    }
    total
}

/// Poll until a movie reaches one of `states`, or panic after `secs`.
pub async fn wait_state(engine: &Engine, imdb: &str, states: &[&str], secs: u64) -> String {
    let mut last = String::new();
    for _ in 0..(secs * 10) {
        if let Some(m) = engine.get_movie(imdb).unwrap() {
            last = m.state.clone();
            if states.contains(&m.state.as_str()) {
                return last;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{imdb} never reached {states:?}; last state {last:?}");
}

// ---- test-only log capture -------------------------------------------------
//
// Some tests assert on library log output (e.g. that a torrent is not re-hashed).
// Rather than ship an in-memory ring in the product, the capture lives here.

use std::sync::{Mutex, OnceLock};

static CAPTURE: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn captured() -> &'static Mutex<Vec<String>> {
    CAPTURE.get_or_init(|| Mutex::new(Vec::new()))
}

struct CaptureLayer;

struct MessageVisitor<'a>(&'a mut String);
impl tracing::field::Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut msg = String::new();
        event.record(&mut MessageVisitor(&mut msg));
        captured().lock().unwrap().push(msg);
    }
}

/// Install a subscriber that captures log messages for [`log_count`]. Idempotent.
pub fn init_log_capture() {
    use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new("info,librqbit=info"))
        .with(CaptureLayer)
        .try_init();
}

/// How many captured messages so far contain `needle`.
pub fn log_count(needle: &str) -> usize {
    captured()
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains(needle))
        .count()
}

/// Install the Prometheus meter provider and metric instruments once for the whole
/// test process, the way `main` does at startup. Idempotent: only the first call
/// builds the provider (the global meter provider and the metrics registry are
/// process-wide singletons). OTLP export stays off (no endpoint).
pub fn init_telemetry() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let telemetry = tornas::telemetry::init(None, "tornas").expect("telemetry init");
        tornas::metrics::set_registry(telemetry.registry());
        tornas::metrics::install();
        // Leak it: the providers must outlive every test in the process.
        std::mem::forget(telemetry);
    });
}
