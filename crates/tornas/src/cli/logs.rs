//! `logs`: print recent lines from the server's in-memory ring, optionally following.

use std::time::Duration;

use anyhow::Context;

use super::api::{chrono_like, s};
use crate::config::LogsOpts;

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
