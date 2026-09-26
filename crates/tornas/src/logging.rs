//! Logging. Under systemd the daemon logs to journald with structured fields;
//! otherwise (interactive use, dev, non-systemd hosts) it logs to the console.
//! Retention, rotation and remote shipping are journald's and the OTLP collector's
//! job, not this process's.

use anyhow::anyhow;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Install the global subscriber. `filter` is a `tracing` env-filter directive such
/// as `info` or `tornas=debug,librqbit=info`. Safe to call once.
pub fn init(filter: &str) -> anyhow::Result<()> {
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(env_filter);

    // Prefer journald when systemd started us: it captures the structured fields and
    // owns retention. Logging to stdout as well would double up, since systemd also
    // captures stdout. Everywhere else, the console is what a person wants.
    if crate::service::systemd::is_managed()
        && let Ok(journald) = tracing_journald::layer()
    {
        return registry
            .with(journald)
            .try_init()
            .map_err(|e| anyhow!("installing journald logger: {e}"));
    }

    registry
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .try_init()
        .map_err(|e| anyhow!("installing console logger: {e}"))
}
