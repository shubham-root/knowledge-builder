//! `kb-watcher` — File detection subsystem for Knowledge Builder.
//!
//! Provides:
//! - [`events`]    — `notify`/FSEvents subscription with extension + glob filtering.
//! - [`stability`] — Per-path stability state machine (poll size+mtime).
//! - [`hasher`]    — Streaming SHA-256 content hashing.
//! - [`scanner`]   — Periodic `walkdir` backstop for missed events.

pub mod events;
pub mod hasher;
pub mod scanner;
pub mod stability;

// Re-export the primary stability API at the crate root for convenience.
pub use stability::{StableFile, StabilityTracker};
