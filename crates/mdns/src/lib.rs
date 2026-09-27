//! mDNS / DNS-SD advertisement: announce a service on the local network so the box
//! answers as `<name>.local` and shows up in LAN browsers. Borrowed from rqbit's
//! own implementation.
//!
//! Knows nothing about what is being advertised. Describe a [`Service`], then
//! [`Service::start`] it; the announcement lives until the returned
//! [`Advertisement`] is dropped.
//!
//! ```ignore
//! let _ad = Service::new("_http._tcp.local.", "tornas", 3030)?
//!     .txt("path", "/")
//!     .txt("api", "/api")
//!     .start(listen_addr.ip())?;
//! ```

use std::net::IpAddr;

use anyhow::{Context, bail};
use mdns_sd::{DaemonEvent, ServiceDaemon, ServiceInfo};
use tracing::{debug, info, warn};

/// A service to announce. Nothing happens until [`Service::start`].
#[derive(Debug, Clone)]
pub struct Service {
    service_type: String,
    /// Already sanitised: DNS-SD label rules are enforced on construction.
    instance: String,
    port: u16,
    txt: Vec<(String, String)>,
}

impl Service {
    /// `service_type` is a DNS-SD type such as `_http._tcp.local.`. `instance` is
    /// both the service name and the `<instance>.local` hostname, and is reduced to
    /// what DNS allows — see [`label`].
    pub fn new(service_type: impl Into<String>, instance: &str, port: u16) -> anyhow::Result<Self> {
        Ok(Self {
            service_type: service_type.into(),
            instance: label(instance)?,
            port,
            txt: Vec::new(),
        })
    }

    /// Add a TXT record. Clients read these to find paths without guessing.
    pub fn txt(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.txt.push((key.into(), value.into()));
        self
    }

    /// The name this service answers to, e.g. `tornas.local`.
    pub fn hostname(&self) -> String {
        format!("{}.local", self.instance)
    }

    /// Begin announcing on `addr`. An unspecified address (`0.0.0.0` or `::`) lets
    /// the daemon track the machine's real addresses as they change.
    pub fn start(&self, addr: IpAddr) -> anyhow::Result<Advertisement> {
        if addr.is_loopback() {
            bail!("cannot advertise over mDNS: the listen address is loopback");
        }
        let daemon = ServiceDaemon::new().context("creating mDNS daemon")?;
        spawn_monitor(&daemon)?;
        daemon
            .register(self.service_info(addr)?)
            .context("registering mDNS service")?;
        info!(
            "mDNS: advertising http://{}:{}/",
            self.hostname(),
            self.port
        );
        Ok(Advertisement {
            daemon,
            hostname: self.hostname(),
        })
    }

    fn service_info(&self, addr: IpAddr) -> anyhow::Result<ServiceInfo> {
        let track_addresses = addr.is_unspecified();
        let addr = if track_addresses {
            String::new()
        } else {
            addr.to_string()
        };
        let txt: Vec<(&str, &str)> = self
            .txt
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let info = ServiceInfo::new(
            &self.service_type,
            &self.instance,
            &format!("{}.local.", self.instance),
            addr.as_str(),
            self.port,
            &txt[..],
        )
        .context("building mDNS service info")?;
        Ok(if track_addresses {
            info.enable_addr_auto()
        } else {
            info
        })
    }
}

/// A live announcement. Dropping it withdraws the service from the network.
pub struct Advertisement {
    daemon: ServiceDaemon,
    pub hostname: String,
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        if let Err(e) = self.daemon.shutdown() {
            warn!("error shutting down mDNS daemon: {e:#}");
        }
    }
}

/// Reduce a name to a DNS label: lowercase, letters, digits and hyphens only.
/// Anything else becomes a hyphen, and leading or trailing hyphens are dropped.
fn label(name: &str) -> anyhow::Result<String> {
    let label: String = name
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
    if label.is_empty() {
        bail!("mDNS name {name:?} has nothing usable in it");
    }
    Ok(label)
}

/// Log what the daemon reports: announcements, and the renames that happen when two
/// boxes pick the same name.
fn spawn_monitor(daemon: &ServiceDaemon) -> anyhow::Result<()> {
    let monitor = daemon.monitor().context("monitoring mDNS daemon")?;
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_become_dns_labels() {
        assert_eq!(label("tornas").unwrap(), "tornas");
        assert_eq!(label("Living Room Box").unwrap(), "living-room-box");
        assert_eq!(label("--x--").unwrap(), "x");
        assert_eq!(label("piÑata_2").unwrap(), "pi-ata-2");
        assert!(label("").is_err());
        assert!(
            label("...").is_err(),
            "nothing usable is an error, not an empty name"
        );
    }

    #[test]
    fn a_service_knows_the_name_it_will_answer_to() {
        let s = Service::new("_http._tcp.local.", "Living Room", 3030).unwrap();
        assert_eq!(s.hostname(), "living-room.local");
    }

    #[test]
    fn loopback_is_refused_before_any_daemon_is_created() {
        let s = Service::new("_http._tcp.local.", "tornas", 3030).unwrap();
        let Err(err) = s.start("127.0.0.1".parse().unwrap()) else {
            panic!("loopback must be refused");
        };
        assert!(err.to_string().contains("loopback"), "{err}");
    }

    #[test]
    fn txt_records_are_kept_in_order() {
        let s = Service::new("_http._tcp.local.", "tornas", 3030)
            .unwrap()
            .txt("path", "/")
            .txt("api", "/api");
        assert_eq!(
            s.txt,
            vec![
                ("path".to_owned(), "/".to_owned()),
                ("api".to_owned(), "/api".to_owned())
            ]
        );
    }
}
