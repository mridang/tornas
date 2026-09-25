//! The HTTP surface of the protocol.
//!
//! Routes are registered **by how many path segments they have**, not by shape.
//! `/{resource}/{type}/{id}/{extra}.json` and `/{config}/{resource}/{type}/{id}.json`
//! are both four dynamic segments, so registering both makes axum's router panic
//! on a conflict at construction time; instead one route per arity is registered
//! and the ambiguous case is resolved by looking at the segments.

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
use super::handler::{
    AddonCatalogHandler, AddonCatalogRequest, CatalogHandler, CatalogRequest, Error, MetaHandler,
    MetaRequest, Reply, StreamHandler, StreamRequest, SubtitlesHandler, SubtitlesRequest,
};
use super::model::{ContentType, Resource};

/// Whether this addon takes user data in the URL.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConfigMode {
    /// No user data: a four-segment path is always `resource/type/id/extra`.
    #[default]
    Disabled,
    /// User data is optional; four-segment paths are resolved by inspecting them.
    Auto,
    /// Every request carries user data.
    Required,
}

#[derive(Debug, Clone)]
pub struct RouterOptions {
    /// Serve a landing page at `/` (and `/configure` when configurable). Turn off
    /// when the host application already serves something at `/`.
    pub landing: bool,
    /// Answer unmatched paths with the protocol's 404 body. Turn off when mounting
    /// beside other routes that should keep their own 404s.
    pub fallback: bool,
    pub config_mode: ConfigMode,
    /// Absolute URL this addon is reachable at. Leave `None` to derive it from
    /// each request's `Host` header, which is what a LAN box wants.
    pub public_url: Option<String>,
}

impl Default for RouterOptions {
    fn default() -> Self {
        Self {
            landing: true,
            fallback: true,
            config_mode: ConfigMode::Disabled,
            public_url: None,
        }
    }
}

/// Every addon route, with a landing page and protocol 404s.
///
/// The caller supplies CORS: the protocol requires every route, including the
/// manifest, to allow all origins, and a host application usually has its own
/// layer already.
pub fn router<C, M, S, Sb, Ac>(addon: Addon<C, M, S, Sb, Ac>) -> Router
where
    C: CatalogHandler,
    M: MetaHandler,
    S: StreamHandler,
    Sb: SubtitlesHandler,
    Ac: AddonCatalogHandler,
{
    router_with(addon, RouterOptions::default())
}

pub fn router_with<C, M, S, Sb, Ac>(addon: Addon<C, M, S, Sb, Ac>, opts: RouterOptions) -> Router
where
    C: CatalogHandler,
    M: MetaHandler,
    S: StreamHandler,
    Sb: SubtitlesHandler,
    Ac: AddonCatalogHandler,
{
    let state = Arc::new(Mounted { addon, opts });
    let mut r = Router::new()
        .route("/manifest.json", get(manifest_root::<C, M, S, Sb, Ac>))
        .route("/{p1}/{p2}/{p3}", get(dispatch::<C, M, S, Sb, Ac>))
        .route("/{p1}/{p2}/{p3}/{p4}", get(dispatch::<C, M, S, Sb, Ac>))
        .route(
            "/{p1}/{p2}/{p3}/{p4}/{p5}",
            get(dispatch::<C, M, S, Sb, Ac>),
        );
    if state.opts.config_mode != ConfigMode::Disabled {
        r = r.route(
            "/{p1}/manifest.json",
            get(manifest_configured::<C, M, S, Sb, Ac>),
        );
    }
    if state.opts.landing {
        r = r
            .route("/", get(landing::<C, M, S, Sb, Ac>))
            .route("/configure", get(landing::<C, M, S, Sb, Ac>));
    }
    if state.opts.fallback {
        r = r.fallback(|| async { protocol_error(StatusCode::NOT_FOUND, "not found") });
    }
    r.with_state(state)
}

struct Mounted<C, M, S, Sb, Ac> {
    addon: Addon<C, M, S, Sb, Ac>,
    opts: RouterOptions,
}

type Shared<C, M, S, Sb, Ac> = State<Arc<Mounted<C, M, S, Sb, Ac>>>;

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

async fn manifest_root<C, M, S, Sb, Ac>(State(m): Shared<C, M, S, Sb, Ac>) -> Response
where
    C: CatalogHandler,
    M: MetaHandler,
    S: StreamHandler,
    Sb: SubtitlesHandler,
    Ac: AddonCatalogHandler,
{
    if m.opts.config_mode == ConfigMode::Required {
        // Without user data there is nothing to serve yet, but the manifest still
        // has to describe the addon so Stremio can offer the configure page.
        return json(&m.addon.manifest_for(None), None);
    }
    json(m.addon.manifest(), None)
}

async fn manifest_configured<C, M, S, Sb, Ac>(
    State(m): Shared<C, M, S, Sb, Ac>,
    params: RawPathParams,
) -> Response
where
    C: CatalogHandler,
    M: MetaHandler,
    S: StreamHandler,
    Sb: SubtitlesHandler,
    Ac: AddonCatalogHandler,
{
    let config = segments(&params).into_iter().next();
    json(&m.addon.manifest_for(config.as_deref()), None)
}

async fn landing<C, M, S, Sb, Ac>(State(m): Shared<C, M, S, Sb, Ac>) -> Response
where
    C: CatalogHandler,
    M: MetaHandler,
    S: StreamHandler,
    Sb: SubtitlesHandler,
    Ac: AddonCatalogHandler,
{
    let man = m.addon.manifest();
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>{name}</title>\
         <body style=\"font-family:system-ui;max-width:40rem;margin:4rem auto;padding:0 1rem\">\
         <h1>{name}</h1><p>{description}</p>\
         <p>Version {version}. Install this addon in Stremio by adding its \
         <a href=\"manifest.json\">manifest.json</a> URL.</p>",
        name = escape(&man.name),
        description = escape(&man.description),
        version = escape(&man.version),
    );
    let mut resp = Response::new(html.into());
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;")
}

/// The raw, still-encoded path segments in declaration order.
fn segments(params: &RawPathParams) -> Vec<String> {
    params.iter().map(|(_, v)| v.to_owned()).collect()
}

fn strip_json(s: &str) -> &str {
    s.strip_suffix(".json").unwrap_or(s)
}

/// One handler for every arity; which reading applies is decided here.
async fn dispatch<C, M, S, Sb, Ac>(
    State(m): Shared<C, M, S, Sb, Ac>,
    params: RawPathParams,
    headers: HeaderMap,
) -> Response
where
    C: CatalogHandler,
    M: MetaHandler,
    S: StreamHandler,
    Sb: SubtitlesHandler,
    Ac: AddonCatalogHandler,
{
    let segs = segments(&params);
    let Some(req) = parse(&segs, m.opts.config_mode) else {
        return protocol_error(StatusCode::NOT_FOUND, "not found");
    };
    let Parsed {
        resource,
        content_type,
        id,
        extra,
        config,
    } = req;
    let base_url = base_url(&m.opts, &headers);

    match resource {
        Resource::Catalog => reply(
            m.addon
                .catalog
                .catalog(CatalogRequest {
                    base_url,
                    content_type,
                    id,
                    extra,
                    config,
                })
                .await,
        ),
        Resource::Meta => reply(
            m.addon
                .meta
                .meta(MetaRequest {
                    base_url,
                    content_type,
                    id,
                    config,
                })
                .await,
        ),
        Resource::Stream => reply(
            m.addon
                .stream
                .stream(StreamRequest {
                    base_url,
                    content_type,
                    id,
                    config,
                })
                .await,
        ),
        Resource::Subtitles => reply(
            m.addon
                .subtitles
                .subtitles(SubtitlesRequest {
                    base_url,
                    content_type,
                    id,
                    extra,
                    config,
                })
                .await,
        ),
        Resource::AddonCatalog => reply(
            m.addon
                .addon_catalog
                .addon_catalog(AddonCatalogRequest {
                    base_url,
                    content_type,
                    id,
                    config,
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
    pub config: Option<String>,
}

/// Work out which reading of the path applies.
///
/// Three segments is always `resource/type/id`; five is always
/// `config/resource/type/id/extra`. Four is ambiguous, and is read as a config
/// prefix only when that is the reading which parses — with `ConfigMode::Auto`
/// both the resource *and* the type must line up before user data is assumed,
/// so a user-data blob that happens to read like a resource name cannot hijack it.
pub(super) fn parse(segs: &[String], mode: ConfigMode) -> Option<Parsed> {
    let decoded = |s: &String| percent_decode(s);
    let build = |config: Option<String>, rest: &[String], extra: Option<&String>| {
        let resource = Resource::parse(&decoded(&rest[0]))?;
        let content_type = ContentType::parse(&decoded(&rest[1]))?;
        Some(Parsed {
            resource,
            content_type,
            id: strip_json(&decoded(&rest[2])).to_owned(),
            extra: extra.map(|e| Extra::parse(e)).unwrap_or_default(),
            config,
        })
    };

    match (segs.len(), mode) {
        (3, ConfigMode::Required) => None,
        (3, _) => build(None, &segs[0..3], None),
        (4, ConfigMode::Disabled) => build(None, &segs[0..3], Some(&segs[3])),
        (4, ConfigMode::Required) => build(Some(decoded(&segs[0])), &segs[1..4], None),
        (4, ConfigMode::Auto) => {
            // Prefer the no-config reading, and only fall back to treating the first
            // segment as user data when the remaining three genuinely parse.
            build(None, &segs[0..3], Some(&segs[3]))
                .or_else(|| build(Some(decoded(&segs[0])), &segs[1..4], None))
        }
        (5, ConfigMode::Disabled) => None,
        (5, _) => build(Some(decoded(&segs[0])), &segs[1..4], Some(&segs[4])),
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
    Extra::parse(&format!("x={s}"))
        .get("x")
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn three_segments_are_resource_type_id() {
        let p = parse(
            &segs(&["catalog", "movie", "local.json"]),
            ConfigMode::Disabled,
        )
        .unwrap();
        assert_eq!(p.resource, Resource::Catalog);
        assert_eq!(p.content_type, ContentType::Movie);
        assert_eq!(p.id, "local");
        assert!(p.extra.is_empty());
        assert!(p.config.is_none());
    }

    #[test]
    fn four_segments_without_config_carry_extras() {
        let p = parse(
            &segs(&["catalog", "movie", "local", "search=Tom%20%26%20Jerry.json"]),
            ConfigMode::Disabled,
        )
        .unwrap();
        assert_eq!(p.extra.search(), Some("Tom & Jerry"));
        assert!(p.config.is_none());
    }

    #[test]
    fn four_segments_with_required_config_are_user_data() {
        let p = parse(
            &segs(&["abc123", "stream", "movie", "tt0111161.json"]),
            ConfigMode::Required,
        )
        .unwrap();
        assert_eq!(p.config.as_deref(), Some("abc123"));
        assert_eq!(p.resource, Resource::Stream);
        assert_eq!(p.id, "tt0111161");
    }

    #[test]
    fn auto_mode_prefers_the_reading_that_parses() {
        // Looks like config/resource/type/id, and only that reading parses.
        let p = parse(
            &segs(&["abc123", "meta", "movie", "tt1.json"]),
            ConfigMode::Auto,
        )
        .unwrap();
        assert_eq!(p.config.as_deref(), Some("abc123"));

        // Looks like resource/type/id/extra, which parses first.
        let p = parse(
            &segs(&["catalog", "movie", "local", "skip=100.json"]),
            ConfigMode::Auto,
        )
        .unwrap();
        assert!(p.config.is_none());
        assert_eq!(p.extra.skip(), Some(100));
    }

    #[test]
    fn five_segments_are_config_plus_extras() {
        let p = parse(
            &segs(&["cfg", "catalog", "movie", "local", "genre=Drama.json"]),
            ConfigMode::Auto,
        )
        .unwrap();
        assert_eq!(p.config.as_deref(), Some("cfg"));
        assert_eq!(p.extra.genre(), Some("Drama"));

        assert!(
            parse(
                &segs(&["cfg", "catalog", "movie", "local", "genre=Drama.json"]),
                ConfigMode::Disabled
            )
            .is_none(),
            "an addon without user data must not accept a config prefix"
        );
    }

    #[test]
    fn nonsense_paths_are_rejected() {
        assert!(parse(&segs(&["nope", "movie", "x"]), ConfigMode::Disabled).is_none());
        assert!(parse(&segs(&["catalog", "banana", "x"]), ConfigMode::Disabled).is_none());
        assert!(parse(&segs(&["catalog", "movie"]), ConfigMode::Disabled).is_none());
        assert!(parse(&segs(&["a", "b", "c", "d", "e", "f"]), ConfigMode::Auto).is_none());
    }
}
