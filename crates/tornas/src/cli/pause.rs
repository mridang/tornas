//! `pause` and `resume`: drive the global kill switch over the API.

use super::api::Api;
use crate::config::{PauseOpts, ResumeOpts};
use crate::units::human_age;

pub async fn pause(o: PauseOpts) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "duration": o.duration.map(|d| d.as_secs()),
        "indefinite": o.indefinite,
    });
    let p = Api::new(&o.server, o.token)?.pause(body).await?;
    match p.remaining_secs {
        Some(r) => crate::outln!("paused; resumes on its own in {}", human_age(r)),
        None => crate::outln!("paused until you run `tornas resume`"),
    }
    Ok(())
}

/// `resume`: lift the pause.
pub async fn resume(o: ResumeOpts) -> anyhow::Result<()> {
    let r = Api::new(&o.server, o.token)?.resume().await?;
    crate::outln!("resumed {} torrents", r.resumed);
    Ok(())
}
