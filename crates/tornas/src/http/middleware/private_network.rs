//! The Private Network Access preflight. Older Chrome/Edge ask a public site like
//! web.stremio.com for permission to talk to a LAN address; this answers yes. Newer
//! Chrome (Local Network Access) shows the user a prompt instead, where this header
//! is harmless.

use axum::{http::HeaderValue, response::Response};

pub(in crate::http) async fn allow_private_network(
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
