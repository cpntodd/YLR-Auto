//! Filing rules, MakeMKV monitoring and the TMDb client for YLR-Auto, with no GUI code.
//!
//! Functions here block on the file system, subprocesses or the network, so call
//! them off the GUI thread.

pub mod autosort;
pub mod monitor;
pub mod tmdb;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
