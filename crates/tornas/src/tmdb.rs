//! Minimal TMDB client: IMDb id -> movie details. Auth is a v4 read access token
//! (Bearer header) or a v3 API key (query parameter).

use anyhow::{Context, bail};
use serde::Deserialize;

const IMAGE_BASE: &str = "https://image.tmdb.org/t/p";

#[derive(Clone)]
pub struct Tmdb {
    client: reqwest::Client,
    base: String,
    auth: Auth,
}

#[derive(Clone)]
enum Auth {
    Bearer(String),
    ApiKey(String),
}

#[derive(Debug, Clone)]
pub struct TmdbMovie {
    pub tmdb_id: i64,
    pub title: String,
    pub year: Option<i32>,
    pub overview: Option<String>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub runtime_min: Option<i64>,
    pub genres: Vec<String>,
    pub rating: Option<f64>,
    pub raw: serde_json::Value,
}

#[derive(Deserialize)]
struct FindResponse {
    #[serde(default)]
    movie_results: Vec<FindMovie>,
}
#[derive(Deserialize)]
struct FindMovie {
    id: i64,
}

impl Tmdb {
    pub fn new(base_url: &str, token: Option<String>, api_key: Option<String>) -> Option<Self> {
        let auth = match (
            token.filter(|s| !s.is_empty()),
            api_key.filter(|s| !s.is_empty()),
        ) {
            (Some(t), _) => Auth::Bearer(t),
            (None, Some(k)) => Auth::ApiKey(k),
            (None, None) => return None,
        };
        Some(Self {
            client: reqwest::Client::builder()
                .user_agent(concat!("tornas/", env!("CARGO_PKG_VERSION")))
                .build()
                .ok()?,
            base: base_url.trim_end_matches('/').to_owned(),
            auth,
        })
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> anyhow::Result<serde_json::Value> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.client.get(&url).query(query);
        req = match &self.auth {
            Auth::Bearer(t) => req.bearer_auth(t),
            Auth::ApiKey(k) => req.query(&[("api_key", k.as_str())]),
        };
        let resp = req
            .send()
            .await
            .with_context(|| format!("TMDB request {url}"))?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!("TMDB {url} returned {status}: {body}");
        }
        serde_json::from_str(&body).with_context(|| format!("TMDB {url}: bad JSON"))
    }

    pub async fn find_by_imdb(&self, imdb_id: &str) -> anyhow::Result<TmdbMovie> {
        let found: FindResponse = serde_json::from_value(
            self.get(
                &format!("/find/{imdb_id}"),
                &[("external_source", "imdb_id")],
            )
            .await?,
        )?;
        let Some(first) = found.movie_results.first() else {
            bail!("TMDB has no movie for {imdb_id}");
        };
        let raw = self.get(&format!("/movie/{}", first.id), &[]).await?;
        Ok(Self::parse_movie(first.id, raw))
    }

    fn parse_movie(tmdb_id: i64, raw: serde_json::Value) -> TmdbMovie {
        let s = |k: &str| raw.get(k).and_then(|v| v.as_str()).map(str::to_owned);
        let img = |k: &str, size: &str| s(k).map(|p| format!("{IMAGE_BASE}/{size}{p}"));
        TmdbMovie {
            tmdb_id,
            title: s("title")
                .or_else(|| s("original_title"))
                .unwrap_or_else(|| format!("TMDB {tmdb_id}")),
            year: s("release_date").and_then(|d| d.get(..4)?.parse().ok()),
            overview: s("overview").filter(|o| !o.is_empty()),
            poster_url: img("poster_path", "w500"),
            backdrop_url: img("backdrop_path", "w1280"),
            runtime_min: raw
                .get("runtime")
                .and_then(|v| v.as_i64())
                .filter(|r| *r > 0),
            genres: raw
                .get("genres")
                .and_then(|g| g.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|g| g.get("name")?.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            rating: raw
                .get("vote_average")
                .and_then(|v| v.as_f64())
                .filter(|r| *r > 0.0),
            raw,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_details() {
        let raw = serde_json::json!({
            "title": "The Shawshank Redemption", "release_date": "1994-09-23",
            "overview": "Two imprisoned men...", "poster_path": "/p.jpg", "backdrop_path": "/b.jpg",
            "runtime": 142, "genres": [{"id": 18, "name": "Drama"}, {"id": 80, "name": "Crime"}],
            "vote_average": 8.7
        });
        let m = Tmdb::parse_movie(278, raw);
        assert_eq!(m.year, Some(1994));
        assert_eq!(m.genres, vec!["Drama", "Crime"]);
        assert_eq!(
            m.poster_url.as_deref(),
            Some("https://image.tmdb.org/t/p/w500/p.jpg")
        );
        assert_eq!(m.runtime_min, Some(142));
    }
}
