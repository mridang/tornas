//! Watching the data disk. With `--require-mount` on, a disk that disappears pauses
//! everything rather than letting downloads land on the boot medium, and the pause
//! lifts itself when the disk comes back.

use tracing::{info, warn};

use crate::utils::now_secs;

use super::*;

impl Engine {
    pub fn is_disk_missing(&self) -> bool {
        self.disk_missing.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The data directory is on its own filesystem and readable. Only meaningful
    /// with --require-mount; otherwise always true.
    pub(super) fn data_disk_ok(&self) -> bool {
        if !self.opts.require_mount {
            return true;
        }
        matches!(
            crate::utils::mount::is_on_separate_filesystem(&self.opts.data_dir),
            Ok(true)
        ) && std::fs::read_dir(&self.torrents_dir).is_ok()
            && crate::utils::mount::backing_device_present(&self.torrents_dir)
    }

    /// Pause for a missing disk: in memory only, never persisted, lifted when the
    /// disk returns.
    pub(super) async fn pause_for_disk(&self) {
        let _guard = self.add_lock.lock().await;
        if self.pause.lock().is_some() {
            return;
        }
        *self.pause.lock() = Some(PauseState {
            since: now_secs(),
            until: None,
            reason: PauseReason::DiskMissing,
        });
        let n = self.apply_pause().await;
        crate::metrics::paused();
        warn!(
            "the data disk at {} is not mounted: paused everything ({n} torrents) until it is back",
            self.opts.data_dir.display()
        );
    }

    /// Resume once the pause expires, and while paused re-pause anything that
    /// came loose (a re-announce or restore racing the pause).
    pub async fn check_pause(&self) -> anyhow::Result<()> {
        let ok = self.data_disk_ok();
        let was_missing = self
            .disk_missing
            .swap(!ok, std::sync::atomic::Ordering::SeqCst);
        if ok && was_missing {
            info!("the data disk is back");
            let _ = self
                .library
                .store()
                .add_event("disk", "the data disk came back");
        }
        let reason = self.pause.lock().as_ref().map(|p| p.reason);
        match disk_action(ok, reason) {
            DiskAction::PauseForDisk => {
                self.pause_for_disk().await;
                return Ok(());
            }
            DiskAction::ResumeFromDisk => {
                self.resume_all("disk").await?;
                return Ok(());
            }
            DiskAction::Nothing => {}
        }
        // Under systemd this process has a private mount namespace, so a disk that
        // is plugged back in never shows up here. When the host has it mounted
        // again, let systemd start a fresh process that sees it.
        if !ok
            && crate::systemd::is_managed()
            && crate::utils::mount::host_has_disk(&self.opts.data_dir)
        {
            let _ = self
                .library
                .store()
                .add_event("disk", "the data disk came back; restarting");
            crate::systemd::restart_me("the data disk is mounted again on the host");
        }
        let st = self.pause.lock().clone();
        match st {
            None => {
                self.balance_queue().await?;
            }
            // A timed pause does not end while the disk is missing.
            Some(p) if ok && p.until.is_some_and(|u| now_secs() >= u) => {
                self.resume_all("auto").await?;
            }
            Some(_) => {
                let n = self.apply_pause().await;
                if n > 0 {
                    warn!("pause enforcement re-paused {n} torrents");
                }
            }
        }
        Ok(())
    }
}
