//! A server-side implementation of the Stremio addon protocol.
//!
//! An application implements [`Handler`](handler::Handler) for its own data, builds a
//! [`Manifest`](model::Manifest), pairs them with [`Addon::new`](server::Addon::new),
//! and mounts the [`router`](server::router).
//!
//! ```text
//! /manifest.json
//! /{resource}/{type}/{id}.json
//! /{resource}/{type}/{id}/{extra}.json
//! ```
//!
//! where `{extra}` is a query-string-shaped blob *inside the path*, e.g.
//! `search=blade%20runner&skip=100`.
//!
//! Laid out like the `dlna` crate: `handler` is the trait the app implements, `model`
//! the wire types, `server` the server; `extra` is the one extra piece this richer
//! protocol needs (the query-in-path parser).

pub mod extra;
pub mod handler;
pub mod header;
pub mod model;
pub mod server;

pub use extra::Extra;
pub use handler::{CatalogRequest, Error, Handler, MetaRequest, Reply, StreamRequest};
pub use header::{ForwardedProto, Scheme};
pub use model::*;
pub use server::{Addon, router};
