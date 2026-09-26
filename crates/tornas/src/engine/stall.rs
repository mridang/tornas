//! Dropping downloads that have stopped making progress, so a dead torrent cannot
//! hold disk budget forever.

use tracing::warn;

use crate::utils::now_secs;

use super::*;

impl Engine {
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
                crate::utils::human_age(secs)
            );
            self.catalog.add_event(
                "stalled",
                &format!(
                    "{imdb} made no progress for {}, evicted",
                    crate::utils::human_age(secs)
                ),
            )?;
            self.evict(&hash).await?;
            crate::metrics::stalled_eviction();
            out.push(hash);
        }
        Ok(out)
    }
}
