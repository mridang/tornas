//! HTTP surface: JSON API, Stremio addon protocol, video streaming with Range
//! support, and the mounted UPnP router. Permissive CORS everywhere so
//! web.stremio.com (an HTTPS page) can call the addon.

use std::{io::SeekFrom, sync::Arc};

use anyhow::Context;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use tower_http::cors::{Any, CorsLayer};
use tracing::debug;

use crate::engine::{AddMovieRequest, Engine, MovieView};

pub type AppState = Arc<Engine>;

/// JSON error envelope: `{"error": {"kind": "...", "message": "..."}}` with a status
/// derived from the engine's typed fault kind.
pub struct ApiError {
    status: StatusCode,
    kind: &'static str,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
        }
    }
    pub fn not_found(what: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", what)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "kind": self.kind, "message": self.message } });
        (self.status, Json(body)).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        use crate::engine::{FaultKind, fault_kind};
        let message = format!("{e:#}");
        let (status, kind) = match fault_kind(&e) {
            Some(FaultKind::NotFound) => (StatusCode::NOT_FOUND, "not_found"),
            Some(FaultKind::Conflict) => (StatusCode::CONFLICT, "conflict"),
            Some(FaultKind::Invalid) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid"),
            Some(FaultKind::NoSpace) => (StatusCode::INSUFFICIENT_STORAGE, "no_space"),
            Some(FaultKind::Upstream) => (StatusCode::BAD_GATEWAY, "upstream"),
            None => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        Self {
            status,
            kind,
            message,
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// `Json` extractor whose rejections use the same error envelope as everything else.
pub struct AppJson<T>(pub T);

impl<S, T> axum::extract::FromRequest<S> for AppJson<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        use axum::extract::rejection::JsonRejection;
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(AppJson(v)),
            Err(JsonRejection::MissingJsonContentType(_)) => Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "expected Content-Type: application/json",
            )),
            Err(JsonRejection::JsonDataError(e)) => Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid",
                e.body_text(),
            )),
            Err(JsonRejection::JsonSyntaxError(e)) => Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "bad_request",
                e.body_text(),
            )),
            Err(e) => Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "bad_request",
                e.body_text(),
            )),
        }
    }
}

pub fn router(engine: AppState, upnp: Option<Router>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    let r = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        // JSON API
        .route("/api", get(api_root))
        .route("/api/openapi.json", get(openapi))
        .route("/api/status", get(api_status))
        .route("/api/budget", get(api_budget))
        .route("/api/session", get(api_session))
        .route("/api/events", get(api_events))
        .route(
            "/api/trackers",
            get(api_trackers).post(api_trackers_refresh),
        )
        .route("/api/config", get(api_config))
        .route("/api/logs", get(api_logs))
        .route("/api/movies", get(api_list).post(api_add))
        .route(
            "/api/movies/{imdb_id}",
            get(api_get).patch(api_patch).delete(api_delete),
        )
        // Stremio addon protocol
        .route("/manifest.json", get(manifest))
        .route("/catalog/movie/{id}", get(catalog))
        .route("/catalog/movie/{id}/{extra}", get(catalog))
        .route("/meta/movie/{id}", get(meta))
        .route("/stream/movie/{id}", get(stream_list))
        // Video bytes, shared by Stremio and DLNA
        .route("/video/{imdb_id}/{filename}", get(video))
        .route("/video/{imdb_id}", get(video));
    let mut r = r
        .layer(axum::middleware::from_fn_with_state(
            engine.clone(),
            require_token,
        ))
        .with_state(engine);
    if let Some(u) = upnp {
        r = r.nest("/upnp", u);
    }
    // Outermost last: the private-network middleware wraps CORS so it can decorate
    // the preflight response that CORS produces.
    r.layer(cors)
        .layer(axum::middleware::from_fn(allow_private_network))
}

/// When an API token is configured, mutating calls under /api and the log
/// endpoint need `Authorization: Bearer <token>` (or `X-Api-Token`). Everything
/// Stremio and DLNA players use stays open.
async fn require_token(
    State(e): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(token) = e.opts.api_token.as_deref().filter(|t| !t.is_empty()) else {
        return next.run(req).await;
    };
    let path = req.uri().path();
    let protected = path.starts_with("/api")
        && (req.method() != http::Method::GET
            && req.method() != http::Method::HEAD
            && req.method() != http::Method::OPTIONS
            || path == "/api/logs");
    if !protected {
        return next.run(req).await;
    }
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| {
            req.headers()
                .get("x-api-token")
                .and_then(|v| v.to_str().ok())
        });
    if presented == Some(token) {
        next.run(req).await
    } else {
        crate::metrics::unauthorized();
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or wrong API token",
        )
        .into_response()
    }
}

/// Chrome's Private Network Access preflight (older Chrome/Edge) asks a public site
/// like web.stremio.com for permission to talk to a LAN address. Newer Chrome
/// (Local Network Access) shows the user a permission prompt instead; this header
/// is harmless there.
async fn allow_private_network(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let asked = req
        .headers()
        .get("access-control-request-private-network")
        .is_some();
    let mut resp = next.run(req).await;
    if asked {
        resp.headers_mut().insert(
            "access-control-allow-private-network",
            HeaderValue::from_static("true"),
        );
    }
    resp
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Human landing page: install links for the Stremio app and web.stremio.com,
/// plus every movie in the library with play links.
async fn index(State(e): State<AppState>, headers: HeaderMap) -> ApiResult<impl IntoResponse> {
    let base = public_base(&e, &headers);
    let manifest = format!("{base}/manifest.json");
    let stremio_install =
        manifest
            .replacen("http://", "stremio://", 1)
            .replacen("https://", "stremio://", 1);
    let web_install = format!(
        "https://web.stremio.com/#/addons?addon={}",
        url::form_urlencoded::byte_serialize(manifest.as_bytes()).collect::<String>()
    );
    let status = e.status()?;
    let name = e.opts.dlna_name.clone().unwrap_or_else(|| "Tornas".into());

    let mut rows = String::new();
    for m in &status.movies {
        let id = &m.movie.imdb_id;
        let title = esc(&m.movie.title);
        let year = m.movie.year.map(|y| y.to_string()).unwrap_or_default();
        let poster = m
            .movie
            .poster_url
            .as_deref()
            .map(|p| format!(r#"<img src="{}" alt="">"#, esc(p)))
            .unwrap_or_default();
        let pct = (m.progress_bytes * 100)
            .checked_div(m.total_bytes)
            .unwrap_or(0);
        let video = m
            .torrent
            .as_ref()
            .map(|t| {
                let f: String =
                    url::form_urlencoded::byte_serialize(t.video_file_name.as_bytes()).collect();
                format!("{base}/video/{id}/{}", f.replace('+', "%20"))
            })
            .unwrap_or_default();
        rows.push_str(&format!(
            r#"<li>{poster}<div><h3>{title} <small>{year}</small></h3>
<p class="meta">{id} · {size} · {pct}% · {state}{prot}</p>
<p class="links">
<a class="btn" href="stremio:///detail/movie/{id}/{id}">Play in Stremio app</a>
<a class="btn" href="https://web.stremio.com/#/detail/movie/{id}/{id}" target="_blank" rel="noopener">Play on web.stremio.com</a>
<a href="{video}">direct video</a>
</p></div></li>
"#,
            size = crate::units::human_bytes(m.total_bytes),
            state = esc(&m.state),
            prot = if m.protected { " · protected" } else { "" },
        ));
    }
    let b = &status.budget;
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{name}</title>
<style>
body{{font:15px/1.5 system-ui,sans-serif;margin:0;background:#0f1116;color:#e6e6e6}}
main{{max-width:960px;margin:0 auto;padding:24px 16px}}
h1{{margin:0 0 4px}} h3{{margin:0 0 4px;font-size:18px}} small{{color:#9aa;font-weight:normal}}
.card{{background:#1a1d26;border-radius:10px;padding:16px;margin:16px 0}}
.btn{{display:inline-block;background:#7b5cff;color:#fff;padding:6px 12px;border-radius:6px;text-decoration:none;margin:2px 6px 2px 0}}
.btn.alt{{background:#2c3040}}
code{{background:#262a36;padding:2px 6px;border-radius:4px;word-break:break-all}}
ul{{list-style:none;padding:0;margin:0}} li{{display:flex;gap:16px;padding:12px 0;border-top:1px solid #2a2e3a}}
li img{{width:80px;height:120px;object-fit:cover;border-radius:6px;flex:none}}
.meta{{color:#9aa;margin:0 0 6px}} .links a{{margin-right:10px}} p{{margin:4px 0}}
</style></head><body><main>
<h1>{name}</h1>
<p class="meta">{used} of {limit} used · {free} free on disk · {n} movies</p>
<div class="card"><h3>Add this addon to Stremio</h3>
<p><a class="btn" href="{stremio_install}">Install in Stremio app</a>
<a class="btn alt" href="{web_install}" target="_blank" rel="noopener">Install on web.stremio.com</a></p>
<p>Manifest URL: <code>{manifest}</code></p></div>
<div class="card"><h3>Library</h3><ul>{rows}</ul></div>
<p class="meta"><a href="{base}/api/status">JSON status</a> · <code>tornas status</code> on the host</p>
</main></body></html>"#,
        name = esc(&name),
        used = crate::units::human_bytes(b.used),
        limit = crate::units::human_bytes(b.limit),
        free = crate::units::human_bytes(b.disk_free),
        n = status.movies.len(),
        manifest = esc(&manifest),
        stremio_install = esc(&stremio_install),
        web_install = esc(&web_install),
    );
    Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html))
}

async fn metrics(State(e): State<AppState>) -> ApiResult<impl IntoResponse> {
    let body = crate::metrics::render(&e)?;
    Ok((
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    ))
}

async fn healthz(State(e): State<AppState>) -> Response {
    match e.probe() {
        Ok(()) => (StatusCode::OK, "ok").into_response(),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("unhealthy: {err:#}"),
        )
            .into_response(),
    }
}

async fn api_root() -> impl IntoResponse {
    Json(json!({
        "name": "tornas",
        "version": env!("CARGO_PKG_VERSION"),
        "openapi": "/api/openapi.json",
        "resources": {
            "movies": "/api/movies",
            "logs": "/api/logs",
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

async fn openapi() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        include_str!("openapi.json"),
    )
}

async fn api_trackers(State(e): State<AppState>) -> impl IntoResponse {
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
async fn api_trackers_refresh(State(e): State<AppState>) -> ApiResult<impl IntoResponse> {
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
async fn api_config(State(e): State<AppState>) -> impl IntoResponse {
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
    }))
}

#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default)]
    since: u64,
    #[serde(default = "default_log_limit")]
    limit: usize,
    level: Option<String>,
}
fn default_log_limit() -> usize {
    200
}

async fn api_logs(axum::extract::Query(q): axum::extract::Query<LogsQuery>) -> impl IntoResponse {
    let min = q
        .level
        .as_deref()
        .and_then(|l| l.parse::<tracing::Level>().ok());
    Json(crate::logging::recent(q.since, q.limit.min(2000), min))
}

async fn api_status(State(e): State<AppState>) -> ApiResult<impl IntoResponse> {
    Ok(Json(e.status()?))
}

async fn api_budget(State(e): State<AppState>) -> ApiResult<impl IntoResponse> {
    Ok(Json(e.status()?.budget))
}

async fn api_session(State(e): State<AppState>) -> ApiResult<impl IntoResponse> {
    Ok(Json(e.status()?.session))
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    50
}

async fn api_events(
    State(e): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<EventsQuery>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(e.catalog.recent_events(q.limit.min(500))?))
}

#[derive(Deserialize)]
struct ListQuery {
    state: Option<String>,
}

async fn api_list(
    State(e): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListQuery>,
) -> ApiResult<impl IntoResponse> {
    let mut movies = e.list_movies()?;
    if let Some(st) = q.state {
        movies.retain(|m| m.state == st);
    }
    Ok(Json(movies))
}

async fn api_add(
    State(e): State<AppState>,
    AppJson(req): AppJson<AddMovieRequest>,
) -> ApiResult<impl IntoResponse> {
    let v = e.add_movie(req).await?;
    let location = format!("/api/movies/{}", v.movie.imdb_id);
    Ok((StatusCode::CREATED, [(header::LOCATION, location)], Json(v)))
}

async fn api_get(
    State(e): State<AppState>,
    Path(imdb_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    match e.get_movie(&imdb_id)? {
        Some(v) => Ok(Json(v)),
        None => Err(ApiError::not_found("no such movie")),
    }
}

/// Partial update. Today the only writable field is `last_used_at`
/// (unix seconds, or the string "now"), which moves the movie to the back of
/// the eviction queue.
#[derive(Deserialize)]
struct MoviePatch {
    last_used_at: Option<serde_json::Value>,
}

async fn api_patch(
    State(e): State<AppState>,
    Path(imdb_id): Path<String>,
    AppJson(patch): AppJson<MoviePatch>,
) -> ApiResult<impl IntoResponse> {
    let Some(v) = patch.last_used_at else {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "nothing to update: supported fields are last_used_at",
        ));
    };
    let ts = match v {
        serde_json::Value::String(s) if s == "now" => None,
        serde_json::Value::Number(n) => n.as_i64(),
        _ => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid",
                "last_used_at must be unix seconds or \"now\"",
            ));
        }
    };
    Ok(Json(e.set_last_used(&imdb_id, ts)?))
}

async fn api_delete(
    State(e): State<AppState>,
    Path(imdb_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    if e.remove_movie(&imdb_id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("no such movie"))
    }
}

// ---- Stremio ---------------------------------------------------------------

fn strip_json(s: &str) -> &str {
    s.strip_suffix(".json").unwrap_or(s)
}

fn public_base(e: &Engine, headers: &HeaderMap) -> String {
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

async fn manifest(State(e): State<AppState>) -> impl IntoResponse {
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

fn meta_preview(v: &MovieView) -> serde_json::Value {
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

async fn catalog(
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

async fn meta(State(e): State<AppState>, Path(id): Path<String>) -> ApiResult<impl IntoResponse> {
    let id = strip_json(&id);
    match e.get_movie(id)? {
        Some(v) => Ok(Json(json!({ "meta": meta_preview(&v) }))),
        None => Ok(Json(json!({ "meta": null }))),
    }
}

async fn stream_list(
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

// ---- Video -----------------------------------------------------------------

#[derive(Deserialize)]
pub struct VideoPath {
    imdb_id: String,
    #[allow(dead_code)]
    filename: Option<String>,
}

async fn video(
    State(e): State<AppState>,
    Path(p): Path<VideoPath>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let (handle, file_idx, filename) = e.stream_target(&p.imdb_id)?;
    let mut stream = handle.stream(file_idx).await.context("opening stream")?;
    let mut out = HeaderMap::new();
    out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(mime) = mime_guess::from_path(&filename).first_raw() {
        out.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    }
    // DLNA renderers ask for these; answering them makes seeking work on TVs.
    if headers
        .get("getcontentFeatures.dlna.org")
        .is_some_and(|v| v.as_bytes() == b"1")
    {
        out.insert(
            "contentFeatures.dlna.org",
            HeaderValue::from_static("DLNA.ORG_OP=01"),
        );
    }
    if headers.get("transferMode.dlna.org").is_some() {
        out.insert(
            "transferMode.dlna.org",
            HeaderValue::from_static("Streaming"),
        );
    }

    let len = stream.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split_once('-'))
        .and_then(|(start, end)| {
            let start: u64 = start.parse().ok()?;
            let end = if end.is_empty() {
                None
            } else {
                Some(end.parse::<u64>().ok()?.saturating_add(1))
            };
            Some((start, end))
        });
    debug!(imdb = p.imdb_id, ?range, "video request");

    let (status, body): (StatusCode, Box<dyn AsyncRead + Send + Unpin>) = match range {
        Some((start, end)) => {
            if start >= len || end.is_some_and(|e| e <= start || e > len) {
                out.insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{len}")).unwrap(),
                );
                return Ok((StatusCode::RANGE_NOT_SATISFIABLE, out).into_response());
            }
            let end = end.unwrap_or(len);
            stream.seek(SeekFrom::Start(start)).await.context("seek")?;
            let take = end - start;
            out.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&take.to_string()).unwrap(),
            );
            out.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end - 1, len)).unwrap(),
            );
            crate::metrics::stream("range", take);
            (StatusCode::PARTIAL_CONTENT, Box::new(stream.take(take)))
        }
        None => {
            out.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&len.to_string()).unwrap(),
            );
            crate::metrics::stream("full", len);
            (StatusCode::OK, Box::new(stream))
        }
    };
    let s = tokio_util::io::ReaderStream::with_capacity(body, 64 * 1024);
    let _ = Bytes::new();
    Ok((status, out, Body::from_stream(s)).into_response())
}
