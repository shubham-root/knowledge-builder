//! `kb-watcher` — File detection subsystem for Knowledge Builder.
//!
//! Provides:
//! - [`events`]    — `notify`/FSEvents subscription with extension + glob filtering.
//! - [`stability`] — Per-path stability state machine (poll size+mtime).
//! - [`hasher`]    — Streaming SHA-256 content hashing.
//! - [`scanner`]   — Periodic `walkdir` backstop for missed events.
//!
//! The top-level re-exports surface the types most commonly used by
//! downstream crates (`kb-cli`, integration tests) without requiring them
//! to navigate the module hierarchy.

pub mod events;
pub mod hasher;
pub mod scanner;
pub mod stability;

// ── Re-exports ────────────────────────────────────────────────────────────────

pub use events::{FileWatcher, WatchEvent, WatchEventKind, WatcherError};
