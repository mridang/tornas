//! Small shared helpers, each in a file named for what it is: [`size`] for byte,
//! rate and age formatting and size parsing, and [`mount`] for the disk and
//! mount-point checks the engine's disk guard relies on. Deliberately not a
//! grab-bag — anything with a real home keeps its helpers there.

pub mod mount;
mod size;

pub use size::*;
