//! The row structs Diesel reads and writes, and their conversion to the domain types
//! in [`store`](crate::media_catalog::store).
//!
//! SQLite stores integers as i64 and has no JSON or unsigned types, so these mirror
//! the columns in native types; the conversions handle the JSON `genres` blob and the
//! `i64` ↔ `u64`/`usize`/`u32` narrowing.

use diesel::prelude::*;

use super::schema::{movies, torrents};
use crate::media_catalog::store::{Movie, TorrentRow};

#[derive(Queryable)]
pub(super) struct MovieRecord {
    imdb_id: String,
    tmdb_id: Option<i64>,
    title: String,
    year: Option<i32>,
    overview: Option<String>,
    poster_url: Option<String>,
    backdrop_url: Option<String>,
    runtime_min: Option<i64>,
    genres: String,
    rating: Option<f64>,
    #[allow(dead_code)]
    tmdb_json: Option<String>,
    added_at: i64,
    last_used_at: i64,
}

impl From<MovieRecord> for Movie {
    fn from(r: MovieRecord) -> Self {
        Movie {
            imdb_id: r.imdb_id,
            tmdb_id: r.tmdb_id,
            title: r.title,
            year: r.year,
            overview: r.overview,
            poster_url: r.poster_url,
            backdrop_url: r.backdrop_url,
            runtime_min: r.runtime_min,
            genres: serde_json::from_str(&r.genres).unwrap_or_default(),
            rating: r.rating,
            added_at: r.added_at,
            last_used_at: r.last_used_at,
        }
    }
}

#[derive(Insertable)]
#[diesel(table_name = movies)]
pub(super) struct MovieInsert<'a> {
    pub imdb_id: &'a str,
    pub tmdb_id: Option<i64>,
    pub title: &'a str,
    pub year: Option<i32>,
    pub overview: Option<&'a str>,
    pub poster_url: Option<&'a str>,
    pub backdrop_url: Option<&'a str>,
    pub runtime_min: Option<i64>,
    pub genres: String,
    pub rating: Option<f64>,
    pub tmdb_json: Option<&'a str>,
    pub added_at: i64,
    pub last_used_at: i64,
}

impl<'a> MovieInsert<'a> {
    pub fn new(m: &'a Movie, tmdb_json: Option<&'a str>) -> anyhow::Result<Self> {
        Ok(Self {
            imdb_id: &m.imdb_id,
            tmdb_id: m.tmdb_id,
            title: &m.title,
            year: m.year,
            overview: m.overview.as_deref(),
            poster_url: m.poster_url.as_deref(),
            backdrop_url: m.backdrop_url.as_deref(),
            runtime_min: m.runtime_min,
            genres: serde_json::to_string(&m.genres)?,
            rating: m.rating,
            tmdb_json,
            added_at: m.added_at,
            last_used_at: m.last_used_at,
        })
    }
}

#[derive(Queryable)]
pub(super) struct TorrentRecord {
    info_hash: String,
    imdb_id: String,
    magnet: String,
    size_bytes: i64,
    video_file_idx: i64,
    video_file_name: String,
    added_at: i64,
    private: bool,
    download_limit: Option<i64>,
    upload_limit: Option<i64>,
    peer_limit: Option<i64>,
    completed_at: Option<i64>,
}

impl From<TorrentRecord> for TorrentRow {
    fn from(r: TorrentRecord) -> Self {
        TorrentRow {
            info_hash: r.info_hash,
            imdb_id: r.imdb_id,
            magnet: r.magnet,
            size_bytes: r.size_bytes as u64,
            video_file_idx: r.video_file_idx as usize,
            video_file_name: r.video_file_name,
            added_at: r.added_at,
            private: r.private,
            download_limit: r.download_limit.map(|v| v as u32),
            upload_limit: r.upload_limit.map(|v| v as u32),
            peer_limit: r.peer_limit.map(|v| v as u32),
            completed_at: r.completed_at,
        }
    }
}

/// Inserted columns for a torrent. `completed_at` is deliberately absent so that
/// re-adding a torrent (an `INSERT OR REPLACE`) resets it to NULL, i.e. "downloading
/// again".
#[derive(Insertable)]
#[diesel(table_name = torrents)]
pub(super) struct TorrentInsert<'a> {
    pub info_hash: &'a str,
    pub imdb_id: &'a str,
    pub magnet: &'a str,
    pub size_bytes: i64,
    pub video_file_idx: i64,
    pub video_file_name: &'a str,
    pub added_at: i64,
    pub private: bool,
    pub download_limit: Option<i64>,
    pub upload_limit: Option<i64>,
    pub peer_limit: Option<i64>,
}

impl<'a> From<&'a TorrentRow> for TorrentInsert<'a> {
    fn from(t: &'a TorrentRow) -> Self {
        Self {
            info_hash: &t.info_hash,
            imdb_id: &t.imdb_id,
            magnet: &t.magnet,
            size_bytes: t.size_bytes as i64,
            video_file_idx: t.video_file_idx as i64,
            video_file_name: &t.video_file_name,
            added_at: t.added_at,
            private: t.private,
            download_limit: t.download_limit.map(i64::from),
            upload_limit: t.upload_limit.map(i64::from),
            peer_limit: t.peer_limit.map(i64::from),
        }
    }
}
