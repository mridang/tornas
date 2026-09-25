//! `self-update`: fetch the matching release binary from GitHub, verify its
//! SHA-256 against the published checksum, and atomically replace this executable.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;
use tracing::info;

use crate::config::UpdateOpts;

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// Written by the .deb postinst: the package manager owns the binary, so the
/// daemon must not replace it. `apt upgrade` is the update path instead.
pub const APT_MANAGED_MARKER: &str = "/etc/tornas/apt-managed";

pub fn apt_managed() -> bool {
    std::path::Path::new(APT_MANAGED_MARKER).exists()
}

/// Release asset suffix for the running binary's architecture.
pub fn asset_arch() -> anyhow::Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-amd64",
        ("linux", "aarch64") => "linux-arm64",
        ("linux", "arm") => "linux-armv7",
        (os, arch) => bail!("no release binary for {os}/{arch}"),
    })
}

/// Parse `<hex>  <name>` or bare `<hex>` checksum files.
pub fn parse_sha256(text: &str) -> Option<String> {
    text.split_whitespace()
        .next()
        .filter(|h| h.len() == 64)
        .map(|h| h.to_ascii_lowercase())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn version_newer(remote_tag: &str, local: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split('.')
            .filter_map(|p| p.parse().ok())
            .collect()
    };
    parse(remote_tag) > parse(local)
}

/// Outcome of an update attempt.
pub enum Outcome {
    UpToDate { current: String, available: String },
    Installed { version: String, path: PathBuf },
}

/// What a release lookup found.
pub struct Available {
    pub current: String,
    pub latest: String,
    /// Whether `latest` is actually newer than what is running.
    pub newer: bool,
    release: Release,
}

/// An update to perform: which repository, which version, and how strict to be.
///
/// Construct it once — the defaults are the safe ones, and each relaxation has to
/// be asked for by name rather than passed as a positional flag.
pub struct Updater {
    repo: String,
    /// A specific tag, or the latest release.
    version: Option<String>,
    /// Install even when the running version is not older.
    force: bool,
    /// Install even when the release publishes no checksum.
    allow_unverified: bool,
    /// Where to write, if not this executable.
    install_path: Option<PathBuf>,
    client: reqwest::Client,
}

impl Updater {
    pub fn new(repo: impl Into<String>) -> anyhow::Result<Self> {
        Ok(Self {
            repo: repo.into(),
            version: None,
            force: false,
            allow_unverified: false,
            install_path: None,
            client: reqwest::Client::builder()
                .user_agent(concat!("tornas/", env!("CARGO_PKG_VERSION")))
                .build()
                .context("building HTTP client")?,
        })
    }

    /// The settings behind `tornas self-update`.
    pub fn from_opts(opts: &UpdateOpts) -> anyhow::Result<Self> {
        Ok(Self {
            version: opts.version.clone(),
            force: opts.force,
            allow_unverified: opts.allow_unverified,
            install_path: opts.install_path.clone(),
            ..Self::new(opts.repo.clone())?
        })
    }

    pub fn version(mut self, tag: impl Into<String>) -> Self {
        self.version = Some(tag.into());
        self
    }

    pub fn force(mut self, yes: bool) -> Self {
        self.force = yes;
        self
    }

    pub fn allow_unverified(mut self, yes: bool) -> Self {
        self.allow_unverified = yes;
        self
    }

    pub fn install_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.install_path = Some(path.into());
        self
    }

    /// Ask GitHub what is published, without downloading anything.
    pub async fn check(&self) -> anyhow::Result<Available> {
        let url = match &self.version {
            Some(v) => format!(
                "https://api.github.com/repos/{}/releases/tags/{}",
                self.repo,
                v.trim_start_matches('v')
            ),
            None => format!("https://api.github.com/repos/{}/releases/latest", self.repo),
        };
        let release: Release = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .context("GitHub release lookup")?
            .json()
            .await?;
        let current = env!("CARGO_PKG_VERSION").to_owned();
        Ok(Available {
            newer: version_newer(&release.tag_name, &current),
            latest: release.tag_name.clone(),
            current,
            release,
        })
    }

    /// Whether this update should go ahead: normally only when the release is
    /// newer, unless a version was pinned or `force` was asked for.
    fn wanted(&self, found: &Available) -> bool {
        found.newer || self.force || self.version.is_some()
    }

    /// Look up the latest release and install it if it is newer.
    pub async fn install(&self) -> anyhow::Result<Outcome> {
        let found = self.check().await?;
        self.install_when_wanted(&found).await
    }

    /// Install a release already looked up by [`Updater::check`], unless nothing
    /// newer is published.
    pub async fn install_when_wanted(&self, found: &Available) -> anyhow::Result<Outcome> {
        if !self.wanted(found) {
            return Ok(Outcome::UpToDate {
                current: found.current.clone(),
                available: found.latest.clone(),
            });
        }
        self.install_release(found).await
    }

    /// Install a release already looked up by [`Updater::check`].
    pub async fn install_release(&self, found: &Available) -> anyhow::Result<Outcome> {
        let bytes = self.download_verified(&found.release).await?;
        let target = match &self.install_path {
            Some(p) => p.clone(),
            None => std::env::current_exe().context("locating current executable")?,
        };
        install_atomically(&target, &bytes)?;
        Ok(Outcome::Installed {
            version: found.latest.clone(),
            path: target,
        })
    }

    /// The release binary for this architecture, checked against its published
    /// SHA-256. Refuses an unverified download unless that was asked for.
    async fn download_verified(&self, release: &Release) -> anyhow::Result<Vec<u8>> {
        let bin_name = format!("tornas-{}", asset_arch()?);
        let bin = release
            .assets
            .iter()
            .find(|a| a.name == bin_name)
            .with_context(|| format!("release has no asset {bin_name}"))?;
        info!(
            "downloading {} ({} bytes)",
            bin.browser_download_url, bin.size
        );
        let bytes = self.get_bytes(&bin.browser_download_url).await?;

        let checksum = release
            .assets
            .iter()
            .find(|a| a.name == format!("{bin_name}.sha256"));
        match checksum {
            Some(sum) => {
                let text = String::from_utf8(self.get_bytes(&sum.browser_download_url).await?)
                    .context("checksum asset is not text")?;
                let expected = parse_sha256(&text).context("unreadable checksum asset")?;
                let actual = sha256_hex(&bytes);
                if expected != actual {
                    bail!("checksum mismatch: expected {expected}, got {actual}; not installing");
                }
            }
            None if self.allow_unverified => {}
            None => bail!(
                "release has no {bin_name}.sha256 asset; refusing to install unverified \
                 (pass --allow-unverified to override)"
            ),
        }
        Ok(bytes)
    }

    async fn get_bytes(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        Ok(self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec())
    }
}

/// Replace this process with the freshly installed binary, keeping argv and the
/// PID (so systemd's MAINPID and notify socket stay valid). Only returns on failure.
pub fn exec_self() -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return e,
    };
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    std::process::Command::new(exe).args(args).exec()
}

/// Background loop: check on `interval` with random jitter, install when newer,
/// then stop the engine cleanly and exec the new binary.
pub async fn auto_update_forever(
    engine: std::sync::Arc<crate::engine::Engine>,
    interval: std::time::Duration,
    repo: String,
    cancel: tokio_util::sync::CancellationToken,
) {
    use rand::Rng;
    if apt_managed() {
        info!(
            "auto-update disabled: installed from a .deb ({APT_MANAGED_MARKER} exists); use apt upgrade"
        );
        return;
    }
    let interval = interval.max(std::time::Duration::from_secs(600));
    info!("auto-update enabled: checking {repo} every {interval:?}");
    loop {
        let jitter = std::time::Duration::from_secs(
            rand::rng().random_range(0..(interval.as_secs() / 10).max(1)),
        );
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(interval + jitter) => {}
        }
        let updater = match Updater::new(repo.clone()) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("auto-update: {e:#}");
                return;
            }
        };
        match updater.install().await {
            Ok(Outcome::UpToDate { .. }) => {}
            Ok(Outcome::Installed { version, path }) => {
                info!(
                    "auto-update installed {version} at {}; restarting",
                    path.display()
                );
                let _ = engine
                    .catalog
                    .add_event("update", &format!("installed {version}, restarting"));
                crate::metrics::update_installed();
                crate::systemd::stopping();
                engine.session.stop().await;
                let err = exec_self();
                tracing::error!(
                    "exec of new binary failed: {err}; exiting so the supervisor restarts us"
                );
                std::process::exit(0);
            }
            Err(e) => tracing::warn!("auto-update check failed: {e:#}"),
        }
    }
}

/// `tornas self-update`: report what is published, then install it unless this is
/// only a check.
pub async fn run(opts: UpdateOpts) -> anyhow::Result<()> {
    if apt_managed() && !opts.force && opts.install_path.is_none() {
        bail!(
            "tornas was installed from a .deb; run `apt update && apt install tornas` instead (or pass --force)"
        );
    }
    let restart = opts.restart;
    let service = opts.service.clone();
    let check_only = opts.check;
    let updater = Updater::from_opts(&opts)?;

    let found = updater.check().await?;
    crate::outln!(
        "current {}, available {} ({})",
        found.current,
        found.latest,
        asset_arch()?
    );
    if check_only {
        if found.newer {
            crate::outln!("update available");
            std::process::exit(10);
        }
        crate::outln!("up to date");
        return Ok(());
    }
    match updater.install_when_wanted(&found).await? {
        Outcome::UpToDate { .. } => {
            crate::outln!("up to date");
            return Ok(());
        }
        Outcome::Installed { version, path } => {
            crate::outln!("installed {version} to {}", path.display());
        }
    }

    if restart {
        match std::process::Command::new("systemctl")
            .args(["restart", &service])
            .status()
        {
            Ok(s) if s.success() => crate::outln!("restarted {service}"),
            Ok(s) => bail!("systemctl restart exited with {s}"),
            Err(e) => bail!("running systemctl: {e}"),
        }
    }
    Ok(())
}

/// Write next to the target and rename over it, so a crash mid-write never
/// leaves a half binary in place.
pub fn install_atomically(target: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = target.parent().context("target has no parent directory")?;
    let tmp: PathBuf = dir.join(format!(
        ".{}.new",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("tornas")
    ));
    std::fs::write(&tmp, bytes)
        .with_context(|| format!("writing {tmp:?} (need write access to {dir:?})"))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, target).with_context(|| format!("replacing {target:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_checksums() {
        let h = "a".repeat(64);
        assert_eq!(
            parse_sha256(&format!("{h}  tornas-linux-arm64\n")).unwrap(),
            h
        );
        assert_eq!(parse_sha256(&h).unwrap(), h);
        assert!(parse_sha256("nope").is_none());
    }

    #[test]
    fn compares_versions() {
        assert!(version_newer("v0.2.0", "0.1.0"));
        assert!(version_newer("1.0.0", "0.9.9"));
        assert!(!version_newer("v0.1.0", "0.1.0"));
        assert!(!version_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn installs_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("bin");
        std::fs::write(&target, b"old").unwrap();
        install_atomically(&target, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(std::fs::read_dir(dir.path()).unwrap().count() == 1);
    }

    fn found(latest: &str, newer: bool) -> Available {
        Available {
            current: "1.0.0".into(),
            latest: latest.into(),
            newer,
            release: Release {
                tag_name: latest.into(),
                assets: vec![],
            },
        }
    }

    #[test]
    fn decides_when_an_update_should_go_ahead() {
        let u = Updater::new("mridang/tornas").unwrap();
        assert!(u.wanted(&found("v1.1.0", true)), "a newer release installs");
        assert!(
            !u.wanted(&found("v1.0.0", false)),
            "the same version does not"
        );
        assert!(
            u.force(true).wanted(&found("v1.0.0", false)),
            "--force installs anyway"
        );
        assert!(
            Updater::new("mridang/tornas")
                .unwrap()
                .version("v0.9.0")
                .wanted(&found("v0.9.0", false)),
            "a pinned version installs even when it is older"
        );
    }
}
