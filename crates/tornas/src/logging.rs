//! Logging: console output, or daily-rotated files, in text or JSON.

use std::{path::Path, sync::OnceLock};

use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

static FILE_GUARD: OnceLock<tracing_appender::non_blocking::WorkerGuard> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    Text,
    Json,
}

/// Install the global subscriber. `file` enables daily-rotated files in that directory
/// (kept alongside console output). Safe to call once.
pub fn init(
    filter: &str,
    format: LogFormat,
    file: Option<&Path>,
    keep_files: usize,
) -> anyhow::Result<()> {
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(env_filter);

    let console_json = matches!(format, LogFormat::Json);
    let console = tracing_subscriber::fmt::layer().with_target(false);
    let console: Box<dyn Layer<_> + Send + Sync> = if console_json {
        Box::new(console.json().flatten_event(true))
    } else {
        Box::new(console)
    };

    let file_layer: Option<Box<dyn Layer<_> + Send + Sync>> = match file {
        Some(dir) => {
            std::fs::create_dir_all(dir)?;
            let appender = tracing_appender::rolling::Builder::new()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("tornas")
                .filename_suffix("log")
                .max_log_files(keep_files.max(1))
                .build(dir)?;
            let (nb, guard) = tracing_appender::non_blocking(appender);
            let _ = FILE_GUARD.set(guard);
            let l = tracing_subscriber::fmt::layer()
                .with_writer(nb)
                .with_ansi(false)
                .with_target(true);
            Some(if console_json {
                Box::new(l.json().flatten_event(true))
            } else {
                Box::new(l)
            })
        }
        None => None,
    };

    registry
        .with(console)
        .with(file_layer)
        .try_init()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
