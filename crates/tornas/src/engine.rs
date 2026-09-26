//! The torrent engine: owns the librqbit session and the catalog, and is the only
//! part of the crate that knows how torrents work.
//!
//! This file holds the shared state and the small helpers; everything else is a
//! submodule, and each one adds its own `impl Engine` block.

pub mod add;
pub mod bandwidth;
pub mod disk;
pub mod eviction;
pub mod fault;
pub mod limits;
pub mod pause;
pub mod queue;
pub mod session;
pub mod stall;
pub mod trackers;
pub mod views;

pub use fault::{Fault, FaultKind, fault, fault_kind};
pub use pause::{DiskAction, PauseReason, PauseState, PauseView, disk_action};
pub use views::{AddMovieRequest, BudgetView, MovieView, SessionView, StatusView};

use std::{path::PathBuf, sync::Arc, time::Instant};

use librqbit::Session;
use librqbit::{ManagedTorrent, dht::Id20};
pub(crate) type ManagedTorrentHandle = Arc<ManagedTorrent>;

use crate::{
    config::{FileConfig, ServerOpts},
    media_catalog::MediaCatalog,
    trackers::TrackerFeed,
};

pub struct Engine {
    pub session: Arc<Session>,
    /// The movie store, eviction policy and metadata client. Shared so other
    /// subsystems (DLNA, for one) can read the library without holding the whole
    /// engine.
    pub library: Arc<MediaCatalog>,
    pub opts: ServerOpts,
    pub file_config: FileConfig,
    pub acl: crate::http::netacl::Acl,
    pub trackers: TrackerFeed,
    pub torrents_dir: PathBuf,
    /// Saved .torrent files for movies added from a file rather than a magnet.
    meta_dir: PathBuf,
    /// librqbit's session state, including each torrent's saved piece map.
    session_dir: PathBuf,
    started: Instant,
    weak: parking_lot::RwLock<std::sync::Weak<Engine>>,
    /// info_hash -> (progress bytes, when it last changed), for stall detection.
    progress_seen: parking_lot::Mutex<std::collections::HashMap<String, (u64, i64)>>,
    /// Global kill switch. `Some` while everything is paused.
    pause: parking_lot::Mutex<Option<PauseState>>,
    /// Set while the data disk is missing (only watched with --require-mount).
    disk_missing: std::sync::atomic::AtomicBool,
    /// Serialises add + evict so two concurrent adds cannot both pass the budget check.
    add_lock: tokio::sync::Mutex<()>,
    /// Machine-dependent limits actually in force.
    pub tuning: crate::tuning::Tuning,
    pub blocklist: crate::tuning::IpListStatus,
    pub allowlist: crate::tuning::IpListStatus,
    /// Compiled `[bandwidth]` windows and the index of the one in force.
    bandwidth: Vec<crate::schedule::Window>,
    bandwidth_active: parking_lot::Mutex<Option<usize>>,
}

fn hash_hex(h: Id20) -> String {
    h.as_string()
}

/// One word for a torrent's state, shared by the API, the TUI and metrics.
pub fn state_label(stats: &librqbit::TorrentStats) -> &'static str {
    match &stats.state {
        librqbit::TorrentStatsState::Initializing { .. } => "checking",
        librqbit::TorrentStatsState::Live if stats.finished => "seeding",
        librqbit::TorrentStatsState::Live => "downloading",
        librqbit::TorrentStatsState::Paused if stats.finished => "done",
        librqbit::TorrentStatsState::Paused => "paused",
        librqbit::TorrentStatsState::Error => "error",
    }
}

/// Everything the metrics endpoint reports about one torrent. Peer addresses are
/// deliberately absent: they would be unbounded label values.
#[derive(Debug, Clone, Default)]
pub struct TorrentFacts {
    pub imdb_id: String,
    pub info_hash: String,
    pub title: String,
    pub state: &'static str,
    pub private: bool,
    pub size_bytes: u64,
    pub selected_bytes: u64,
    pub progress_bytes: u64,
    pub piece_length: u64,
    pub pieces: u64,
    pub pieces_verified: u64,
    pub files: u64,
    pub fetched_bytes: u64,
    pub uploaded_bytes: u64,
    pub download_bps: u64,
    pub upload_bps: u64,
    /// queued, connecting, live, seen, dead, not_needed
    pub peers: [(&'static str, u64); 6],
    /// tcp, utp, socks
    pub peers_live_by_transport: [(&'static str, u64); 3],
    pub piece_download_secs_avg: Option<f64>,
    pub eta_secs: Option<u64>,
    pub idle_secs: i64,
    pub trackers_by_scheme: std::collections::BTreeMap<String, u64>,
}

pub fn is_video(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["mp4", "mkv", "avi", "mov", "webm", "m4v", "ts", "wmv"]
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")))
}
