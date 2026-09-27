//! `/video`: the bytes themselves, shared by Stremio players and DLNA renderers.
//! Range handling (seeking, 206/416) is done by `axum-range`; we add the content
//! type and the two DLNA headers TVs ask for.

use anyhow::Context;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use axum_extra::{TypedHeader, headers::Range};
use axum_range::{KnownSize, Ranged};
use serde::Deserialize;
use tracing::debug;

use super::{ApiResult, AppState};

#[derive(Deserialize)]
pub struct VideoPath {
    imdb_id: String,
    /// Present only so the pretty `/video/{id}/{name}` URL matches; the file is
    /// chosen by imdb id, not by this name.
    #[allow(dead_code)]
    filename: Option<String>,
}

pub(super) async fn video(
    State(e): State<AppState>,
    Path(p): Path<VideoPath>,
    range: Option<TypedHeader<Range>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let (handle, file_idx, filename) = e.stream_target(&p.imdb_id)?;
    let stream = handle.stream(file_idx).await.context("opening stream")?;
    let len = stream.len();
    let body = KnownSize::sized(stream, len);
    let range = range.map(|TypedHeader(r)| r);

    let mut out = HeaderMap::new();
    if let Some(mime) = mime_guess::from_path(&filename).first_raw() {
        out.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    }
    // DLNA renderers send these; answering them makes seeking work on TVs. The
    // endpoint is shared, so it speaks a little DLNA even though most callers (the
    // web player) ignore it.
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

    debug!(imdb = p.imdb_id, ?range, "video request");

    let response = (out, Ranged::new(range, body)).into_response();
    // Count what we actually served, by the status axum-range chose.
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
        crate::metrics::stream(kind, served);
    }
    Ok(response)
}
