//! The library, seen as a Stremio addon.
//!
//! Takes the shared [`Library`] and maps each `MediaEntry` to Stremio's protocol
//! objects. Same shape as the DLNA adapter: hold the library, read entries, map to
//! the protocol's type. The `stremio` module knows nothing about any of this.

use std::sync::Arc;

use crate::{
    media_catalog::{Library, MediaEntry},
    stremio::{
        AddonBuilder, BuildError, CatalogDef, CatalogRequest, CatalogResponse, ContentType, Error,
        ExtraDef, Handler, Meta, MetaPreview, MetaRequest, MetaResponse, PosterShape, Reply,
        Stream, StreamBehaviorHints, StreamRequest, StreamResponse, StreamSource, Video,
    },
    utils::human_bytes,
};

/// The id of the single catalogue this addon publishes.
pub const CATALOG_ID: &str = "local";

/// A Stremio view of the library. Cloneable because the addon registers it as the
/// catalogue, metadata and stream handler.
#[derive(Clone)]
pub struct StremioLibrary(pub Arc<dyn Library>);

/// The addon this crate serves: catalogue, metadata and streams, all from the library.
pub type TornasAddon = crate::stremio::Addon<StremioLibrary>;

/// The Stremio addon as an axum router, ready to merge at the root. Panics only on
/// a malformed manifest, which is a programming error, not a runtime condition.
pub fn router(
    library: Arc<dyn Library>,
    addon_name: String,
    public_url: Option<String>,
) -> axum::Router {
    let addon = addon(StremioLibrary(library), addon_name).expect("valid addon manifest");
    crate::stremio::router_with(
        addon,
        crate::stremio::RouterOptions {
            // tornas serves its own dashboard at `/` and applies its own CORS and
            // source-address checks to every route, these included.
            fallback: false,
            public_url,
        },
    )
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
            ..Default::default()
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
