//! Glue between the product and the self-contained protocol servers.
//!
//! [`dlna`](self::dlna) and (later) the Stremio handlers live here because they are
//! the only code that knows both worlds: they read the catalog and the engine, and
//! speak the vocabulary each server defines. Keeping them here is what lets
//! `crate::dlna` and `crate::mdns` stay free of any reference to this crate.

pub mod dlna;
pub mod stremio;
