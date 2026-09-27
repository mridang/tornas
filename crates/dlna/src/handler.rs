//! What this DLNA server needs from whoever owns the media: a browsable library.
//! The trait the application implements (the counterpart to Stremio's `Handler`).

use super::model::MediaItem;

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
