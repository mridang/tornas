//! Adding and removing movies: metadata lookup, budget check, saving the .torrent
//! so a reload never has to refetch it, and deleting everything on the way out.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, bail};
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, api::TorrentIdOrHash};
use tracing::info;

use crate::{
    media_catalog::{Movie, TorrentRow},
    utils::{human_bytes, now_secs},
};

use super::*;

impl Engine {
    /// Turn the stored source (magnet link or `file://` path to a saved .torrent) into an add request.
    pub(super) fn meta_path(&self, info_hash: &str) -> PathBuf {
        self.meta_dir.join(format!("{info_hash}.torrent"))
    }

    /// How to re-add a catalogued torrent: from its saved metadata when there is
    /// one (instant, no network), otherwise from the stored magnet or file.
    pub(super) fn source_for(&self, row: &TorrentRow) -> anyhow::Result<AddTorrent<'static>> {
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

    pub async fn remove_movie(&self, imdb_id: &str) -> anyhow::Result<bool> {
        let Some(t) = self.library.store().torrent_for_movie(imdb_id)? else {
            return self.library.store().delete_movie(imdb_id);
        };
        if let Some(h) = self.handle_for(&t.info_hash) {
            self.session
                .delete(TorrentIdOrHash::Id(h.id()), true)
                .await?;
        }
        self.library.store().delete_movie(imdb_id)?;
        self.library
            .store()
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

    pub(super) async fn add_movie_inner(
        self: &Arc<Self>,
        req: AddMovieRequest,
    ) -> anyhow::Result<MovieView> {
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
        if let Some(existing) = self.library.store().torrent_for_movie(&imdb_id)? {
            if self.handle_for(&existing.info_hash).is_some() {
                return Err(fault(
                    FaultKind::Conflict,
                    format!("{imdb_id} is already in the library"),
                ));
            }
        }

        // 1. Metadata first so a bad id fails before we touch the network for peers.
        let now = now_secs();
        let (movie, raw) = match self.library.tmdb() {
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
                    Some(m.raw),
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
        self.library.store().upsert_movie(&movie, raw.as_deref())?;
        self.library.store().insert_torrent(&TorrentRow {
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
        self.library.store().add_event("add", &msg)?;
        Ok(self.view_movie(movie, Some(handle)))
    }
}
