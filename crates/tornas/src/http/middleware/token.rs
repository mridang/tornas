//! The API-token gate: when a token is configured, mutating calls under `/api` need
//! `Authorization: Bearer <token>` (or `X-Api-Token`). Everything Stremio and DLNA
//! players use stays open.

use std::sync::{Arc, OnceLock};

use axum::{
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use opentelemetry::metrics::Counter;

use crate::{http::ApiError, metrics::meter};

/// Refused requests, counted where they are refused. Seeded at zero by [`seed`].
fn refused() -> &'static Counter<u64> {
    static C: OnceLock<Counter<u64>> = OnceLock::new();
    C.get_or_init(|| {
        meter()
            .u64_counter("tornas_unauthorized_total")
            .with_description("Requests refused for a missing or wrong API token")
            .build()
    })
}

pub(super) fn seed() {
    refused().add(0, &[]);
}

/// The gate's only input: the configured token, or `None` when auth is off. Built
/// from config and handed to the layer as state, so the gate needs nothing else.
#[derive(Clone, Default)]
pub struct ApiToken(pub Option<Arc<str>>);

impl ApiToken {
    /// Empty or absent means auth is off.
    pub fn new(token: Option<String>) -> Self {
        Self(token.filter(|t| !t.is_empty()).map(Arc::from))
    }
}

pub(in crate::http) async fn require_token(
    State(ApiToken(token)): State<ApiToken>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(token) = token.as_deref() else {
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
        refused().add(1, &[]);
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or wrong API token",
        )
        .into_response()
    }
}
