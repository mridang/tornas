//! Configuration: the command line, the TOML file, and validation of both.
//!
//! - [`args`] is the clap surface and the `TORNAS_*` environment variables.
//! - [`file`] is the TOML config file.
//! - [`check`] backs `tornas config check`.
//!
//! Everything from `args` and `file` is re-exported here, so the rest of the crate
//! writes `crate::config::ServerOpts` without caring which file it lives in.

pub mod args;
pub mod check;
pub mod file;

pub use args::*;
pub use file::*;
