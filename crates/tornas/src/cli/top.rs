//! `top`: the live terminal dashboard.

use std::{io, time::Duration};

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

use super::api::{HEADERS, fetch, movie_rows, pause_line, s, u};
use crate::{
    config::ClientOpts,
    units::{human_age, human_bytes, human_rate, now_secs},
};

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
