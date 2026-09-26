//! Request gates, outermost first: the source-address check that keeps the open
//! endpoints safe on a LAN, the API token for writes, and the private-network
//! preflight that Chrome needs before web.stremio.com may call this box.

use axum::{
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

use super::{ApiError, AppState};

/// Refuse anything from a source address outside `network.allow_from`. This is the
/// primary protection: on the defaults only the LAN, loopback and your tailnet get
/// through, so Stremio and DLNA need no credentials and an exposed port serves
/// nothing.
pub(super) async fn require_allowed_source(
    State(e): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if e.acl.allows_everything() {
        return next.run(req).await;
    }
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    // No connect info (in-process tests) means no socket to judge; let it through.
    let Some(peer) = peer else {
        return next.run(req).await;
    };
    let xff = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok());
    let client = e.acl.client_ip(peer, xff);
    if e.acl.allows(client) {
        return next.run(req).await;
    }
    tracing::debug!(%client, path = %req.uri().path(), "refused: source address not allowed");
    crate::metrics::forbidden_source();
    ApiError::new(
        StatusCode::FORBIDDEN,
        "forbidden",
        format!("{client} is not in an allowed source range"),
    )
    .into_response()
}

/// When an API token is configured, mutating calls under /api and the log
/// endpoint need `Authorization: Bearer <token>` (or `X-Api-Token`). Everything
/// Stremio and DLNA players use stays open.
pub(super) async fn require_token(
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

/// Chrome's Private Network Access preflight (older Chrome/Edge) asks a public site
/// like web.stremio.com for permission to talk to a LAN address. Newer Chrome
/// (Local Network Access) shows the user a permission prompt instead; this header
/// is harmless there.
pub(super) async fn allow_private_network(
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
