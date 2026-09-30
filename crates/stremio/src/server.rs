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

use super::builder::Addon;
use super::extra::Extra;
use super::handler::{CatalogRequest, Error, Handler, MetaRequest, Reply, StreamRequest};
use super::model::{ContentType, Resource};

#[derive(Debug, Clone, Default)]
pub struct RouterOptions {
    /// Answer unmatched paths with the protocol's 404 body. Turn off when mounting
    /// beside other routes that should keep their own 404s.
    pub fallback: bool,
    /// Absolute URL this addon is reachable at. Leave `None` to derive it from
    /// each request's `Host` header, which is what a LAN box wants.
    pub public_url: Option<String>,
}

/// Every addon route, with a landing page and protocol 404s.
///
/// The caller supplies CORS: the protocol requires every route, including the
/// manifest, to allow all origins, and a host application usually has its own
/// layer already.
pub fn router<H>(addon: Addon<H>) -> Router
where
    H: Handler,
{
    router_with(addon, RouterOptions::default())
}

pub fn router_with<H>(addon: Addon<H>, opts: RouterOptions) -> Router
where
    H: Handler,
{
    let state = Arc::new(Mounted { addon, opts });
    let mut r = Router::new()
        .route("/manifest.json", get(manifest_root::<H>))
        .route("/{p1}/{p2}/{p3}", get(dispatch::<H>))
        .route("/{p1}/{p2}/{p3}/{p4}", get(dispatch::<H>));
    if state.opts.fallback {
        r = r.fallback(|| async { protocol_error(StatusCode::NOT_FOUND, "not found") });
    }
    r.with_state(state)
}

struct Mounted<H> {
    addon: Addon<H>,
    opts: RouterOptions,
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

/// The raw, still-encoded path segments in declaration order.
fn segments(params: &RawPathParams) -> Vec<String> {
    params.iter().map(|(_, v)| v.to_owned()).collect()
}

fn strip_json(s: &str) -> &str {
    s.strip_suffix(".json").unwrap_or(s)
}

/// One handler for every arity; which reading applies is decided here.
async fn dispatch<H>(State(m): Shared<H>, params: RawPathParams, headers: HeaderMap) -> Response
where
    H: Handler,
{
    let segs = segments(&params);
    let Some(req) = parse(&segs) else {
        return protocol_error(StatusCode::NOT_FOUND, "not found");
    };
    let Parsed {
        resource,
        content_type,
        id,
        extra,
    } = req;
    let base_url = base_url(&m.opts, &headers);

    match resource {
        Resource::Catalog => reply(
            m.addon
                .handler
                .catalog(CatalogRequest {
                    base_url,
                    content_type,
                    id,
                    extra,
                })
                .await,
        ),
        Resource::Meta => reply(
            m.addon
                .handler
                .meta(MetaRequest {
                    base_url,
                    content_type,
                    id,
                })
                .await,
        ),
        Resource::Stream => reply(
            m.addon
                .handler
                .stream(StreamRequest {
                    base_url,
                    content_type,
                    id,
                })
                .await,
        ),
    }
}

pub(super) struct Parsed {
    pub resource: Resource,
    pub content_type: ContentType,
    pub id: String,
    pub extra: Extra,
}

/// Work out which reading of the path applies. Three segments is
/// `resource/type/id`; four adds an `extra` blob. Anything else is not a route.
pub(super) fn parse(segs: &[String]) -> Option<Parsed> {
    let decoded = |s: &String| percent_decode(s);
    let build = |rest: &[String], extra: Option<&String>| {
        let resource = Resource::parse(&decoded(&rest[0]))?;
        let content_type = ContentType::parse(&decoded(&rest[1]))?;
        Some(Parsed {
            resource,
            content_type,
            id: strip_json(&decoded(&rest[2])).to_owned(),
            extra: extra.map(|e| Extra::parse(e)).unwrap_or_default(),
        })
    };

    match segs.len() {
        3 => build(&segs[0..3], None),
        4 => build(&segs[0..3], Some(&segs[3])),
        _ => None,
    }
}

/// Where the caller reached us, honouring a configured public URL and the usual
/// reverse-proxy header.
fn base_url(opts: &RouterOptions, headers: &HeaderMap) -> String {
    if let Some(u) = &opts.public_url {
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

    fn segs(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn three_segments_are_resource_type_id() {
        let p = parse(&segs(&["catalog", "movie", "local.json"])).unwrap();
        assert_eq!(p.resource, Resource::Catalog);
        assert_eq!(p.content_type, ContentType::Movie);
        assert_eq!(p.id, "local");
        assert!(p.extra.is_empty());
    }

    #[test]
    fn four_segments_carry_extras() {
        let p = parse(&segs(&[
            "catalog",
            "movie",
            "local",
            "search=Tom%20%26%20Jerry.json",
        ]))
        .unwrap();
        assert_eq!(p.extra.search(), Some("Tom & Jerry"));
    }

    #[test]
    fn nonsense_paths_are_rejected() {
        assert!(parse(&segs(&["nope", "movie", "x"])).is_none());
        assert!(parse(&segs(&["catalog", "banana", "x"])).is_none());
        assert!(parse(&segs(&["catalog", "movie"])).is_none());
        assert!(parse(&segs(&["a", "b", "c", "d", "e", "f"])).is_none());
    }
}
