//! The terminal commands: everything reachable from `tornas <command>` that is not
//! the server itself. They talk to a running server over its JSON API rather than
//! touching the engine, so they work from any machine on the network.

pub mod api;
pub mod doctor;
pub mod health;
pub mod pause;
pub mod status;
pub mod top;

pub use doctor::run as doctor;
pub use health::run as health;
pub use pause::{pause, resume};
pub use status::status;
pub use top::top;
