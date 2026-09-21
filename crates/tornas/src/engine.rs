//! The core: one librqbit session plus the catalog and the disk-budget policy.
//! Everything the HTTP layer, DLNA layer and sweep loop do goes through here.

use std::{
    net::{Ipv6Addr, SocketAddr},
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use std::str::FromStr;

use anyhow::{Context, bail};
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListenerOptions, Session,
    SessionOptions, SessionPersistenceConfig, api::TorrentIdOrHash, limits::LimitsConfig,
};
use librqbit::{ManagedTorrent, dht::Id20};
type ManagedTorrentHandle = Arc<ManagedTorrent>;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{
    budget::{self, Candidate},
    catalog::{Catalog, Event, Movie, TorrentRow},
    config::{FileConfig, ServerOpts},
    tmdb::Tmdb,
    trackers::TrackerFeed,
    units::{human_bytes, now_secs},
};

/// Why an operation failed, so the HTTP layer can pick a status without parsing messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultKind {
    NotFound,
    Conflict,
    Invalid,
    NoSpace,
    Upstream,
}

#[derive(Debug)]
pub struct Fault {
    pub kind: FaultKind,
    pub message: String,
}
impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Fault {}

pub fn fault(kind: FaultKind, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Fault {
        kind,
        message: message.into(),
    })
}

pub fn fault_kind(e: &anyhow::Error) -> Option<FaultKind> {
    e.chain()
        .find_map(|c| c.downcast_ref::<Fault>().map(|f| f.kind))
}

pub struct Engine {
    pub session: Arc<Session>,
    pub catalog: Catalog,
    pub tmdb: Option<Tmdb>,
    pub opts: ServerOpts,
    pub file_config: FileConfig,
    pub acl: crate::netacl::Acl,
    pub trackers: TrackerFeed,
    pub torrents_dir: PathBuf,
    /// Saved .torrent files for movies added from a file rather than a magnet.
    meta_dir: PathBuf,
    started: Instant,
    weak: parking_lot::RwLock<std::sync::Weak<Engine>>,
    /// info_hash -> (progress bytes, when it last changed), for stall detection.
    progress_seen: parking_lot::Mutex<std::collections::HashMap<String, (u64, i64)>>,
    /// Serialises add + evict so two concurrent adds cannot both pass the budget check.
    add_lock: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddMovieRequest {
    pub imdb_id: String,
    /// Magnet link. Exactly one of `magnet`, `torrent_url`, `torrent_base64` is required.
    #[serde(default)]
    pub magnet: Option<String>,
    /// URL of a .torrent file (what private trackers hand out).
    #[serde(default)]
    pub torrent_url: Option<String>,
    /// Base64-encoded .torrent file contents.
    #[serde(default)]
    pub torrent_base64: Option<String>,
    /// Optional explicit peers (host:port) for local testing without DHT/trackers.
    #[serde(default)]
    pub initial_peers: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MovieView {
    #[serde(flatten)]
    pub movie: Movie,
    pub torrent: Option<TorrentRow>,
    pub state: String,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub finished: bool,
    pub download_bps: u64,
    pub upload_bps: u64,
    pub peers: u64,
    pub protected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BudgetView {
    pub limit: u64,
    pub used: u64,
    pub min_free: u64,
    pub disk_free: u64,
    pub disk_total: u64,
    pub next_eviction: Option<Candidate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    pub download_bps: u64,
    pub upload_bps: u64,
    pub peers_live: u64,
    pub uptime_secs: u64,
    pub torrents: usize,
    pub listen_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusView {
    pub version: &'static str,
    pub hostname: String,
    /// Non-fatal problems: low disk, over budget, missing credentials.
    pub warnings: Vec<String>,
    pub budget: BudgetView,
    pub session: SessionView,
    pub movies: Vec<MovieView>,
    pub events: Vec<Event>,
}

/// Loose view over librqbit's JSON stats so we never depend on private field names.
fn json_u64(v: &serde_json::Value, path: &[&str]) -> u64 {
    let mut cur = v;
    for p in path {
        cur = match cur.get(p) {
            Some(c) => c,
            None => return 0,
        };
    }
    cur.as_u64()
        .or_else(|| cur.as_f64().map(|f| f as u64))
        .unwrap_or(0)
}

pub fn disk_usage(path: &Path) -> anyhow::Result<(u64, u64)> {
    let st = nix::sys::statvfs::statvfs(path).with_context(|| format!("statvfs {path:?}"))?;
    let frag = st.fragment_size() as u64;
    let free = st.blocks_available() as u64 * frag;
    let total = st.blocks() as u64 * frag;
    Ok((free, total))
}

fn hash_hex(h: Id20) -> String {
    h.as_string()
}

impl Engine {
    pub async fn start(opts: ServerOpts) -> anyhow::Result<Arc<Self>> {
        let data_dir = &opts.data_dir;
        std::fs::create_dir_all(data_dir).with_context(|| format!("creating {data_dir:?}"))?;
        crate::health::check_mount(data_dir, opts.require_mount)?;
        let torrents_dir = data_dir.join("torrents");
        let session_dir = data_dir.join("session");
        std::fs::create_dir_all(&torrents_dir)
            .with_context(|| format!("creating {torrents_dir:?}"))?;
        std::fs::create_dir_all(&session_dir)?;

        let meta_dir = data_dir.join("meta");
        std::fs::create_dir_all(&meta_dir)?;
        let file_config = opts.resolve_file_config()?;
        let acl = crate::netacl::Acl::new(
            &file_config.network.allow_from,
            &file_config.network.trusted_proxies,
        )?;
        if acl.allows_everything() {
            warn!(
                "network.allow_from permits every source address; do not expose this port to the internet"
            );
        }
        let trackers = TrackerFeed::new(file_config.trackers.clone(), data_dir);
        let ipv6 = file_config.network.ipv6;
        let catalog = Catalog::open(&data_dir.join("catalog.db"))?;
        let tmdb = Tmdb::new(
            &opts.tmdb_base_url,
            opts.tmdb_token.clone(),
            opts.tmdb_api_key.clone(),
        );
        if tmdb.is_none() {
            warn!("no TMDB credentials: movies will be catalogued by IMDb id only");
        }

        let listen_addr: SocketAddr = (Ipv6Addr::UNSPECIFIED, opts.listen_port.unwrap_or(0)).into();
        let sopts = SessionOptions {
            dht: if opts.disable_dht {
                None
            } else {
                Some(DhtSessionConfig {
                    persistence: Some(librqbit::dht::DhtPersistenceConfig {
                        config_filename: Some(session_dir.join("dht.json")),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            },
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(session_dir.clone()),
            }),
            fastresume: true,
            listen: Some(ListenerOptions {
                listen_addr,
                enable_upnp_port_forwarding: !opts.disable_upnp_port_forward,
                ..Default::default()
            }),
            ipv4_only: !ipv6,
            ratelimits: LimitsConfig {
                download_bps: opts.ratelimit_download.and_then(NonZeroU32::new),
                upload_bps: opts.ratelimit_upload.and_then(NonZeroU32::new),
            },
            ..Default::default()
        };
        let session = Session::new_with_opts(torrents_dir.clone(), sopts)
            .await
            .context("starting torrent session")?;

        let engine = Arc::new(Self {
            session,
            catalog,
            tmdb,
            opts,
            file_config,
            acl,
            trackers,
            torrents_dir,
            meta_dir,
            started: Instant::now(),
            weak: parking_lot::RwLock::new(std::sync::Weak::new()),
            progress_seen: parking_lot::Mutex::new(std::collections::HashMap::new()),
            add_lock: tokio::sync::Mutex::new(()),
        });
        *engine.weak.write() = Arc::downgrade(&engine);
        if engine.trackers.config.read().enabled && engine.trackers.state().trackers.is_empty() {
            if let Err(e) = engine.trackers.refresh().await {
                warn!("initial tracker fetch failed, continuing without public trackers: {e:#}");
            }
        }
        engine.reconcile().await?;
        engine.catalog.add_event("start", "server started")?;
        let handles: Vec<ManagedTorrentHandle> = engine
            .session
            .with_torrents(|it| it.map(|(_, h)| h.clone()).collect());
        for h in handles {
            engine.watch_completion(h);
        }
        Ok(engine)
    }

    /// Make the session and the catalog agree after a restart.
    async fn reconcile(&self) -> anyhow::Result<()> {
        let known: Vec<TorrentRow> = self.catalog.list_torrents()?;
        let in_session: Vec<(String, usize)> = self
            .session
            .with_torrents(|it| it.map(|(id, h)| (hash_hex(h.info_hash()), id)).collect());

        for (hash, id) in &in_session {
            if !known.iter().any(|k| &k.info_hash == hash) {
                warn!(hash, "torrent in session but not in catalog; removing it");
                if let Err(e) = self.session.delete(TorrentIdOrHash::Id(*id), true).await {
                    warn!("failed to remove orphan torrent {hash}: {e:#}");
                }
            }
        }
        for row in known {
            if !in_session.iter().any(|(h, _)| h == &row.info_hash) {
                info!(imdb = row.imdb_id, "re-adding torrent missing from session");
                let source = match self.source_for(&row.magnet) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("could not load source for {}: {e:#}", row.imdb_id);
                        continue;
                    }
                };
                let res = self
                    .session
                    .add_torrent(
                        source,
                        Some(AddTorrentOptions {
                            overwrite: true,
                            only_files: Some(vec![row.video_file_idx]),
                            trackers: Some(self.trackers.current()),
                            ..Default::default()
                        }),
                    )
                    .await;
                if let Err(e) = res {
                    warn!("could not re-add {}: {e:#}", row.imdb_id);
                }
            }
        }
        Ok(())
    }

    /// Turn the stored source (magnet link or `file://` path to a saved .torrent) into an add request.
    fn source_for(&self, stored: &str) -> anyhow::Result<AddTorrent<'static>> {
        if let Some(path) = stored.strip_prefix("file://") {
            let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
            Ok(AddTorrent::from_bytes(bytes))
        } else {
            Ok(AddTorrent::from_url(stored.to_owned()))
        }
    }

    /// Periodically refresh the public tracker list and re-announce active downloads.
    pub async fn tracker_refresh_forever(self: Arc<Self>) {
        let cfg = self.trackers.config.read().clone();
        if !cfg.enabled {
            info!("public tracker feed disabled");
            return;
        }
        let mut interval = tokio::time::interval(cfg.refresh.max(Duration::from_secs(60)));
        loop {
            interval.tick().await;
            match self.trackers.refresh().await {
                Ok(true) if cfg.reannounce_active => {
                    if let Err(e) = self.reannounce_active().await {
                        warn!("re-announce after tracker refresh: {e:#}");
                    }
                }
                Ok(_) => {}
                Err(e) => warn!("tracker refresh: {e:#}"),
            }
        }
    }

    /// Restart torrents that are still downloading so they pick up the current tracker list.
    /// Private torrents and finished/paused ones are left alone.
    pub async fn reannounce_active(&self) -> anyhow::Result<usize> {
        let list = self.trackers.current();
        let rows = self.catalog.list_torrents()?;
        let mut n = 0;
        for row in rows {
            let Some(h) = self.handle_for(&row.info_hash) else {
                continue;
            };
            let stats = h.stats();
            let private = h.with_metadata(|m| m.info.info().private).unwrap_or(false);
            if private || stats.finished || h.is_paused() {
                continue;
            }
            let id = h.id();
            drop(h);
            let source = self.source_for(&row.magnet)?;
            self.session.delete(TorrentIdOrHash::Id(id), false).await?;
            let res = self
                .session
                .add_torrent(
                    source,
                    Some(AddTorrentOptions {
                        overwrite: true,
                        only_files: Some(vec![row.video_file_idx]),
                        trackers: Some(list.clone()),
                        ..Default::default()
                    }),
                )
                .await?;
            if let AddTorrentResponse::Added(_, handle) = res {
                self.watch_completion_arc(handle);
            }
            n += 1;
        }
        if n > 0 {
            info!(
                "re-announced {n} active downloads with {} trackers",
                list.len()
            );
        }
        Ok(n)
    }

    /// Once a torrent finishes downloading, pause it unless configured to keep seeding.
    /// A paused torrent still streams from disk.
    pub fn watch_completion(self: &Arc<Self>, handle: ManagedTorrentHandle) {
        *self.weak.write() = Arc::downgrade(self);
        self.watch_completion_arc(handle);
    }

    fn watch_completion_arc(&self, handle: ManagedTorrentHandle) {
        if self.opts.keep_seeding {
            return;
        }
        let Some(engine) = self.weak.read().upgrade() else {
            return;
        };
        tokio::spawn(async move {
            if let Err(e) = handle.wait_until_completed().await {
                warn!("waiting for completion: {e:#}");
                return;
            }
            if handle.is_paused() {
                return;
            }
            match engine.session.pause(&handle).await {
                Ok(()) => {
                    let name = handle.name().unwrap_or_default();
                    info!("download complete, paused seeding: {name}");
                    crate::metrics::seeding_paused();
                    let _ = engine
                        .catalog
                        .add_event("done", &format!("downloaded {name}, seeding paused"));
                }
                Err(e) => warn!("could not pause finished torrent: {e:#}"),
            }
        });
    }

    fn handle_for(&self, info_hash: &str) -> Option<ManagedTorrentHandle> {
        let id = Id20::from_str(info_hash).ok()?;
        self.session.get(TorrentIdOrHash::Hash(id))
    }

    fn is_protected(&self, last_used_at: i64) -> bool {
        now_secs() - last_used_at < self.opts.stream_grace.as_secs() as i64
    }

    fn candidates(&self) -> anyhow::Result<(Vec<Candidate>, u64)> {
        let movies = self.catalog.list_movies()?;
        let torrents = self.catalog.list_torrents()?;
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

    /// Evict until `incoming` more bytes fit under the budget and `min_free` stays free on disk.
    pub async fn ensure_space(&self, incoming: u64) -> anyhow::Result<Vec<Candidate>> {
        let (cands, used) = self.candidates()?;
        let (disk_free, _) = disk_usage(&self.torrents_dir)?;
        // Bytes that must be freed on disk to keep min_free after the incoming torrent lands.
        let extra = (incoming + self.opts.min_free).saturating_sub(disk_free);
        let plan = budget::plan(&cands, used, incoming, self.opts.disk_budget, extra)
            .map_err(|e| fault(FaultKind::NoSpace, e.to_string()))?;
        for c in &plan.evict {
            self.evict(&c.key).await?;
        }
        Ok(plan.evict)
    }

    pub async fn evict(&self, info_hash: &str) -> anyhow::Result<()> {
        let row = self.catalog.torrent_by_hash(info_hash)?;
        let title = row
            .as_ref()
            .and_then(|r| self.catalog.get_movie(&r.imdb_id).ok().flatten())
            .map(|m| m.title)
            .unwrap_or_else(|| info_hash.to_owned());
        if let Some(h) = self.handle_for(info_hash) {
            self.session
                .delete(TorrentIdOrHash::Id(h.id()), true)
                .await
                .with_context(|| format!("deleting torrent {info_hash}"))?;
        }
        if let Some(r) = &row {
            self.catalog.delete_movie(&r.imdb_id)?;
        }
        crate::metrics::eviction(row.as_ref().map(|r| r.size_bytes).unwrap_or(0));
        let msg = format!("evicted {title} ({info_hash})");
        info!("{msg}");
        self.catalog.add_event("evict", &msg)?;
        Ok(())
    }

    pub async fn remove_movie(&self, imdb_id: &str) -> anyhow::Result<bool> {
        let Some(t) = self.catalog.torrent_for_movie(imdb_id)? else {
            return self.catalog.delete_movie(imdb_id);
        };
        if let Some(h) = self.handle_for(&t.info_hash) {
            self.session
                .delete(TorrentIdOrHash::Id(h.id()), true)
                .await?;
        }
        self.catalog.delete_movie(imdb_id)?;
        self.catalog
            .add_event("remove", &format!("removed {imdb_id}"))?;
        crate::metrics::removal();
        Ok(true)
    }

    pub async fn add_movie(self: &Arc<Self>, req: AddMovieRequest) -> anyhow::Result<MovieView> {
        let _guard = self.add_lock.lock().await;
        let res = self.add_movie_inner(req).await;
        match &res {
            Ok(_) => crate::metrics::add("ok"),
            Err(e)
                if format!("{e:#}").contains("evictable space")
                    || format!("{e:#}").contains("larger than the whole budget") =>
            {
                crate::metrics::add("refused")
            }
            Err(_) => crate::metrics::add("error"),
        }
        res
    }

    async fn add_movie_inner(self: &Arc<Self>, req: AddMovieRequest) -> anyhow::Result<MovieView> {
        let imdb_id = req.imdb_id.trim().to_owned();
        if !imdb_id.starts_with("tt")
            || imdb_id.len() < 4
            || !imdb_id[2..].bytes().all(|b| b.is_ascii_digit())
        {
            return Err(fault(
                FaultKind::Invalid,
                "imdb_id must look like tt0111161",
            ));
        }
        let sources = [
            req.magnet.is_some(),
            req.torrent_url.is_some(),
            req.torrent_base64.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count();
        if sources != 1 {
            return Err(fault(
                FaultKind::Invalid,
                "provide exactly one of magnet, torrent_url, torrent_base64",
            ));
        }
        if let Some(m) = &req.magnet
            && !m.starts_with("magnet:?")
        {
            return Err(fault(FaultKind::Invalid, "magnet must be a magnet: link"));
        }
        if let Some(existing) = self.catalog.torrent_for_movie(&imdb_id)? {
            if self.handle_for(&existing.info_hash).is_some() {
                return Err(fault(
                    FaultKind::Conflict,
                    format!("{imdb_id} is already in the library"),
                ));
            }
        }

        // 1. Metadata first so a bad id fails before we touch the network for peers.
        let now = now_secs();
        let (movie, raw) = match &self.tmdb {
            Some(t) => {
                let m = t
                    .find_by_imdb(&imdb_id)
                    .await
                    .inspect_err(|_| crate::metrics::tmdb_error())?;
                (
                    Movie {
                        imdb_id: imdb_id.clone(),
                        tmdb_id: Some(m.tmdb_id),
                        title: m.title,
                        year: m.year,
                        overview: m.overview,
                        poster_url: m.poster_url,
                        backdrop_url: m.backdrop_url,
                        runtime_min: m.runtime_min,
                        genres: m.genres,
                        rating: m.rating,
                        added_at: now,
                        last_used_at: now,
                    },
                    Some(m.raw.to_string()),
                )
            }
            None => (
                Movie {
                    imdb_id: imdb_id.clone(),
                    tmdb_id: None,
                    title: imdb_id.clone(),
                    year: None,
                    overview: None,
                    poster_url: None,
                    backdrop_url: None,
                    runtime_min: None,
                    genres: vec![],
                    rating: None,
                    added_at: now,
                    last_used_at: now,
                },
                None,
            ),
        };

        // 2. Obtain the torrent source: magnet, or .torrent bytes from a URL or inline.
        let torrent_bytes: Option<bytes::Bytes> = if let Some(u) = &req.torrent_url {
            let resp = reqwest::Client::new()
                .get(u)
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .map_err(|e| fault(FaultKind::Upstream, format!("fetching torrent file: {e}")))?;
            if !resp.status().is_success() {
                return Err(fault(
                    FaultKind::Upstream,
                    format!("torrent file URL returned {}", resp.status()),
                ));
            }
            Some(
                resp.bytes()
                    .await
                    .map_err(|e| fault(FaultKind::Upstream, e.to_string()))?,
            )
        } else if let Some(b) = &req.torrent_base64 {
            use base64::Engine as _;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(b.trim())
                .map_err(|e| fault(FaultKind::Invalid, format!("torrent_base64: {e}")))?;
            Some(bytes::Bytes::from(decoded))
        } else {
            None
        };
        let make_source = || match &torrent_bytes {
            Some(b) => AddTorrent::from_bytes(b.clone()),
            None => AddTorrent::from_url(req.magnet.clone().unwrap_or_default()),
        };

        // 3. Resolve metadata without downloading anything.
        let mut resolved: Vec<SocketAddr> = Vec::new();
        for p in &req.initial_peers {
            let addrs = tokio::net::lookup_host(p.as_str())
                .await
                .with_context(|| format!("resolving peer {p}"))?;
            resolved.extend(addrs);
        }
        let peers = if resolved.is_empty() {
            None
        } else {
            Some(resolved)
        };
        let listed = self
            .session
            .add_torrent(
                make_source(),
                Some(AddTorrentOptions {
                    list_only: true,
                    initial_peers: peers.clone(),
                    ..Default::default()
                }),
            )
            .await
            .context("resolving torrent metadata")?;
        let listing = match listed {
            AddTorrentResponse::ListOnly(l) => l,
            AddTorrentResponse::AlreadyManaged(_, h) => {
                return Err(fault(
                    FaultKind::Conflict,
                    format!("torrent {} is already managed", hash_hex(h.info_hash())),
                ));
            }
            AddTorrentResponse::Added(..) => bail!("unexpected: list_only add started a download"),
        };
        let info_hash = hash_hex(listing.info_hash);
        let private = listing.info.info().private;
        if private && torrent_bytes.is_none() {
            // The magnet resolution already touched DHT; refuse so the user re-adds from the .torrent file.
            return Err(fault(
                FaultKind::Invalid,
                "this torrent is private; add it from its .torrent file (torrent_url or torrent_base64), not a magnet",
            ));
        }
        let public_trackers = if private {
            vec![]
        } else {
            self.trackers.current()
        };
        // Persist the source so restarts and re-announces can re-add it.
        let stored_source = match &torrent_bytes {
            Some(b) => {
                let path = self.meta_dir.join(format!("{info_hash}.torrent"));
                std::fs::write(&path, b).with_context(|| format!("saving {path:?}"))?;
                format!("file://{}", path.display())
            }
            None => req.magnet.clone().unwrap_or_default(),
        };
        let files: Vec<(usize, String, u64)> = listing
            .info
            .iter_file_details()
            .enumerate()
            .map(|(i, f)| (i, f.filename.to_string(), f.len))
            .collect();
        let (video_idx, video_name, video_len) = files
            .iter()
            .filter(|(_, name, _)| is_video(name))
            .max_by_key(|(_, _, len)| *len)
            .or_else(|| files.iter().max_by_key(|(_, _, len)| *len))
            .cloned()
            .context("torrent has no files")?;

        // 4. Make room, then start the real download of just the video file.
        let evicted = self.ensure_space(video_len).await?;
        let added = self
            .session
            .add_torrent(
                make_source(),
                Some(AddTorrentOptions {
                    overwrite: true,
                    only_files: Some(vec![video_idx]),
                    initial_peers: peers,
                    trackers: if public_trackers.is_empty() {
                        None
                    } else {
                        Some(public_trackers.clone())
                    },
                    ..Default::default()
                }),
            )
            .await
            .context("adding torrent")?;
        let handle = match added {
            AddTorrentResponse::Added(_, h) | AddTorrentResponse::AlreadyManaged(_, h) => h,
            AddTorrentResponse::ListOnly(_) => bail!("unexpected list-only response"),
        };

        self.watch_completion(handle.clone());

        // 5. Record it.
        self.catalog.upsert_movie(&movie, raw.as_deref())?;
        self.catalog.insert_torrent(&TorrentRow {
            info_hash: info_hash.clone(),
            imdb_id: imdb_id.clone(),
            magnet: stored_source,
            size_bytes: video_len,
            video_file_idx: video_idx,
            video_file_name: video_name,
            added_at: now,
        })?;
        let msg = format!(
            "added {} ({}) {}{}{}",
            movie.title,
            imdb_id,
            human_bytes(video_len),
            if private { " [private]" } else { "" },
            if evicted.is_empty() {
                String::new()
            } else {
                format!(", evicted {}", evicted.len())
            }
        );
        info!("{msg}");
        self.catalog.add_event("add", &msg)?;
        Ok(self.view_movie(movie, Some(handle)))
    }

    /// Open a stream for a movie's main video file and mark it as just used.
    /// Resolve a movie to its torrent handle and main video file, marking it as just used.
    pub fn stream_target(
        &self,
        imdb_id: &str,
    ) -> anyhow::Result<(ManagedTorrentHandle, usize, String)> {
        let t = self
            .catalog
            .torrent_for_movie(imdb_id)?
            .context("no such movie")?;
        let h = self
            .handle_for(&t.info_hash)
            .context("torrent not loaded")?;
        self.catalog.touch(imdb_id, now_secs())?;
        Ok((h, t.video_file_idx, t.video_file_name))
    }

    pub fn touch(&self, imdb_id: &str) {
        let _ = self.catalog.touch(imdb_id, now_secs());
    }

    /// Set a movie's last-used time explicitly; `None` means now.
    pub fn set_last_used(&self, imdb_id: &str, ts: Option<i64>) -> anyhow::Result<MovieView> {
        if self.catalog.get_movie(imdb_id)?.is_none() {
            return Err(fault(FaultKind::NotFound, "no such movie"));
        }
        self.catalog.touch(imdb_id, ts.unwrap_or_else(now_secs))?;
        self.get_movie(imdb_id)?
            .ok_or_else(|| fault(FaultKind::NotFound, "no such movie"))
    }

    fn view_movie(&self, movie: Movie, handle: Option<ManagedTorrentHandle>) -> MovieView {
        let torrent = self
            .catalog
            .torrent_for_movie(&movie.imdb_id)
            .ok()
            .flatten();
        let handle =
            handle.or_else(|| torrent.as_ref().and_then(|t| self.handle_for(&t.info_hash)));
        let protected = self.is_protected(movie.last_used_at);
        match handle {
            Some(h) => {
                let stats = h.stats();
                let live = stats
                    .live
                    .as_ref()
                    .map(|l| serde_json::to_value(l).unwrap_or_default());
                let live = live.unwrap_or_default();
                MovieView {
                    state: match &stats.state {
                        librqbit::TorrentStatsState::Initializing { .. } => "checking".into(),
                        librqbit::TorrentStatsState::Live => {
                            if stats.finished {
                                "seeding".into()
                            } else {
                                "downloading".into()
                            }
                        }
                        librqbit::TorrentStatsState::Paused => {
                            if stats.finished {
                                "done".into()
                            } else {
                                "paused".into()
                            }
                        }
                        librqbit::TorrentStatsState::Error => "error".into(),
                    },
                    progress_bytes: stats.progress_bytes,
                    total_bytes: stats.total_bytes,
                    finished: stats.finished,
                    download_bps: (json_u64(&live, &["download_speed", "mbps"]) as f64 * 125_000.0)
                        as u64,
                    upload_bps: (json_u64(&live, &["upload_speed", "mbps"]) as f64 * 125_000.0)
                        as u64,
                    peers: json_u64(&live, &["snapshot", "peer_stats", "live"]),
                    movie,
                    torrent,
                    protected,
                }
            }
            None => MovieView {
                state: "missing".into(),
                progress_bytes: 0,
                total_bytes: torrent.as_ref().map(|t| t.size_bytes).unwrap_or(0),
                finished: false,
                download_bps: 0,
                upload_bps: 0,
                peers: 0,
                movie,
                torrent,
                protected,
            },
        }
    }

    pub fn list_movies(&self) -> anyhow::Result<Vec<MovieView>> {
        Ok(self
            .catalog
            .list_movies()?
            .into_iter()
            .map(|m| self.view_movie(m, None))
            .collect())
    }

    pub fn get_movie(&self, imdb_id: &str) -> anyhow::Result<Option<MovieView>> {
        Ok(self
            .catalog
            .get_movie(imdb_id)?
            .map(|m| self.view_movie(m, None)))
    }

    pub fn status(&self) -> anyhow::Result<StatusView> {
        let (cands, used) = self.candidates()?;
        let (disk_free, disk_total) = disk_usage(&self.torrents_dir)?;
        let next_eviction = cands
            .iter()
            .filter(|c| !c.protected)
            .min_by_key(|c| c.last_used_at)
            .cloned();
        let snap = serde_json::to_value(self.session.stats_snapshot()).unwrap_or_default();
        Ok(StatusView {
            version: env!("CARGO_PKG_VERSION"),
            hostname: gethostname::gethostname().to_string_lossy().into_owned(),
            warnings: self.warnings(),
            budget: BudgetView {
                limit: self.opts.disk_budget,
                used,
                min_free: self.opts.min_free,
                disk_free,
                disk_total,
                next_eviction,
            },
            session: SessionView {
                download_bps: (json_u64(&snap, &["download_speed", "mbps"]) as f64 * 125_000.0)
                    as u64,
                upload_bps: (json_u64(&snap, &["upload_speed", "mbps"]) as f64 * 125_000.0) as u64,
                peers_live: json_u64(&snap, &["peers", "live"]),
                uptime_secs: self.started.elapsed().as_secs(),
                torrents: self.session.with_torrents(|it| it.count()),
                listen_addr: self.session.listen_addr(),
            },
            movies: self.list_movies()?,
            events: self.catalog.recent_events(20)?,
        })
    }

    /// Liveness only: the catalog answers, the session lock is not wedged and the
    /// data directory is still there. Deliberately does NOT fail on low disk: the
    /// watchdog gates systemd restarts on this, and a full disk is a condition to
    /// report, not a reason to kill a working daemon. See `warnings()`.
    pub fn probe(&self) -> anyhow::Result<()> {
        self.catalog.recent_events(1)?;
        let _ = self.session.with_torrents(|it| it.count());
        disk_usage(&self.torrents_dir)?;
        Ok(())
    }

    /// Operational problems worth surfacing, without failing health checks.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok((free, _)) = disk_usage(&self.torrents_dir)
            && free < self.opts.min_free
        {
            out.push(format!(
                "only {} free on disk, below the configured minimum of {}: new movies will be refused until space is reclaimed",
                human_bytes(free),
                human_bytes(self.opts.min_free)
            ));
        }
        if let Ok((_, used)) = self.candidates()
            && used > self.opts.disk_budget
        {
            out.push(format!(
                "usage ({}) exceeds the budget ({}); the next sweep will evict",
                human_bytes(used),
                human_bytes(self.opts.disk_budget)
            ));
        }
        if self.tmdb.is_none() {
            out.push(
                "no TMDB credentials configured: movies are catalogued by IMDb id only".into(),
            );
        }
        out
    }

    /// Evict downloads that have made no progress for `stall_timeout`. Returns evicted hashes.
    pub async fn evict_stalled(&self) -> anyhow::Result<Vec<String>> {
        let timeout = self.opts.stall_timeout.as_secs() as i64;
        if timeout == 0 {
            return Ok(vec![]);
        }
        let now = now_secs();
        let mut stalled = Vec::new();
        {
            let rows = self.catalog.list_torrents()?;
            let mut seen = self.progress_seen.lock();
            seen.retain(|h, _| rows.iter().any(|r| &r.info_hash == h));
            for row in rows {
                let Some(h) = self.handle_for(&row.info_hash) else {
                    continue;
                };
                let stats = h.stats();
                if stats.finished || h.is_paused() {
                    seen.remove(&row.info_hash);
                    continue;
                }
                let entry = seen
                    .entry(row.info_hash.clone())
                    .or_insert((stats.progress_bytes, now));
                if stats.progress_bytes > entry.0 {
                    *entry = (stats.progress_bytes, now);
                    continue;
                }
                let movie_last_used = self
                    .catalog
                    .get_movie(&row.imdb_id)?
                    .map(|m| m.last_used_at)
                    .unwrap_or(0);
                if now - entry.1 >= timeout && !self.is_protected(movie_last_used) {
                    stalled.push((row.info_hash.clone(), row.imdb_id.clone(), now - entry.1));
                }
            }
        }
        let mut out = Vec::new();
        for (hash, imdb, secs) in stalled {
            warn!(
                "evicting stalled download {imdb}: no progress for {}",
                crate::units::human_age(secs)
            );
            self.catalog.add_event(
                "stalled",
                &format!(
                    "{imdb} made no progress for {}, evicted",
                    crate::units::human_age(secs)
                ),
            )?;
            self.evict(&hash).await?;
            crate::metrics::stalled_eviction();
            out.push(hash);
        }
        Ok(out)
    }

    /// Periodic safety net: re-run the budget check with nothing incoming.
    pub async fn sweep_forever(self: Arc<Self>) {
        let mut interval =
            tokio::time::interval(self.opts.sweep_interval.max(Duration::from_secs(5)));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(e) = self.evict_stalled().await {
                warn!("stall check: {e:#}");
            }
            match self.ensure_space(0).await {
                Ok(ev) if !ev.is_empty() => info!("sweep evicted {} torrents", ev.len()),
                Ok(_) => {}
                Err(e) => warn!("sweep: {e:#}"),
            }
        }
    }
}

pub fn is_video(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["mp4", "mkv", "avi", "mov", "webm", "m4v", "ts", "wmv"]
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")))
}
