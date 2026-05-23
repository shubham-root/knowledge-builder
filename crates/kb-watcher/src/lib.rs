//! `kb-watcher` — File detection subsystem for Knowledge Builder.
//!
//! Provides:
//! - [`events`]    — `notify`/FSEvents subscription with extension + glob filtering.
//! - [`stability`] — Per-path stability state machine (poll size+mtime).
//! - [`hasher`]    — Streaming SHA-256 content hashing.
//! - [`scanner`]   — Periodic `walkdir` backstop for missed events.
//! - [`pipeline`]  — End-to-end detection pipeline that wires all of the above
//!                   together with the [`kb_core::StateStore`].

pub mod events;
pub mod hasher;
pub mod pipeline;
pub mod scanner;
pub mod stability;

// Re-export the primary stability API at the crate root for convenience.
pub use stability::{StableFile, StabilityTracker};

// Re-export the pipeline API so callers need only one import.
pub use pipeline::{DetectionPipeline, PipelineError};

// Re-export scanner API so callers can start the periodic backstop scanner
// and/or invoke a one-shot scan from the `kb scan` CLI command.
pub use scanner::{PeriodicScanner, ScanError, passes_filter, scan_once};

// Re-export CancellationToken so callers that only depend on kb-watcher
// can use the shutdown mechanism without adding tokio-util themselves.
pub use tokio_util::sync::CancellationToken;
