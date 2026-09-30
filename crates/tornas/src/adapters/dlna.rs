//! The library, seen as something a TV can browse over DLNA.
//!
//! Takes the shared [`Library`] and maps each `MediaEntry` to a DLNA [`MediaItem`].
//! Same shape as the Stremio adapter: hold the library, read entries, map to the
//! protocol's type.

use std::sync::Arc;

use anyhow::Context;
use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use axum_extra::{TypedHeader, headers::Range};
use axum_range::{KnownSize, Ranged};
use serde::Deserialize;
use tracing::debug;

use crate::{
    http::{ApiError, AppState},
    media_catalog::Library,
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
                // DLNA has its own byte route; `video_path` gives `/video/...`, so
                // prefix it to reach this protocol's `/dlna/video/...`.
                path: format!("/dlna{}", e.video_path()),
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
pub fn video_router(engine: AppState) -> Router {
    Router::new()
        .route("/dlna/video/{imdb_id}/{filename}", get(video))
        .route("/dlna/video/{imdb_id}", get(video))
        .with_state(engine)
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
    State(e): State<AppState>,
    Path(p): Path<VideoPath>,
    range: Option<TypedHeader<Range>>,
    headers: HeaderMap,
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

    let response = (out, Ranged::new(range, body)).into_response();
    if matches!(
        response.status(),
        StatusCode::OK | StatusCode::PARTIAL_CONTENT
    ) {
        let served = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(len);
        let kind = if response.status() == StatusCode::PARTIAL_CONTENT {
            "range"
        } else {
            "full"
        };
        crate::adapters::stream(kind, served);
    }
    Ok(response)
}
