//! The source-address gate: refuse anything from outside `network.allow_from`.
//! This is the primary protection — on the defaults only the LAN, loopback and your
//! tailnet get through, so Stremio and DLNA need no credentials and an exposed port
//! serves nothing.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::http::{ApiError, acl::Acl};

pub(in crate::http) async fn require_allowed_source(
    State(acl): State<Acl>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if acl.allows_everything() {
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
    let client = acl.client_ip(peer, xff);
    if acl.allows(client) {
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
