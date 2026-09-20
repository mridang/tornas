//! systemd integration: READY once the HTTP listener is up, periodic WATCHDOG
//! pings only while the engine actually answers, STOPPING on shutdown. A no-op
//! when not started by systemd (no NOTIFY_SOCKET).

use std::{sync::Arc, time::Duration};

use sd_notify::NotifyState;
use tracing::{info, warn};

use crate::engine::Engine;

pub fn ready() {
    let _ = sd_notify::notify(&[NotifyState::Ready]);
}

pub fn status(line: &str) {
    let _ = sd_notify::notify(&[NotifyState::Status(line)]);
}

pub fn stopping() {
    let _ = sd_notify::notify(&[NotifyState::Stopping]);
}

/// Returns the watchdog interval systemd asked for, if any.
pub fn interval() -> Option<Duration> {
    sd_notify::watchdog_enabled().filter(|d| !d.is_zero())
}

/// Ping systemd at half the watchdog interval, but only after a real health probe
/// passes. If the engine hangs, the pings stop and systemd restarts the service.
pub async fn run(engine: Arc<Engine>) {
    let Some(iv) = interval() else {
        return;
    };
    let period = iv / 2;
    info!("systemd watchdog enabled, pinging every {period:?}");
    let mut tick = tokio::time::interval(period);
    loop {
        tick.tick().await;
        let e = engine.clone();
        let probe =
            tokio::time::timeout(period, tokio::task::spawn_blocking(move || e.probe())).await;
        match probe {
            Ok(Ok(Ok(()))) => {
                let _ = sd_notify::notify(&[NotifyState::Watchdog]);
            }
            Ok(Ok(Err(e))) => warn!("health probe failed, skipping watchdog ping: {e:#}"),
            Ok(Err(e)) => warn!("health probe panicked: {e}"),
            Err(_) => warn!("health probe timed out, skipping watchdog ping"),
        }
    }
}
