//! `logs`: print recent lines from the server's in-memory ring, optionally following.

use std::time::Duration;

use super::api::{Api, chrono_like};
use crate::config::LogsOpts;

pub async fn logs(opts: LogsOpts) -> anyhow::Result<()> {
    const FOLLOW_LIMIT: usize = 2000;
    let api = Api::new(&opts.server, opts.token.clone())?;
    let mut since = 0u64;
    let mut limit = opts.lines;
    loop {
        for l in api.logs(since, limit, opts.level.as_deref()).await? {
            crate::outln!(
                "{} {:<5} {} {}",
                chrono_like(l.ts),
                l.level,
                l.target,
                l.message
            );
            since = since.max(l.seq);
        }
        if !opts.follow {
            return Ok(());
        }
        limit = FOLLOW_LIMIT;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
