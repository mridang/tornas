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
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListenerMode,
    ListenerOptions, Session, SessionOptions, SessionPersistenceConfig, api::TorrentIdOrHash,
    limits::LimitsConfig,
};
use librqbit::{ManagedTorrent, dht::Id20};
type ManagedTorrentHandle = Arc<ManagedTorrent>;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

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
    /// Global limits in force right now (after the bandwidth schedule); null = unlimited.
    pub download_limit: Option<u32>,
    pub upload_limit: Option<u32>,
    /// Index into `[[bandwidth.schedule]]` of the window in force, if any.
    pub schedule_window: Option<usize>,
    /// Downloads held back by `--max-active-downloads`.
    pub queued: usize,
    pub peers_live: u64,
    pub uptime_secs: u64,
    pub torrents: usize,
    pub listen_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusView {
    pub pause: PauseView,
    pub version: &'static str,
    pub hostname: String,
    /// Non-fatal problems: low disk, over budget, missing credentials.
    pub warnings: Vec<String>,
    pub budget: BudgetView,
    pub session: SessionView,
    pub movies: Vec<MovieView>,
    pub events: Vec<Event>,
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
        // Same rules as `tornas config check`, so a bad file fails loudly here
        // instead of misbehaving later.
        let report = crate::configcheck::validate_file_config(&file_config);
        for w in &report.warnings {
            warn!("config: {w}");
        }
        if !report.errors.is_empty() {
            bail!(
                "invalid configuration (run `tornas config check` for details):\n  {}",
                report.errors.join("\n  ")
            );
        }
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

        let listen_port = opts.listen_port.unwrap_or(0);
        let listen_addr: SocketAddr = if ipv6 {
            (Ipv6Addr::UNSPECIFIED, listen_port).into()
        } else {
            (std::net::Ipv4Addr::UNSPECIFIED, listen_port).into()
        };
        let tuning = crate::tuning::from_opts(&opts);
        let blocklist = crate::tuning::prepare_ip_list(
            opts.peer_blocklist.as_deref(),
            &data_dir.join("peer-blocklist.cache"),
            "blocklist",
            false,
        )
        .await?;
        let allowlist = crate::tuning::prepare_ip_list(
            opts.peer_allowlist.as_deref(),
            &data_dir.join("peer-allowlist.cache"),
            "allowlist",
            true,
        )
        .await?;
        // A router port mapping points at the LAN address, which is not where traffic
        // flows once it is bound to a VPN interface.
        let upnp = !opts.disable_upnp_port_forward && opts.bind_device.is_none();
        if opts.bind_device.is_some() && !opts.disable_upnp_port_forward {
            info!("router port forwarding disabled: torrent traffic is bound to an interface");
        }
        let sopts = SessionOptions {
            dht: if opts.disable_dht {
                None
            } else {
                Some(DhtSessionConfig {
                    port: opts.dht_port,
                    bootstrap_addrs: (!opts.dht_bootstrap.is_empty())
                        .then(|| opts.dht_bootstrap.clone()),
                    persistence: Some(librqbit::dht::DhtPersistenceConfig {
                        config_filename: Some(session_dir.join("dht.json")),
                        ..Default::default()
                    }),
                })
            },
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(session_dir.clone()),
            }),
            fastresume: true,
            listen: Some(ListenerOptions {
                mode: if opts.utp {
                    ListenerMode::TcpAndUtp
                } else {
                    ListenerMode::TcpOnly
                },
                listen_addr,
                enable_upnp_port_forwarding: upnp,
                announce_port: opts.announce_port,
                ipv4_only: !ipv6,
                ..Default::default()
            }),
            bind_device_name: opts.bind_device.clone(),
            ipv4_only: !ipv6,
            disable_local_service_discovery: opts.disable_lsd,
            blocklist_url: blocklist.loaded_from.clone(),
            allowlist_url: allowlist.loaded_from.clone(),
            peer_limit: Some(tuning.peer_limit as usize),
            concurrent_init_limit: Some(tuning.concurrent_checks as usize),
            ratelimits: LimitsConfig {
                download_bps: opts.ratelimit_download.and_then(NonZeroU32::new),
                upload_bps: opts.ratelimit_upload.and_then(NonZeroU32::new),
            },
            ..Default::default()
        };
        let session = Session::new_with_opts(torrents_dir.clone(), sopts)
            .await
            .context("starting torrent session")?;

        let pause0 = load_pause(data_dir);
        let file_config_bandwidth = file_config.bandwidth.clone();
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
            session_dir: session_dir.clone(),
            started: Instant::now(),
            weak: parking_lot::RwLock::new(std::sync::Weak::new()),
            progress_seen: parking_lot::Mutex::new(std::collections::HashMap::new()),
            pause: parking_lot::Mutex::new(pause0),
            disk_missing: std::sync::atomic::AtomicBool::new(false),
            add_lock: tokio::sync::Mutex::new(()),
            tuning,
            blocklist,
            allowlist,
            bandwidth: crate::schedule::compile(&file_config_bandwidth)?,
            bandwidth_active: parking_lot::Mutex::new(None),
        });
        engine.apply_bandwidth();
        *engine.weak.write() = Arc::downgrade(&engine);
        // The tracker list is fetched by the background refresh loop, whose first tick
        // fires immediately. Never block startup on it: a slow or offline source would
        // otherwise hold the HTTP server, Stremio and DLNA unreachable.
        engine.reconcile().await?;
        if engine.is_paused() {
            let n = engine.apply_pause().await;
            let v = engine.pause_view();
            match v.remaining_secs {
                Some(r) => warn!(
                    "restored a pause from disk: everything stays paused for another {}",
                    crate::units::human_age(r)
                ),
                None => warn!("restored a pause from disk: everything stays paused until resumed"),
            }
            if n > 0 {
                info!("re-paused {n} torrents that restored unpaused");
            }
        }
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
                let source = match self.source_for(&row) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("could not load source for {}: {e:#}", row.imdb_id);
                        continue;
                    }
                };
                let res = self
                    .session
                    .add_torrent(source, Some(self.torrent_options(&row, self.is_paused())))
                    .await;
                if let Err(e) = res {
                    warn!("could not re-add {}: {e:#}", row.imdb_id);
                }
            }
        }
        Ok(())
    }

    /// Options to (re)start a catalogued torrent: only the video file, public trackers
    /// unless private, and this movie's own limits.
    fn torrent_options(&self, row: &TorrentRow, paused: bool) -> AddTorrentOptions {
        AddTorrentOptions {
            overwrite: true,
            only_files: Some(vec![row.video_file_idx]),
            trackers: (!row.private).then(|| self.trackers.current()),
            paused,
            ratelimits: LimitsConfig {
                download_bps: row.download_limit.and_then(NonZeroU32::new),
                upload_bps: row.upload_limit.and_then(NonZeroU32::new),
            },
            peer_limit: row.peer_limit.map(|p| p as usize),
            ..Default::default()
        }
    }

    /// Restart one torrent with fresh options (new trackers or limits), which librqbit
    /// only reads when a torrent starts. Removing a torrent from the session deletes
    /// its saved piece map, which would force a full re-hash of the file on re-add, so
    /// the current bitfield is taken from memory and written back before re-adding;
    /// fastresume then spot-checks a few pieces instead of hashing everything. The
    /// re-add uses the saved metadata, so nothing is fetched from peers.
    pub async fn reload_torrent(&self, row: &TorrentRow) -> anyhow::Result<()> {
        let Some(h) = self.handle_for(&row.info_hash) else {
            // Not loaded; the new options apply the next time it starts.
            return Ok(());
        };
        let was_paused = h.is_paused();
        if !was_paused {
            // Pausing writes the piece map out, so the copy below is current.
            if let Err(e) = self.session.pause(&h).await {
                debug!("pause before reload: {e:#}");
            }
        }
        let bitv = self.session_dir.join(format!("{}.bitv", row.info_hash));
        // The bitfield on disk is written asynchronously and can lag behind; take the
        // exact one from memory instead (the torrent is paused, so it is final).
        let saved: Option<Vec<u8>> = librqbit::Api::new(self.session.clone(), None)
            .api_dump_haves(TorrentIdOrHash::Id(h.id()))
            .ok()
            .map(|(bf, _)| bf.as_raw_slice().to_vec());
        // Movies added before metadata was kept: save it now, while it is in memory,
        // so the re-add below does not have to fetch it from peers.
        let meta = self.meta_path(&row.info_hash);
        if !meta.is_file()
            && let Ok(bytes) = h.with_metadata(|m| m.torrent_bytes.clone())
        {
            std::fs::write(&meta, &bytes).with_context(|| format!("saving {meta:?}"))?;
        }
        let id = h.id();
        drop(h);
        let source = self.source_for(row)?;
        self.session
            .delete(TorrentIdOrHash::Id(id), false)
            .await
            .with_context(|| format!("reloading {}", row.imdb_id))?;
        if let Some(bytes) = saved {
            let tmp = bitv.with_extension("bitv.tmp");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &bitv)?;
        }
        let res = self
            .session
            .add_torrent(source, Some(self.torrent_options(row, was_paused)))
            .await
            .with_context(|| format!("re-adding {}", row.imdb_id))?;
        if let AddTorrentResponse::Added(_, handle) = res {
            // The saved piece map makes this a quick spot check; wait for it so
            // callers see the settled state instead of a moment of "checking".
            match tokio::time::timeout(Duration::from_secs(10), handle.wait_until_initialized())
                .await
            {
                Ok(Err(e)) => warn!("{}: re-check after reload failed: {e:#}", row.imdb_id),
                Err(_) => debug!("{}: still checking after reload", row.imdb_id),
                Ok(Ok(())) => {}
            }
            self.watch_completion_arc(handle);
        }
        Ok(())
    }

    /// Set or clear one movie's speed and peer limits, and apply them now.
    pub async fn set_movie_limits(
        &self,
        imdb_id: &str,
        download: Option<u32>,
        upload: Option<u32>,
        peers: Option<u32>,
    ) -> anyhow::Result<MovieView> {
        if self.is_disk_missing() {
            return Err(fault(
                FaultKind::Conflict,
                "the data disk is missing; limits can be changed once it is back",
            ));
        }
        let _guard = self.add_lock.lock().await;
        let row = self
            .catalog
            .torrent_for_movie(imdb_id)?
            .ok_or_else(|| fault(FaultKind::NotFound, "no such movie"))?;
        self.catalog
            .set_limits(&row.info_hash, download, upload, peers)?;
        let row = TorrentRow {
            download_limit: download,
            upload_limit: upload,
            peer_limit: peers,
            ..row
        };
        self.reload_torrent(&row).await?;
        let fmt = |v: Option<u32>, unit: &str| match v {
            Some(v) if unit == "peers" => format!("{v} peers"),
            Some(v) => crate::units::human_rate(u64::from(v)),
            None => "default".to_owned(),
        };
        self.catalog.add_event(
            "limits",
            &format!(
                "{imdb_id}: download {}, upload {}, {}",
                fmt(download, "bps"),
                fmt(upload, "bps"),
                match peers {
                    Some(p) => format!("{p} peers"),
                    None => "default peers".to_owned(),
                }
            ),
        )?;
        self.get_movie(imdb_id)?
            .ok_or_else(|| fault(FaultKind::NotFound, "no such movie"))
    }

    /// Turn the stored source (magnet link or `file://` path to a saved .torrent) into an add request.
    fn meta_path(&self, info_hash: &str) -> PathBuf {
        self.meta_dir.join(format!("{info_hash}.torrent"))
    }

    /// How to re-add a catalogued torrent: from its saved metadata when there is
    /// one (instant, no network), otherwise from the stored magnet or file.
    fn source_for(&self, row: &TorrentRow) -> anyhow::Result<AddTorrent<'static>> {
        if let Ok(bytes) = std::fs::read(self.meta_path(&row.info_hash)) {
            return Ok(AddTorrent::from_bytes(bytes));
        }
        if let Some(path) = row.magnet.strip_prefix("file://") {
            let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
            Ok(AddTorrent::from_bytes(bytes))
        } else {
            Ok(AddTorrent::from_url(row.magnet.clone()))
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
            // The tracker cache lives in the data directory.
            if self.is_disk_missing() {
                continue;
            }
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
        if self.is_paused() {
            return Ok(0);
        }
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
            drop(h);
            self.reload_torrent(&row).await?;
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
        let Some(engine) = self.weak.read().upgrade() else {
            return;
        };
        tokio::spawn(async move {
            if let Err(e) = handle.wait_until_completed().await {
                warn!("waiting for completion: {e:#}");
                return;
            }
            // A finished download frees a queue slot for the next one.
            if engine.opts.keep_seeding || handle.is_paused() {
                if let Err(e) = engine.balance_queue().await {
                    debug!("queue after completion: {e:#}");
                }
                return;
            }
            let paused = engine.session.pause(&handle).await;
            if let Err(e) = engine.balance_queue().await {
                debug!("queue after completion: {e:#}");
            }
            match paused {
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
        // While paused, do nothing that touches the network, including magnet lookups.
        if self.is_paused() {
            return Err(fault(
                FaultKind::Conflict,
                "tornas is paused; resume it before adding movies",
            ));
        }
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
        // Keep the torrent's metadata for every source, magnets included, so a restart
        // or reload never has to fetch it from peers again.
        let meta_path = self.meta_path(&info_hash);
        let meta_bytes = torrent_bytes
            .clone()
            .unwrap_or_else(|| listing.torrent_bytes.clone());
        std::fs::write(&meta_path, &meta_bytes).with_context(|| format!("saving {meta_path:?}"))?;
        let stored_source = match &torrent_bytes {
            Some(_) => format!("file://{}", meta_path.display()),
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
                    paused: self.is_paused() || self.queue_is_full(),
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
            private,
            download_limit: None,
            upload_limit: None,
            peer_limit: None,
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
            .ok_or_else(|| fault(FaultKind::NotFound, "no such movie"))?;
        let h = self.handle_for(&t.info_hash).ok_or_else(|| {
            fault(
                FaultKind::Conflict,
                "the torrent is not loaded (the data disk may be missing)",
            )
        })?;
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
                let live = stats.live.as_ref();
                let mut state = state_label(&stats);
                if state == "paused" && !self.is_paused() {
                    // Not finished, not globally paused: held back by the queue.
                    state = "queued";
                }
                MovieView {
                    state: state.into(),
                    progress_bytes: stats.progress_bytes,
                    total_bytes: stats.total_bytes,
                    finished: stats.finished,
                    download_bps: live.map(|l| l.download_speed.as_bytes()).unwrap_or(0),
                    upload_bps: live.map(|l| l.upload_speed.as_bytes()).unwrap_or(0),
                    peers: live
                        .map(|l| u64::from(l.snapshot.peer_stats.live))
                        .unwrap_or(0),
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
        if self.is_disk_missing() {
            return Ok(self.status_without_disk());
        }
        let (cands, used) = self.candidates()?;
        let (disk_free, disk_total) = disk_usage(&self.torrents_dir)?;
        let next_eviction = cands
            .iter()
            .filter(|c| !c.protected)
            .min_by_key(|c| c.last_used_at)
            .cloned();
        let snap = self.session.stats_snapshot();
        Ok(StatusView {
            version: env!("CARGO_PKG_VERSION"),
            hostname: gethostname::gethostname().to_string_lossy().into_owned(),
            pause: self.pause_view(),
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
                download_bps: snap.download_speed.as_bytes(),
                upload_bps: snap.upload_speed.as_bytes(),
                download_limit: self.session.ratelimits.get_download_bps().map(|b| b.get()),
                upload_limit: self.session.ratelimits.get_upload_bps().map(|b| b.get()),
                schedule_window: *self.bandwidth_active.lock(),
                queued: self.queued_count(),
                peers_live: u64::from(snap.peers.live),
                uptime_secs: self.started.elapsed().as_secs(),
                torrents: self.session.with_torrents(|it| it.count()),
                listen_addr: self.session.listen_addr(),
            },
            movies: self.list_movies()?,
            events: self.catalog.recent_events(20)?,
        })
    }

    /// True when a new download would exceed `--max-active-downloads`.
    fn queue_is_full(&self) -> bool {
        let Some(max) = self.opts.max_active_downloads else {
            return false;
        };
        let active = self.session.with_torrents(|it| {
            it.filter(|(_, h)| !h.is_paused() && !h.stats().finished)
                .count()
        });
        active >= max as usize
    }

    /// What can be shown with the data disk gone: the pause, warnings and the
    /// in-memory session numbers. The library itself lives on the missing disk.
    fn status_without_disk(&self) -> StatusView {
        let snap = self.session.stats_snapshot();
        StatusView {
            pause: self.pause_view(),
            version: env!("CARGO_PKG_VERSION"),
            hostname: gethostname::gethostname().to_string_lossy().into_owned(),
            warnings: self.warnings(),
            budget: BudgetView {
                limit: self.opts.disk_budget,
                used: 0,
                min_free: self.opts.min_free,
                disk_free: 0,
                disk_total: 0,
                next_eviction: None,
            },
            session: SessionView {
                download_bps: snap.download_speed.as_bytes(),
                upload_bps: snap.upload_speed.as_bytes(),
                download_limit: self.session.ratelimits.get_download_bps().map(|b| b.get()),
                upload_limit: self.session.ratelimits.get_upload_bps().map(|b| b.get()),
                schedule_window: *self.bandwidth_active.lock(),
                queued: self.queued_count(),
                peers_live: u64::from(snap.peers.live),
                uptime_secs: self.started.elapsed().as_secs(),
                torrents: self.session.with_torrents(|it| it.count()),
                listen_addr: self.session.listen_addr(),
            },
            movies: vec![],
            events: vec![],
        }
    }

    // ---- global pause -----------------------------------------------------

    pub fn is_paused(&self) -> bool {
        self.pause.lock().is_some()
    }

    pub fn pause_view(&self) -> PauseView {
        let st = self.pause.lock().clone();
        let now = now_secs();
        PauseView {
            paused: st.is_some(),
            since: st.as_ref().map(|p| p.since),
            until: st.as_ref().and_then(|p| p.until),
            remaining_secs: st.as_ref().and_then(|p| p.until).map(|u| (u - now).max(0)),
            indefinite: st.as_ref().is_some_and(|p| p.until.is_none()),
            reason: st.as_ref().map(|p| p.reason),
            default_duration_secs: self.opts.pause_duration.as_secs(),
        }
    }

    /// Pause every loaded torrent that is not already paused. Idempotent.
    async fn apply_pause(&self) -> usize {
        let handles: Vec<ManagedTorrentHandle> = self
            .session
            .with_torrents(|it| it.map(|(_, h)| h.clone()).collect());
        let mut n = 0;
        for h in handles {
            if h.is_paused() {
                continue;
            }
            match self.session.pause(&h).await {
                Ok(()) => n += 1,
                Err(e) => debug!("pause {}: {e:#}", h.info_hash().as_string()),
            }
        }
        n
    }

    /// Pause everything for `duration` (the configured default when `None`), or
    /// until resumed when `indefinite`. Calling it while paused changes the end time
    /// and keeps the original start.
    pub async fn pause_all(
        &self,
        duration: Option<Duration>,
        indefinite: bool,
    ) -> anyhow::Result<PauseView> {
        // Serialise with adds so a movie being added right now cannot slip out unpaused.
        let _guard = self.add_lock.lock().await;
        let now = now_secs();
        let dur = duration.unwrap_or(self.opts.pause_duration);
        if !indefinite && (dur.is_zero() || dur > Duration::from_secs(30 * 86_400)) {
            return Err(fault(
                FaultKind::Invalid,
                "pause duration must be between 1 second and 30 days",
            ));
        }
        let since = self.pause.lock().as_ref().map(|p| p.since).unwrap_or(now);
        let st = PauseState {
            since,
            until: (!indefinite).then(|| now + dur.as_secs() as i64),
            reason: PauseReason::Manual,
        };
        // With the data disk gone its directory may be the bare mount point on the
        // boot disk, so nothing is written until it is back.
        if !self.is_disk_missing() {
            store_pause(&self.opts.data_dir, Some(&st))?;
        }
        *self.pause.lock() = Some(st);
        let n = self.apply_pause().await;
        let msg = if indefinite {
            format!("paused everything ({n} torrents) until resumed")
        } else {
            format!(
                "paused everything ({n} torrents) for {}",
                humantime::format_duration(dur)
            )
        };
        warn!("{msg}");
        let _ = self.catalog.add_event("pause", &msg);
        crate::metrics::paused();
        Ok(self.pause_view())
    }

    /// Lift the pause. Finished movies stay paused unless `--keep-seeding`.
    /// Returns how many torrents were resumed.
    pub async fn resume_all(&self, trigger: &'static str) -> anyhow::Result<usize> {
        if self.is_disk_missing() {
            return Err(fault(
                FaultKind::Conflict,
                "the data disk is not mounted; tornas resumes on its own when it is back",
            ));
        }
        let _guard = self.add_lock.lock().await;
        if self.pause.lock().take().is_none() {
            return Ok(0);
        }
        store_pause(&self.opts.data_dir, None)?;
        // Restart through the queue, so a resume never starts more downloads than
        // --max-active-downloads allows.
        let n = self.balance_queue().await?;
        let msg = format!("resumed ({trigger}): {n} torrents restarted");
        info!("{msg}");
        self.catalog.add_event("resume", &msg)?;
        crate::metrics::resumed(trigger);
        Ok(n)
    }

    // ---- download queue ---------------------------------------------------

    /// Run at most `--max-active-downloads` unfinished torrents, oldest first, and
    /// hold the rest paused. Finished movies are resumed only with --keep-seeding.
    /// Returns how many torrents it started. Does nothing while paused.
    pub async fn balance_queue(&self) -> anyhow::Result<usize> {
        if self.is_paused() {
            return Ok(0);
        }
        let max = self
            .opts
            .max_active_downloads
            .map(|m| m as usize)
            .unwrap_or(usize::MAX);
        let mut rows = self.catalog.list_torrents()?;
        rows.sort_by_key(|r| (r.added_at, r.info_hash.clone()));
        let mut started = 0;
        let mut slot = 0;
        for row in rows {
            let Some(h) = self.handle_for(&row.info_hash) else {
                continue;
            };
            let stats = h.stats();
            if state_label(&stats) == "error" {
                continue;
            }
            let want_running = if stats.finished {
                self.opts.keep_seeding
            } else {
                slot += 1;
                slot <= max
            };
            match (want_running, h.is_paused()) {
                (true, true) => match self.session.unpause(&h).await {
                    Ok(()) => started += 1,
                    Err(e) => debug!("start {}: {e:#}", row.imdb_id),
                },
                (false, false) if !stats.finished => {
                    if let Err(e) = self.session.pause(&h).await {
                        debug!("queue {}: {e:#}", row.imdb_id);
                    } else {
                        info!("queued {}: {max} downloads already running", row.imdb_id);
                    }
                }
                _ => {}
            }
        }
        Ok(started)
    }

    // ---- data disk watch --------------------------------------------------

    pub fn is_disk_missing(&self) -> bool {
        self.disk_missing.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The data directory is on its own filesystem and readable. Only meaningful
    /// with --require-mount; otherwise always true.
    fn data_disk_ok(&self) -> bool {
        if !self.opts.require_mount {
            return true;
        }
        matches!(
            crate::health::is_on_separate_filesystem(&self.opts.data_dir),
            Ok(true)
        ) && std::fs::read_dir(&self.torrents_dir).is_ok()
    }

    /// Pause for a missing disk: in memory only, never persisted, lifted when the
    /// disk returns.
    async fn pause_for_disk(&self) {
        let _guard = self.add_lock.lock().await;
        if self.pause.lock().is_some() {
            return;
        }
        *self.pause.lock() = Some(PauseState {
            since: now_secs(),
            until: None,
            reason: PauseReason::DiskMissing,
        });
        let n = self.apply_pause().await;
        crate::metrics::paused();
        warn!(
            "the data disk at {} is not mounted: paused everything ({n} torrents) until it is back",
            self.opts.data_dir.display()
        );
    }

    /// Resume once the pause expires, and while paused re-pause anything that
    /// came loose (a re-announce or restore racing the pause).
    pub async fn check_pause(&self) -> anyhow::Result<()> {
        let ok = self.data_disk_ok();
        let was_missing = self
            .disk_missing
            .swap(!ok, std::sync::atomic::Ordering::SeqCst);
        if ok && was_missing {
            info!("the data disk is back");
            let _ = self.catalog.add_event("disk", "the data disk came back");
        }
        let reason = self.pause.lock().as_ref().map(|p| p.reason);
        match disk_action(ok, reason) {
            DiskAction::PauseForDisk => {
                self.pause_for_disk().await;
                return Ok(());
            }
            DiskAction::ResumeFromDisk => {
                self.resume_all("disk").await?;
                return Ok(());
            }
            DiskAction::Nothing => {}
        }
        let st = self.pause.lock().clone();
        match st {
            None => {
                self.balance_queue().await?;
            }
            // A timed pause does not end while the disk is missing.
            Some(p) if ok && p.until.is_some_and(|u| now_secs() >= u) => {
                self.resume_all("auto").await?;
            }
            Some(_) => {
                let n = self.apply_pause().await;
                if n > 0 {
                    warn!("pause enforcement re-paused {n} torrents");
                }
            }
        }
        Ok(())
    }

    /// Set the global limits from the bandwidth schedule for the current local time.
    pub fn apply_bandwidth(&self) {
        let (day, minute) = crate::schedule::now_local();
        self.apply_bandwidth_at(day, minute);
    }

    fn apply_bandwidth_at(&self, day: u8, minute: u16) {
        use crate::schedule::{Limit, active, resolve};
        let idx = active(&self.bandwidth, day, minute);
        let (dl, ul) = idx
            .map(|i| (self.bandwidth[i].download, self.bandwidth[i].upload))
            .unwrap_or((Limit::Inherit, Limit::Inherit));
        let dl = resolve(dl, self.opts.ratelimit_download.and_then(NonZeroU32::new));
        let ul = resolve(ul, self.opts.ratelimit_upload.and_then(NonZeroU32::new));
        let r = &self.session.ratelimits;
        let changed = r.get_download_bps() != dl || r.get_upload_bps() != ul;
        if changed {
            r.set_download_bps(dl);
            r.set_upload_bps(ul);
        }
        let prev = std::mem::replace(&mut *self.bandwidth_active.lock(), idx);
        if prev != idx || changed {
            let show = |b: Option<NonZeroU32>| {
                b.map(|b| crate::units::human_rate(u64::from(b.get())))
                    .unwrap_or_else(|| "unlimited".into())
            };
            let which = idx
                .map(|i| format!("schedule window {i}"))
                .unwrap_or_else(|| "global limits".into());
            info!(
                "bandwidth: {which}: download {}, upload {}",
                show(dl),
                show(ul)
            );
        }
    }

    pub async fn bandwidth_forever(self: Arc<Self>) {
        if self.bandwidth.is_empty() {
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tick.tick().await;
            self.apply_bandwidth();
        }
    }

    /// Unfinished torrents held paused by the download queue.
    pub fn queued_count(&self) -> usize {
        if self.opts.max_active_downloads.is_none() || self.is_paused() {
            return 0;
        }
        self.session.with_torrents(|it| {
            it.filter(|(_, h)| h.is_paused() && !h.stats().finished)
                .count()
        })
    }

    pub async fn pause_watch_forever(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            if let Err(e) = self.check_pause().await {
                warn!("pause check: {e:#}");
            }
        }
    }

    /// Per-torrent facts for `/metrics`, one entry per catalogued movie that has a
    /// loaded torrent.
    pub fn torrent_facts(&self) -> anyhow::Result<Vec<TorrentFacts>> {
        let now = now_secs();
        let movies = self.catalog.list_movies()?;
        let mut out = Vec::new();
        for row in self.catalog.list_torrents()? {
            let Some(h) = self.handle_for(&row.info_hash) else {
                continue;
            };
            let movie = movies.iter().find(|m| m.imdb_id == row.imdb_id);
            let stats = h.stats();
            let (size_bytes, piece_length, pieces, files, private) = h
                .with_metadata(|m| {
                    let l = m.info.lengths();
                    (
                        l.total_length(),
                        u64::from(l.default_piece_length()),
                        u64::from(l.total_pieces()),
                        m.info.iter_file_details().count() as u64,
                        m.info.info().private,
                    )
                })
                .unwrap_or_default();
            let live = stats.live.as_ref();
            let p = live.map(|l| &l.snapshot.peer_stats);
            // librqbit does not re-export the peer stats type, so read fields through a
            // macro and let the compiler infer it.
            macro_rules! pv {
                ($f:ident) => {
                    p.map(|p| u64::from(p.$f)).unwrap_or(0)
                };
            }
            let down = live.map(|l| l.download_speed.as_bytes()).unwrap_or(0);
            let remaining = stats.total_bytes.saturating_sub(stats.progress_bytes);
            let mut trackers_by_scheme = std::collections::BTreeMap::new();
            for t in &h.shared().trackers {
                *trackers_by_scheme.entry(t.scheme().to_owned()).or_insert(0) += 1;
            }
            out.push(TorrentFacts {
                imdb_id: row.imdb_id.clone(),
                info_hash: row.info_hash.clone(),
                title: movie.map(|m| m.title.clone()).unwrap_or_default(),
                state: state_label(&stats),
                private,
                size_bytes,
                selected_bytes: stats.total_bytes,
                progress_bytes: stats.progress_bytes,
                piece_length,
                pieces,
                pieces_verified: live
                    .map(|l| l.snapshot.downloaded_and_checked_pieces)
                    .unwrap_or(0),
                files,
                fetched_bytes: live.map(|l| l.snapshot.fetched_bytes).unwrap_or(0),
                uploaded_bytes: live
                    .map(|l| l.snapshot.uploaded_bytes)
                    .unwrap_or(stats.uploaded_bytes),
                download_bps: down,
                upload_bps: live.map(|l| l.upload_speed.as_bytes()).unwrap_or(0),
                peers: [
                    ("queued", pv!(queued)),
                    ("connecting", pv!(connecting)),
                    ("live", pv!(live)),
                    ("seen", pv!(seen)),
                    ("dead", pv!(dead)),
                    ("not_needed", pv!(not_needed)),
                ],
                peers_live_by_transport: [
                    ("tcp", pv!(live_tcp)),
                    ("utp", pv!(live_utp)),
                    ("socks", pv!(live_socks)),
                ],
                piece_download_secs_avg: live
                    .and_then(|l| l.average_piece_download_time)
                    .map(|d| d.as_secs_f64()),
                eta_secs: (!stats.finished && down > 0).then(|| remaining / down),
                idle_secs: now - movie.map(|m| m.last_used_at).unwrap_or(row.added_at),
                trackers_by_scheme,
            });
        }
        Ok(out)
    }

    /// DHT routing table sizes and in-flight queries, if DHT is enabled.
    pub fn dht_stats(&self) -> Option<(u64, u64, u64)> {
        self.session.get_dht().map(|d| {
            let s = d.stats();
            (
                s.routing_table_size as u64,
                s.routing_table_size_v6 as u64,
                s.outstanding_requests as u64,
            )
        })
    }

    /// Liveness only: the catalog answers, the session lock is not wedged and the
    /// data directory is still there. Deliberately does NOT fail on low disk: the
    /// watchdog gates systemd restarts on this, and a full disk is a condition to
    /// report, not a reason to kill a working daemon. See `warnings()`.
    pub fn probe(&self) -> anyhow::Result<()> {
        if self.is_disk_missing() {
            // The catalog lives on the missing disk. The daemon is doing the right
            // thing (everything paused); a restart would only fail the mount guard.
            let _ = self.session.with_torrents(|it| it.count());
            return Ok(());
        }
        self.catalog.recent_events(1)?;
        let _ = self.session.with_torrents(|it| it.count());
        disk_usage(&self.torrents_dir)?;
        Ok(())
    }

    /// Operational problems worth surfacing, without failing health checks.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.is_disk_missing() {
            out.push(format!(
                "the data disk at {} is not mounted: everything is paused until it is back",
                self.opts.data_dir.display()
            ));
        }
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
        if let Some(n) = &self.blocklist.note {
            out.push(format!("peer blocklist: {n}"));
        }
        if let Some(n) = &self.allowlist.note {
            out.push(format!("peer allowlist: {n}"));
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
            if self.is_disk_missing() {
                continue;
            }
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

/// A global pause, persisted so a restart or power cut mid-pause stays paused.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PauseState {
    pub since: i64,
    /// `None` means until explicitly resumed.
    pub until: Option<i64>,
    #[serde(default)]
    pub reason: PauseReason,
}

/// Why everything is paused.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// Someone pressed pause (dashboard, CLI or API).
    #[default]
    Manual,
    /// The data disk disappeared; lifted automatically when it comes back.
    DiskMissing,
}

/// What the disk watch should do, given whether the data disk is usable and the
/// current pause (if any). Pure, so it can be tested without real mounts.
#[derive(Debug, PartialEq, Eq)]
pub enum DiskAction {
    Nothing,
    PauseForDisk,
    ResumeFromDisk,
}

pub fn disk_action(disk_ok: bool, pause: Option<PauseReason>) -> DiskAction {
    match (disk_ok, pause) {
        (false, None) => DiskAction::PauseForDisk,
        (true, Some(PauseReason::DiskMissing)) => DiskAction::ResumeFromDisk,
        _ => DiskAction::Nothing,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PauseView {
    pub paused: bool,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub remaining_secs: Option<i64>,
    pub indefinite: bool,
    pub reason: Option<PauseReason>,
    /// What a pause lasts when no duration is given.
    pub default_duration_secs: u64,
}

fn pause_file(data_dir: &Path) -> PathBuf {
    data_dir.join("pause.json")
}

/// Load a persisted pause, dropping it if it has already expired.
fn load_pause(data_dir: &Path) -> Option<PauseState> {
    let bytes = std::fs::read(pause_file(data_dir)).ok()?;
    let st: PauseState = serde_json::from_slice(&bytes).ok()?;
    match st.until {
        Some(u) if u <= now_secs() => None,
        _ => Some(st),
    }
}

/// Write atomically: a torn write after a power cut must not lose the pause.
fn store_pause(data_dir: &Path, st: Option<&PauseState>) -> anyhow::Result<()> {
    let path = pause_file(data_dir);
    match st {
        None => {
            let _ = std::fs::remove_file(&path);
        }
        Some(st) => {
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(st)?)?;
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(&tmp, &path)?;
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_watch_decisions() {
        use DiskAction::*;
        use PauseReason::*;
        // Disk gone and nothing paused: pause.
        assert_eq!(disk_action(false, None), PauseForDisk);
        // Disk gone during a manual pause: keep the manual pause.
        assert_eq!(disk_action(false, Some(Manual)), Nothing);
        assert_eq!(disk_action(false, Some(DiskMissing)), Nothing);
        // Disk back: lift only a pause the disk caused.
        assert_eq!(disk_action(true, Some(DiskMissing)), ResumeFromDisk);
        assert_eq!(disk_action(true, Some(Manual)), Nothing);
        assert_eq!(disk_action(true, None), Nothing);
    }

    #[test]
    fn old_pause_files_still_load() {
        // pause.json written before reasons existed.
        let st: PauseState = serde_json::from_str(r#"{"since":1,"until":2}"#).unwrap();
        assert_eq!(st.reason, PauseReason::Manual);
    }
}
