//! `status`: a one-shot view of a running server.

use super::api::{HEADERS, fetch, movie_rows, pause_line, s, u};
use crate::{
    config::ClientOpts,
    units::{human_age, human_bytes, human_rate, now_secs},
};

pub async fn status(opts: ClientOpts) -> anyhow::Result<()> {
    let v = fetch(&opts.server).await?;
    if opts.json {
        crate::outln!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let b = &v["budget"];
    if let Some(line) = pause_line(&v) {
        crate::outln!("{line}");
    }
    crate::outln!(
        "tornas {} on {}  up {}",
        s(&v, "version"),
        s(&v, "hostname"),
        human_age(u(&v, &["session", "uptime_secs"]) as i64)
    );
    crate::outln!(
        "budget: {} / {} used, disk free {} (min {}), next eviction: {}",
        human_bytes(u(b, &["used"])),
        human_bytes(u(b, &["limit"])),
        human_bytes(u(b, &["disk_free"])),
        human_bytes(u(b, &["min_free"])),
        b.get("next_eviction")
            .and_then(|n| n.get("title"))
            .and_then(|t| t.as_str())
            .unwrap_or("none")
    );
    crate::outln!(
        "session: down {}  up {}  peers {}  torrents {}",
        human_rate(u(&v, &["session", "download_bps"])),
        human_rate(u(&v, &["session", "upload_bps"])),
        u(&v, &["session", "peers_live"]),
        u(&v, &["session", "torrents"])
    );
    if let Some(ws) = v.get("warnings").and_then(|w| w.as_array())
        && !ws.is_empty()
    {
        for w in ws {
            crate::outln!("warning: {}", w.as_str().unwrap_or_default());
        }
    }
    crate::outln!();
    let rows = movie_rows(&v);
    let mut widths: Vec<usize> = HEADERS.iter().map(|h| h.len()).collect();
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count().min(40));
        }
    }
    let fmt = |cells: &[String]| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                format!(
                    "{:<w$}",
                    c.chars().take(40).collect::<String>(),
                    w = widths[i]
                )
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    crate::outln!(
        "{}",
        fmt(&HEADERS.iter().map(|h| h.to_string()).collect::<Vec<_>>())
    );
    for r in &rows {
        crate::outln!("{}", fmt(r));
    }
    crate::outln!();
    if let Some(events) = v.get("events").and_then(|e| e.as_array()) {
        crate::outln!("recent events:");
        for e in events.iter().take(10) {
            let ts = e.get("ts").and_then(|t| t.as_i64()).unwrap_or(0);
            crate::outln!(
                "  {:>5} ago  {:<7} {}",
                human_age(now_secs() - ts),
                s(e, "kind"),
                s(e, "message")
            );
        }
    }
    Ok(())
}
