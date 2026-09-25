//! A server-side implementation of the Stremio addon protocol.
//!
//! Knows nothing about this crate: no engine, no catalog, no torrents. An
//! application implements the handler traits in [`handler`] for its own data, wires
//! them up with [`builder::AddonBuilder`], and mounts the router from [`router`].
//!
//! ```text
//! /manifest.json
//! /{resource}/{type}/{id}.json
//! /{resource}/{type}/{id}/{extra}.json
//! ```
//!
//! where `{extra}` is a query-string-shaped blob *inside the path*, e.g.
//! `search=blade%20runner&skip=100`.

pub mod builder;
pub mod extra;
pub mod handler;
pub mod model;
pub mod router;

pub use builder::{Addon, AddonBuilder, BuildError};
pub use extra::Extra;
pub use handler::{
    AddonCatalogHandler, CatalogHandler, CatalogRequest, Error, MetaHandler, MetaRequest, Reply,
    StreamHandler, StreamRequest, SubtitlesHandler, SubtitlesRequest,
};
pub use model::*;
pub use router::{ConfigMode, RouterOptions, router, router_with};
