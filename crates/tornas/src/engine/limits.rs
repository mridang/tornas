//! Per-movie speed and peer limits. librqbit only takes these when a torrent is
//! added, so changing one reloads the torrent — carrying its piece map across so
//! nothing is re-checked or re-downloaded.

use std::time::Duration;

use anyhow::Context;
use librqbit::{AddTorrentResponse, api::TorrentIdOrHash};
use tracing::{debug, warn};

use crate::catalog::TorrentRow;

use super::*;

impl Engine {
    pub async fn reload_torrent(&self, row: &TorrentRow) -> anyhow::Result<()> {
        let Some(h) = self.handle_for(&row.info_hash) else {
            // Not loaded; the new options apply the next time it starts.
            return Ok(());
        };
        let was_paused = h.is_paused();
        // Peers it is talking to now, so the re-added torrent reconnects at once
        // instead of waiting for DHT or trackers.
        let peers = self.live_peers(&h);
        if !was_paused {
            // Pausing writes the piece map out, so the copy below is current.
            if let Err(e) = self.session.pause(&h).await {
                debug!("pause before reload: {e:#}");
            }
        }
        // The bitfield on disk is written asynchronously and can lag behind; take the
        // exact one from memory instead (the torrent is paused, so it is final).
        let saved = self.piece_map(&h);
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
            self.write_piece_map(&row.info_hash, &bytes)?;
        }
        let mut options = self.torrent_options(row, was_paused);
        if !peers.is_empty() {
            options.initial_peers = Some(peers);
        }
        let res = self
            .session
            .add_torrent(source, Some(options))
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
            Some(v) => crate::utils::human_rate(u64::from(v)),
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
}
