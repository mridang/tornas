//! The TOML config file: `[trackers]`, `[network]` and `[bandwidth]`. Everything
//! here has a default, and flags and environment variables override it.

use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::trackers::TrackersConfig;

/// Optional TOML config file. Everything in it has a default; flags and
/// environment variables override the file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FileConfig {
    pub trackers: TrackersConfig,
    pub network: NetworkConfig,
    pub bandwidth: crate::schedule::BandwidthConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct NetworkConfig {
    /// Bind IPv6 dual-stack sockets (`[::]`) for BitTorrent, DHT and HTTP. When false
    /// everything binds IPv4 only.
    pub ipv6: bool,
    /// Source ranges allowed to reach the HTTP server. Defaults to loopback, the
    /// private ranges and Tailscale, so the box is LAN-only out of the box.
    pub allow_from: Vec<String>,
    /// Proxies whose `X-Forwarded-For` header may name the real client.
    pub trusted_proxies: Vec<String>,
}
impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            ipv6: true,
            allow_from: crate::http::netacl::DEFAULT_ALLOW
                .split(',')
                .map(str::to_owned)
                .collect(),
            trusted_proxies: Vec::new(),
        }
    }
}

impl FileConfig {
    /// Standard locations, first match wins: /etc/tornas/config.toml (Debian/FHS),
    /// then $XDG_CONFIG_HOME/tornas/config.toml, then ~/.config/tornas/config.toml.
    pub fn discover() -> Option<PathBuf> {
        let mut candidates = vec![PathBuf::from("/etc/tornas/config.toml")];
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
            candidates.push(PathBuf::from(x).join("tornas/config.toml"));
        }
        if let Some(h) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(h).join(".config/tornas/config.toml"));
        }
        candidates.into_iter().find(|p| p.is_file())
    }

    pub fn load(path: Option<&std::path::Path>) -> anyhow::Result<Self> {
        let discovered;
        let path = match path {
            Some(p) => p,
            None => match Self::discover() {
                Some(p) => {
                    tracing::info!("using config file {}", p.display());
                    discovered = p;
                    &discovered
                }
                None => return Ok(Self::default()),
            },
        };
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading config {path:?}"))?;
        toml::from_str(&text).with_context(|| format!("parsing config {path:?}"))
    }
}
