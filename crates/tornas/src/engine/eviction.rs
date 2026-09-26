//! Keeping the library inside its disk budget: which movies may be evicted, making
//! room for an incoming one, and the periodic sweep.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use librqbit::api::TorrentIdOrHash;
use tracing::{info, warn};

use crate::media_catalog::eviction::Candidate;

use super::*;

impl Engine {
    /// Evict until `incoming` more bytes fit under the budget and `min_free` stays free on disk.
    /// The library owns the policy; the engine reads free disk and executes the plan.
    pub async fn ensure_space(&self, incoming: u64) -> anyhow::Result<Vec<Candidate>> {
        let (cands, used) = self.library.candidates()?;
        let (disk_free, _) = crate::utils::mount::disk_usage(&self.torrents_dir)?;
        let plan = self
            .library
            .plan(&cands, used, disk_free, incoming)
            .map_err(|e| fault(FaultKind::NoSpace, e.to_string()))?;
        for c in &plan.evict {
            self.evict(&c.key).await?;
        }
        Ok(plan.evict)
    }

    pub async fn evict(&self, info_hash: &str) -> anyhow::Result<()> {
        let row = self.library.store().torrent_by_hash(info_hash)?;
        let title = row
            .as_ref()
            .and_then(|r| self.library.store().get_movie(&r.imdb_id).ok().flatten())
            .map(|m| m.title)
            .unwrap_or_else(|| info_hash.to_owned());
        if let Some(h) = self.handle_for(info_hash) {
            self.session
                .delete(TorrentIdOrHash::Id(h.id()), true)
                .await
                .with_context(|| format!("deleting torrent {info_hash}"))?;
        }
        if let Some(r) = &row {
            self.library.store().delete_movie(&r.imdb_id)?;
        }
        crate::metrics::eviction(row.as_ref().map(|r| r.size_bytes).unwrap_or(0));
        let msg = format!("evicted {title} ({info_hash})");
        info!("{msg}");
        self.library.store().add_event("evict", &msg)?;
        Ok(())
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
