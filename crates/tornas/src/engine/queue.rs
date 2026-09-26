//! The download queue: at most `--max-active-downloads` unfinished torrents run at
//! once, oldest first, and finished ones stop unless `--keep-seeding` is set.

use std::sync::Arc;

use tracing::{debug, info, warn};

use super::*;

impl Engine {
    /// Once a torrent finishes downloading, pause it unless configured to keep seeding.
    /// A paused torrent still streams from disk.
    pub fn watch_completion(self: &Arc<Self>, handle: ManagedTorrentHandle) {
        *self.weak.write() = Arc::downgrade(self);
        self.watch_completion_arc(handle);
    }

    pub(super) fn watch_completion_arc(&self, handle: ManagedTorrentHandle) {
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
                        .library
                        .store()
                        .add_event("done", &format!("downloaded {name}, seeding paused"));
                }
                Err(e) => warn!("could not pause finished torrent: {e:#}"),
            }
        });
    }

    /// True when a new download would exceed `--max-active-downloads`.
    pub(super) fn queue_is_full(&self) -> bool {
        let Some(max) = self.opts.max_active_downloads else {
            return false;
        };
        let active = self.session.with_torrents(|it| {
            it.filter(|(_, h)| !h.is_paused() && !h.stats().finished)
                .count()
        });
        active >= max as usize
    }

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
        let mut rows = self.library.store().list_torrents()?;
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
}
