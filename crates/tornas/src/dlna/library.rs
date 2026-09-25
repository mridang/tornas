//! What this DLNA server needs from whoever owns the media.
//!
//! Deliberately its own vocabulary — playable files with a title, a size and a URL.
//! It is not shared with the Stremio module, which wants catalogue entries with
//! posters and IMDb ids; one trait serving both would couple two protocols that
//! have nothing to do with each other.

/// One playable file.
pub struct MediaItem {
    /// Stable identity within the library; used to build the UPnP object id.
    pub id: String,
    /// What a TV shows in the list.
    pub title: String,
    /// Path on the serving host, e.g. `/video/tt0111161/shawshank.mp4`. The host
    /// and scheme are filled in per request, since renderers address us by the
    /// interface they reached us on.
    pub path: String,
    pub size_bytes: u64,
    pub mime: Option<mime_guess::Mime>,
}

/// A library this server can browse. Implement it over a database, a directory, or
/// anything else.
pub trait Browsable: Send + Sync + 'static {
    /// The name of the single folder renderers see.
    fn folder_name(&self) -> String {
        "Media".to_owned()
    }

    /// Everything in that folder, in the order it should be shown.
    fn items(&self) -> Vec<MediaItem>;
}
