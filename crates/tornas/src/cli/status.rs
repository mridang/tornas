//! `status`: a one-shot view of a running server.

use super::api::{Api, HEADERS, Status, movie_rows, pause_line};
use crate::{
    config::ClientOpts,
    utils::{human_age, human_bytes, human_rate, now_secs},
};

pub async fn status(opts: ClientOpts) -> anyhow::Result<()> {
    let api = Api::new(&opts.server, None)?;
    if opts.json {
        // Print what the server actually said, not a re-serialisation of the
        // subset this command models.
        crate::outln!("{}", api.status_json().await?);
        return Ok(());
    }
    let s = api.status().await?;
    if let Some(line) = pause_line(&s.pause) {
        crate::outln!("{line}");
    }
    print_summary(&s);
    for w in &s.warnings {
        crate::outln!("warning: {w}");
    }
    crate::outln!();
    print_table(&s);
    crate::outln!();
    print_events(&s);
    Ok(())
}

fn print_summary(s: &Status) {
    crate::outln!(
        "tornas {} on {}  up {}",
        s.version,
        s.hostname,
        human_age(s.session.uptime_secs as i64)
    );
    crate::outln!(
        "budget: {} / {} used, disk free {} (min {}), next eviction: {}",
        human_bytes(s.budget.used),
        human_bytes(s.budget.limit),
        human_bytes(s.budget.disk_free),
        human_bytes(s.budget.min_free),
        s.budget
            .next_eviction
            .as_ref()
            .map_or("none", |c| c.title.as_str())
    );
    crate::outln!(
        "session: down {}  up {}  peers {}  torrents {}",
        human_rate(s.session.download_bps),
        human_rate(s.session.upload_bps),
        s.session.peers_live,
        s.session.torrents
    );
}

/// The library as a left-aligned table, each column as wide as its widest cell
/// and titles cut at 40 characters.
fn print_table(s: &Status) {
    const MAX_CELL: usize = 40;
    let rows = movie_rows(s);
    let mut widths: Vec<usize> = HEADERS.iter().map(|h| h.len()).collect();
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count().min(MAX_CELL));
        }
    }
    let line = |cells: &[String]| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let cell: String = c.chars().take(MAX_CELL).collect();
                format!("{cell:<width$}", width = widths[i])
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    crate::outln!("{}", line(&HEADERS.map(str::to_owned)));
    for r in &rows {
        crate::outln!("{}", line(r));
    }
}

fn print_events(s: &Status) {
    if s.events.is_empty() {
        return;
    }
    crate::outln!("recent events:");
    let now = now_secs();
    for e in s.events.iter().take(10) {
        crate::outln!("  {:>5} ago  {}", human_age(now - e.ts), e.message);
    }
}
