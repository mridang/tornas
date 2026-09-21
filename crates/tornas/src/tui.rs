//! `status` (one-shot) and `top` (live) views of a running server, over its JSON API.

use std::{io, time::Duration};

use anyhow::Context;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Row, Table},
};
use serde_json::Value;

use crate::{
    config::{ClientOpts, LogsOpts, PauseOpts, ResumeOpts},
    units::{human_age, human_bytes, human_rate, now_secs},
};

async fn fetch(server: &str) -> anyhow::Result<Value> {
    let url = format!("{}/api/status", server.trim_end_matches('/'));
    let v = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .json()
        .await?;
    Ok(v)
}

fn u(v: &Value, path: &[&str]) -> u64 {
    let mut c = v;
    for p in path {
        c = match c.get(p) {
            Some(x) => x,
            None => return 0,
        };
    }
    c.as_u64().unwrap_or(0)
}
fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn movie_rows(v: &Value) -> Vec<Vec<String>> {
    let now = now_secs();
    v.get("movies")
        .and_then(|m| m.as_array())
        .map(|ms| {
            ms.iter()
                .map(|m| {
                    let total = u(m, &["total_bytes"]);
                    let prog = u(m, &["progress_bytes"]);
                    let pct = (prog * 100).checked_div(total).unwrap_or(0);
                    let last = m.get("last_used_at").and_then(|x| x.as_i64()).unwrap_or(0);
                    let title = match m.get("year").and_then(|y| y.as_i64()) {
                        Some(y) => format!("{} ({y})", s(m, "title")),
                        None => s(m, "title").to_owned(),
                    };
                    vec![
                        s(m, "imdb_id").to_owned(),
                        title,
                        human_bytes(total),
                        format!("{pct}%"),
                        s(m, "state").to_owned(),
                        human_rate(u(m, &["download_bps"])),
                        u(m, &["peers"]).to_string(),
                        human_age(now - last),
                        if m.get("protected")
                            .and_then(|p| p.as_bool())
                            .unwrap_or(false)
                        {
                            "yes".into()
                        } else {
                            "".into()
                        },
                    ]
                })
                .collect()
        })
        .unwrap_or_default()
}

const HEADERS: [&str; 9] = [
    "IMDb",
    "Title",
    "Size",
    "Done",
    "State",
    "Down",
    "Peers",
    "Last used",
    "Protected",
];

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

pub async fn top(opts: ClientOpts) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let res = top_loop(&mut terminal, &opts).await;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

async fn top_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    opts: &ClientOpts,
) -> anyhow::Result<()> {
    let mut last: Result<Value, String> = Err("loading...".into());
    let mut next_fetch = tokio::time::Instant::now();
    loop {
        if tokio::time::Instant::now() >= next_fetch {
            last = fetch(&opts.server).await.map_err(|e| format!("{e:#}"));
            next_fetch = tokio::time::Instant::now() + opts.interval;
        }
        terminal.draw(|f| draw(f, &last, &opts.server))?;
        if event::poll(Duration::from_millis(200))?
            && let Event::Key(k) = event::read()?
            && matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            return Ok(());
        }
    }
}

fn draw(f: &mut ratatui::Frame, data: &Result<Value, String>, server: &str) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(8),
        ])
        .split(area);

    let v = match data {
        Ok(v) => v,
        Err(e) => {
            f.render_widget(
                Paragraph::new(format!("{server}: {e}"))
                    .block(Block::default().borders(Borders::ALL).title("tornas")),
                area,
            );
            return;
        }
    };
    let b = &v["budget"];
    let used = u(b, &["used"]);
    let limit = u(b, &["limit"]).max(1);
    let ratio = (used as f64 / limit as f64).clamp(0.0, 1.0);
    let colour = if ratio > 0.9 {
        Color::Red
    } else if ratio > 0.75 {
        Color::Yellow
    } else {
        Color::Green
    };
    f.render_widget(
        Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(format!(
                    " budget  {} / {}   disk free {}   next eviction: {} ",
                    human_bytes(used),
                    human_bytes(limit),
                    human_bytes(u(b, &["disk_free"])),
                    b.get("next_eviction")
                        .and_then(|n| n.get("title"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("none")
                )))
            .gauge_style(Style::default().fg(colour))
            .ratio(ratio),
        chunks[0],
    );
    let pause_banner = pause_line(v);
    let sess = Line::from(vec![
        Span::styled(
            format!(" {} @ {} ", s(v, "version"), s(v, "hostname")),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " up {}   down {}   up {}   peers {}   torrents {}   listen {}",
            human_age(u(v, &["session", "uptime_secs"]) as i64),
            human_rate(u(v, &["session", "download_bps"])),
            human_rate(u(v, &["session", "upload_bps"])),
            u(v, &["session", "peers_live"]),
            u(v, &["session", "torrents"]),
            v["session"]["listen_addr"].as_str().unwrap_or("-")
        )),
    ]);
    let (sess, sess_title) = match &pause_banner {
        Some(b) => (
            Line::from(Span::styled(
                format!(" {b} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            " PAUSED ",
        ),
        None => (sess, " session "),
    };
    f.render_widget(
        Paragraph::new(sess).block(Block::default().borders(Borders::ALL).title(sess_title)),
        chunks[1],
    );

    let rows: Vec<Row> = movie_rows(v).into_iter().map(Row::new).collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Min(20),
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(5),
            Constraint::Length(9),
            Constraint::Length(9),
        ],
    )
    .header(Row::new(HEADERS.to_vec()).style(Style::default().add_modifier(Modifier::BOLD)))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" movies (LRU order = Last used) "),
    );
    f.render_widget(table, chunks[2]);

    let now = now_secs();
    let warn_lines: Vec<Line> = v["warnings"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|w| w.as_str())
                .map(|w| {
                    Line::from(Span::styled(
                        format!("! {w}"),
                        Style::default().fg(Color::Yellow),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let events: Vec<Line> = v["events"]
        .as_array()
        .map(|a| {
            a.iter()
                .take(6)
                .map(|e| {
                    let ts = e.get("ts").and_then(|t| t.as_i64()).unwrap_or(0);
                    Line::from(format!(
                        "{:>4} ago  {:<6} {}",
                        human_age(now - ts),
                        s(e, "kind"),
                        s(e, "message")
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let events = [warn_lines, events].concat();
    f.render_widget(
        Paragraph::new(events).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" events  (q to quit) "),
        ),
        chunks[3],
    );
}

/// `logs`: print recent lines from the server's in-memory ring, optionally following.
pub async fn logs(opts: LogsOpts) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let base = opts.server.trim_end_matches('/').to_owned();
    let mut since = 0u64;
    let mut first = true;
    loop {
        let mut req = client.get(format!("{base}/api/logs")).query(&[
            ("since", since.to_string()),
            ("limit", if first { opts.lines } else { 2000 }.to_string()),
        ]);
        if let Some(l) = &opts.level {
            req = req.query(&[("level", l)]);
        }
        if let Some(t) = &opts.token {
            req = req.bearer_auth(t);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("GET {base}/api/logs"))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!("server requires an API token (pass --token or TORNAS_API_TOKEN)");
        }
        let lines: Vec<serde_json::Value> = resp.error_for_status()?.json().await?;
        for l in &lines {
            let ts = l["ts"].as_i64().unwrap_or(0);
            let t = chrono_like(ts);
            crate::outln!(
                "{t} {:<5} {} {}",
                s(l, "level"),
                s(l, "target"),
                s(l, "message")
            );
            since = since.max(l["seq"].as_u64().unwrap_or(0));
        }
        first = false;
        if !opts.follow {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// UTC `HH:MM:SS` from unix seconds without pulling in chrono.
fn chrono_like(ts: i64) -> String {
    let s = ts.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// "PAUSED ..." summary, or None when running.
fn pause_line(v: &Value) -> Option<String> {
    let p = v.get("pause")?;
    if !p.get("paused").and_then(|x| x.as_bool()).unwrap_or(false) {
        return None;
    }
    Some(match p.get("remaining_secs").and_then(|x| x.as_i64()) {
        Some(r) => format!("PAUSED: everything is paused, resumes in {}", human_age(r)),
        None => "PAUSED: everything is paused until resumed".to_owned(),
    })
}

async fn send(
    method: reqwest::Method,
    server: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> anyhow::Result<reqwest::Response> {
    let url = format!("{}/api/pause", server.trim_end_matches('/'));
    let mut req = reqwest::Client::new()
        .request(method, &url)
        .timeout(Duration::from_secs(30));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("server requires an API token (pass --token or TORNAS_API_TOKEN)");
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("{status}: {text}");
    }
    Ok(resp)
}

/// `pause`: pause everything now.
pub async fn pause(o: PauseOpts) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "duration": o.duration.map(|d| d.as_secs()),
        "indefinite": o.indefinite,
    });
    let v: Value = send(
        reqwest::Method::PUT,
        &o.server,
        o.token.as_deref(),
        Some(body),
    )
    .await?
    .json()
    .await?;
    match v.get("remaining_secs").and_then(|x| x.as_i64()) {
        Some(r) => crate::outln!("paused; resumes on its own in {}", human_age(r)),
        None => crate::outln!("paused until you run `tornas resume`"),
    }
    Ok(())
}

/// `resume`: lift the pause.
pub async fn resume(o: ResumeOpts) -> anyhow::Result<()> {
    send(reqwest::Method::DELETE, &o.server, o.token.as_deref(), None).await?;
    crate::outln!("resumed");
    Ok(())
}
