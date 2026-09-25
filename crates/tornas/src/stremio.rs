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

pub mod extra;
pub mod model;

pub use extra::Extra;
pub use model::*;
