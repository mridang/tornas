//! Applying the time-of-day bandwidth schedule to the session's global limits.

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use tracing::info;

use super::*;

impl Engine {
    /// Set the global limits from the bandwidth schedule for the current local time.
    pub fn apply_bandwidth(&self) {
        let (day, minute) = crate::schedule::now_local();
        self.apply_bandwidth_at(day, minute);
    }

    pub(super) fn apply_bandwidth_at(&self, day: u8, minute: u16) {
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
                b.map(|b| crate::utils::human_rate(u64::from(b.get())))
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
}
