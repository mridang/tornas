//! The JSON API under `/api`, plus `/healthz` and `/metrics`. Reads are open;
//! writes need the API token when one is configured (see [`super::middleware`]).

use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;

use super::{ApiError, ApiResult, AppJson, AppState};
use crate::engine::AddMovieRequest;

/// Liveness: 503 only when the daemon is actually broken. Low disk is a warning,
/// not a failure (see `Engine::probe`).
pub(super) async fn healthz(State(e): State<AppState>) -> Response {
    match e.probe() {
        Ok(()) => (StatusCode::OK, "ok").into_response(),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("unhealthy: {err:#}"),
        )
            .into_response(),
    }
}

pub(super) async fn prometheus(State(e): State<AppState>) -> ApiResult<impl IntoResponse> {
    let body = crate::metrics::render(&e)?;
    Ok((
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    ))
}

// ---- global pause ----------------------------------------------------------

#[derive(Deserialize, Default)]
pub(super) struct PauseRequest {
    /// Seconds as a number, or a duration string such as "3h" or "90m".
    /// Omitted or null means the configured default.
    #[serde(default)]
    duration: Option<serde_json::Value>,
    #[serde(default)]
    indefinite: bool,
}

pub(super) async fn api_pause_get(State(e): State<AppState>) -> impl IntoResponse {
    Json(e.pause_view())
}

/// Pause everything. The body is optional: `PUT /api/pause` alone uses the default
/// duration. Repeating it while paused moves the end time.
pub(super) async fn api_pause_put(
    State(e): State<AppState>,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
    let req: PauseRequest = if body.iter().all(u8::is_ascii_whitespace) {
        PauseRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|err| {
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", err.to_string())
        })?
    };
    let duration = match req.duration {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(n)) => Some(std::time::Duration::from_secs(
            n.as_u64().ok_or_else(|| {
                ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid",
                    "duration must be a positive number of seconds",
                )
            })?,
        )),
        Some(serde_json::Value::String(s)) => {
            Some(humantime::parse_duration(&s).map_err(|err| {
                ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid",
                    format!("duration: {err}"),
                )
            })?)
        }
        Some(_) => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid",
                "duration must be seconds or a string like \"3h\"",
            ));
        }
    };
    Ok(Json(e.pause_all(duration, req.indefinite).await?))
}

/// Lift the pause. Idempotent: resuming when not paused is a no-op.
pub(super) async fn api_pause_delete(State(e): State<AppState>) -> ApiResult<impl IntoResponse> {
    let n = e.resume_all("manual").await?;
    let mut v = serde_json::to_value(e.pause_view()).unwrap_or_default();
    v["resumed"] = json!(n);
    Ok(Json(v))
}

pub(super) async fn api_root() -> impl IntoResponse {
    Json(json!({
        "name": "tornas",
        "version": env!("CARGO_PKG_VERSION"),
        "openapi": "/api/openapi.json",
        "resources": {
            "movies": "/api/movies",
            "pause": "/api/pause",
            "trackers": "/api/trackers",
            "config": "/api/config",
            "budget": "/api/budget",
            "session": "/api/session",
            "events": "/api/events",
            "status": "/api/status",
            "metrics": "/metrics"
        }
    }))
}

pub(super) async fn openapi() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        include_str!("openapi.json"),
    )
}

pub(super) async fn api_trackers(State(e): State<AppState>) -> impl IntoResponse {
    let st = e.trackers.state();
    Json(json!({
        "enabled": e.trackers.config.read().enabled,
        "active": e.trackers.current(),
        "updated_at": st.updated_at,
        "rejected": st.rejected,
        "deduplicated": st.deduplicated,
        "sources": st.sources,
    }))
}

/// Re-fetch all sources now and re-announce active downloads if the list changed.
pub(super) async fn api_trackers_refresh(
    State(e): State<AppState>,
) -> ApiResult<impl IntoResponse> {
    let changed = e.trackers.refresh().await?;
    let reannounced = if changed && e.trackers.config.read().reannounce_active {
        e.reannounce_active().await?
    } else {
        0
    };
    let st = e.trackers.state();
    Ok(Json(
        json!({ "changed": changed, "active": st.trackers.len(), "reannounced": reannounced }),
    ))
}

/// Effective configuration after file + env + flags, secrets omitted.
pub(super) async fn api_config(State(e): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "config_file": e.opts.config,
        "trackers": e.trackers.config.read().clone(),
        "network": e.file_config.network,
        "disk_budget": e.opts.disk_budget,
        "min_free": e.opts.min_free,
        "stream_grace_secs": e.opts.stream_grace.as_secs(),
        "keep_seeding": e.opts.keep_seeding,
        "http_listen": e.opts.http_listen,
        "public_url": e.opts.public_url,
        "dlna": !e.opts.disable_dlna,
        "dht": !e.opts.disable_dht,
        "tmdb": e.tmdb.is_some(),
        "bandwidth": e.file_config.bandwidth,
        "ratelimit_download": e.opts.ratelimit_download,
        "ratelimit_upload": e.opts.ratelimit_upload,
        "max_active_downloads": e.opts.max_active_downloads,
        "require_mount": e.opts.require_mount,
        "engine": {
            "utp": e.opts.utp,
            "bind_device": e.opts.bind_device,
            "announce_port": e.opts.announce_port,
            "dht_port": e.opts.dht_port,
            "dht_bootstrap": e.opts.dht_bootstrap,
            "local_discovery": !e.opts.disable_lsd,
            "peer_limit": e.tuning.peer_limit,
            "concurrent_checks": e.tuning.concurrent_checks,
            "memory_bytes": e.tuning.memory_bytes,
            "small_board": e.tuning.small_board,
            "blocklist": e.blocklist,
            "allowlist": e.allowlist,
        },
    }))
}

pub(super) async fn api_status(State(e): State<AppState>) -> ApiResult<impl IntoResponse> {
    Ok(Json(e.status()?))
}

pub(super) async fn api_budget(State(e): State<AppState>) -> ApiResult<impl IntoResponse> {
    Ok(Json(e.status()?.budget))
}

pub(super) async fn api_session(State(e): State<AppState>) -> ApiResult<impl IntoResponse> {
    Ok(Json(e.status()?.session))
}

#[derive(Deserialize)]
pub(super) struct EventsQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}
pub(super) fn default_limit() -> usize {
    50
}

pub(super) async fn api_events(
    State(e): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<EventsQuery>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(e.catalog.recent_events(q.limit.min(500))?))
}

#[derive(Deserialize)]
pub(super) struct ListQuery {
    state: Option<String>,
}

pub(super) async fn api_list(
    State(e): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListQuery>,
) -> ApiResult<impl IntoResponse> {
    let mut movies = e.list_movies()?;
    if let Some(st) = q.state {
        movies.retain(|m| m.state == st);
    }
    Ok(Json(movies))
}

pub(super) async fn api_add(
    State(e): State<AppState>,
    AppJson(req): AppJson<AddMovieRequest>,
) -> ApiResult<impl IntoResponse> {
    let v = e.add_movie(req).await?;
    let location = format!("/api/movies/{}", v.movie.imdb_id);
    Ok((StatusCode::CREATED, [(header::LOCATION, location)], Json(v)))
}

pub(super) async fn api_get(
    State(e): State<AppState>,
    Path(imdb_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    match e.get_movie(&imdb_id)? {
        Some(v) => Ok(Json(v)),
        None => Err(ApiError::not_found("no such movie")),
    }
}

/// Partial update. Writable fields:
/// - `last_used_at`: unix seconds, or "now"; moves the movie in the eviction queue.
/// - `download_limit` / `upload_limit`: bytes per second as a number or a size
///   like "2M"; null goes back to no per-movie limit.
/// - `peer_limit`: peers for this torrent; null goes back to the default.
///
/// Changing a limit reloads the torrent in the engine, keeping its downloaded pieces.
pub(super) async fn api_patch(
    State(e): State<AppState>,
    Path(imdb_id): Path<String>,
    AppJson(patch): AppJson<serde_json::Map<String, serde_json::Value>>,
) -> ApiResult<impl IntoResponse> {
    const FIELDS: [&str; 4] = [
        "last_used_at",
        "download_limit",
        "upload_limit",
        "peer_limit",
    ];
    let invalid = |msg: String| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", msg);
    if let Some(k) = patch.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(invalid(format!(
            "unknown field {k:?}: supported fields are {}",
            FIELDS.join(", ")
        )));
    }
    if patch.is_empty() {
        return Err(invalid(format!(
            "nothing to update: supported fields are {}",
            FIELDS.join(", ")
        )));
    }
    let rate = |key: &str| -> Result<Option<u32>, ApiError> {
        match &patch[key] {
            serde_json::Value::Null => Ok(None),
            serde_json::Value::Number(n) => n
                .as_u64()
                .filter(|v| *v > 0)
                .and_then(|v| u32::try_from(v).ok())
                .map(Some)
                .ok_or_else(|| invalid(format!("{key} must be between 1 and 4294967295 bytes/s"))),
            serde_json::Value::String(s) => crate::units::parse_size(s)
                .ok()
                .filter(|v| *v > 0)
                .and_then(|v| u32::try_from(v).ok())
                .map(Some)
                .ok_or_else(|| invalid(format!("{key}: {s:?} is not a size like \"2M\""))),
            _ => Err(invalid(format!("{key} must be a number, a size or null"))),
        }
    };
    let last_used = match patch.get("last_used_at") {
        None => None,
        Some(serde_json::Value::String(s)) if s == "now" => Some(None),
        Some(serde_json::Value::Number(n)) if n.as_i64().is_some() => Some(n.as_i64()),
        Some(_) => {
            return Err(invalid(
                "last_used_at must be unix seconds or \"now\"".to_owned(),
            ));
        }
    };
    let limits_touched = ["download_limit", "upload_limit", "peer_limit"]
        .iter()
        .any(|k| patch.contains_key(*k));
    let mut view = None;
    if limits_touched {
        let current = e
            .get_movie(&imdb_id)?
            .ok_or_else(|| ApiError::not_found("no such movie"))?;
        let row = current
            .torrent
            .ok_or_else(|| ApiError::not_found("movie has no torrent"))?;
        let download = if patch.contains_key("download_limit") {
            rate("download_limit")?
        } else {
            row.download_limit
        };
        let upload = if patch.contains_key("upload_limit") {
            rate("upload_limit")?
        } else {
            row.upload_limit
        };
        let peers = match patch.get("peer_limit") {
            None => row.peer_limit,
            Some(serde_json::Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .filter(|p| (1..=10_000).contains(p))
                    .ok_or_else(|| invalid("peer_limit must be 1..10000 or null".to_owned()))?
                    as u32,
            ),
        };
        view = Some(
            e.set_movie_limits(&imdb_id, download, upload, peers)
                .await?,
        );
    }
    if let Some(ts) = last_used {
        view = Some(e.set_last_used(&imdb_id, ts)?);
    }
    Ok(Json(view.expect("at least one field was set")))
}

pub(super) async fn api_delete(
    State(e): State<AppState>,
    Path(imdb_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    if e.remove_movie(&imdb_id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("no such movie"))
    }
}
