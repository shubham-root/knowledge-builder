//! `kb-worker` — Job execution subsystem for Knowledge Builder.
//!
//! Provides:
//! - [`pool`]      — Tokio semaphore-based bounded worker pool with atomic job claiming.
//! - [`pipeline`]  — In-process extract → integrate → sweep flow (replaces the
//!                    legacy Python subprocess processor).
//! - [`validate`]  — Output path invariant enforcement
//!                    (vault_root ⊃ output ⊄ sources_dir).

pub mod pipeline;
pub mod pool;
pub mod validate;

// Re-export output validation at the crate root for convenience.
pub use validate::{validate_processor_outputs, ValidationError};

// Re-export the worker pool and process_job placeholder at the crate root.
pub use pool::{process_job, WorkerPool};

// Re-export the in-process pipeline entry point.
pub use pipeline::run_pipeline;
