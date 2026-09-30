//! The library, seen as a Stremio addon.
//!
//! Takes the shared [`Library`] and maps each `MediaEntry` to Stremio's protocol
//! objects. Same shape as the DLNA adapter: hold the library, read entries, map to
//! the protocol's type. The `stremio` module knows nothing about any of this.

use std::sync::Arc;

use anyhow::Context;
use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
    routing::get,
};
use axum_extra::{TypedHeader, headers::Range};
use axum_range::{KnownSize, Ranged};
use serde::Deserialize;
use tracing::debug;

use crate::{
    http::{ApiError, AppState},
    media_catalog::{Library, MediaEntry},
    utils::human_bytes,
};
use stremio::{
    AddonBuilder, BuildError, CatalogDef, CatalogRequest, CatalogResponse, ContentType, Error,
    ExtraDef, Handler, Meta, MetaPreview, MetaRequest, MetaResponse, PosterShape, Reply, Stream,
    StreamBehaviorHints, StreamRequest, StreamResponse, StreamSource, Video,
};

/// The id of the single catalogue this addon publishes.
pub const CATALOG_ID: &str = "local";

/// A Stremio view of the library. Cloneable because the addon registers it as the
/// catalogue, metadata and stream handler.
#[derive(Clone)]
pub struct StremioLibrary(pub Arc<dyn Library>);

/// The addon this crate serves: catalogue, metadata and streams, all from the library.
pub type TornasAddon = stremio::Addon<StremioLibrary>;

/// The Stremio addon as an axum router, ready to merge at the root: the manifest
/// and catalog/meta/stream endpoints, plus this protocol's own `/video` byte route.
/// Panics only on a malformed manifest, a programming error, not a runtime one.
pub fn router(engine: AppState) -> Router {
    let name = engine
        .opts
        .addon_name
        .clone()
        .unwrap_or_else(|| "Tornas".to_owned());
    let addon = addon(StremioLibrary(engine.library.clone()), name).expect("valid addon manifest");
    let addon_router = stremio::router_with(
        addon,
        stremio::RouterOptions {
            // tornas serves its own dashboard at `/` and applies its own CORS and
            // source-address checks to every route, these included.
            fallback: false,
            public_url: engine.opts.public_url.clone(),
        },
    );
    // The bytes Stremio players fetch. This is the URL `stream_for` hands out
    // (`MediaEntry::video_path`), so keep the two in step. The stream-metrics layer is
    // scoped to these routes, so it never counts the addon's JSON endpoints.
    addon_router.merge(
        Router::new()
            .route("/video/{imdb_id}/{filename}", get(video))
            .route("/video/{imdb_id}", get(video))
            .route_layer(axum::middleware::from_fn(crate::adapters::track_stream))
            .with_state(engine),
    )
}

/// The path parameters of `/video/{imdb_id}/{filename}`.
#[derive(Deserialize)]
struct VideoPath {
    imdb_id: String,
    /// Present only so the pretty URL matches; the file is chosen by imdb id.
    #[allow(dead_code)]
    filename: Option<String>,
}

/// Serve the movie's bytes, with range support (seeking, 206/416) from `axum-range`.
async fn video(
    State(e): State<AppState>,
    Path(p): Path<VideoPath>,
    range: Option<TypedHeader<Range>>,
) -> Result<Response, ApiError> {
    let (handle, file_idx, filename) = e.stream_target(&p.imdb_id)?;
    let stream = handle.stream(file_idx).await.context("opening stream")?;
    let len = stream.len();
    let body = KnownSize::sized(stream, len);
    let range = range.map(|TypedHeader(r)| r);

    let mut out = HeaderMap::new();
    if let Some(mime) = mime_guess::from_path(&filename).first_raw() {
        out.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    }

    debug!(imdb = p.imdb_id, ?range, "stremio video request");

    // Byte counting is done by the `track_stream` layer on this route.
    Ok((out, Ranged::new(range, body)).into_response())
}

/// Assemble the addon. The manifest follows from the handlers registered here.
pub fn addon(library: StremioLibrary, name: String) -> Result<TornasAddon, BuildError> {
    AddonBuilder::new("org.mridang.tornas", name, env!("CARGO_PKG_VERSION"))
        .description("Movies downloaded to this home media center")
        .logo("https://raw.githubusercontent.com/Stremio/stremio-art/main/originals/Stremio-logo-white.png")
        .types([ContentType::Movie])
        .id_prefixes(["tt"])
        .catalogs([CatalogDef::new(ContentType::Movie, CATALOG_ID, "Local Library")
            .extra(ExtraDef::optional("search"))
            .extra(ExtraDef::optional("skip"))
            .extra(ExtraDef::optional("genre"))])
        .meta([ContentType::Movie])
        .stream([ContentType::Movie])
        .build(library)
}

/// Stremio pages in hundreds; a shorter page tells it the catalogue has ended.
const PAGE: usize = 100;

fn preview(e: &MediaEntry) -> MetaPreview {
    MetaPreview {
        id: e.id.clone(),
        content_type: Some(ContentType::Movie),
        name: e.title.clone(),
        poster: e.poster.clone(),
        poster_shape: Some(PosterShape::Poster),
        genres: e.genres.clone(),
        imdb_rating: e.rating.map(|r| format!("{r:.1}")),
        release_info: e.year.map(|y| y.to_string()),
        description: e.overview.clone(),
        links: Vec::new(),
    }
}

fn full_meta(e: &MediaEntry) -> Meta {
    Meta {
        id: e.id.clone(),
        content_type: Some(ContentType::Movie),
        name: e.title.clone(),
        genres: e.genres.clone(),
        poster: e.poster.clone(),
        poster_shape: Some(PosterShape::Poster),
        background: e.backdrop.clone(),
        description: e.overview.clone(),
        release_info: e.year.map(|y| y.to_string()),
        imdb_rating: e.rating.map(|r| format!("{r:.1}")),
        // Only the year is known, so use the first of January rather than invent a
        // day; Stremio only renders the year for movies.
        released: e.year.map(|y| format!("{y}-01-01T00:00:00.000Z")),
        runtime: e.runtime_min.map(|r| format!("{r} min")),
        // A movie is a single video whose id matches the meta id, which is what
        // Stremio assumes when `videos` is empty — but being explicit lets players
        // that ask for the video list find it.
        videos: vec![Video {
            id: e.id.clone(),
            title: e.title.clone(),
            released: e
                .year
                .map(|y| format!("{y}-01-01T00:00:00.000Z"))
                .unwrap_or_default(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn stream_for(e: &MediaEntry, base_url: &str) -> Stream {
    let mp4 = e.file_name.to_ascii_lowercase().ends_with(".mp4");
    Stream {
        name: Some("Tornas".to_owned()),
        description: Some(format!("{}\n{}", e.file_name, human_bytes(e.file_size))),
        behavior_hints: StreamBehaviorHints {
            // Anything but MP4 over plain HTTP is not playable in the web player.
            not_web_ready: !mp4,
            // Subtitle addons match on the filename; omitting it is why they used
            // to find nothing.
            filename: Some(e.file_name.clone()),
            video_size: Some(e.file_size),
            binge_group: Some("tornas".to_owned()),
        },
        ..Stream::new(StreamSource::Url(format!("{base_url}{}", e.video_path())))
    }
}

impl Handler for StremioLibrary {
    async fn catalog(&self, req: CatalogRequest) -> Result<Reply<CatalogResponse>, Error> {
        if req.id != CATALOG_ID {
            return Err(Error::NotFound);
        }
        let mut entries = self.0.entries();
        if let Some(q) = req.extra.search() {
            let q = q.to_lowercase();
            entries.retain(|e| e.title.to_lowercase().contains(&q));
        }
        if let Some(g) = req.extra.genre() {
            entries.retain(|e| e.genres.iter().any(|x| x.eq_ignore_ascii_case(g)));
        }
        let metas = entries
            .iter()
            .skip(req.extra.skip().unwrap_or(0))
            .take(PAGE)
            .map(preview)
            .collect();
        // Short: the library changes whenever something finishes downloading.
        Ok(Reply::new(CatalogResponse {
            metas,
            metas_detailed: Vec::new(),
        })
        .cache_max_age(30))
    }

    async fn meta(&self, req: MetaRequest) -> Result<Reply<MetaResponse>, Error> {
        Ok(Reply::new(MetaResponse {
            meta: self.0.entry(&req.id).as_ref().map(full_meta),
        })
        .cache_max_age(30))
    }

    async fn stream(&self, req: StreamRequest) -> Result<Reply<StreamResponse>, Error> {
        let streams = self
            .0
            .entry(&req.id)
            .map(|e| vec![stream_for(&e, &req.base_url)])
            .unwrap_or_default();
        Ok(Reply::new(StreamResponse { streams }))
    }
}
