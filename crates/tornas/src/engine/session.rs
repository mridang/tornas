//! Starting and stopping: building the librqbit session from the configuration,
//! reconciling it with the catalog, and flushing piece maps on the way out.

use std::{
    net::{Ipv6Addr, SocketAddr},
    num::NonZeroU32,
    sync::Arc,
    time::Instant,
};

use std::str::FromStr;

use anyhow::{Context, bail};
use librqbit::dht::Id20;
use librqbit::{
    AddTorrentOptions, DhtSessionConfig, ListenerMode, ListenerOptions, Session, SessionOptions,
    SessionPersistenceConfig, api::TorrentIdOrHash, limits::LimitsConfig,
};
use tracing::{debug, info, warn};

use crate::{
    catalog::{Catalog, TorrentRow},
    config::ServerOpts,
    tmdb::Tmdb,
    trackers::TrackerFeed,
    tuning::{IpListStatus, PeerList},
};

use super::*;

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
        let report = crate::config::check::validate_file_config(&file_config);
        for w in &report.warnings {
            warn!("config: {w}");
        }
        if !report.errors.is_empty() {
            bail!(
                "invalid configuration (run `tornas config check` for details):\n  {}",
                report.errors.join("\n  ")
            );
        }
        let acl = crate::http::netacl::Acl::new(
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
        let catalog = Arc::new(Catalog::open(&data_dir.join("catalog.db"))?);
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
        let blocklist = IpListStatus::prepare(
            opts.peer_blocklist.as_deref().and_then(PeerList::blocklist),
            &data_dir.join("peer-blocklist.cache"),
        )
        .await?;
        let allowlist = IpListStatus::prepare(
            opts.peer_allowlist.as_deref().and_then(PeerList::allowlist),
            &data_dir.join("peer-allowlist.cache"),
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

        let pause0 = super::pause::load_pause(data_dir);
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
                    crate::utils::human_age(r)
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
    pub(super) async fn reconcile(&self) -> anyhow::Result<()> {
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
    pub(super) fn torrent_options(&self, row: &TorrentRow, paused: bool) -> AddTorrentOptions {
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
    /// The exact piece map from memory.
    pub(super) fn piece_map(&self, h: &ManagedTorrentHandle) -> Option<Vec<u8>> {
        librqbit::Api::new(self.session.clone(), None)
            .api_dump_haves(TorrentIdOrHash::Id(h.id()))
            .ok()
            .map(|(bf, _)| bf.as_raw_slice().to_vec())
    }

    /// Replace librqbit's saved piece map. A rename, so a flush still in flight
    /// from librqbit lands on the old file and cannot overwrite this one.
    pub(super) fn write_piece_map(&self, info_hash: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let bitv = self.session_dir.join(format!("{info_hash}.bitv"));
        let tmp = bitv.with_extension("bitv.tmp");
        std::fs::write(&tmp, bytes).with_context(|| format!("writing {tmp:?}"))?;
        std::fs::rename(&tmp, &bitv).with_context(|| format!("replacing {bitv:?}"))?;
        Ok(())
    }

    pub(super) fn live_peers(&self, h: &ManagedTorrentHandle) -> Vec<SocketAddr> {
        // The default filter is "live peers only".
        let Ok(filter) = serde_json::from_value(serde_json::json!({})) else {
            return Vec::new();
        };
        librqbit::Api::new(self.session.clone(), None)
            .api_peer_stats(TorrentIdOrHash::Id(h.id()), filter)
            .map(|snap| snap.peers.keys().filter_map(|a| a.parse().ok()).collect())
            .unwrap_or_default()
    }

    /// Stop the engine, keeping every piece downloaded so far. librqbit writes piece
    /// maps every 16 MiB and loses the last write when the process exits, so after
    /// it stops, the exact maps are written from memory.
    pub async fn shutdown(&self) {
        self.session.stop().await;
        if self.is_disk_missing() {
            return;
        }
        let handles: Vec<_> = self
            .session
            .with_torrents(|it| it.map(|(_, h)| h.clone()).collect());
        let mut saved = 0;
        for h in handles {
            let hash = hash_hex(h.info_hash());
            if let Some(bytes) = self.piece_map(&h) {
                match self.write_piece_map(&hash, &bytes) {
                    Ok(()) => saved += 1,
                    Err(e) => warn!("saving piece map for {hash}: {e:#}"),
                }
            }
        }
        debug!("saved {saved} piece maps");
    }

    pub(super) fn handle_for(&self, info_hash: &str) -> Option<ManagedTorrentHandle> {
        let id = Id20::from_str(info_hash).ok()?;
        self.session.get(TorrentIdOrHash::Hash(id))
    }
}
