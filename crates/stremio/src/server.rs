//! The HTTP surface of the protocol.
//!
//! Routes are registered **by how many path segments they have**: `/manifest.json`,
//! `/{resource}/{type}/{id}.json`, and the same with a trailing `/{extra}.json`.
//! One handler per arity inspects the segments to decide what was asked for.

use std::sync::Arc;

use axum::{
    Router,
    extract::{RawPathParams, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use tracing::warn;

use super::extra::Extra;
use super::handler::{CatalogRequest, Error, Handler, MetaRequest, Reply, StreamRequest};
use super::model::{ContentType, Manifest};

/// A finished addon: the manifest plus the one handler behind it.
pub struct Addon<H> {
    manifest: Manifest,
    handler: H,
}

impl<H> Addon<H> {
    /// Pair a manifest with the handler that answers its requests.
    pub fn new(manifest: Manifest, handler: H) -> Self {
        Self { manifest, handler }
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
}

/// Mount the addon as an axum router: the manifest route and the resource routes.
///
/// The caller supplies CORS (the protocol requires every route to allow all origins)
/// and owns unmatched paths — this router adds no fallback, because tornas serves its
/// own dashboard and 404s. `public_url` is the absolute URL the addon is reached at;
/// `None` derives it per request from the `Host` header, which is what a LAN box wants.
pub fn router<H>(addon: Addon<H>, public_url: Option<String>) -> Router
where
    H: Handler,
{
    let state = Arc::new(Mounted { addon, public_url });
    Router::new()
        .route("/manifest.json", get(manifest_root::<H>))
        // The protocol shape is `/{resource}/{type}/{id}.json`; catalog may add a
        // trailing `/{extra}.json` blob (search/skip/genre).
        .route("/catalog/{content_type}/{id}", get(catalog::<H>))
        .route("/catalog/{content_type}/{id}/{extra}", get(catalog::<H>))
        .route("/meta/{content_type}/{id}", get(meta::<H>))
        .route("/stream/{content_type}/{id}", get(stream::<H>))
        .with_state(state)
}

struct Mounted<H> {
    addon: Addon<H>,
    public_url: Option<String>,
}

type Shared<H> = State<Arc<Mounted<H>>>;

/// `{"err": "..."}` — the shape the addon SDK uses, which Stremio understands.
fn protocol_error(status: StatusCode, msg: &str) -> Response {
    (status, json(&serde_json::json!({ "err": msg }), None)).into_response()
}

/// Serialise with the exact content type the protocol asks for.
fn json<T: Serialize>(body: &T, cache: Option<String>) -> Response {
    let Ok(text) = serde_json::to_string(body) else {
        return protocol_error(StatusCode::INTERNAL_SERVER_ERROR, "handler error");
    };
    let mut resp = Response::new(text.into());
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    if let Some(c) = cache.and_then(|c| HeaderValue::from_str(&c).ok()) {
        resp.headers_mut().insert(header::CACHE_CONTROL, c);
    }
    resp
}

fn reply<T: Serialize>(r: Result<Reply<T>, Error>) -> Response {
    match r {
        Ok(r) => json(&r.body, r.cache_control()),
        Err(Error::NotFound) => protocol_error(StatusCode::NOT_FOUND, "not found"),
        Err(Error::Internal(e)) => {
            warn!("stremio handler failed: {e:#}");
            protocol_error(StatusCode::INTERNAL_SERVER_ERROR, "handler error")
        }
    }
}

async fn manifest_root<H>(State(m): Shared<H>) -> Response
where
    H: Handler,
{
    json(m.addon.manifest(), None)
}

fn strip_json(s: &str) -> &str {
    s.strip_suffix(".json").unwrap_or(s)
}

/// The raw (still percent-encoded) value of a named path parameter.
fn raw<'a>(params: &'a RawPathParams, name: &str) -> Option<&'a str> {
    params.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
}

/// The `{content_type}` and `{id}` every resource route carries. The type must be one
/// we serve, or there is no such resource (404). Both are percent-decoded; the id's
/// `.json` suffix is dropped.
fn content_type_and_id(params: &RawPathParams) -> Option<(ContentType, String)> {
    let content_type = ContentType::parse(&percent_decode(raw(params, "content_type")?))?;
    let id = strip_json(&percent_decode(raw(params, "id")?)).to_owned();
    Some((content_type, id))
}

async fn catalog<H>(State(m): Shared<H>, params: RawPathParams, headers: HeaderMap) -> Response
where
    H: Handler,
{
    let Some((content_type, id)) = content_type_and_id(&params) else {
        return protocol_error(StatusCode::NOT_FOUND, "not found");
    };
    // The extra blob is split on `&`/`=` *before* decoding, so it is parsed from the
    // raw segment — decoding first would mangle a value like "Tom & Jerry".
    let extra = raw(&params, "extra").map(Extra::parse).unwrap_or_default();
    let req = CatalogRequest {
        base_url: base_url(m.public_url.as_deref(), &headers),
        content_type,
        id,
        extra,
    };
    reply(m.addon.handler.catalog(req).await)
}

async fn meta<H>(State(m): Shared<H>, params: RawPathParams, headers: HeaderMap) -> Response
where
    H: Handler,
{
    let Some((content_type, id)) = content_type_and_id(&params) else {
        return protocol_error(StatusCode::NOT_FOUND, "not found");
    };
    let req = MetaRequest {
        base_url: base_url(m.public_url.as_deref(), &headers),
        content_type,
        id,
    };
    reply(m.addon.handler.meta(req).await)
}

async fn stream<H>(State(m): Shared<H>, params: RawPathParams, headers: HeaderMap) -> Response
where
    H: Handler,
{
    let Some((content_type, id)) = content_type_and_id(&params) else {
        return protocol_error(StatusCode::NOT_FOUND, "not found");
    };
    let req = StreamRequest {
        base_url: base_url(m.public_url.as_deref(), &headers),
        content_type,
        id,
    };
    reply(m.addon.handler.stream(req).await)
}

/// Where the caller reached us, honouring a configured public URL and the usual
/// reverse-proxy header.
fn base_url(public_url: Option<&str>, headers: &HeaderMap) -> String {
    if let Some(u) = public_url {
        return u.trim_end_matches('/').to_owned();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

/// Only for segments that are not extras; extras are decoded after splitting.
fn percent_decode(s: &str) -> String {
    super::extra::decode(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_json_drops_only_the_suffix() {
        assert_eq!(strip_json("local.json"), "local");
        assert_eq!(strip_json("tt0111161"), "tt0111161");
        assert_eq!(strip_json("a.json.json"), "a.json");
    }
}
