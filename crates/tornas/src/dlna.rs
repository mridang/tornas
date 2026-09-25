//! A DLNA/UPnP media server that browses any library implementing [`Browsable`].
//!
//! Knows nothing about this crate: no engine, no catalog, no movies. What a TV sees
//! is decided entirely by the [`Browsable`] implementation handed to [`Directory`].

pub mod browse;
pub mod library;

pub use browse::Directory;
pub use library::{Browsable, MediaItem};
