//! The library, seen as something a TV can browse over DLNA.
//!
//! Takes the shared [`Library`] and maps each `MediaEntry` to a DLNA [`MediaItem`].
//! Same shape as the Stremio adapter: hold the library, read entries, map to the
//! protocol's type.

use std::sync::Arc;

use axum::{extract::Request, http::HeaderValue, middleware::Next, response::Response};

use crate::media_catalog::Library;
use dlna::{Browsable, MediaItem};

/// A DLNA view of the library.
pub struct DlnaLibrary(pub Arc<dyn Library>);

/// A layer for the shared `/video` route that answers the two headers DLNA
/// renderers send, so seeking works on TVs. It reads the request's DLNA headers and
/// decorates the response; every other caller (the web player) never sends them and
/// sees nothing added. This keeps the byte-serving handler free of DLNA specifics.
pub async fn stream_headers(req: Request, next: Next) -> Response {
    let wants_features = req
        .headers()
        .get("getcontentFeatures.dlna.org")
        .is_some_and(|v| v.as_bytes() == b"1");
    let wants_transfer = req.headers().get("transferMode.dlna.org").is_some();

    let mut res = next.run(req).await;
    let h = res.headers_mut();
    if wants_features {
        h.insert(
            "contentFeatures.dlna.org",
            HeaderValue::from_static("DLNA.ORG_OP=01"),
        );
    }
    if wants_transfer {
        h.insert(
            "transferMode.dlna.org",
            HeaderValue::from_static("Streaming"),
        );
    }
    res
}

impl Browsable for DlnaLibrary {
    fn folder_name(&self) -> String {
        "Movies".to_owned()
    }

    fn items(&self) -> Vec<MediaItem> {
        self.0
            .entries()
            .into_iter()
            .map(|e| MediaItem {
                path: e.video_path(),
                title: e.display_title(),
                size_bytes: e.file_size,
                mime: mime_guess::from_path(&e.file_name).first(),
                id: e.id,
            })
            .collect()
    }
}
