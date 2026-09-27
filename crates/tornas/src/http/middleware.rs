//! Request gates, one per file. In request order (outermost first): the
//! source-address check that keeps the open endpoints safe on a LAN, the
//! private-network preflight Chrome needs before web.stremio.com may call this box,
//! and the API token for writes. `http::shared` wires them in the right order.

mod private_network;
mod source_acl;
mod token;

pub(super) use private_network::allow_private_network;
pub(super) use source_acl::require_allowed_source;
pub(super) use token::require_token;

// The source ACL's policy type is built from config and held by the engine, so it
// is public beyond the gate itself.
pub use source_acl::{Acl, DEFAULT_ALLOW};
