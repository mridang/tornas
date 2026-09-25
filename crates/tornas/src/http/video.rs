//! `/video`: the bytes themselves, shared by Stremio players and DLNA renderers.
//! Supports a single HTTP Range per request, which is what seeking needs, and
//! answers the two DLNA headers TVs ask for.

use std::io::SeekFrom;

use anyhow::Context;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use tracing::debug;

use super::{ApiResult, AppState};

// ---- Video -----------------------------------------------------------------

#[derive(Deserialize)]
pub struct VideoPath {
    imdb_id: String,
    #[allow(dead_code)]
    filename: Option<String>,
}

pub(super) async fn video(
    State(e): State<AppState>,
    Path(p): Path<VideoPath>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let (handle, file_idx, filename) = e.stream_target(&p.imdb_id)?;
    let mut stream = handle.stream(file_idx).await.context("opening stream")?;
    let mut out = HeaderMap::new();
    out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(mime) = mime_guess::from_path(&filename).first_raw() {
        out.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    }
    // DLNA renderers ask for these; answering them makes seeking work on TVs.
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

    let len = stream.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split_once('-'))
        .and_then(|(start, end)| {
            let start: u64 = start.parse().ok()?;
            let end = if end.is_empty() {
                None
            } else {
                Some(end.parse::<u64>().ok()?.saturating_add(1))
            };
            Some((start, end))
        });
    debug!(imdb = p.imdb_id, ?range, "video request");

    let (status, body): (StatusCode, Box<dyn AsyncRead + Send + Unpin>) = match range {
        Some((start, end)) => {
            if start >= len || end.is_some_and(|e| e <= start || e > len) {
                out.insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{len}")).unwrap(),
                );
                return Ok((StatusCode::RANGE_NOT_SATISFIABLE, out).into_response());
            }
            let end = end.unwrap_or(len);
            stream.seek(SeekFrom::Start(start)).await.context("seek")?;
            let take = end - start;
            out.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&take.to_string()).unwrap(),
            );
            out.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end - 1, len)).unwrap(),
            );
            crate::metrics::stream("range", take);
            (StatusCode::PARTIAL_CONTENT, Box::new(stream.take(take)))
        }
        None => {
            out.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&len.to_string()).unwrap(),
            );
            crate::metrics::stream("full", len);
            (StatusCode::OK, Box::new(stream))
        }
    };
    let s = tokio_util::io::ReaderStream::with_capacity(body, 64 * 1024);
    Ok((status, out, Body::from_stream(s)).into_response())
}
