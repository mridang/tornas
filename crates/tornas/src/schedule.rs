//! Speed limits by time of day. Windows are in local time; the first window that
//! matches wins, and outside every window the global limits apply. A window whose
//! end is earlier than its start runs past midnight, and `days` names the day it
//! starts on.

use std::num::NonZeroU32;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct BandwidthConfig {
    pub schedule: Vec<BandwidthWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BandwidthWindow {
    /// Days the window starts on (mon..sun). Empty means every day.
    #[serde(default)]
    pub days: Vec<String>,
    /// Start, "HH:MM" local time.
    pub from: String,
    /// End, "HH:MM" local time; earlier than `from` means the next day.
    pub to: String,
    /// Bytes per second like "2M", or "unlimited". Omitted keeps the global limit.
    #[serde(default)]
    pub download: Option<String>,
    #[serde(default)]
    pub upload: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// Keep the global limit.
    Inherit,
    Unlimited,
    Bps(NonZeroU32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Bit per weekday, Monday = bit 0. 0x7f means every day.
    days: u8,
    from: u16,
    to: u16,
    pub download: Limit,
    pub upload: Limit,
}

fn parse_hhmm(s: &str) -> anyhow::Result<u16> {
    let (h, m) = s.trim().split_once(':').context("expected HH:MM")?;
    let (h, m): (u16, u16) = (
        h.parse().context("bad hour")?,
        m.parse().context("bad minute")?,
    );
    if h > 23 || m > 59 || s.trim().len() != 5 {
        bail!("expected HH:MM between 00:00 and 23:59");
    }
    Ok(h * 60 + m)
}

fn parse_day(s: &str) -> anyhow::Result<u8> {
    let d = s.trim().to_ascii_lowercase();
    let idx = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
        .iter()
        .position(|p| {
            d.starts_with(p)
                && [
                    "monday",
                    "tuesday",
                    "wednesday",
                    "thursday",
                    "friday",
                    "saturday",
                    "sunday",
                ]
                .iter()
                .any(|full| full.starts_with(d.as_str()))
        })
        .with_context(|| format!("unknown day {s:?} (use mon, tue, ... sun)"))?;
    Ok(idx as u8)
}

fn parse_limit(s: Option<&str>) -> anyhow::Result<Limit> {
    let Some(s) = s.map(str::trim) else {
        return Ok(Limit::Inherit);
    };
    if s.eq_ignore_ascii_case("unlimited") {
        return Ok(Limit::Unlimited);
    }
    let bytes = crate::utils::parse_size(s)?;
    let bps = u32::try_from(bytes).context("more than 4 GiB/s")?;
    let bps = NonZeroU32::new(bps).context("use \"unlimited\" rather than 0")?;
    Ok(Limit::Bps(bps))
}

impl Window {
    pub fn parse(w: &BandwidthWindow) -> anyhow::Result<Self> {
        let from = parse_hhmm(&w.from).with_context(|| format!("from {:?}", w.from))?;
        let to = parse_hhmm(&w.to).with_context(|| format!("to {:?}", w.to))?;
        if from == to {
            bail!("from and to are both {}; a window cannot be empty", w.from);
        }
        let mut days = 0u8;
        for d in &w.days {
            days |= 1 << parse_day(d)?;
        }
        Ok(Self {
            days: if days == 0 { 0x7f } else { days },
            from,
            to,
            download: parse_limit(w.download.as_deref()).context("download")?,
            upload: parse_limit(w.upload.as_deref()).context("upload")?,
        })
    }

    /// `weekday`: Monday = 0. `minute`: minutes since local midnight.
    pub fn matches(&self, weekday: u8, minute: u16) -> bool {
        let on = |d: u8| self.days & (1 << (d % 7)) != 0;
        if self.from < self.to {
            on(weekday) && (self.from..self.to).contains(&minute)
        } else {
            (on(weekday) && minute >= self.from) || (on((weekday + 6) % 7) && minute < self.to)
        }
    }
}

pub fn compile(cfg: &BandwidthConfig) -> anyhow::Result<Vec<Window>> {
    cfg.schedule
        .iter()
        .enumerate()
        .map(|(i, w)| Window::parse(w).with_context(|| format!("bandwidth.schedule[{i}]")))
        .collect()
}

/// Index of the first matching window.
pub fn active(windows: &[Window], weekday: u8, minute: u16) -> Option<usize> {
    windows.iter().position(|w| w.matches(weekday, minute))
}

/// Limits to apply: the window's, or the global ones where it says to keep them.
pub fn resolve(limit: Limit, global: Option<NonZeroU32>) -> Option<NonZeroU32> {
    match limit {
        Limit::Inherit => global,
        Limit::Unlimited => None,
        Limit::Bps(b) => Some(b),
    }
}

/// Local weekday (Monday = 0) and minute of the day.
pub fn now_local() -> (u8, u16) {
    use chrono::{Datelike, Timelike};
    let now = chrono::Local::now();
    (
        now.weekday().num_days_from_monday() as u8,
        (now.hour() * 60 + now.minute()) as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(days: &[&str], from: &str, to: &str, dl: Option<&str>) -> Window {
        Window::parse(&BandwidthWindow {
            days: days.iter().map(|d| d.to_string()).collect(),
            from: from.into(),
            to: to.into(),
            download: dl.map(str::to_owned),
            upload: None,
        })
        .unwrap()
    }
    const MON: u8 = 0;
    const TUE: u8 = 1;
    const FRI: u8 = 4;
    const SAT: u8 = 5;
    const SUN: u8 = 6;
    fn hm(h: u16, m: u16) -> u16 {
        h * 60 + m
    }

    #[test]
    fn same_day_windows() {
        let e = w(&[], "18:00", "23:30", Some("2M"));
        assert!(e.matches(MON, hm(18, 0)));
        assert!(e.matches(SUN, hm(23, 29)));
        assert!(!e.matches(MON, hm(23, 30)), "end is exclusive");
        assert!(!e.matches(MON, hm(17, 59)));
        assert_eq!(
            e.download,
            Limit::Bps(NonZeroU32::new(2 * 1024 * 1024).unwrap())
        );
    }

    #[test]
    fn overnight_windows_belong_to_their_start_day() {
        let fri_night = w(&["fri"], "22:00", "06:00", Some("unlimited"));
        assert!(fri_night.matches(FRI, hm(23, 0)));
        assert!(
            fri_night.matches(SAT, hm(5, 59)),
            "Saturday early morning is still Friday's window"
        );
        assert!(!fri_night.matches(SAT, hm(6, 0)));
        assert!(
            !fri_night.matches(SAT, hm(23, 0)),
            "Saturday night is not Friday"
        );
        assert!(
            !fri_night.matches(FRI, hm(3, 0)),
            "Friday early morning belongs to Thursday"
        );
        let sun_night = w(&["sunday"], "23:00", "01:00", None);
        assert!(
            sun_night.matches(MON, hm(0, 30)),
            "wraps from Sunday into Monday"
        );
        assert_eq!(sun_night.download, Limit::Inherit);
    }

    #[test]
    fn first_match_wins() {
        let ws = vec![
            w(&["tue"], "19:00", "21:00", Some("1M")),
            w(&[], "18:00", "23:00", Some("4M")),
        ];
        assert_eq!(active(&ws, TUE, hm(20, 0)), Some(0));
        assert_eq!(active(&ws, MON, hm(20, 0)), Some(1));
        assert_eq!(active(&ws, MON, hm(12, 0)), None);
    }

    #[test]
    fn resolves_against_global() {
        let g = NonZeroU32::new(1000);
        assert_eq!(resolve(Limit::Inherit, g), g);
        assert_eq!(resolve(Limit::Unlimited, g), None);
        assert_eq!(
            resolve(Limit::Bps(NonZeroU32::new(5).unwrap()), g),
            NonZeroU32::new(5)
        );
    }

    #[test]
    fn rejects_bad_windows() {
        let bad = |days: &[&str], from: &str, to: &str, dl: Option<&str>| {
            Window::parse(&BandwidthWindow {
                days: days.iter().map(|d| d.to_string()).collect(),
                from: from.into(),
                to: to.into(),
                download: dl.map(str::to_owned),
                upload: None,
            })
            .is_err()
        };
        assert!(bad(&[], "18:00", "18:00", None), "empty window");
        assert!(bad(&[], "24:00", "02:00", None));
        assert!(bad(&[], "7:00", "09:00", None), "needs two-digit hours");
        assert!(bad(&["funday"], "07:00", "09:00", None));
        assert!(bad(&[], "07:00", "09:00", Some("0")));
        assert!(bad(&[], "07:00", "09:00", Some("fast")));
        assert!(bad(&[], "07:00", "09:00", Some("8G")), "over 4 GiB/s");
    }
}
