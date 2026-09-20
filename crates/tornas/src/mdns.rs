//! mDNS / DNS-SD advertisement so the box answers as `<name>.local` and shows up
//! as an HTTP service in LAN browsers. Borrowed from rqbit's own implementation.

use std::net::SocketAddr;

use anyhow::{Context, bail};
use mdns_sd::{DaemonEvent, ServiceDaemon, ServiceInfo};
use tracing::{debug, info, warn};

pub struct MdnsAdvertisement {
    daemon: ServiceDaemon,
    pub hostname: String,
}

impl Drop for MdnsAdvertisement {
    fn drop(&mut self) {
        if let Err(e) = self.daemon.shutdown() {
            warn!("error shutting down mDNS daemon: {e:#}");
        }
    }
}

/// `name` becomes both the DNS-SD instance and the `<name>.local` hostname.
pub fn advertise(name: &str, listen_addr: SocketAddr) -> anyhow::Result<MdnsAdvertisement> {
    const SERVICE_TYPE: &str = "_http._tcp.local.";
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    if name.is_empty() {
        bail!("mDNS name is empty after sanitising");
    }
    let hostname = format!("{name}.local.");
    let ip = listen_addr.ip();
    if ip.is_loopback() {
        bail!("cannot advertise over mDNS: HTTP listen address is loopback");
    }
    let addr_auto = ip.is_unspecified();
    let addr = if addr_auto {
        String::new()
    } else {
        ip.to_string()
    };
    let properties = [
        ("path", "/"),
        ("manifest", "/manifest.json"),
        ("api", "/api"),
    ];
    let mut info = ServiceInfo::new(
        SERVICE_TYPE,
        &name,
        &hostname,
        addr.as_str(),
        listen_addr.port(),
        &properties[..],
    )
    .context("building mDNS service info")?;
    if addr_auto {
        info = info.enable_addr_auto();
    }
    let daemon = ServiceDaemon::new().context("creating mDNS daemon")?;
    let monitor = daemon.monitor().context("monitoring mDNS daemon")?;
    daemon.register(info).context("registering mDNS service")?;
    std::thread::Builder::new()
        .name("mdns-monitor".into())
        .spawn(move || {
            while let Ok(ev) = monitor.recv() {
                match ev {
                    DaemonEvent::Announce(name, addr) => info!("mDNS announced {name} at {addr}"),
                    DaemonEvent::NameChange(c) => warn!("mDNS name changed after conflict: {c:?}"),
                    other => debug!("mDNS: {other:?}"),
                }
            }
        })
        .context("spawning mDNS monitor thread")?;
    info!(
        "mDNS: advertising http://{}:{}/",
        hostname.trim_end_matches('.'),
        listen_addr.port()
    );
    Ok(MdnsAdvertisement {
        daemon,
        hostname: hostname.trim_end_matches('.').to_owned(),
    })
}
