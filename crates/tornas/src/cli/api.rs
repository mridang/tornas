//! The HTTP client the terminal commands share, and the shapes it reads.
//!
//! The types below mirror the server's JSON rather than reusing its structs on
//! purpose: the CLI talks to a server over the network, possibly a different
//! version of one, so it declares only what it needs and tolerates the rest being
//! absent.

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use crate::units::{human_age, human_bytes, human_rate, now_secs};

/// A server to talk to. The base URL is validated once, here, rather than being
/// trimmed and formatted at every call site.
pub(super) struct Api {
    base: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl Api {
    pub(super) fn new(server: &str, token: Option<String>) -> anyhow::Result<Self> {
        let base = server.trim_end_matches('/').to_owned();
        let parsed = url::Url::parse(&base).with_context(|| format!("server URL {server:?}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            anyhow::bail!("server URL {server:?} must be http or https");
        }
        Ok(Self {
            base,
            token,
            client: reqwest::Client::new(),
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let req = self.client.request(method, format!("{}{path}", self.base));
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    /// Everything the dashboard shows, in one call.
    pub(super) async fn status(&self) -> anyhow::Result<Status> {
        let resp = self
            .request(reqwest::Method::GET, "/api/status")
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .with_context(|| format!("GET {}/api/status", self.base))?;
        Ok(checked(resp).await?.json().await?)
    }

    /// The status response verbatim, for `status --json`: printing what the server
    /// said beats re-serialising the subset this client models.
    pub(super) async fn status_json(&self) -> anyhow::Result<String> {
        let resp = self
            .request(reqwest::Method::GET, "/api/status")
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .with_context(|| format!("GET {}/api/status", self.base))?;
        let text = checked(resp).await?.text().await?;
        let value: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        Ok(serde_json::to_string_pretty(&value)?)
    }

    /// Start or extend the global pause.
    pub(super) async fn pause(&self, body: serde_json::Value) -> anyhow::Result<Pause> {
        let resp = self
            .request(reqwest::Method::PUT, "/api/pause")
            .timeout(Duration::from_secs(30))
            .json(&body)
            .send()
            .await
            .context("PUT /api/pause")?;
        Ok(checked(resp).await?.json().await?)
    }

    /// Lift the pause; answers with how many torrents resumed.
    pub(super) async fn resume(&self) -> anyhow::Result<Resumed> {
        let resp = self
            .request(reqwest::Method::DELETE, "/api/pause")
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("DELETE /api/pause")?;
        Ok(checked(resp).await?.json().await?)
    }
}

/// Turn a non-success status into a message worth reading.
async fn checked(resp: reqwest::Response) -> anyhow::Result<reqwest::Response> {
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

// ---- what the server sends -------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Status {
    pub version: String,
    pub hostname: String,
    pub warnings: Vec<String>,
    pub pause: Pause,
    pub budget: Budget,
    pub session: Session,
    pub movies: Vec<Movie>,
    pub events: Vec<Event>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Pause {
    pub paused: bool,
    pub remaining_secs: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Budget {
    pub limit: u64,
    pub used: u64,
    pub disk_free: u64,
    pub min_free: u64,
    pub next_eviction: Option<Candidate>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Candidate {
    pub title: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Session {
    pub listen_addr: Option<String>,
    pub download_bps: u64,
    pub upload_bps: u64,
    pub peers_live: u64,
    pub uptime_secs: u64,
    pub torrents: u64,
    pub queued: usize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Movie {
    pub imdb_id: String,
    pub title: String,
    pub year: Option<i64>,
    pub state: String,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub download_bps: u64,
    pub peers: u64,
    pub protected: bool,
    pub last_used_at: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Event {
    pub ts: i64,
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Resumed {
    pub resumed: u64,
}

// ---- shared formatting -----------------------------------------------------

impl Movie {
    pub(super) fn percent(&self) -> u64 {
        (self.progress_bytes * 100)
            .checked_div(self.total_bytes)
            .unwrap_or(0)
    }

    pub(super) fn display_title(&self) -> String {
        match self.year {
            Some(y) => format!("{} ({y})", self.title),
            None => self.title.clone(),
        }
    }

    /// One table row, in the order of [`HEADERS`].
    pub(super) fn row(&self, now: i64) -> Vec<String> {
        vec![
            self.imdb_id.clone(),
            self.display_title(),
            human_bytes(self.total_bytes),
            format!("{}%", self.percent()),
            self.state.clone(),
            human_rate(self.download_bps),
            self.peers.to_string(),
            human_age(now - self.last_used_at),
            if self.protected { "yes" } else { "" }.to_owned(),
        ]
    }
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

pub(super) fn movie_rows(status: &Status) -> Vec<Vec<String>> {
    let now = now_secs();
    status.movies.iter().map(|m| m.row(now)).collect()
}

/// The "PAUSED ..." banner, or nothing when the server is running.
pub(super) fn pause_line(p: &Pause) -> Option<String> {
    if !p.paused {
        return None;
    }
    Some(match p.remaining_secs {
        Some(r) => format!("PAUSED: everything is paused, resumes in {}", human_age(r)),
        None => "PAUSED: everything is paused until resumed".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_url_is_checked_once() {
        assert!(Api::new("http://127.0.0.1:3030", None).is_ok());
        assert!(Api::new("http://127.0.0.1:3030/", None).is_ok());
        assert!(Api::new("box.lan:3030", None).is_err(), "no scheme");
        assert!(Api::new("ftp://box.lan", None).is_err(), "wrong scheme");
    }

    #[test]
    fn missing_fields_do_not_break_an_older_server() {
        let s: Status = serde_json::from_str(r#"{"hostname":"box","session":{"torrents":2}}"#)
            .expect("absent fields fall back to defaults");
        assert_eq!(s.hostname, "box");
        assert_eq!(s.session.torrents, 2);
        assert_eq!(s.session.queued, 0);
        assert!(s.movies.is_empty());
    }

    #[test]
    fn rows_follow_the_headers() {
        let m = Movie {
            imdb_id: "tt0111161".into(),
            title: "The Shawshank Redemption".into(),
            year: Some(1994),
            state: "done".into(),
            progress_bytes: 50,
            total_bytes: 200,
            protected: true,
            ..Default::default()
        };
        let row = m.row(now_secs());
        assert_eq!(row.len(), HEADERS.len());
        assert_eq!(row[1], "The Shawshank Redemption (1994)");
        assert_eq!(row[3], "25%");
        assert_eq!(row[8], "yes");
    }

    #[test]
    fn a_movie_with_no_size_is_not_a_division_by_zero() {
        assert_eq!(Movie::default().percent(), 0);
    }
}
