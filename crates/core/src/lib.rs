//! Shared primitives for `onedrived` and `onedrive-tui`.
//!
//! Everything in here is deliberately free of rclone, D-Bus and FUSE concerns so
//! that the CLI can link it without pulling the daemon's dependency graph.

pub mod code;
pub mod config;
pub mod ipc;
pub mod log;
pub mod paths;
pub mod state;
pub mod unit;
pub mod units;

pub use code::{Code, CodedError};
pub use units::{Dur, Size};

/// Version of the on-disk formats (config + database) this build speaks.
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// Version reported by `onedrive-tui status` and written into the state db.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
