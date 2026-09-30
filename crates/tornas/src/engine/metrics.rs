//! The engine's own metrics: the synchronous event counters it increments as things
//! happen (adds, evictions, pauses, …), plus the observable gauges that read engine
//! state at scrape time (in [`observe`]). They live with the engine because the
//! engine is what records them; they build from the shared `tornas` meter, so
//! `/metrics` gathers them exactly as if they were declared centrally.

use std::sync::OnceLock;

use opentelemetry::{KeyValue, metrics::Counter};

use crate::metrics::meter;

mod observe;
pub use observe::observe;

struct Instruments {
    adds: Counter<u64>,
    evictions: Counter<u64>,
    evicted_bytes: Counter<u64>,
    tmdb_errors: Counter<u64>,
    removals: Counter<u64>,
    seeding_paused: Counter<u64>,
    stalled_evictions: Counter<u64>,
    pauses: Counter<u64>,
    resumes: Counter<u64>,
}

fn instruments() -> &'static Instruments {
    static I: OnceLock<Instruments> = OnceLock::new();
    I.get_or_init(|| {
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
            removals: counter("tornas_removals_total", "Movies removed through the API"),
            seeding_paused: counter(
                "tornas_seeding_paused_total",
                "Downloads that finished and were paused",
            ),
            stalled_evictions: counter(
                "tornas_stalled_evictions_total",
                "Downloads evicted after making no progress",
            ),
            pauses: counter("tornas_pauses_total", "Times everything was paused"),
            resumes: counter(
                "tornas_resumes_total",
                "Times a pause ended, by trigger (manual or auto)",
            ),
        }
    })
}

/// Seed the labelled counters at zero so the first scrape already carries the full
/// set. Call once, after the meter provider is installed.
pub fn install() {
    let i = instruments();
    for r in ["ok", "refused", "error"] {
        i.adds.add(0, &[KeyValue::new("result", r)]);
    }
    for t in ["manual", "auto"] {
        i.resumes.add(0, &[KeyValue::new("trigger", t)]);
    }
    for c in [
        &i.evictions,
        &i.evicted_bytes,
        &i.tmdb_errors,
        &i.removals,
        &i.seeding_paused,
        &i.stalled_evictions,
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
pub fn seeding_paused() {
    instruments().seeding_paused.add(1, &[]);
}
pub fn stalled_eviction() {
    instruments().stalled_evictions.add(1, &[]);
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
