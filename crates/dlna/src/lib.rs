//! A DLNA/UPnP media server that browses any library implementing [`Browsable`].
//!
//! Knows nothing about the application: no engine, no catalog, no movies. What a TV
//! sees is decided entirely by the [`Browsable`] implementation handed to
//! [`Directory`]. Laid out like the `stremio` crate: `handler` is the trait the app
//! implements, `model` the data types, `browse` the server.

pub mod browse;
pub mod handler;
pub mod model;

pub use browse::Directory;
pub use handler::Browsable;
pub use model::MediaItem;
