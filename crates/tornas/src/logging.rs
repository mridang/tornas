//! Logging. The daemon logs to stdout; systemd (journald) or Docker capture it and
//! own retention. When OTLP export is on, logs and spans also go to the collector.

use anyhow::anyhow;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::telemetry::Telemetry;

/// Install the global subscriber. `filter` is a `tracing` env-filter directive such
/// as `info` or `tornas=debug,librqbit=info`. `telemetry` adds OTLP layers when
/// export is on. Safe to call once.
pub fn init(filter: &str, telemetry: &Telemetry) -> anyhow::Result<()> {
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));

    let spans = telemetry
        .tracer()
        .map(|tracer| tracing_opentelemetry::layer().with_tracer(tracer));
    let logs = telemetry
        .logger_provider()
        .map(OpenTelemetryTracingBridge::new);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(spans)
        .with(logs)
        .try_init()
        .map_err(|e| anyhow!("installing logger: {e}"))
}
