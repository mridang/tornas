//! `health` subcommand for scripts and container HEALTHCHECKs, plus the mount guard.

use std::{path::Path, time::Duration};

use anyhow::{Context, bail};

use crate::{config::HealthOpts, utils::human_bytes};

/// Exit 0 when the server answers and its own probe passes; 1 otherwise.
/// Prints one line either way.
pub async fn run(opts: HealthOpts) -> anyhow::Result<()> {
    let url = format!("{}/healthz", opts.server.trim_end_matches('/'));
    let client = reqwest::Client::builder().timeout(opts.timeout).build()?;
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            crate::outln!("UNHEALTHY {url}: {e}");
            std::process::exit(1);
        }
    };
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        crate::outln!("OK {body}");
        Ok(())
    } else {
        crate::outln!("UNHEALTHY {status} {body}");
        std::process::exit(1);
    }
}

/// True when `path` lives on a different filesystem than `/`, i.e. on a mounted disk.
/// Free and total bytes on the filesystem holding `path`.
pub fn disk_usage(path: &Path) -> anyhow::Result<(u64, u64)> {
    let st = nix::sys::statvfs::statvfs(path).with_context(|| format!("statvfs {path:?}"))?;
    let frag = st.fragment_size() as u64;
    let free = st.blocks_available() as u64 * frag;
    let total = st.blocks() as u64 * frag;
    Ok((free, total))
}

pub fn is_on_separate_filesystem(path: &Path) -> anyhow::Result<bool> {
    use nix::sys::stat::stat;
    let root = stat("/").context("stat /")?;
    let p = stat(path).with_context(|| format!("stat {path:?}"))?;
    Ok(root.st_dev != p.st_dev)
}

/// Whether the block device holding `path` still exists. A USB disk that is
/// pulled out keeps its mount (and cached directory listings) alive in every mount
/// namespace, systemd's included, but its node under /sys/dev/block goes away.
/// Filesystems without a block device (tmpfs, btrfs subvolumes, network mounts)
/// and systems without sysfs count as present.
pub fn backing_device_present(path: &Path) -> bool {
    match nix::sys::stat::stat(path) {
        // dev_t is u64 on Linux but i32 on macOS.
        #[allow(clippy::unnecessary_cast)]
        Ok(st) => device_present_in(Path::new("/sys/dev/block"), st.st_dev as u64),
        Err(_) => false,
    }
}

fn device_present_in(sys_block: &Path, dev: u64) -> bool {
    // Linux dev_t encoding (glibc and musl agree); elsewhere there is no sysfs.
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    if major == 0 || !sys_block.is_dir() {
        return true;
    }
    sys_block.join(format!("{major}:{minor}")).exists()
}

/// The device (major, minor) mounted at `path` according to a mountinfo table,
/// taking the topmost mount when several are stacked.
pub fn mounted_device(mountinfo: &str, path: &Path) -> Option<(u64, u64)> {
    let unescape = |f: &str| {
        // mountinfo escapes space, tab, newline and backslash as \ooo.
        let mut out = String::new();
        let mut it = f.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\\' {
                let code: String = it.by_ref().take(3).collect();
                match u8::from_str_radix(&code, 8) {
                    Ok(b) => out.push(b as char),
                    Err(_) => {
                        out.push(c);
                        out.push_str(&code);
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    mountinfo
        .lines()
        .rev()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            let (maj, min) = f.get(2)?.split_once(':')?;
            (Path::new(&unescape(f.get(4)?)) == path)
                .then(|| Some((maj.parse().ok()?, min.parse().ok()?)))?
        })
        .next()
}

/// Whether the host (PID 1's view, not this service's private mount namespace)
/// has a working disk mounted at `path`. Under systemd, a disk plugged back in is
/// mounted on the host but never appears inside a namespace made before it left.
pub fn host_has_disk(path: &Path) -> bool {
    let Ok(info) = std::fs::read_to_string("/proc/1/mountinfo") else {
        return false;
    };
    match mounted_device(&info, path) {
        Some((0, _)) => true,
        Some((major, minor)) => Path::new(&format!("/sys/dev/block/{major}:{minor}")).exists(),
        None => false,
    }
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
    match disk_usage(path) {
        Ok((free, total)) => format!("{} free of {}", human_bytes(free), human_bytes(total)),
        Err(e) => format!("unknown ({e})"),
    }
}

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    #[test]
    fn device_presence_follows_sysfs() {
        let sys = tempfile::tempdir().unwrap();
        let makedev =
            |major: u64, minor: u64| (major << 8) | (minor & 0xff) | ((minor & !0xff) << 12);
        let dev = makedev(8, 1);
        assert!(!super::device_present_in(sys.path(), dev), "pulled disk");
        std::fs::create_dir(sys.path().join("8:1")).unwrap();
        assert!(super::device_present_in(sys.path(), dev));
        let tmpfs = makedev(0, 66);
        assert!(
            super::device_present_in(sys.path(), tmpfs),
            "no block device to lose"
        );
        assert!(
            super::device_present_in(&sys.path().join("absent"), dev),
            "no sysfs"
        );
        let nvme = makedev(259, 300);
        std::fs::create_dir(sys.path().join("259:300")).unwrap();
        assert!(super::device_present_in(sys.path(), nvme), "large minors");
    }

    #[test]
    fn finds_the_topmost_mount() {
        let info = "\
22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
90 22 8:1 / /var/lib/tornas rw,relatime shared:50 - ext4 /dev/sda1 rw
91 90 8:17 / /var/lib/tornas rw,relatime shared:51 - ext4 /dev/sdb1 rw
92 22 8:33 / /media/my\\040disk rw,relatime shared:52 - ext4 /dev/sdc1 rw
";
        let p = |s: &str| std::path::PathBuf::from(s);
        assert_eq!(
            super::mounted_device(info, &p("/var/lib/tornas")),
            Some((8, 17))
        );
        assert_eq!(
            super::mounted_device(info, &p("/media/my disk")),
            Some((8, 33))
        );
        assert_eq!(super::mounted_device(info, &p("/srv")), None);
    }
}
