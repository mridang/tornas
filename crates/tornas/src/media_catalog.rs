//! The media library: the movie [`store`](self::store), the disk-[`eviction`]
//! policy, and the metadata client, behind one configured facade.
//!
//! [`MediaCatalog`] owns the SQLite store and the pluggable eviction policy, and is
//! built through a fluent builder so the whole subsystem is configured in one
//! place. Eviction here is pure *planning* — deciding which torrents to drop;
//! executing that (removing torrents from the session) stays with the engine,
//! which is the only thing that knows librqbit.

pub mod eviction;
pub mod store;

use std::{path::Path, time::Duration};

use anyhow::Context;

pub use store::{Catalog, Event, Movie, TorrentRow};

use crate::{tmdb::Tmdb, utils::now_secs};
use eviction::{Candidate, EvictionPolicy, Need, Plan, PlanError};

/// The library subsystem: what movies exist, what backs them, and which get
/// evicted to stay under the disk budget.
pub struct MediaCatalog {
    store: Catalog,
    tmdb: Option<Tmdb>,
    eviction: Eviction,
}

/// The configured eviction behaviour: a pluggable [`EvictionPolicy`] plus the
/// budget, the free-space floor and the stream-grace window it works within.
pub struct Eviction {
    policy: Box<dyn EvictionPolicy>,
    budget: u64,
    min_free: u64,
    stream_grace: Duration,
}

impl MediaCatalog {
    /// Start building a library backed by the SQLite database at `db_path`.
    pub fn builder(db_path: impl AsRef<Path>) -> MediaCatalogBuilder {
        MediaCatalogBuilder {
            db_path: db_path.as_ref().to_path_buf(),
            tmdb: None,
            eviction: None,
        }
    }

    /// The movie store, for the CRUD the engine and adapters need.
    pub fn store(&self) -> &Catalog {
        &self.store
    }

    /// The TMDB client, when credentials were configured.
    pub fn tmdb(&self) -> Option<&Tmdb> {
        self.tmdb.as_ref()
    }

    /// The configured disk budget.
    pub fn budget(&self) -> u64 {
        self.eviction.budget
    }

    /// The configured minimum free disk space.
    pub fn min_free(&self) -> u64 {
        self.eviction.min_free
    }

    /// Whether an item last used at `last_used_at` is inside the stream-grace
    /// window and therefore must not be evicted.
    pub fn is_protected(&self, last_used_at: i64) -> bool {
        now_secs() - last_used_at < self.eviction.stream_grace.as_secs() as i64
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

    /// Apply the eviction policy: given the current candidates and free disk,
    /// decide what to drop so `incoming` more bytes fit under the budget while
    /// keeping `min_free` free on the filesystem.
    pub fn plan(
        &self,
        candidates: &[Candidate],
        used: u64,
        disk_free: u64,
        incoming: u64,
    ) -> Result<Plan, PlanError> {
        let extra = (incoming + self.eviction.min_free).saturating_sub(disk_free);
        let need = Need {
            used,
            incoming,
            limit: self.eviction.budget,
            extra_needed: extra,
        };
        self.eviction.policy.select(candidates, need)
    }
}

/// Builds a [`MediaCatalog`], opening the store on `build`.
pub struct MediaCatalogBuilder {
    db_path: std::path::PathBuf,
    tmdb: Option<Tmdb>,
    eviction: Option<Eviction>,
}

impl MediaCatalogBuilder {
    /// The metadata client (movies are catalogued by IMDb id only when absent).
    pub fn metadata(mut self, tmdb: Option<Tmdb>) -> Self {
        self.tmdb = tmdb;
        self
    }

    /// The eviction behaviour.
    pub fn eviction(mut self, eviction: Eviction) -> Self {
        self.eviction = Some(eviction);
        self
    }

    /// Open the store and assemble the library.
    pub fn build(self) -> anyhow::Result<MediaCatalog> {
        let store = Catalog::open(&self.db_path)
            .with_context(|| format!("opening catalog {:?}", self.db_path))?;
        Ok(MediaCatalog {
            store,
            tmdb: self.tmdb,
            eviction: self.eviction.expect("eviction policy is required"),
        })
    }
}

impl Eviction {
    /// Start building an eviction policy.
    pub fn builder() -> EvictionBuilder {
        EvictionBuilder {
            policy: None,
            budget: 0,
            min_free: 0,
            stream_grace: Duration::ZERO,
        }
    }
}

/// Fluent builder for [`Eviction`]: choose the strategy and its constraints.
pub struct EvictionBuilder {
    policy: Option<Box<dyn EvictionPolicy>>,
    budget: u64,
    min_free: u64,
    stream_grace: Duration,
}

impl EvictionBuilder {
    /// The hard disk budget: usage may not exceed this.
    pub fn budget(mut self, bytes: u64) -> Self {
        self.budget = bytes;
        self
    }

    /// Keep at least this many bytes free on the filesystem.
    pub fn min_free(mut self, bytes: u64) -> Self {
        self.min_free = bytes;
        self
    }

    /// Never evict something streamed within this window.
    pub fn protect_streamed(mut self, grace: Duration) -> Self {
        self.stream_grace = grace;
        self
    }

    /// The ranking strategy (e.g. [`eviction::Lru`]).
    pub fn strategy(mut self, policy: impl EvictionPolicy + 'static) -> Self {
        self.policy = Some(Box::new(policy));
        self
    }

    pub fn build(self) -> Eviction {
        Eviction {
            policy: self.policy.expect("an eviction strategy is required"),
            budget: self.budget,
            min_free: self.min_free,
            stream_grace: self.stream_grace,
        }
    }
}
