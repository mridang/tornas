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

use crate::{tmdb::Tmdb, utils::now_secs};
use eviction::{Candidate, Plan, PlanError};

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
