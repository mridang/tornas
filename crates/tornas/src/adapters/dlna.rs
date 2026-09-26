//! The library, seen as something a TV can browse over DLNA.
//!
//! Takes the shared [`Library`] and maps each `MediaEntry` to a DLNA [`MediaItem`].
//! Same shape as the Stremio adapter: hold the library, read entries, map to the
//! protocol's type.

use std::sync::Arc;

use crate::{
    dlna::{Browsable, MediaItem},
    media_catalog::Library,
};

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
                path: e.video_path(),
                title: e.display_title(),
                size_bytes: e.file_size,
                mime: mime_guess::from_path(&e.file_name).first(),
                id: e.id,
            })
            .collect()
    }
}
