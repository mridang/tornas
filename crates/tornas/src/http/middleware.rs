//! Request layers, one per file. In request order (outermost first): the
//! source-address check that keeps the open endpoints safe on a LAN, the
//! private-network preflight Chrome needs before web.stremio.com may call this box,
//! the API token for writes, and the metrics layer that counts and times requests.
//! `http::shared` wires them in the right order.

mod metrics;
mod private_network;
mod source_acl;
mod token;

pub(super) use metrics::track_http;
pub(super) use private_network::allow_private_network;
pub(super) use source_acl::require_allowed_source;
pub(super) use token::{ApiToken, require_token};

/// Seed the refusal counters at zero so `/metrics` carries them before the first
/// refusal. The request-timing metric is route-labelled, so there is nothing to seed.
pub(super) fn install() {
    source_acl::seed();
    token::seed();
}

// The source ACL's policy type is built from config and held by the engine, so it
// is public beyond the gate itself.
pub use source_acl::{Acl, DEFAULT_ALLOW};
