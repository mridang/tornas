//! Read-only projections of engine state: what a movie looks like to the API, the
//! dashboard and the metrics endpoint. Nothing here changes anything.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::{
    catalog::{Event, Movie, TorrentRow},
    media_catalog::eviction::Candidate,
    utils::{human_bytes, now_secs},
};

use super::*;

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

#[derive(Debug, Clone, Default, Serialize)]
pub struct BudgetView {
    pub limit: u64,
    pub used: u64,
    pub min_free: u64,
    pub disk_free: u64,
    pub disk_total: u64,
    pub next_eviction: Option<Candidate>,
}

#[derive(Debug, Clone, Default, Serialize)]
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

impl Engine {
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

    pub(super) fn view_movie(
        &self,
        movie: Movie,
        handle: Option<ManagedTorrentHandle>,
    ) -> MovieView {
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
        let (disk_free, disk_total) = crate::utils::mount::disk_usage(&self.torrents_dir)?;
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

    /// What can be shown with the data disk gone: the pause, warnings and the
    /// in-memory session numbers. The library itself lives on the missing disk.
    pub(super) fn status_without_disk(&self) -> StatusView {
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
        crate::utils::mount::disk_usage(&self.torrents_dir)?;
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
        if let Ok((free, _)) = crate::utils::mount::disk_usage(&self.torrents_dir)
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
}
