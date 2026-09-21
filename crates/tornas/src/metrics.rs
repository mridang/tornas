//! Prometheus metrics. Counters are recorded through the `metrics` facade
//! (which also captures librqbit's uTP metrics); gauges that describe current
//! state are rendered fresh on every scrape from the engine's status.

use std::{fmt::Write, sync::OnceLock};

use metrics::{counter, describe_counter};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use crate::engine::{Engine, StatusView};

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the global recorder once per process. Safe to call repeatedly.
pub fn install() -> Option<&'static PrometheusHandle> {
    if let Some(h) = HANDLE.get() {
        return Some(h);
    }
    let handle = PrometheusBuilder::new().install_recorder().ok()?;
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
        "Video stream requests, by client hint (range or full)"
    );
    describe_counter!(
        "tornas_stream_bytes_total",
        "Bytes requested by video stream clients"
    );
    describe_counter!("tornas_removals_total", "Movies removed through the API");
    // Register every counter at zero so a scrape shows the full set before any event fires.
    for r in ["ok", "refused", "error"] {
        counter!("tornas_adds_total", "result" => r).absolute(0);
    }
    counter!("tornas_evictions_total").absolute(0);
    counter!("tornas_evicted_bytes_total").absolute(0);
    counter!("tornas_tmdb_errors_total").absolute(0);
    for k in ["range", "full"] {
        counter!("tornas_streams_total", "kind" => k).absolute(0);
    }
    counter!("tornas_stream_bytes_total").absolute(0);
    counter!("tornas_removals_total").absolute(0);
    counter!("tornas_seeding_paused_total").absolute(0);
    counter!("tornas_updates_installed_total").absolute(0);
    counter!("tornas_stalled_evictions_total").absolute(0);
    counter!("tornas_unauthorized_total").absolute(0);
    counter!("tornas_forbidden_source_total").absolute(0);
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
pub fn trackers<'a>(
    active: usize,
    sources: impl Iterator<Item = &'a crate::trackers::SourceStatus>,
) {
    metrics::gauge!("tornas_trackers_active").set(active as f64);
    for s in sources {
        let name = s.name.clone();
        metrics::gauge!("tornas_tracker_source_accepted", "source" => name.clone())
            .set(s.accepted as f64);
        metrics::gauge!("tornas_tracker_source_ok", "source" => name).set(
            if s.last_error.is_none() && s.last_ok_at.is_some() {
                1.0
            } else {
                0.0
            },
        );
    }
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

fn gauge(out: &mut String, name: &str, help: &str, value: impl std::fmt::Display) {
    let _ = writeln!(
        out,
        "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
    );
}

/// Full scrape body: our counters, our gauges, then librqbit's session counters.
pub fn render(engine: &Engine) -> anyhow::Result<String> {
    let status: StatusView = engine.status()?;
    let mut out = HANDLE.get().map(|h| h.render()).unwrap_or_default();
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
        "tornas_movies",
        "Movies in the library",
        status.movies.len(),
    );
    gauge(
        &mut out,
        "tornas_movies_protected",
        "Movies inside the stream grace window",
        status.movies.iter().filter(|m| m.protected).count(),
    );
    gauge(
        &mut out,
        "tornas_movies_downloading",
        "Movies not yet fully downloaded",
        status.movies.iter().filter(|m| !m.finished).count(),
    );
    gauge(
        &mut out,
        "tornas_uptime_seconds",
        "Seconds since the server started",
        status.session.uptime_secs,
    );
    gauge(
        &mut out,
        "tornas_session_download_bytes_per_second",
        "Aggregate download rate",
        status.session.download_bps,
    );
    gauge(
        &mut out,
        "tornas_session_upload_bytes_per_second",
        "Aggregate upload rate",
        status.session.upload_bps,
    );
    gauge(
        &mut out,
        "tornas_session_peers",
        "Connected peers",
        status.session.peers_live,
    );
    let now = crate::units::now_secs();
    let _ = writeln!(
        out,
        "# HELP tornas_movie_progress_ratio Download progress per movie\n# TYPE tornas_movie_progress_ratio gauge"
    );
    for m in &status.movies {
        let ratio = if m.total_bytes > 0 {
            m.progress_bytes as f64 / m.total_bytes as f64
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "tornas_movie_progress_ratio{{imdb_id=\"{}\",title=\"{}\",state=\"{}\"}} {ratio:.4}",
            m.movie.imdb_id,
            m.movie.title.replace('"', "'"),
            m.state
        );
    }
    let _ = writeln!(
        out,
        "# HELP tornas_movie_size_bytes Size of each movie's video file\n# TYPE tornas_movie_size_bytes gauge"
    );
    for m in &status.movies {
        let _ = writeln!(
            out,
            "tornas_movie_size_bytes{{imdb_id=\"{}\"}} {}",
            m.movie.imdb_id, m.total_bytes
        );
    }
    let _ = writeln!(
        out,
        "# HELP tornas_movie_idle_seconds Seconds since each movie was last streamed or added\n# TYPE tornas_movie_idle_seconds gauge"
    );
    for m in &status.movies {
        let _ = writeln!(
            out,
            "tornas_movie_idle_seconds{{imdb_id=\"{}\"}} {}",
            m.movie.imdb_id,
            now - m.movie.last_used_at
        );
    }
    let _ = writeln!(
        out,
        "# HELP tornas_movie_peers Live peers per movie\n# TYPE tornas_movie_peers gauge"
    );
    for m in &status.movies {
        let _ = writeln!(
            out,
            "tornas_movie_peers{{imdb_id=\"{}\"}} {}",
            m.movie.imdb_id, m.peers
        );
    }
    let _ = writeln!(
        out,
        "# HELP tornas_movie_download_bytes_per_second Download rate per movie\n# TYPE tornas_movie_download_bytes_per_second gauge"
    );
    for m in &status.movies {
        let _ = writeln!(
            out,
            "tornas_movie_download_bytes_per_second{{imdb_id=\"{}\"}} {}",
            m.movie.imdb_id, m.download_bps
        );
    }
    let _ = writeln!(
        out,
        "# HELP tornas_movie_upload_bytes_per_second Upload rate per movie\n# TYPE tornas_movie_upload_bytes_per_second gauge"
    );
    for m in &status.movies {
        let _ = writeln!(
            out,
            "tornas_movie_upload_bytes_per_second{{imdb_id=\"{}\"}} {}",
            m.movie.imdb_id, m.upload_bps
        );
    }
    engine.session.stats_snapshot().as_prometheus(&mut out);
    Ok(out)
}
