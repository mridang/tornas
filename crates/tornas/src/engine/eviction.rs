//! Keeping the library inside its disk budget: which movies may be evicted, making
//! room for an incoming one, and the periodic sweep.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use librqbit::api::TorrentIdOrHash;
use tracing::{info, warn};

use crate::{
    budget::{self, Candidate},
    utils::now_secs,
};

use super::*;

impl Engine {
    pub(super) fn is_protected(&self, last_used_at: i64) -> bool {
        now_secs() - last_used_at < self.opts.stream_grace.as_secs() as i64
    }

    pub(super) fn candidates(&self) -> anyhow::Result<(Vec<Candidate>, u64)> {
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
        let (disk_free, _) = crate::health::disk_usage(&self.torrents_dir)?;
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
