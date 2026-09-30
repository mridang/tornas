//! The shared metrics infrastructure: the `tornas` meter every subsystem records
//! against, and the `/metrics` scrape.
//!
//! There is no central instrument registry any more. Each subsystem owns the metrics
//! it records, next to the code that records them: the engine's event counters and
//! observable gauges in [`engine::metrics`](crate::engine::metrics), the stream
//! counters in [`adapters`](crate::adapters), and the HTTP request timing and
//! refusal counters in `http::middleware`. They all build from [`meter`], so they
//! register into the one provider whose Prometheus reader this module scrapes.
//!
//! The meter provider and its Prometheus reader live in [`o11y`](crate::o11y); when
//! an OTLP endpoint is configured the same instruments are also pushed to a collector.

use std::sync::OnceLock;

use anyhow::Context;
use opentelemetry::metrics::Meter;

/// The one meter every subsystem records against; its name is the single source of
/// truth for instrument ownership.
pub fn meter() -> Meter {
    opentelemetry::global::meter("tornas")
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
