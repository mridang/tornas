//! The movie library, seen as a Stremio addon.
//!
//! This is where the engine's `MovieView` becomes protocol objects. The `stremio`
//! module knows nothing about any of these types.

use std::sync::Arc;

use crate::{
    engine::{Engine, MovieView},
    stremio::{
        AddonBuilder, BuildError, CatalogDef, CatalogHandler, CatalogRequest, CatalogResponse,
        ContentType, Error, ExtraDef, Meta, MetaHandler, MetaPreview, MetaRequest, MetaResponse,
        PosterShape, Reply, Stream, StreamBehaviorHints, StreamHandler, StreamRequest,
        StreamResponse, StreamSource, Video,
    },
    units::human_bytes,
};

/// The id of the single catalogue this addon publishes.
pub const CATALOG_ID: &str = "local";

/// The addon this crate serves: catalogue, metadata and streams, all answered by
/// the engine.
pub type TornasAddon = crate::stremio::Addon<Arc<Engine>, Arc<Engine>, Arc<Engine>>;

/// The Stremio addon as an axum router, ready to merge at the root. Panics only on
/// a malformed manifest, which is a programming error, not a runtime condition.
pub fn router(engine: Arc<Engine>) -> axum::Router {
    let public_url = engine.opts.public_url.clone();
    let addon = addon(engine).expect("valid addon manifest");
    crate::stremio::router_with(
        addon,
        crate::stremio::RouterOptions {
            // tornas serves its own dashboard at `/` and applies its own CORS and
            // source-address checks to every route, these included.
            landing: false,
            fallback: false,
            config_mode: crate::stremio::ConfigMode::Disabled,
            public_url,
        },
    )
}

/// Assemble the addon. The manifest follows from the handlers registered here.
pub fn addon(engine: Arc<Engine>) -> Result<TornasAddon, BuildError> {
    let name = engine
        .opts
        .addon_name
        .clone()
        .unwrap_or_else(|| "Tornas".to_owned());
    AddonBuilder::new("org.mridang.tornas", name, env!("CARGO_PKG_VERSION"))
        .description("Movies downloaded to this home media center")
        .logo("https://raw.githubusercontent.com/Stremio/stremio-art/main/originals/Stremio-logo-white.png")
        .types([ContentType::Movie])
        .id_prefixes(["tt"])
        .catalogs(
            [CatalogDef::new(ContentType::Movie, CATALOG_ID, "Local Library")
                .extra(ExtraDef::optional("search"))
                .extra(ExtraDef::optional("skip"))
                .extra(ExtraDef::optional("genre"))],
            engine.clone(),
        )
        .meta([ContentType::Movie], engine.clone())
        .stream([ContentType::Movie], engine)
        .build()
}

/// Stremio pages in hundreds; a shorter page tells it the catalogue has ended.
const PAGE: usize = 100;

fn preview(v: &MovieView) -> MetaPreview {
    let m = &v.movie;
    MetaPreview {
        id: m.imdb_id.clone(),
        content_type: Some(ContentType::Movie),
        name: m.title.clone(),
        poster: m.poster_url.clone(),
        poster_shape: Some(PosterShape::Poster),
        genres: m.genres.clone(),
        imdb_rating: m.rating.map(|r| format!("{r:.1}")),
        release_info: m.year.map(|y| y.to_string()),
        description: m.overview.clone(),
        links: Vec::new(),
    }
}

fn full_meta(v: &MovieView) -> Meta {
    let m = &v.movie;
    Meta {
        id: m.imdb_id.clone(),
        content_type: Some(ContentType::Movie),
        name: m.title.clone(),
        genres: m.genres.clone(),
        poster: m.poster_url.clone(),
        poster_shape: Some(PosterShape::Poster),
        background: m.backdrop_url.clone(),
        description: m.overview.clone(),
        release_info: m.year.map(|y| y.to_string()),
        imdb_rating: m.rating.map(|r| format!("{r:.1}")),
        // Only the year is known, so use the first of January rather than invent a
        // day; Stremio only renders the year for movies.
        released: m.year.map(|y| format!("{y}-01-01T00:00:00.000Z")),
        runtime: m.runtime_min.map(|r| format!("{r} min")),
        // A movie is a single video whose id matches the meta id, which is what
        // Stremio assumes when `videos` is empty — but being explicit lets players
        // that ask for the video list find it.
        videos: vec![Video {
            id: m.imdb_id.clone(),
            title: m.title.clone(),
            released: m
                .year
                .map(|y| format!("{y}-01-01T00:00:00.000Z"))
                .unwrap_or_default(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn stream_for(v: &MovieView, base_url: &str) -> Option<Stream> {
    let t = v.torrent.as_ref()?;
    let filename = url::form_urlencoded::byte_serialize(t.video_file_name.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    let progress = (v.progress_bytes * 100)
        .checked_div(v.total_bytes)
        .unwrap_or(0);
    let mp4 = t.video_file_name.to_ascii_lowercase().ends_with(".mp4");
    Some(Stream {
        name: Some("Tornas".to_owned()),
        description: Some(format!(
            "{}\n{} · {progress}%",
            t.video_file_name,
            human_bytes(t.size_bytes)
        )),
        behavior_hints: StreamBehaviorHints {
            // Anything but MP4 over plain HTTP is not playable in the web player.
            not_web_ready: !mp4,
            // Subtitle addons match on the filename; omitting it is why they used
            // to find nothing.
            filename: Some(t.video_file_name.clone()),
            video_size: Some(t.size_bytes),
            binge_group: Some("tornas".to_owned()),
            ..Default::default()
        },
        ..Stream::new(StreamSource::Url(format!(
            "{base_url}/video/{}/{filename}",
            v.movie.imdb_id
        )))
    })
}

impl CatalogHandler for Arc<Engine> {
    async fn catalog(&self, req: CatalogRequest) -> Result<Reply<CatalogResponse>, Error> {
        if req.id != CATALOG_ID {
            return Err(Error::NotFound);
        }
        let mut movies = self.list_movies().map_err(Error::internal)?;
        if let Some(q) = req.extra.search() {
            let q = q.to_lowercase();
            movies.retain(|m| m.movie.title.to_lowercase().contains(&q));
        }
        if let Some(g) = req.extra.genre() {
            movies.retain(|m| m.movie.genres.iter().any(|x| x.eq_ignore_ascii_case(g)));
        }
        let metas = movies
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
}

impl MetaHandler for Arc<Engine> {
    async fn meta(&self, req: MetaRequest) -> Result<Reply<MetaResponse>, Error> {
        let movie = self.get_movie(&req.id).map_err(Error::internal)?;
        Ok(Reply::new(MetaResponse {
            meta: movie.as_ref().map(full_meta),
        })
        .cache_max_age(30))
    }
}

impl StreamHandler for Arc<Engine> {
    async fn stream(&self, req: StreamRequest) -> Result<Reply<StreamResponse>, Error> {
        let movie = self.get_movie(&req.id).map_err(Error::internal)?;
        let streams = movie
            .as_ref()
            .and_then(|v| stream_for(v, &req.base_url))
            .map(|s| vec![s])
            .unwrap_or_default();
        // Not cached: whether a movie is playable changes as it downloads, and a
        // stale "no streams" is the most annoying answer to get.
        Ok(Reply::new(StreamResponse { streams }))
    }
}
