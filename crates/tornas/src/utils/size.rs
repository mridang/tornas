//! Byte, rate and age formatting, size parsing (binary suffixes, as disks report
//! usage), and wall-clock seconds.

use anyhow::{Context, bail};

/// Parse sizes like `800G`, `1.5T`, `120M`, `512K`, `1000000` (bytes).
/// Suffixes are binary (1G = 1024^3) because that is how disks report usage.
pub fn parse_size(s: &str) -> anyhow::Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty size");
    }
    let (num, suffix) = match s.find(|c: char| !(c.is_ascii_digit() || c == '.')) {
        Some(i) => s.split_at(i),
        None => (s, ""),
    };
    let value: f64 = num
        .parse()
        .with_context(|| format!("bad number in size {s:?}"))?;
    let mult: f64 = match suffix.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" | "KIB" => 1024.0,
        "M" | "MB" | "MIB" => 1024f64.powi(2),
        "G" | "GB" | "GIB" => 1024f64.powi(3),
        "T" | "TB" | "TIB" => 1024f64.powi(4),
        other => bail!("unknown size suffix {other:?} in {s:?}"),
    };
    Ok((value * mult) as u64)
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b}B")
    } else {
        format!("{v:.1}{}", UNITS[i])
    }
}

pub fn human_rate(bytes_per_sec: u64) -> String {
    format!("{}/s", human_bytes(bytes_per_sec))
}

/// Age in seconds rendered as `12s`, `5m`, `3h`, `2d`.
pub fn human_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("100").unwrap(), 100);
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("120M").unwrap(), 120 * 1024 * 1024);
        assert_eq!(parse_size("800G").unwrap(), 800 * 1024u64.pow(3));
        assert_eq!(parse_size("1.5T").unwrap(), (1.5 * 1024f64.powi(4)) as u64);
        assert_eq!(parse_size(" 2 gb ").unwrap(), 2 * 1024u64.pow(3));
        assert!(parse_size("10X").is_err());
        assert!(parse_size("").is_err());
    }

    #[test]
    fn formats() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1536), "1.5K");
        assert_eq!(human_age(59), "59s");
        assert_eq!(human_age(3600 * 5), "5h");
    }
}
