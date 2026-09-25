//! mDNS / DNS-SD advertisement: announce any service on the local network so the
//! box answers as `<name>.local` and shows up in LAN browsers. Borrowed from
//! rqbit's own implementation.
//!
//! Knows nothing about what is being advertised — the service type and TXT records
//! are arguments.

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
/// Advertise a service on the local network until the returned handle is dropped.
///
/// `service_type` is a DNS-SD type such as `_http._tcp.local.`; `properties` become
/// the TXT record. Nothing here is specific to any one application.
pub fn advertise(
    service_type: &str,
    name: &str,
    listen_addr: SocketAddr,
    properties: &[(&str, &str)],
) -> anyhow::Result<MdnsAdvertisement> {
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
    let mut info = ServiceInfo::new(
        service_type,
        &name,
        &hostname,
        addr.as_str(),
        listen_addr.port(),
        properties,
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
