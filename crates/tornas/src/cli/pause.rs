//! `pause` and `resume`: drive the global kill switch over the API.

use serde_json::Value;

use super::api::send;
use crate::config::{PauseOpts, ResumeOpts};
use crate::units::human_age;

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
