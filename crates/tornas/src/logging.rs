//! Logging: console or rotating file, text or JSON, plus an in-memory ring of
//! recent lines served at /api/logs so a box without journald can still be read.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Mutex, OnceLock},
};

use serde::Serialize;
use tracing::Subscriber;
use tracing_subscriber::{
    EnvFilter, Layer, layer::SubscriberExt, registry::LookupSpan, util::SubscriberInitExt,
};

#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    pub seq: u64,
    pub ts: i64,
    pub level: String,
    pub target: String,
    pub message: String,
}

const RING_CAPACITY: usize = 2000;
static RING: OnceLock<Mutex<(u64, VecDeque<LogLine>)>> = OnceLock::new();
static FILE_GUARD: OnceLock<tracing_appender::non_blocking::WorkerGuard> = OnceLock::new();

fn ring() -> &'static Mutex<(u64, VecDeque<LogLine>)> {
    RING.get_or_init(|| Mutex::new((0, VecDeque::with_capacity(RING_CAPACITY))))
}

/// Lines with seq > `since`, newest last, at most `limit`, optionally filtered by minimum level.
pub fn recent(since: u64, limit: usize, min_level: Option<tracing::Level>) -> Vec<LogLine> {
    let g = ring().lock().unwrap_or_else(|e| e.into_inner());
    g.1.iter()
        .filter(|l| l.seq > since)
        .filter(|l| min_level.map(|m| level_of(&l.level) <= m).unwrap_or(true))
        .rev()
        .take(limit)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn level_of(s: &str) -> tracing::Level {
    s.parse().unwrap_or(tracing::Level::INFO)
}

struct RingLayer;

struct MessageVisitor(String);
impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        } else {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            self.0.push_str(&format!("{}={:?}", field.name(), value));
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_owned();
        } else {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            self.0.push_str(&format!("{}={value}", field.name()));
        }
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for RingLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut v = MessageVisitor(String::new());
        event.record(&mut v);
        let meta = event.metadata();
        let mut g = ring().lock().unwrap_or_else(|e| e.into_inner());
        g.0 += 1;
        let seq = g.0;
        if g.1.len() >= RING_CAPACITY {
            g.1.pop_front();
        }
        g.1.push_back(LogLine {
            seq,
            ts: crate::units::now_secs(),
            level: meta.level().to_string(),
            target: meta.target().to_owned(),
            message: v.0,
        });
    }
}

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
    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(RingLayer);

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
