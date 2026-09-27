//! The data type this DLNA server hands back — playable files with a title, a size
//! and a URL. Deliberately its own vocabulary, not shared with other protocols.

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
