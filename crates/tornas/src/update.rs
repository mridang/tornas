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

/// Fetch the release, verify and install. Shared by the CLI and the auto-updater.
pub async fn fetch_and_install(
    repo: &str,
    version: Option<&str>,
    force: bool,
    allow_unverified: bool,
    install_path: Option<&Path>,
) -> anyhow::Result<Outcome> {
    let arch = asset_arch()?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("tornas/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let url = match version {
        Some(v) => format!(
            "https://api.github.com/repos/{repo}/releases/tags/{}",
            v.trim_start_matches('v')
        ),
        None => format!("https://api.github.com/repos/{repo}/releases/latest"),
    };
    let release: Release = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("GitHub release lookup")?
        .json()
        .await?;
    let current = env!("CARGO_PKG_VERSION").to_owned();
    let newer = version_newer(&release.tag_name, &current);
    if !newer && !force && version.is_none() {
        return Ok(Outcome::UpToDate {
            current,
            available: release.tag_name,
        });
    }
    let bin_name = format!("tornas-{arch}");
    let bin = release
        .assets
        .iter()
        .find(|a| a.name == bin_name)
        .with_context(|| format!("release has no asset {bin_name}"))?;
    let sum = release
        .assets
        .iter()
        .find(|a| a.name == format!("{bin_name}.sha256"));
    info!(
        "downloading {} ({} bytes)",
        bin.browser_download_url, bin.size
    );
    let bytes = client
        .get(&bin.browser_download_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    if let Some(sum) = sum {
        let text = client
            .get(&sum.browser_download_url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let expected = parse_sha256(&text).context("unreadable checksum asset")?;
        let actual = sha256_hex(&bytes);
        if expected != actual {
            bail!("checksum mismatch: expected {expected}, got {actual}; not installing");
        }
    } else if !allow_unverified {
        bail!("release has no {bin_name}.sha256 asset; refusing to install unverified");
    }
    let target = match install_path {
        Some(p) => p.to_owned(),
        None => std::env::current_exe().context("locating current executable")?,
    };
    install_atomically(&target, &bytes)?;
    Ok(Outcome::Installed {
        version: release.tag_name,
        path: target,
    })
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
        match fetch_and_install(&repo, None, false, false, None).await {
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

pub async fn run(opts: UpdateOpts) -> anyhow::Result<()> {
    if apt_managed() && !opts.force && opts.install_path.is_none() {
        bail!(
            "tornas was installed from a .deb; run `apt update && apt install tornas` instead (or pass --force)"
        );
    }
    let arch = asset_arch()?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("tornas/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let url = match &opts.version {
        Some(v) => format!(
            "https://api.github.com/repos/{}/releases/tags/{}",
            opts.repo,
            v.trim_start_matches('v')
        ),
        None => format!("https://api.github.com/repos/{}/releases/latest", opts.repo),
    };
    let release: Release = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("GitHub release lookup")?
        .json()
        .await?;
    let current = env!("CARGO_PKG_VERSION");
    let newer = version_newer(&release.tag_name, current);
    crate::outln!("current {current}, available {} ({arch})", release.tag_name);
    if opts.check {
        if newer {
            crate::outln!("update available");
            std::process::exit(10);
        }
        crate::outln!("up to date");
        return Ok(());
    }
    if !newer && !opts.force && opts.version.is_none() {
        crate::outln!("up to date");
        return Ok(());
    }
    let bin_name = format!("tornas-{arch}");
    let bin = release
        .assets
        .iter()
        .find(|a| a.name == bin_name)
        .with_context(|| format!("release has no asset {bin_name}"))?;
    let sum = release
        .assets
        .iter()
        .find(|a| a.name == format!("{bin_name}.sha256"));

    info!(
        "downloading {} ({} bytes)",
        bin.browser_download_url, bin.size
    );
    let bytes = client
        .get(&bin.browser_download_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    if let Some(sum) = sum {
        let text = client
            .get(&sum.browser_download_url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let expected = parse_sha256(&text).context("unreadable checksum asset")?;
        let actual = sha256_hex(&bytes);
        if expected != actual {
            bail!("checksum mismatch: expected {expected}, got {actual}; not installing");
        }
        crate::outln!("checksum verified");
    } else if !opts.allow_unverified {
        bail!("release has no {bin_name}.sha256 asset; pass --allow-unverified to install anyway");
    }

    let target = match &opts.install_path {
        Some(p) => p.clone(),
        None => std::env::current_exe().context("locating current executable")?,
    };
    install_atomically(&target, &bytes)?;
    crate::outln!("installed {} to {}", release.tag_name, target.display());

    if opts.restart {
        let st = std::process::Command::new("systemctl")
            .args(["restart", &opts.service])
            .status();
        match st {
            Ok(s) if s.success() => crate::outln!("restarted {}", opts.service),
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
}
