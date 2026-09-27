//! The peer block and allow lists: IP ranges tornas refuses to connect to, or
//! restricts itself to. They are fetched and cached so an offline boot never stops
//! the server from starting.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Serialize;
use tracing::{info, warn};

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

/// A peer block or allow list, as configured. Construction decides what kind of
/// list this is and how a failure to load it should be treated; nothing is
/// fetched until [`PeerList::prepare`].
#[derive(Debug, Clone)]
pub struct PeerList {
    /// Where the list comes from: an http(s) URL, a path, or a `file://` URL.
    source: String,
    /// "blocklist" or "allowlist", for messages.
    kind: &'static str,
    /// Whether an unavailable list stops the server. The allowlist is the one that
    /// must fail closed: running without it would talk to every peer, which is the
    /// opposite of what was asked for.
    fail_closed: bool,
}

impl PeerList {
    /// Peers never to talk to. A list that cannot be loaded is a warning: the
    /// server still starts, just without it.
    pub fn blocklist(spec: &str) -> Option<Self> {
        Self::new(spec, "blocklist", false)
    }

    /// The only peers to talk to. A list that cannot be loaded stops the server.
    pub fn allowlist(spec: &str) -> Option<Self> {
        Self::new(spec, "allowlist", true)
    }

    /// `None` when nothing was configured, which is the normal case.
    fn new(spec: &str, kind: &'static str, fail_closed: bool) -> Option<Self> {
        let spec = spec.trim();
        (!spec.is_empty()).then(|| Self {
            source: spec.to_owned(),
            kind,
            fail_closed,
        })
    }

    /// What to show in logs and `/api/config`: never the query string, which is
    /// where list providers put account keys.
    fn shown(&self) -> String {
        redact_url(&self.source)
    }

    /// Fetch or locate the list and hand librqbit a `file://` URL for it.
    /// `cache` is where a downloaded copy is kept so an offline boot still works.
    pub async fn prepare(&self, cache: &Path) -> anyhow::Result<IpListStatus> {
        let shown = self.shown();
        let mut st = IpListStatus {
            source: Some(shown.clone()),
            ..Default::default()
        };
        let kind = self.kind;
        let give_up = |st: &mut IpListStatus, why: String| -> anyhow::Result<()> {
            if self.fail_closed {
                bail!("peer {kind} {shown}: {why}; refusing to start without it");
            }
            warn!("peer {kind} {shown}: {why}; running without it");
            st.note = Some(format!("{why}; running without the {kind}"));
            Ok(())
        };

        if self.source.starts_with("http://") || self.source.starts_with("https://") {
            match download(&self.source).await {
                Ok(bytes) => {
                    let tmp = cache.with_extension("tmp");
                    std::fs::write(&tmp, &bytes)?;
                    std::fs::rename(&tmp, cache)?;
                    info!(
                        "peer {kind}: downloaded {} from {shown}",
                        crate::utils::human_bytes(bytes.len() as u64)
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
            let path = PathBuf::from(self.source.strip_prefix("file://").unwrap_or(&self.source));
            match file_url(&path) {
                Ok(u) => st.loaded_from = Some(u),
                Err(e) => give_up(&mut st, format!("{e:#}"))?,
            }
        }
        Ok(st)
    }
}

/// The status of a list that was not configured at all.
impl IpListStatus {
    /// Prepare `list` if there is one, otherwise report "not configured".
    pub async fn prepare(list: Option<PeerList>, cache: &Path) -> anyhow::Result<Self> {
        match list {
            Some(l) => l.prepare(cache).await,
            None => Ok(Self::default()),
        }
    }
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
            crate::utils::human_bytes(MAX as u64)
        );
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ip_lists_fail_open_or_closed() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("blocklist.cache");

        // A missing local file: the blocklist starts without it, the allowlist won't.
        let missing = "/nonexistent/list.txt";
        let st = PeerList::blocklist(missing)
            .unwrap()
            .prepare(&cache)
            .await
            .unwrap();
        assert!(st.loaded_from.is_none() && st.note.is_some());
        assert!(
            PeerList::allowlist(missing)
                .unwrap()
                .prepare(&cache)
                .await
                .is_err()
        );

        // A local file becomes a file:// URL.
        let f = dir.path().join("list.txt");
        std::fs::write(&f, "bad:1.2.3.4-1.2.3.5\n").unwrap();
        let st = PeerList::blocklist(f.to_str().unwrap())
            .unwrap()
            .prepare(&cache)
            .await
            .unwrap();
        assert!(st.loaded_from.unwrap().starts_with("file://"));

        // An unreachable URL falls back to the cached copy, even fail-closed.
        std::fs::write(&cache, "bad:1.2.3.4-1.2.3.5\n").unwrap();
        let st = PeerList::allowlist("http://127.0.0.1:9/list")
            .unwrap()
            .prepare(&cache)
            .await
            .unwrap();
        assert!(st.loaded_from.is_some() && st.note.unwrap().contains("cached"));
    }

    #[test]
    fn nothing_configured_is_not_a_list() {
        assert!(PeerList::blocklist("").is_none());
        assert!(PeerList::allowlist("   ").is_none());
        assert!(
            PeerList::blocklist(" /etc/list.txt ").is_some(),
            "specs are trimmed"
        );
    }

    #[tokio::test]
    async fn an_absent_list_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let st = IpListStatus::prepare(None, &dir.path().join("x.cache"))
            .await
            .unwrap();
        assert!(st.source.is_none() && st.loaded_from.is_none());
    }
}
