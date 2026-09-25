//! The Stremio addon endpoints: manifest, catalogue, metadata and stream links.
//!
//! This is the hand-rolled subset that predates the `stremio` module; it is due to
//! be replaced by that module's protocol implementation.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, header},
    response::IntoResponse,
};
use serde_json::json;

use super::{ApiResult, AppState};
use crate::engine::{Engine, MovieView};

// ---- Stremio ---------------------------------------------------------------

pub(super) fn strip_json(s: &str) -> &str {
    s.strip_suffix(".json").unwrap_or(s)
}

pub(super) fn public_base(e: &Engine, headers: &HeaderMap) -> String {
    if let Some(p) = &e.opts.public_url {
        return p.trim_end_matches('/').to_owned();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("127.0.0.1:3030");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or(if e.opts.tls_cert.is_some() {
            "https"
        } else {
            "http"
        });
    format!("{scheme}://{host}")
}

pub(super) async fn manifest(State(e): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "id": "org.mridang.tornas",
        "version": env!("CARGO_PKG_VERSION"),
        "name": e.opts.dlna_name.clone().unwrap_or_else(|| "Tornas".into()),
        "description": "Movies downloaded to this home media center",
        "logo": "https://raw.githubusercontent.com/Stremio/stremio-art/main/originals/Stremio-logo-white.png",
        "resources": ["catalog", "meta", "stream"],
        "types": ["movie"],
        "idPrefixes": ["tt"],
        "catalogs": [{ "type": "movie", "id": "local", "name": "Local Library" }],
        "behaviorHints": { "configurable": false, "configurationRequired": false }
    }))
}

pub(super) fn meta_preview(v: &MovieView) -> serde_json::Value {
    let m = &v.movie;
    json!({
        "id": m.imdb_id,
        "type": "movie",
        "name": m.title,
        "poster": m.poster_url,
        "background": m.backdrop_url,
        "description": m.overview,
        "releaseInfo": m.year.map(|y| y.to_string()),
        "imdbRating": m.rating.map(|r| format!("{r:.1}")),
        "runtime": m.runtime_min.map(|r| format!("{r} min")),
        "genres": m.genres,
        "posterShape": "poster"
    })
}

pub(super) async fn catalog(
    State(e): State<AppState>,
    Path(p): Path<Vec<String>>,
) -> ApiResult<impl IntoResponse> {
    let id = p.first().map(|s| strip_json(s)).unwrap_or("");
    if id != "local" {
        return Ok(Json(json!({ "metas": [] })));
    }
    let metas: Vec<_> = e.list_movies()?.iter().map(meta_preview).collect();
    Ok(Json(json!({ "metas": metas })))
}

pub(super) async fn meta(
    State(e): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = strip_json(&id);
    match e.get_movie(id)? {
        Some(v) => Ok(Json(json!({ "meta": meta_preview(&v) }))),
        None => Ok(Json(json!({ "meta": null }))),
    }
}

pub(super) async fn stream_list(
    State(e): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<impl IntoResponse> {
    let id = strip_json(&id);
    let Some(v) = e.get_movie(id)? else {
        return Ok(Json(json!({ "streams": [] })));
    };
    let Some(t) = &v.torrent else {
        return Ok(Json(json!({ "streams": [] })));
    };
    let base = public_base(&e, &headers);
    let filename: String =
        url::form_urlencoded::byte_serialize(t.video_file_name.as_bytes()).collect();
    let progress = (v.progress_bytes * 100)
        .checked_div(v.total_bytes)
        .unwrap_or(0);
    Ok(Json(json!({
        "streams": [{
            "name": "Tornas",
            "title": format!("{}\n{} · {}%", t.video_file_name, crate::units::human_bytes(t.size_bytes), progress),
            "url": format!("{base}/video/{}/{}", v.movie.imdb_id, filename.replace('+', "%20")),
            "behaviorHints": { "notWebReady": !t.video_file_name.to_ascii_lowercase().ends_with(".mp4") }
        }]
    })))
}
