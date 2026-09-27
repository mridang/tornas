//! Minimal TMDB client: IMDb id -> movie details.
//!
//! Auth is either a v4 read access token (Bearer header) or a v3 API key (query
//! parameter); TMDB accepts both against the same endpoints.

use anyhow::{Context, bail};
use serde::{Deserialize, de::DeserializeOwned};

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

/// What this application needs from TMDB, already in its own shape.
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
    /// The details response verbatim. The catalog stores it, so a later version can
    /// read fields this struct does not model without re-fetching.
    pub raw: String,
}

// ---- the wire format -------------------------------------------------------
//
// Only the fields this client uses; serde ignores everything else TMDB sends.

#[derive(Debug, Deserialize)]
struct FindResponse {
    #[serde(default)]
    movie_results: Vec<FindResult>,
}

#[derive(Debug, Deserialize)]
struct FindResult {
    id: i64,
}

#[derive(Debug, Default, Deserialize)]
struct MovieDetails {
    title: Option<String>,
    original_title: Option<String>,
    /// `YYYY-MM-DD`, or an empty string for something unreleased.
    release_date: Option<String>,
    overview: Option<String>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    runtime: Option<i64>,
    #[serde(default)]
    genres: Vec<Genre>,
    vote_average: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Genre {
    name: String,
}

impl MovieDetails {
    fn into_movie(self, tmdb_id: i64, raw: String) -> TmdbMovie {
        let image = |path: Option<String>, size: &str| {
            path.filter(|p| !p.is_empty())
                .map(|p| format!("{IMAGE_BASE}/{size}{p}"))
        };
        TmdbMovie {
            tmdb_id,
            title: self
                .title
                .or(self.original_title)
                .filter(|t| !t.is_empty())
                // A record with no title at all is not worth failing an add over.
                .unwrap_or_else(|| format!("TMDB {tmdb_id}")),
            year: self
                .release_date
                .as_deref()
                .and_then(|d| d.get(..4)?.parse().ok()),
            overview: self.overview.filter(|o| !o.is_empty()),
            poster_url: image(self.poster_path, "w500"),
            backdrop_url: image(self.backdrop_path, "w1280"),
            // TMDB sends 0 for "unknown", which is not a runtime.
            runtime_min: self.runtime.filter(|r| *r > 0),
            genres: self.genres.into_iter().map(|g| g.name).collect(),
            rating: self.vote_average.filter(|r| *r > 0.0),
            raw,
        }
    }
}

impl Tmdb {
    /// `None` when no credentials were configured, which is a supported way to run:
    /// movies are then catalogued by IMDb id alone.
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

    /// The response body, with transport and status failures already turned into
    /// errors that say which call failed.
    async fn get_body(&self, path: &str, query: &[(&str, &str)]) -> anyhow::Result<String> {
        let url = format!("{}{path}", self.base);
        let req = self.client.get(&url).query(query);
        let req = match &self.auth {
            Auth::Bearer(t) => req.bearer_auth(t),
            Auth::ApiKey(k) => req.query(&[("api_key", k.as_str())]),
        };
        let resp = req
            .send()
            .await
            .with_context(|| format!("TMDB request {url}"))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .with_context(|| format!("reading TMDB {url}"))?;
        if !status.is_success() {
            bail!("TMDB {url} returned {status}: {body}");
        }
        Ok(body)
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<T> {
        let body = self.get_body(path, query).await?;
        serde_json::from_str(&body).with_context(|| format!("TMDB {path}: unexpected JSON"))
    }

    /// Look a movie up by IMDb id: one call to resolve the id, one for the details.
    pub async fn find_by_imdb(&self, imdb_id: &str) -> anyhow::Result<TmdbMovie> {
        let found: FindResponse = self
            .get(
                &format!("/find/{imdb_id}"),
                &[("external_source", "imdb_id")],
            )
            .await?;
        let Some(first) = found.movie_results.first() else {
            bail!("TMDB has no movie for {imdb_id}");
        };
        let tmdb_id = first.id;
        // Keep the body rather than re-serialising a parsed value: the catalog
        // stores exactly what TMDB sent.
        let raw = self.get_body(&format!("/movie/{tmdb_id}"), &[]).await?;
        let details: MovieDetails = serde_json::from_str(&raw)
            .with_context(|| format!("TMDB movie {tmdb_id}: unexpected JSON"))?;
        Ok(details.into_movie(tmdb_id, raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: serde_json::Value) -> TmdbMovie {
        let raw = json.to_string();
        serde_json::from_str::<MovieDetails>(&raw)
            .unwrap()
            .into_movie(278, raw)
    }

    #[test]
    fn parses_details() {
        let m = parse(serde_json::json!({
            "title": "The Shawshank Redemption", "release_date": "1994-09-23",
            "overview": "Two imprisoned men...", "poster_path": "/p.jpg", "backdrop_path": "/b.jpg",
            "runtime": 142, "genres": [{"id": 18, "name": "Drama"}, {"id": 80, "name": "Crime"}],
            "vote_average": 8.7, "belongs_to_collection": null, "budget": 25000000
        }));
        assert_eq!(m.title, "The Shawshank Redemption");
        assert_eq!(m.year, Some(1994));
        assert_eq!(m.genres, vec!["Drama", "Crime"]);
        assert_eq!(
            m.poster_url.as_deref(),
            Some("https://image.tmdb.org/t/p/w500/p.jpg")
        );
        assert_eq!(m.runtime_min, Some(142));
        assert_eq!(m.rating, Some(8.7));
        assert!(
            m.raw.contains("budget"),
            "fields this client does not model are kept verbatim"
        );
    }

    #[test]
    fn copes_with_a_sparse_record() {
        // Something unreleased: most fields absent, empty or zero.
        let m = parse(serde_json::json!({
            "original_title": "Untitled Project", "release_date": "", "overview": "",
            "poster_path": null, "runtime": 0, "vote_average": 0.0
        }));
        assert_eq!(
            m.title, "Untitled Project",
            "falls back to the original title"
        );
        assert_eq!(m.year, None);
        assert_eq!(m.overview, None);
        assert_eq!(m.poster_url, None);
        assert_eq!(m.runtime_min, None, "0 minutes means unknown, not zero");
        assert_eq!(m.rating, None);
        assert!(m.genres.is_empty());
    }

    #[test]
    fn a_record_with_no_title_still_parses() {
        let m = parse(serde_json::json!({}));
        assert_eq!(m.title, "TMDB 278");
        assert_eq!(m.year, None);
    }
}
