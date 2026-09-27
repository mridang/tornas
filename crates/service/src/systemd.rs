//! Everything this daemon knows about systemd, in one place.
//!
//! Three separate facilities, all no-ops when the process was not started by
//! systemd:
//!
//! - **Notifications** ([`ready`], [`status`], [`stopping`]): `Type=notify` units
//!   wait for READY before considering the service started, and `systemctl status`
//!   shows the STATUS line.
//! - **The watchdog** ([`watch`]): `WatchdogSec=` asks for periodic pings. We only
//!   ping while a caller-supplied probe passes, so a wedged daemon gets restarted
//!   instead of looking alive.
//! - **Restarting on purpose** ([`restart_me`]): a service started by systemd runs
//!   in a private mount namespace, so a disk that is unplugged and plugged back in
//!   never reappears to this process. Exiting lets `Restart=` start a fresh one
//!   that can see it.
//!
//! Nothing here knows what the daemon does; the probe is a closure and the restart
//! reason is a string.

use std::time::Duration;

use sd_notify::NotifyState;
use tracing::{info, warn};

/// The exit code used when we ask systemd for a restart. `EX_TEMPFAIL` from
/// sysexits.h: the work is fine, the environment was temporarily not.
pub const EXIT_RESTART: i32 = 75;

/// Whether systemd started this process. `INVOCATION_ID` is set for every unit,
/// including ones that are not `Type=notify`.
pub fn is_managed() -> bool {
    std::env::var_os("INVOCATION_ID").is_some()
}

/// Whether systemd is listening for notifications (`Type=notify`).
pub fn notify_socket_present() -> bool {
    std::env::var_os("NOTIFY_SOCKET").is_some()
}

/// The service is up and serving requests.
pub fn ready() {
    let _ = sd_notify::notify(&[NotifyState::Ready]);
}

/// One line of human-readable state, shown by `systemctl status`.
pub fn status(line: &str) {
    let _ = sd_notify::notify(&[NotifyState::Status(line)]);
}

/// Shutting down on purpose, so systemd does not treat the exit as a failure.
pub fn stopping() {
    let _ = sd_notify::notify(&[NotifyState::Stopping]);
}

/// The watchdog interval systemd asked for, if any.
pub fn watchdog_interval() -> Option<Duration> {
    sd_notify::watchdog_enabled().filter(|d| !d.is_zero())
}

/// Leave the process so systemd starts a fresh one. Only does anything when
/// systemd is managing us — elsewhere killing ourselves would just stop the
/// service.
pub fn restart_me(why: &str) -> bool {
    if !is_managed() {
        return false;
    }
    warn!("{why}; restarting");
    stopping();
    std::process::exit(EXIT_RESTART);
}

/// Ping systemd at half the watchdog interval, but only once `probe` says the
/// daemon is healthy. A probe that fails, panics or hangs simply skips the ping,
/// and after `WatchdogSec` without one systemd restarts the service.
///
/// `probe` is blocking and runs on the blocking pool, since a health check that
/// touches the disk can stall.
pub async fn watch<P>(probe: P)
where
    P: Fn() -> anyhow::Result<()> + Send + Sync + Clone + 'static,
{
    let Some(iv) = watchdog_interval() else {
        return;
    };
    let period = iv / 2;
    info!("systemd watchdog enabled, pinging every {period:?}");
    let mut tick = tokio::time::interval(period);
    loop {
        tick.tick().await;
        let probe = probe.clone();
        match tokio::time::timeout(period, tokio::task::spawn_blocking(probe)).await {
            Ok(Ok(Ok(()))) => {
                let _ = sd_notify::notify(&[NotifyState::Watchdog]);
            }
            Ok(Ok(Err(e))) => warn!("health probe failed, skipping watchdog ping: {e:#}"),
            Ok(Err(e)) => warn!("health probe panicked: {e}"),
            Err(_) => warn!("health probe timed out, skipping watchdog ping"),
        }
    }
}

/// The systemd integration as a [`Service`](super::Service) component: announce
/// READY once the service is up, ping the watchdog while a probe passes, and send
/// STOPPING on the way out. Adds nothing when the process was not started by
/// systemd. Any Linux daemon can use it.
pub struct Systemd<P> {
    probe: Option<P>,
}

impl Systemd<fn() -> anyhow::Result<()>> {
    /// systemd notifications without a watchdog.
    pub fn notifications() -> Self {
        Self { probe: None }
    }
}

impl<P> Systemd<P>
where
    P: Fn() -> anyhow::Result<()> + Send + Sync + Clone + 'static,
{
    /// systemd notifications plus a watchdog driven by `probe`: the process only
    /// pings while the probe passes, so a hung service is restarted.
    pub fn with_probe(probe: P) -> Self {
        Self { probe: Some(probe) }
    }
}

impl<P> super::Component for Systemd<P>
where
    P: Fn() -> anyhow::Result<()> + Send + Sync + Clone + 'static,
{
    fn register(self: Box<Self>, svc: &mut super::Service) {
        svc.on_ready(ready);
        if let Some(probe) = self.probe {
            svc.spawn("systemd-watchdog", watch(probe));
        }
        svc.on_shutdown(|| async { stopping() });
    }
}
