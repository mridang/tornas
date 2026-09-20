//! Public tracker feed. Periodically fetches tracker lists from configurable
//! sources, keeps only allowed schemes, normalises and deduplicates them, and
//! hands the merged list to every public torrent. Private torrents never see it.

use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use anyhow::Context;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::units::now_secs;

fn d_true() -> bool {
    true
}
fn d_refresh() -> Duration {
    Duration::from_secs(6 * 3600)
}
fn d_timeout() -> Duration {
    Duration::from_secs(20)
}
fn d_stale() -> Duration {
    Duration::from_secs(7 * 86_400)
}
fn d_schemes() -> Vec<String> {
    vec!["https".into(), "udp".into()]
}
fn d_max() -> usize {
    60
}

/// One place to fetch a tracker list from. Any URL returning one tracker per line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Source {
    pub name: String,
    pub url: String,
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Per-source scheme override; defaults to the global list.
    #[serde(default)]
    pub schemes: Option<Vec<String>>,
    /// Only use the first N entries of this source (lists are usually ranked).
    #[serde(default)]
    pub take: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct StaticLists {
    /// Always included, subject to the scheme filter.
    #[serde(default)]
    pub add: Vec<String>,
    /// Never included, matched after normalisation. Supports `*.example.com` host globs.
    #[serde(default)]
    pub block: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackersConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// How often to re-fetch the sources.
    #[serde(default = "d_refresh", with = "humantime_serde")]
    pub refresh: Duration,
    /// Per-source HTTP timeout.
    #[serde(default = "d_timeout", with = "humantime_serde")]
    pub fetch_timeout: Duration,
    /// Keep serving the cached list this long after the last successful fetch.
    #[serde(default = "d_stale", with = "humantime_serde")]
    pub stale_after: Duration,
    /// Allowed URL schemes. Default: https and udp only.
    #[serde(default = "d_schemes")]
    pub schemes: Vec<String>,
    /// Maximum trackers handed to a torrent, in source order.
    #[serde(default = "d_max")]
    pub max: usize,
    /// Drop trackers whose host is a bare IP address.
    #[serde(default)]
    pub reject_ip_hosts: bool,
    /// Re-announce torrents that are still downloading when the list changes.
    #[serde(default = "d_true")]
    pub reannounce_active: bool,
    #[serde(default = "default_sources")]
    pub sources: Vec<Source>,
    #[serde(default, rename = "static")]
    pub static_lists: StaticLists,
}

impl Default for TrackersConfig {
    fn default() -> Self {
        toml::from_str("").expect("defaults")
    }
}

pub fn default_sources() -> Vec<Source> {
    let s = |name: &str, url: &str, take: Option<usize>| Source {
        name: name.into(),
        url: url.into(),
        enabled: true,
        schemes: None,
        take,
    };
    vec![
        s(
            "newtrackon-stable",
            "https://newtrackon.com/api/stable",
            None,
        ),
        s(
            "ngosang-best",
            "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_best.txt",
            None,
        ),
        s("xiu2-best", "https://cf.trackerslist.com/best.txt", None),
    ]
}

/// Well-known lists users can enable by name in the config or CLI.
pub fn known_sources() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("newtrackon-stable", "https://newtrackon.com/api/stable"),
        ("newtrackon-live", "https://newtrackon.com/api/live"),
        ("newtrackon-udp", "https://newtrackon.com/api/udp"),
        ("newtrackon-https", "https://newtrackon.com/api/https"),
        (
            "ngosang-best",
            "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_best.txt",
        ),
        (
            "ngosang-all",
            "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_all.txt",
        ),
        (
            "ngosang-all-udp",
            "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_all_udp.txt",
        ),
        (
            "ngosang-all-https",
            "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_all_https.txt",
        ),
        (
            "ngosang-all-ip",
            "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_all_ip.txt",
        ),
        ("xiu2-best", "https://cf.trackerslist.com/best.txt"),
        ("xiu2-all", "https://cf.trackerslist.com/all.txt"),
        ("xiu2-http", "https://cf.trackerslist.com/http.txt"),
    ])
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SourceStatus {
    pub name: String,
    pub url: String,
    pub enabled: bool,
    pub last_fetch_at: Option<i64>,
    pub last_ok_at: Option<i64>,
    pub last_error: Option<String>,
    /// Lines in the raw response.
    pub fetched: usize,
    /// Entries that survived parsing and the scheme filter.
    pub accepted: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrackerState {
    pub trackers: Vec<String>,
    pub updated_at: Option<i64>,
    pub sources: Vec<SourceStatus>,
    /// Trackers dropped by the scheme filter, block list or IP-host rule in the last refresh.
    pub rejected: usize,
    /// Duplicates collapsed in the last refresh.
    pub deduplicated: usize,
}

pub struct TrackerFeed {
    pub config: RwLock<TrackersConfig>,
    state: RwLock<TrackerState>,
    cache_path: PathBuf,
    client: reqwest::Client,
}

/// Canonical form for comparison and storage: lowercase scheme and host, default
/// ports dropped, trailing slashes trimmed, fragments removed.
pub fn normalize(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }
    let mut u = url::Url::parse(raw).ok()?;
    u.host_str()?;
    u.set_fragment(None);
    let scheme = u.scheme().to_ascii_lowercase();
    let host = u.host_str()?.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        _ => None,
    };
    let port = match u.port() {
        Some(p) if Some(p) == default_port => None,
        p => p,
    };
    let path = u.path().trim_end_matches('/');
    let path = if path.is_empty() && scheme != "udp" {
        "/announce"
    } else {
        path
    };
    let mut out = format!("{scheme}://{host}");
    if let Some(p) = port {
        out.push_str(&format!(":{p}"));
    }
    out.push_str(path);
    if let Some(q) = u.query() {
        out.push('?');
        out.push_str(q);
    }
    Some(out)
}

fn host_of(tracker: &str) -> Option<String> {
    url::Url::parse(tracker)
        .ok()?
        .host_str()
        .map(|h| h.to_ascii_lowercase())
}

fn scheme_of(tracker: &str) -> Option<String> {
    tracker.split("://").next().map(|s| s.to_ascii_lowercase())
}

fn blocked(tracker: &str, block: &[String]) -> bool {
    let host = host_of(tracker).unwrap_or_default();
    block.iter().any(|b| {
        if let Some(suffix) = b.strip_prefix("*.") {
            host == suffix || host.ends_with(&format!(".{suffix}"))
        } else if b.contains("://") {
            normalize(b).as_deref() == Some(tracker)
        } else {
            host == b.to_ascii_lowercase()
        }
    })
}

fn is_ip_host(tracker: &str) -> bool {
    host_of(tracker)
        .map(|h| {
            h.trim_matches(|c| c == '[' || c == ']')
                .parse::<std::net::IpAddr>()
                .is_ok()
        })
        .unwrap_or(false)
}

/// Pure merge step, unit-tested: applies scheme filter, block list, IP rule,
/// dedup and cap. `lists` are in priority order.
pub fn merge(
    cfg: &TrackersConfig,
    lists: &[(Option<Vec<String>>, Vec<String>)],
) -> (Vec<String>, usize, usize) {
    let global: Vec<String> = cfg.schemes.iter().map(|s| s.to_ascii_lowercase()).collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut rejected = 0;
    let mut dups = 0;
    let statics = (None, cfg.static_lists.add.clone());
    for (schemes, list) in lists.iter().chain(std::iter::once(&statics)) {
        let allowed: Vec<String> = schemes
            .as_ref()
            .map(|s| s.iter().map(|x| x.to_ascii_lowercase()).collect())
            .unwrap_or_else(|| global.clone());
        for raw in list {
            let Some(t) = normalize(raw) else {
                if !raw.trim().is_empty() && !raw.trim_start().starts_with('#') {
                    rejected += 1;
                }
                continue;
            };
            let ok_scheme = scheme_of(&t).map(|s| allowed.contains(&s)).unwrap_or(false);
            if !ok_scheme
                || blocked(&t, &cfg.static_lists.block)
                || (cfg.reject_ip_hosts && is_ip_host(&t))
            {
                rejected += 1;
                continue;
            }
            if !seen.insert(t.clone()) {
                dups += 1;
                continue;
            }
            out.push(t);
        }
    }
    out.truncate(cfg.max);
    (out, rejected, dups)
}

impl TrackerFeed {
    pub fn new(config: TrackersConfig, data_dir: &std::path::Path) -> Self {
        let cache_path = data_dir.join("trackers.json");
        let state = std::fs::read(&cache_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<TrackerState>(&b).ok())
            .unwrap_or_default();
        let client = reqwest::Client::builder()
            .user_agent(concat!("tornas/", env!("CARGO_PKG_VERSION")))
            .timeout(config.fetch_timeout)
            .build()
            .expect("reqwest client");
        if !state.trackers.is_empty() {
            info!(
                "loaded {} cached trackers from {:?}",
                state.trackers.len(),
                cache_path
            );
        }
        Self {
            config: RwLock::new(config),
            state: RwLock::new(state),
            cache_path,
            client,
        }
    }

    pub fn state(&self) -> TrackerState {
        self.state.read().clone()
    }

    /// Trackers to attach to a public torrent right now. Empty if disabled or stale.
    pub fn current(&self) -> Vec<String> {
        let cfg = self.config.read();
        if !cfg.enabled {
            return vec![];
        }
        let st = self.state.read();
        let fresh = st
            .updated_at
            .map(|t| now_secs() - t <= cfg.stale_after.as_secs() as i64)
            .unwrap_or(false);
        if fresh {
            st.trackers.clone()
        } else {
            merge(&cfg, &[]).0
        }
    }

    async fn fetch_source(&self, src: &Source) -> anyhow::Result<Vec<String>> {
        let resp = self
            .client
            .get(&src.url)
            .send()
            .await
            .with_context(|| format!("GET {}", src.url))?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("{} returned {status}", src.url);
        }
        let mut lines: Vec<String> = body
            .lines()
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty())
            .collect();
        if let Some(n) = src.take {
            lines.truncate(n);
        }
        Ok(lines)
    }

    /// Fetch every enabled source, merge, persist. Returns true if the list changed.
    pub async fn refresh(&self) -> anyhow::Result<bool> {
        let cfg = self.config.read().clone();
        if !cfg.enabled {
            return Ok(false);
        }
        let now = now_secs();
        let mut lists = Vec::new();
        let mut statuses = Vec::new();
        for src in cfg.sources.iter() {
            let mut st = SourceStatus {
                name: src.name.clone(),
                url: src.url.clone(),
                enabled: src.enabled,
                ..Default::default()
            };
            if !src.enabled {
                statuses.push(st);
                continue;
            }
            st.last_fetch_at = Some(now);
            match self.fetch_source(src).await {
                Ok(lines) => {
                    st.fetched = lines.len();
                    let (accepted, _, _) = merge(
                        &TrackersConfig {
                            max: usize::MAX,
                            static_lists: Default::default(),
                            ..cfg.clone()
                        },
                        &[(src.schemes.clone(), lines.clone())],
                    );
                    st.accepted = accepted.len();
                    st.last_ok_at = Some(now);
                    lists.push((src.schemes.clone(), lines));
                }
                Err(e) => {
                    warn!("tracker source {}: {e:#}", src.name);
                    st.last_error = Some(format!("{e:#}"));
                    // keep the previous list for this source, if any, so one outage doesn't shrink the set
                }
            }
            statuses.push(st);
        }
        let any_ok = statuses.iter().any(|s| s.last_ok_at == Some(now));
        let (trackers, rejected, deduplicated) = merge(&cfg, &lists);
        let changed = {
            let mut st = self.state.write();
            let changed = st.trackers != trackers;
            st.sources = statuses;
            if any_ok || lists.is_empty() && st.trackers.is_empty() {
                st.trackers = trackers;
                st.updated_at = Some(now);
                st.rejected = rejected;
                st.deduplicated = deduplicated;
            }
            let _ = std::fs::write(
                &self.cache_path,
                serde_json::to_vec_pretty(&*st).unwrap_or_default(),
            );
            changed && any_ok
        };
        let n = self.state.read().trackers.len();
        info!(
            "tracker refresh: {n} trackers ({rejected} rejected, {deduplicated} duplicates), changed={changed}"
        );
        crate::metrics::trackers(n, self.state.read().sources.iter());
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(
            normalize("UDP://Tracker.Example.com:6969/announce/").unwrap(),
            "udp://tracker.example.com:6969/announce"
        );
        assert_eq!(
            normalize("https://t.example.com:443/announce").unwrap(),
            "https://t.example.com/announce"
        );
        assert_eq!(
            normalize("https://t.example.com").unwrap(),
            "https://t.example.com/announce"
        );
        assert_eq!(
            normalize("http://t.example.com:8080/a?x=1#frag").unwrap(),
            "http://t.example.com:8080/a?x=1"
        );
        assert!(normalize("# comment").is_none());
        assert!(normalize("not a url").is_none());
        assert!(normalize("").is_none());
    }

    #[test]
    fn merges_filters_and_dedups() {
        let mut cfg = TrackersConfig::default();
        cfg.static_lists.add = vec!["udp://static.example.com:1337/announce".into()];
        cfg.static_lists.block = vec![
            "*.bad.example".into(),
            "udp://blocked.example.com:1/announce".into(),
        ];
        let lists = vec![
            (
                None,
                vec![
                    "udp://a.example.com:6969/announce".into(),
                    "UDP://A.example.com:6969/announce/".into(),
                    "http://plain.example.com/announce".into(),
                    "wss://ws.example.com/announce".into(),
                    "udp://x.bad.example:1/announce".into(),
                    "udp://blocked.example.com:1/announce".into(),
                    "garbage".into(),
                    "https://b.example.com/announce".into(),
                ],
            ),
            (
                Some(vec!["http".into()]),
                vec![
                    "http://allowed-here.example.com/announce".into(),
                    "udp://not-here.example.com:1/announce".into(),
                ],
            ),
        ];
        let (out, rejected, dups) = merge(&cfg, &lists);
        assert_eq!(
            out,
            vec![
                "udp://a.example.com:6969/announce",
                "https://b.example.com/announce",
                "http://allowed-here.example.com/announce",
                "udp://static.example.com:1337/announce",
            ]
        );
        assert_eq!(dups, 1);
        assert_eq!(rejected, 6);
    }

    #[test]
    fn caps_and_rejects_ip_hosts() {
        let cfg = TrackersConfig {
            max: 2,
            reject_ip_hosts: true,
            ..Default::default()
        };
        let lists = vec![(
            None,
            vec![
                "udp://1.2.3.4:6969/announce".into(),
                "udp://a.example.com:1/announce".into(),
                "udp://b.example.com:1/announce".into(),
                "udp://c.example.com:1/announce".into(),
            ],
        )];
        let (out, rejected, _) = merge(&cfg, &lists);
        assert_eq!(out.len(), 2);
        assert_eq!(rejected, 1);
    }

    #[test]
    fn config_defaults_parse() {
        let cfg: TrackersConfig = toml::from_str(
            r#"
            schemes = ["https", "udp", "http"]
            refresh = "1h"
            [[sources]]
            name = "mine"
            url = "https://example.com/list.txt"
            take = 10
            [static]
            add = ["udp://x.example.com:1/announce"]
        "#,
        )
        .unwrap();
        assert_eq!(cfg.refresh, Duration::from_secs(3600));
        assert_eq!(cfg.sources.len(), 1);
        assert_eq!(cfg.sources[0].take, Some(10));
        assert!(cfg.enabled);
        assert_eq!(TrackersConfig::default().sources.len(), 3);
    }
}
