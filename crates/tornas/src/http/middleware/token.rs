//! The API-token gate: when a token is configured, mutating calls under `/api` need
//! `Authorization: Bearer <token>` (or `X-Api-Token`). Everything Stremio and DLNA
//! players use stays open.

use axum::{
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::http::{ApiError, AppState};

pub(in crate::http) async fn require_token(
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
            && req.method() != http::Method::OPTIONS);
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
