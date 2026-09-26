//! Logging. Under systemd the daemon logs to journald with structured fields;
//! otherwise (interactive use, dev, non-systemd hosts) it logs to the console. When
//! OTLP export is on, logs and spans are also sent to the collector. Retention and
//! rotation are journald's job; remote shipping is the collector's.

use anyhow::anyhow;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::telemetry::Telemetry;

/// Install the global subscriber. `filter` is a `tracing` env-filter directive such
/// as `info` or `tornas=debug,librqbit=info`. `telemetry` adds OTLP layers when
/// export is on. Safe to call once.
pub fn init(filter: &str, telemetry: &Telemetry) -> anyhow::Result<()> {
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));

    // Exactly one of these is `Some`; `Option<Layer>` is a no-op when `None`, so the
    // whole stack composes without boxing or naming intermediate subscriber types.
    // Prefer journald under systemd (it captures structured fields and owns
    // retention, and stdout would double up); the console is for everywhere else.
    let (journald, console) = match crate::service::systemd::is_managed()
        .then(tracing_journald::layer)
        .and_then(Result::ok)
    {
        Some(journald) => (Some(journald), None),
        None => (
            None,
            Some(tracing_subscriber::fmt::layer().with_target(false)),
        ),
    };

    let spans = telemetry
        .tracer()
        .map(|tracer| tracing_opentelemetry::layer().with_tracer(tracer));
    let logs = telemetry
        .logger_provider()
        .map(OpenTelemetryTracingBridge::new);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(journald)
        .with(console)
        .with(spans)
        .with(logs)
        .try_init()
        .map_err(|e| anyhow!("installing logger: {e}"))
}
