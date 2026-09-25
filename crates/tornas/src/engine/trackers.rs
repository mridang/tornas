//! The public tracker feed: refresh on a schedule and re-announce running torrents
//! when the list changes. Private torrents are never touched.

use std::{sync::Arc, time::Duration};

use tracing::{info, warn};

use super::*;

impl Engine {
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
}
