//! The little HTTP client the terminal commands share: fetch `/api/status`, poke
//! the pause endpoints, and pull values out of the untyped JSON they return.

use std::time::Duration;

use anyhow::Context;
use serde_json::Value;

use crate::units::{human_age, human_bytes, human_rate, now_secs};

pub(super) async fn fetch(server: &str) -> anyhow::Result<Value> {
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

pub(super) fn u(v: &Value, path: &[&str]) -> u64 {
    let mut c = v;
    for p in path {
        c = match c.get(p) {
            Some(x) => x,
            None => return 0,
        };
    }
    c.as_u64().unwrap_or(0)
}
pub(super) fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

pub(super) fn movie_rows(v: &Value) -> Vec<Vec<String>> {
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

pub(super) const HEADERS: [&str; 9] = [
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

pub(super) fn chrono_like(ts: i64) -> String {
    let s = ts.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// "PAUSED ..." summary, or None when running.
pub(super) fn pause_line(v: &Value) -> Option<String> {
    let p = v.get("pause")?;
    if !p.get("paused").and_then(|x| x.as_bool()).unwrap_or(false) {
        return None;
    }
    Some(match p.get("remaining_secs").and_then(|x| x.as_i64()) {
        Some(r) => format!("PAUSED: everything is paused, resumes in {}", human_age(r)),
        None => "PAUSED: everything is paused until resumed".to_owned(),
    })
}

pub(super) async fn send(
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
