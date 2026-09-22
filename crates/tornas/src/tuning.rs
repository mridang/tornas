//! Values that depend on the machine: defaults for small boards, and the peer
//! block and allow lists, which are fetched and cached by tornas so an offline
//! boot never stops the server from starting.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Serialize;
use tracing::{info, warn};

use crate::config::ServerOpts;

/// Boards at or under this much RAM get the conservative defaults
/// (Raspberry Pi 3, Zero 2, older Orange Pi and Banana Pi models).
pub const SMALL_BOARD_BYTES: u64 = 1_280 * 1024 * 1024;
const DEFAULT_PEER_LIMIT: u32 = 128;
const SMALL_PEER_LIMIT: u32 = 40;
const DEFAULT_CHECKS: u32 = 3;
const SMALL_CHECKS: u32 = 1;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Tuning {
    pub memory_bytes: Option<u64>,
    pub small_board: bool,
    pub peer_limit: u32,
    pub concurrent_checks: u32,
}

/// Total memory from /proc/meminfo (Linux only).
pub fn total_memory() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_mem_total(&text)
}

fn parse_mem_total(text: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.starts_with("MemTotal:"))?
        .split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()
        .map(|kb| kb * 1024)
}

/// Explicit settings always win; otherwise small boards get lower values.
pub fn choose(memory: Option<u64>, peer_limit: Option<u32>, checks: Option<u32>) -> Tuning {
    let small = memory.is_some_and(|m| m <= SMALL_BOARD_BYTES);
    Tuning {
        memory_bytes: memory,
        small_board: small,
        peer_limit: peer_limit.unwrap_or(if small {
            SMALL_PEER_LIMIT
        } else {
            DEFAULT_PEER_LIMIT
        }),
        concurrent_checks: checks.unwrap_or(if small { SMALL_CHECKS } else { DEFAULT_CHECKS }),
    }
}

pub fn from_opts(opts: &ServerOpts) -> Tuning {
    let t = choose(total_memory(), opts.peer_limit, opts.concurrent_checks);
    if t.small_board {
        info!(
            "small board ({} of memory): using {} peers per torrent and {} simultaneous check(s) unless set explicitly",
            crate::units::human_bytes(t.memory_bytes.unwrap_or(0)),
            t.peer_limit,
            t.concurrent_checks
        );
    }
    t
}

/// Where a list came from and whether it is in force, for warnings and /api/config.
#[derive(Debug, Clone, Serialize, Default)]
pub struct IpListStatus {
    pub source: Option<String>,
    /// The file:// URL handed to librqbit, if any.
    pub loaded_from: Option<String>,
    pub note: Option<String>,
}

/// A list URL as safe to log and show: no user:password and no query string,
/// which is where list providers put account keys.
pub fn redact_url(spec: &str) -> String {
    let Some((scheme, rest)) = spec.split_once("://") else {
        return spec.to_owned();
    };
    let (rest, query) = match rest.split_once('?') {
        Some((r, _)) => (r, "?…"),
        None => (rest, ""),
    };
    let rest = match rest.split_once('/') {
        Some((auth, path)) => format!("{}/{path}", auth.rsplit('@').next().unwrap_or(auth)),
        None => rest.rsplit('@').next().unwrap_or(rest).to_owned(),
    };
    format!("{scheme}://{rest}{query}")
}

fn file_url(path: &Path) -> anyhow::Result<String> {
    let abs = std::fs::canonicalize(path).with_context(|| format!("{path:?} not found"))?;
    Ok(format!("file://{}", abs.display()))
}

/// Resolve a block or allow list into something librqbit can load at startup.
/// http(s) lists are downloaded and cached; on failure the cached copy is used.
/// `fail_closed` (the allowlist) refuses to continue without a list; otherwise the
/// server starts without it and reports why.
pub async fn prepare_ip_list(
    spec: Option<&str>,
    cache: &Path,
    kind: &str,
    fail_closed: bool,
) -> anyhow::Result<IpListStatus> {
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(IpListStatus::default());
    };
    let shown = redact_url(spec);
    let mut st = IpListStatus {
        source: Some(shown.clone()),
        ..Default::default()
    };
    let give_up = |st: &mut IpListStatus, why: String| -> anyhow::Result<()> {
        if fail_closed {
            bail!("peer {kind} {shown}: {why}; refusing to start without it");
        }
        warn!("peer {kind} {shown}: {why}; running without it");
        st.note = Some(format!("{why}; running without the {kind}"));
        Ok(())
    };
    if spec.starts_with("http://") || spec.starts_with("https://") {
        match download(spec).await {
            Ok(bytes) => {
                let tmp = cache.with_extension("tmp");
                std::fs::write(&tmp, &bytes)?;
                std::fs::rename(&tmp, cache)?;
                info!(
                    "peer {kind}: downloaded {} from {shown}",
                    crate::units::human_bytes(bytes.len() as u64)
                );
                st.loaded_from = Some(file_url(cache)?);
            }
            Err(e) if cache.is_file() => {
                warn!("peer {kind}: could not download {shown} ({e:#}); using the cached copy");
                st.note = Some(format!("using a cached copy: {e:#}"));
                st.loaded_from = Some(file_url(cache)?);
            }
            Err(e) => give_up(
                &mut st,
                format!("could not download it ({e:#}) and there is no cached copy"),
            )?,
        }
    } else {
        let path = PathBuf::from(spec.strip_prefix("file://").unwrap_or(spec));
        match file_url(&path) {
            Ok(u) => st.loaded_from = Some(u),
            Err(e) => give_up(&mut st, format!("{e:#}"))?,
        }
    }
    Ok(st)
}

async fn download(url: &str) -> anyhow::Result<Vec<u8>> {
    const MAX: usize = 64 * 1024 * 1024;
    let resp = reqwest::Client::builder()
        .user_agent(concat!("tornas/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()?
        .get(url)
        .send()
        .await?
        .error_for_status()?;
    let bytes = resp.bytes().await?;
    if bytes.len() > MAX {
        bail!(
            "list is larger than {}",
            crate::units::human_bytes(MAX as u64)
        );
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_boards_get_lower_defaults() {
        let gb = 1024 * 1024 * 1024;
        assert_eq!(choose(Some(906 * 1024 * 1024), None, None).peer_limit, 40);
        assert_eq!(
            choose(Some(906 * 1024 * 1024), None, None).concurrent_checks,
            1
        );
        assert_eq!(choose(Some(4 * gb), None, None).peer_limit, 128);
        assert_eq!(choose(Some(4 * gb), None, None).concurrent_checks, 3);
        assert_eq!(
            choose(None, None, None).peer_limit,
            128,
            "unknown memory is not small"
        );
        // explicit settings always win
        let t = choose(Some(512 * 1024 * 1024), Some(200), Some(4));
        assert_eq!(
            (t.peer_limit, t.concurrent_checks, t.small_board),
            (200, 4, true)
        );
    }

    #[test]
    fn parses_meminfo() {
        assert_eq!(
            parse_mem_total("MemTotal:        3884144 kB\nMemFree: 1 kB\n"),
            Some(3_884_144 * 1024)
        );
        assert_eq!(parse_mem_total("nothing"), None);
    }

    #[tokio::test]
    async fn ip_lists_fail_open_or_closed() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("blocklist.cache");
        // missing local file: blocklist starts without it, allowlist refuses
        let st = prepare_ip_list(Some("/nonexistent/list.txt"), &cache, "blocklist", false)
            .await
            .unwrap();
        assert!(st.loaded_from.is_none() && st.note.is_some());
        assert!(
            prepare_ip_list(Some("/nonexistent/list.txt"), &cache, "allowlist", true)
                .await
                .is_err()
        );
        // local file becomes a file:// URL
        let f = dir.path().join("list.txt");
        std::fs::write(&f, "bad:1.2.3.4-1.2.3.5\n").unwrap();
        let st = prepare_ip_list(Some(f.to_str().unwrap()), &cache, "blocklist", false)
            .await
            .unwrap();
        assert!(st.loaded_from.unwrap().starts_with("file://"));
        // unreachable URL with a cached copy uses the cache
        std::fs::write(&cache, "bad:1.2.3.4-1.2.3.5\n").unwrap();
        let st = prepare_ip_list(Some("http://127.0.0.1:9/list"), &cache, "blocklist", true)
            .await
            .unwrap();
        assert!(st.loaded_from.is_some() && st.note.unwrap().contains("cached"));
        // nothing configured
        assert!(
            prepare_ip_list(None, &cache, "blocklist", true)
                .await
                .unwrap()
                .source
                .is_none()
        );
    }

    #[test]
    fn list_urls_are_redacted() {
        assert_eq!(
            redact_url("https://u:p@list.example/bt.gz?id=secret&pin=1"),
            "https://list.example/bt.gz?…"
        );
        assert_eq!(redact_url("https://list.example"), "https://list.example");
        assert_eq!(redact_url("/etc/tornas/allow.txt"), "/etc/tornas/allow.txt");
    }
}
