//! The global kill switch: pause everything, resume everything, and the state that
//! survives a restart. A pause caused by a missing disk is remembered separately so
//! it can lift itself when the disk comes back.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::utils::now_secs;

use super::*;

impl Engine {
    pub fn is_paused(&self) -> bool {
        self.pause.lock().is_some()
    }

    pub fn pause_view(&self) -> PauseView {
        let st = self.pause.lock().clone();
        let now = now_secs();
        PauseView {
            paused: st.is_some(),
            since: st.as_ref().map(|p| p.since),
            until: st.as_ref().and_then(|p| p.until),
            remaining_secs: st.as_ref().and_then(|p| p.until).map(|u| (u - now).max(0)),
            indefinite: st.as_ref().is_some_and(|p| p.until.is_none()),
            reason: st.as_ref().map(|p| p.reason),
            default_duration_secs: self.opts.pause_duration.as_secs(),
        }
    }

    /// Pause every loaded torrent that is not already paused. Idempotent.
    pub(super) async fn apply_pause(&self) -> usize {
        let handles: Vec<ManagedTorrentHandle> = self
            .session
            .with_torrents(|it| it.map(|(_, h)| h.clone()).collect());
        let mut n = 0;
        for h in handles {
            if h.is_paused() {
                continue;
            }
            match self.session.pause(&h).await {
                Ok(()) => n += 1,
                Err(e) => debug!("pause {}: {e:#}", h.info_hash().as_string()),
            }
        }
        n
    }

    /// Pause everything for `duration` (the configured default when `None`), or
    /// until resumed when `indefinite`. Calling it while paused changes the end time
    /// and keeps the original start.
    pub async fn pause_all(
        &self,
        duration: Option<Duration>,
        indefinite: bool,
    ) -> anyhow::Result<PauseView> {
        // Serialise with adds so a movie being added right now cannot slip out unpaused.
        let _guard = self.add_lock.lock().await;
        let now = now_secs();
        let dur = duration.unwrap_or(self.opts.pause_duration);
        if !indefinite && (dur.is_zero() || dur > Duration::from_secs(30 * 86_400)) {
            return Err(fault(
                FaultKind::Invalid,
                "pause duration must be between 1 second and 30 days",
            ));
        }
        let since = self.pause.lock().as_ref().map(|p| p.since).unwrap_or(now);
        let st = PauseState {
            since,
            until: (!indefinite).then(|| now + dur.as_secs() as i64),
            reason: PauseReason::Manual,
        };
        // With the data disk gone its directory may be the bare mount point on the
        // boot disk, so nothing is written until it is back.
        if !self.is_disk_missing() {
            store_pause(&self.opts.data_dir, Some(&st))?;
        }
        *self.pause.lock() = Some(st);
        let n = self.apply_pause().await;
        let msg = if indefinite {
            format!("paused everything ({n} torrents) until resumed")
        } else {
            format!(
                "paused everything ({n} torrents) for {}",
                humantime::format_duration(dur)
            )
        };
        warn!("{msg}");
        let _ = self.catalog.add_event("pause", &msg);
        crate::metrics::paused();
        Ok(self.pause_view())
    }

    /// Lift the pause. Finished movies stay paused unless `--keep-seeding`.
    /// Returns how many torrents were resumed.
    pub async fn resume_all(&self, trigger: &'static str) -> anyhow::Result<usize> {
        if self.is_disk_missing() {
            return Err(fault(
                FaultKind::Conflict,
                "the data disk is not mounted; tornas resumes on its own when it is back",
            ));
        }
        let _guard = self.add_lock.lock().await;
        if self.pause.lock().take().is_none() {
            return Ok(0);
        }
        store_pause(&self.opts.data_dir, None)?;
        // Restart through the queue, so a resume never starts more downloads than
        // --max-active-downloads allows.
        let n = self.balance_queue().await?;
        let msg = format!("resumed ({trigger}): {n} torrents restarted");
        info!("{msg}");
        self.catalog.add_event("resume", &msg)?;
        crate::metrics::resumed(trigger);
        Ok(n)
    }

    pub async fn pause_watch_forever(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            if let Err(e) = self.check_pause().await {
                warn!("pause check: {e:#}");
            }
        }
    }
}

/// A global pause, persisted so a restart or power cut mid-pause stays paused.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PauseState {
    pub since: i64,
    /// `None` means until explicitly resumed.
    pub until: Option<i64>,
    #[serde(default)]
    pub reason: PauseReason,
}

/// Why everything is paused.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// Someone pressed pause (dashboard, CLI or API).
    #[default]
    Manual,
    /// The data disk disappeared; lifted automatically when it comes back.
    DiskMissing,
}

/// What the disk watch should do, given whether the data disk is usable and the
/// current pause (if any). Pure, so it can be tested without real mounts.
#[derive(Debug, PartialEq, Eq)]
pub enum DiskAction {
    Nothing,
    PauseForDisk,
    ResumeFromDisk,
}

pub fn disk_action(disk_ok: bool, pause: Option<PauseReason>) -> DiskAction {
    match (disk_ok, pause) {
        (false, None) => DiskAction::PauseForDisk,
        (true, Some(PauseReason::DiskMissing)) => DiskAction::ResumeFromDisk,
        _ => DiskAction::Nothing,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PauseView {
    pub paused: bool,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub remaining_secs: Option<i64>,
    pub indefinite: bool,
    pub reason: Option<PauseReason>,
    /// What a pause lasts when no duration is given.
    pub default_duration_secs: u64,
}

pub(super) fn pause_file(data_dir: &Path) -> PathBuf {
    data_dir.join("pause.json")
}

/// Load a persisted pause, dropping it if it has already expired.
pub(super) fn load_pause(data_dir: &Path) -> Option<PauseState> {
    let bytes = std::fs::read(pause_file(data_dir)).ok()?;
    let st: PauseState = serde_json::from_slice(&bytes).ok()?;
    match st.until {
        Some(u) if u <= now_secs() => None,
        _ => Some(st),
    }
}

/// Write atomically: a torn write after a power cut must not lose the pause.
pub(super) fn store_pause(data_dir: &Path, st: Option<&PauseState>) -> anyhow::Result<()> {
    let path = pause_file(data_dir);
    match st {
        None => {
            let _ = std::fs::remove_file(&path);
        }
        Some(st) => {
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(st)?)?;
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(&tmp, &path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub(super) fn disk_watch_decisions() {
        use DiskAction::*;
        use PauseReason::*;
        // Disk gone and nothing paused: pause.
        assert_eq!(disk_action(false, None), PauseForDisk);
        // Disk gone during a manual pause: keep the manual pause.
        assert_eq!(disk_action(false, Some(Manual)), Nothing);
        assert_eq!(disk_action(false, Some(DiskMissing)), Nothing);
        // Disk back: lift only a pause the disk caused.
        assert_eq!(disk_action(true, Some(DiskMissing)), ResumeFromDisk);
        assert_eq!(disk_action(true, Some(Manual)), Nothing);
        assert_eq!(disk_action(true, None), Nothing);
    }

    #[test]
    pub(super) fn old_pause_files_still_load() {
        // pause.json written before reasons existed.
        let st: PauseState = serde_json::from_str(r#"{"since":1,"until":2}"#).unwrap();
        assert_eq!(st.reason, PauseReason::Manual);
    }
}
