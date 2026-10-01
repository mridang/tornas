//! The library, seen as something a TV can browse over DLNA.
//!
//! Takes the shared [`Library`] and maps each `MediaEntry` to a DLNA [`MediaItem`].
//! Same shape as the Stremio adapter: hold the library, read entries, map to the
//! protocol's type.

use std::sync::Arc;

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
    http::ApiError,
    media_catalog::{Library, Streamer},
};
use dlna::{Browsable, MediaItem};

/// A DLNA view of the library.
pub struct DlnaLibrary(pub Arc<dyn Library>);

impl Browsable for DlnaLibrary {
    fn folder_name(&self) -> String {
        "Movies".to_owned()
    }

    fn items(&self) -> Vec<MediaItem> {
        self.0
            .entries()
            .into_iter()
            .map(|e| MediaItem {
                // This protocol's own byte route; each adapter builds its own URL.
                path: e.video_path("/dlna/video"),
                title: e.display_title(),
                size_bytes: e.file_size,
                mime: mime_guess::from_path(&e.file_name).first(),
                id: e.id,
            })
            .collect()
    }
}

/// This protocol's own byte route, ready to merge at the root. TVs fetch bytes from
/// `/dlna/video/...` — the URL the browse tree hands out in `items()`.
pub fn video_router(streamer: Arc<dyn Streamer>) -> Router {
    Router::new()
        .route("/dlna/video/{imdb_id}/{filename}", get(video))
        .route("/dlna/video/{imdb_id}", get(video))
        .route_layer(axum::middleware::from_fn(crate::adapters::track_stream))
        .with_state(streamer)
}

/// The path parameters of `/dlna/video/{imdb_id}/{filename}`.
#[derive(Deserialize)]
struct VideoPath {
    imdb_id: String,
    /// Present only so the pretty URL matches; the file is chosen by imdb id.
    #[allow(dead_code)]
    filename: Option<String>,
}

/// Serve the movie's bytes with range support, answering the two headers DLNA
/// renderers send so seeking works on TVs.
async fn video(
    State(streamer): State<Arc<dyn Streamer>>,
    Path(p): Path<VideoPath>,
    range: Option<TypedHeader<Range>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let media = streamer.open(p.imdb_id.clone()).await?;
    let body = KnownSize::sized(media.reader, media.len);
    let range = range.map(|TypedHeader(r)| r);

    let mut out = HeaderMap::new();
    if let Some(mime) = mime_guess::from_path(&media.file_name).first_raw() {
        out.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    }
    if headers
        .get("getcontentFeatures.dlna.org")
        .is_some_and(|v| v.as_bytes() == b"1")
    {
        out.insert(
            "contentFeatures.dlna.org",
            HeaderValue::from_static("DLNA.ORG_OP=01"),
        );
    }
    if headers.get("transferMode.dlna.org").is_some() {
        out.insert(
            "transferMode.dlna.org",
            HeaderValue::from_static("Streaming"),
        );
    }

    debug!(imdb = p.imdb_id, ?range, "dlna video request");

    // Byte counting is done by the `track_stream` layer on this route.
    Ok((out, Ranged::new(range, body)).into_response())
}
