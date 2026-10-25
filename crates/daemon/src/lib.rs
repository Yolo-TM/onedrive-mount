//! `onedrive-rcd` internals.
//!
//! Exposed as a library as well as a binary so the sync-safety matrix can drive
//! the *real* bisync, reconciliation and snapshot code rather than a
//! reimplementation of it. That matters more here than anywhere else in the
//! project: the matrix is the gate that decides whether the sync engine is
//! allowed anywhere near a password database, and a matrix that tests a
//! lookalike proves nothing.

pub mod app;
pub mod bisync;
pub mod configwatch;
pub mod index;
pub mod lifecycle;
pub mod lock;
pub mod logging;
pub mod mount;
pub mod mountloop;
pub mod notify;
pub mod odfs;
pub mod outbox;
pub mod pins;
pub mod pinwatch;
pub mod rclone;
pub mod reconcile;
pub mod resolve;
pub mod sdnotify;
pub mod server;
pub mod snapshot;
