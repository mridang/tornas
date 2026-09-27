//! The media library: the movie [`store`](self::store), the disk-[`eviction`]
//! rule, and the metadata client, in one place.
//!
//! [`MediaCatalog`] owns the SQLite store, the TMDB client, and the disk-budget
//! settings. It decides *which* torrents to evict (pure planning); the engine
//! carries the plan out, because only it knows librqbit.

pub mod eviction;
pub mod store;

use std::{path::Path, time::Duration};

use anyhow::Context;

pub use store::{Catalog, Event, Movie, TorrentRow};

use crate::utils::now_secs;
use eviction::{Candidate, Plan, PlanError};
use tmdb::Tmdb;

/// The library: what movies exist, what backs them, and the budget that decides
/// which get evicted.
pub struct MediaCatalog {
    store: Catalog,
    tmdb: Option<Tmdb>,
    budget: u64,
    min_free: u64,
    stream_grace: Duration,
}

impl MediaCatalog {
    /// Open the store at `db_path` and build the library. `budget` is the disk
    /// budget, `min_free` the free-space floor, `stream_grace` the window during
    /// which a just-streamed movie is safe from eviction.
    pub fn new(
        db_path: impl AsRef<Path>,
        tmdb: Option<Tmdb>,
        budget: u64,
        min_free: u64,
        stream_grace: Duration,
    ) -> anyhow::Result<Self> {
        let db_path = db_path.as_ref();
        let store =
            Catalog::open(db_path).with_context(|| format!("opening catalog {db_path:?}"))?;
        Ok(Self {
            store,
            tmdb,
            budget,
            min_free,
            stream_grace,
        })
    }

    /// The movie store, for the CRUD the engine and adapters need.
    pub fn store(&self) -> &Catalog {
        &self.store
    }

    /// The TMDB client, when credentials were configured.
    pub fn tmdb(&self) -> Option<&Tmdb> {
        self.tmdb.as_ref()
    }

    pub fn budget(&self) -> u64 {
        self.budget
    }

    pub fn min_free(&self) -> u64 {
        self.min_free
    }

    /// Whether an item last used at `last_used_at` is inside the stream-grace
    /// window and therefore must not be evicted.
    pub fn is_protected(&self, last_used_at: i64) -> bool {
        now_secs() - last_used_at < self.stream_grace.as_secs() as i64
    }

    /// Every torrent as an eviction candidate, and the total bytes they occupy.
    pub fn candidates(&self) -> anyhow::Result<(Vec<Candidate>, u64)> {
        let movies = self.store.list_movies()?;
        let torrents = self.store.list_torrents()?;
        let mut used = 0u64;
        let mut out = Vec::new();
        for t in torrents {
            used += t.size_bytes;
            let m = movies.iter().find(|m| m.imdb_id == t.imdb_id);
            let last = m.map(|m| m.last_used_at).unwrap_or(t.added_at);
            out.push(Candidate {
                key: t.info_hash.clone(),
                title: m
                    .map(|m| m.title.clone())
                    .unwrap_or_else(|| t.imdb_id.clone()),
                size: t.size_bytes,
                last_used_at: last,
                protected: self.is_protected(last),
            });
        }
        Ok((out, used))
    }

    /// Decide what to drop so `incoming` more bytes fit under the budget while
    /// keeping `min_free` free on the filesystem.
    pub fn plan(
        &self,
        candidates: &[Candidate],
        used: u64,
        disk_free: u64,
        incoming: u64,
    ) -> Result<Plan, PlanError> {
        let extra = (incoming + self.min_free).saturating_sub(disk_free);
        eviction::plan(candidates, used, incoming, self.budget, extra)
    }
}

/// One piece of playable media, as plain data — everything a playback protocol
/// (DLNA, Stremio, and later others) could want, with no live download state.
#[derive(Debug, Clone)]
pub struct MediaEntry {
    pub id: String,
    pub title: String,
    pub year: Option<i32>,
    pub poster: Option<String>,
    pub backdrop: Option<String>,
    pub genres: Vec<String>,
    pub rating: Option<f64>,
    pub overview: Option<String>,
    pub runtime_min: Option<i64>,
    pub file_name: String,
    pub file_size: u64,
}

impl MediaEntry {
    fn new(m: &Movie, t: &TorrentRow) -> Self {
        Self {
            id: m.imdb_id.clone(),
            title: m.title.clone(),
            year: m.year,
            poster: m.poster_url.clone(),
            backdrop: m.backdrop_url.clone(),
            genres: m.genres.clone(),
            rating: m.rating,
            overview: m.overview.clone(),
            runtime_min: m.runtime_min,
            file_name: t.video_file_name.clone(),
            file_size: t.size_bytes,
        }
    }

    /// `Title (Year)`, or just the title when the year is unknown.
    pub fn display_title(&self) -> String {
        match self.year {
            Some(y) => format!("{} ({y})", self.title),
            None => self.title.clone(),
        }
    }

    /// The server-relative URL that streams this file, e.g. `/video/tt0111161/x.mp4`.
    pub fn video_path(&self) -> String {
        let file = url::form_urlencoded::byte_serialize(self.file_name.as_bytes())
            .collect::<String>()
            .replace('+', "%20");
        format!("/video/{}/{file}", self.id)
    }
}

/// The read model every playback protocol is built on: the completed media in the
/// library, as plain data. Each protocol adapter takes one of these and maps
/// [`MediaEntry`] to its own wire format — none of them touch the engine.
pub trait Library: Send + Sync {
    /// All fully-downloaded media, sorted by title.
    fn entries(&self) -> Vec<MediaEntry>;
    /// One fully-downloaded movie by IMDb id, or `None` if it is absent or still
    /// downloading.
    fn entry(&self, id: &str) -> Option<MediaEntry>;
}

impl Library for MediaCatalog {
    fn entries(&self) -> Vec<MediaEntry> {
        let movies = self.store.list_movies().unwrap_or_default();
        let torrents = self.store.list_torrents().unwrap_or_default();
        let mut out: Vec<MediaEntry> = torrents
            .iter()
            .filter(|t| t.completed_at.is_some())
            .filter_map(|t| {
                movies
                    .iter()
                    .find(|m| m.imdb_id == t.imdb_id)
                    .map(|m| MediaEntry::new(m, t))
            })
            .collect();
        out.sort_by(|a, b| a.title.cmp(&b.title));
        out
    }

    fn entry(&self, id: &str) -> Option<MediaEntry> {
        let t = self.store.torrent_for_movie(id).ok().flatten()?;
        t.completed_at?;
        let m = self.store.get_movie(id).ok().flatten()?;
        Some(MediaEntry::new(&m, &t))
    }
}
