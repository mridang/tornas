//! The catalog's domain types: a movie with its TMDB metadata, the torrent backing
//! it, and a log event. These are the read model the rest of the app sees; how they
//! are persisted is the [`orm`](super::orm) module's business.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Movie {
    pub imdb_id: String,
    pub tmdb_id: Option<i64>,
    pub title: String,
    pub year: Option<i32>,
    pub overview: Option<String>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub runtime_min: Option<i64>,
    pub genres: Vec<String>,
    pub rating: Option<f64>,
    pub added_at: i64,
    pub last_used_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentRow {
    pub info_hash: String,
    pub imdb_id: String,
    pub magnet: String,
    pub size_bytes: u64,
    pub video_file_idx: usize,
    pub video_file_name: String,
    pub added_at: i64,
    /// BEP 27 private torrent: never given public trackers.
    #[serde(default)]
    pub private: bool,
    /// Per-movie overrides, in bytes per second and peers. `None` uses the global value.
    #[serde(default)]
    pub download_limit: Option<u32>,
    #[serde(default)]
    pub upload_limit: Option<u32>,
    #[serde(default)]
    pub peer_limit: Option<u32>,
    /// When the download finished, as unix seconds; `None` while still downloading.
    #[serde(default)]
    pub completed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub ts: i64,
    pub kind: String,
    pub message: String,
}
