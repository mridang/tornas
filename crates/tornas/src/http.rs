//! The HTTP surface: the JSON API and the dashboard.
//!
//! This file owns only the shared plumbing — application state, the error envelope,
//! and the route table. Handlers live in the submodules. Playback (the Stremio
//! addon, the DLNA browse tree and each protocol's own `/video` byte route) belongs
//! to the adapters and is merged in by `router()`.

pub mod api;
pub mod dashboard;
pub mod middleware;

// The source ACL lives with its gate in `middleware/source_acl.rs`; surface the
// policy type here so config and the engine reach it as `crate::http::Acl`.
pub use middleware::{Acl, DEFAULT_ALLOW};

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};

/// The largest request body the API accepts. Generous for a base64 `.torrent`
/// (which are kilobytes, rarely a megabyte), while still refusing a runaway upload.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

use crate::engine::Engine;

use api::{
    api_add, api_budget, api_config, api_delete, api_events, api_get, api_list, api_patch,
    api_pause_delete, api_pause_get, api_pause_put, api_root, api_session, api_status,
    api_trackers, api_trackers_refresh, healthz, openapi, prometheus,
};
use dashboard::index;
use middleware::{allow_private_network, require_allowed_source, require_token, track_http};

pub type AppState = Arc<Engine>;

/// An API error, rendered as an RFC 7807 problem detail (`application/problem+json`)
/// with a status derived from the engine's typed fault kind. See `IntoResponse`.
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
        // RFC 7807 problem+json. `type` carries the machine-readable kind, `title`
        // the status's canonical reason, `detail` the specific message.
        problemdetails::new(self.status)
            .with_type(format!("/problems/{}", self.kind))
            .with_title(self.status.canonical_reason().unwrap_or("Error"))
            .with_detail(self.message)
            .with_value("status", self.status.as_u16())
            .into_response()
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

/// This crate's own routes, with their shared engine state applied: the dashboard
/// and the JSON API. The Stremio and DLNA routers bring their own state and are
/// merged separately.
pub fn routes(engine: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/metrics", get(prometheus))
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
        .route(
            "/api/pause",
            get(api_pause_get)
                .put(api_pause_put)
                .delete(api_pause_delete),
        )
        .route("/api/movies", get(api_list).post(api_add))
        .route(
            "/api/movies/{imdb_id}",
            get(api_get).patch(api_patch).delete(api_delete),
        )
        // Compress the dashboard, the JSON API and the /metrics text. The default
        // predicate skips tiny bodies, already-compressed types and the SSE stream
        // (/api/events), and the video routes are added elsewhere so media is never
        // compressed.
        .layer(CompressionLayer::new())
        .with_state(engine)
}

/// Wrap a merged router in the shared middleware, outermost-last so the
/// source-address check fronts everything and an outside client gets nothing but a
/// 403. The token layer self-selects `/api` writes by path, so applying it to the
/// whole router is the same as protecting only the endpoints that need it.
pub fn shared(engine: AppState) -> impl FnOnce(Router) -> Router {
    move |app| {
        middleware::install();
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
            .expose_headers(Any);
        // Each gate carries only what it needs as state, never the engine: the ACL
        // its ranges, the token gate the configured token.
        let acl = engine.acl.clone();
        let token = middleware::ApiToken::new(engine.opts.api_token.clone());
        // Inside the source-address check (refused requests have their own counter)
        // and after routing (so the route template is known): the body-size cap, then
        // token, then timing, then CORS, then the private-network preflight, then the
        // ACL, with a tracing span wrapping everything so even refused requests appear.
        app.layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
            .layer(axum::middleware::from_fn_with_state(token, require_token))
            .layer(axum::middleware::from_fn(track_http))
            .layer(cors)
            .layer(axum::middleware::from_fn(allow_private_network))
            .layer(axum::middleware::from_fn_with_state(
                acl,
                require_allowed_source,
            ))
            .layer(TraceLayer::new_for_http())
    }
}

/// The whole HTTP app in one call: this crate's routes, the Stremio addon, and
/// optionally the UPnP router, behind the shared middleware. The addon is merged at
/// the root so URLs people already installed keep working.
pub fn router(engine: AppState, upnp: Option<Router>) -> Router {
    // Each playback protocol brings its own routes, including its own `/video` byte
    // route: Stremio at `/video/...`, DLNA at `/dlna/video/...`.
    let mut app = routes(engine.clone())
        .merge(crate::adapters::stremio::router(engine.clone()))
        .merge(crate::adapters::dlna::video_router(engine.clone()));
    if let Some(u) = upnp {
        app = app.merge(Router::new().nest("/upnp", u));
    }
    shared(engine)(app)
}
