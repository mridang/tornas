//! `doctor`: report what this machine can do, including whether piece hashing
//! runs on native SHA-1 instructions, and benchmark it with the same SHA-1
//! implementation the torrent engine uses.

use std::{path::Path, time::Instant};

use sha1w::ISha1;

use crate::{config::DoctorOpts, utils::human_bytes};

/// CPU feature flags relevant to hashing, from /proc/cpuinfo on Linux.
pub fn cpu_features() -> Vec<String> {
    let wanted = [
        "sha1", "sha2", "sha3", "sha_ni", "sha512", "aes", "neon", "asimd", "avx2", "sse4_2",
        "crc32", "pmull",
    ];
    let text = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let mut found: Vec<String> = Vec::new();
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("flags") || lower.starts_with("features") {
            for tok in lower.split_whitespace() {
                if wanted.contains(&tok) && !found.contains(&tok.to_owned()) {
                    found.push(tok.to_owned());
                }
            }
        }
    }
    found.sort();
    found
}

pub fn cpu_model() -> String {
    let text = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    for key in ["model name", "Model", "Hardware", "cpu model"] {
        if let Some(l) = text.lines().find(|l| l.starts_with(key)) {
            if let Some((_, v)) = l.split_once(':') {
                return v.trim().to_owned();
            }
        }
    }
    if cfg!(target_os = "macos") {
        "macOS host".into()
    } else {
        "unknown".into()
    }
}

/// Hash `mib` MiB of data in 1 MiB chunks; returns MiB/s.
pub fn bench_sha1(mib: usize) -> f64 {
    let chunk = vec![0x5au8; 1 << 20];
    let mut h = sha1w::Sha1::new();
    let start = Instant::now();
    for _ in 0..mib {
        h.update(&chunk);
    }
    let _ = h.finish();
    mib as f64 / start.elapsed().as_secs_f64()
}

pub fn run(opts: DoctorOpts) -> anyhow::Result<()> {
    crate::outln!("tornas {}", env!("CARGO_PKG_VERSION"));
    crate::outln!(
        "os/arch:     {}/{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    crate::outln!("cpu:         {}", cpu_model());
    let feats = cpu_features();
    crate::outln!(
        "cpu flags:   {}",
        if feats.is_empty() {
            "(unavailable)".into()
        } else {
            feats.join(" ")
        }
    );
    let hw_sha1 = feats.iter().any(|f| f == "sha1" || f == "sha_ni");
    crate::outln!(
        "sha1 in hw:  {}",
        match (std::env::consts::OS, hw_sha1) {
            ("linux", true) => "yes (aws-lc-rs picks the SHA extension at runtime)",
            ("linux", false) if std::env::consts::ARCH == "arm" =>
                "no (ARMv7 has no SHA extension; software NEON-less path)",
            ("linux", false) => "no (CPU lacks sha1/sha_ni flags; software path)",
            _ => "n/a (reported on Linux only)",
        }
    );
    let rate = bench_sha1(opts.bench_mib);
    crate::outln!(
        "sha1 speed:  {rate:.0} MiB/s over {} MiB (this bounds hash-check speed on add)",
        opts.bench_mib
    );
    crate::outln!(
        "cores:       {}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    );
    if let Some(dir) = &opts.data_dir {
        let dir: &Path = dir;
        crate::outln!(
            "data dir:    {} ({})",
            dir.display(),
            crate::utils::mount::describe_disk(dir)
        );
        match crate::utils::mount::is_on_separate_filesystem(dir) {
            Ok(true) => {
                crate::outln!("mount:       separate filesystem from / (safe for --require-mount)")
            }
            Ok(false) => {
                crate::outln!("mount:       SAME filesystem as / (on a Pi this is the SD card!)")
            }
            Err(e) => crate::outln!("mount:       {e:#}"),
        }
    }
    crate::outln!(
        "systemd:     {}",
        if crate::systemd::notify_socket_present() {
            "notify socket present"
        } else {
            "not started by systemd"
        }
    );
    if let Ok(t) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
        if let Ok(m) = t.trim().parse::<f64>() {
            crate::outln!("cpu temp:    {:.1} C", m / 1000.0);
        }
    }
    if let Ok(mem) = std::fs::read_to_string("/proc/meminfo") {
        let get = |k: &str| {
            mem.lines()
                .find(|l| l.starts_with(k))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
                .map(|kb| human_bytes(kb * 1024))
        };
        if let (Some(t), Some(a)) = (get("MemTotal"), get("MemAvailable")) {
            crate::outln!("memory:      {a} available of {t}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn bench_runs() {
        assert!(super::bench_sha1(8) > 0.0);
    }
}
