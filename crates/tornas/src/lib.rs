//! tornas: a single-binary home media center built on librqbit.
//! Torrent client + Stremio addon + DLNA server + LRU disk budget.

/// `println!` for CLI output that exits quietly when stdout is closed early, as in
/// `tornas status | head`. SIGPIPE stays ignored process-wide on purpose: resetting
/// it would let a peer closing a socket kill the server.
#[macro_export]
macro_rules! outln {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = writeln!(std::io::stdout(), $($t)*) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
        }
    }};
}

pub mod adapters;
pub mod budget;
pub mod catalog;
pub mod cli;
pub mod config;
pub mod dlna;
pub mod engine;
pub mod fixtures;
pub mod health;
pub mod http;
pub mod logging;
pub mod mdns;
pub mod metrics;
pub mod schedule;
pub mod service;
pub mod stremio;
pub mod telemetry;
pub use service::systemd;
pub mod server;
pub mod tmdb;
pub mod trackers;
pub mod tuning;
pub mod update;
pub mod utils;

pub use server::run_server;
