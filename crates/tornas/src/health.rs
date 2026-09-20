//! `health` subcommand for scripts and container HEALTHCHECKs, plus the mount guard.

use std::{path::Path, time::Duration};

use anyhow::{Context, bail};

use crate::{config::HealthOpts, units::human_bytes};

/// Exit 0 when the server answers and its own probe passes; 1 otherwise.
/// Prints one line either way.
pub async fn run(opts: HealthOpts) -> anyhow::Result<()> {
    let url = format!("{}/healthz", opts.server.trim_end_matches('/'));
    let client = reqwest::Client::builder().timeout(opts.timeout).build()?;
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            println!("UNHEALTHY {url}: {e}");
            std::process::exit(1);
        }
    };
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        println!("OK {body}");
        Ok(())
    } else {
        println!("UNHEALTHY {status} {body}");
        std::process::exit(1);
    }
}

/// True when `path` lives on a different filesystem than `/`, i.e. on a mounted disk.
pub fn is_on_separate_filesystem(path: &Path) -> anyhow::Result<bool> {
    use nix::sys::stat::stat;
    let root = stat("/").context("stat /")?;
    let p = stat(path).with_context(|| format!("stat {path:?}"))?;
    Ok(root.st_dev != p.st_dev)
}

/// Refuse to run on the boot medium when asked to. Protects SD cards when the
/// USB disk failed to mount.
pub fn check_mount(data_dir: &Path, require: bool) -> anyhow::Result<()> {
    if !require {
        return Ok(());
    }
    if !is_on_separate_filesystem(data_dir)? {
        bail!(
            "--require-mount: {data_dir:?} is on the root filesystem; refusing to start so downloads do not land on the boot disk"
        );
    }
    Ok(())
}

pub fn describe_disk(path: &Path) -> String {
    match crate::engine::disk_usage(path) {
        Ok((free, total)) => format!("{} free of {}", human_bytes(free), human_bytes(total)),
        Err(e) => format!("unknown ({e})"),
    }
}

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
