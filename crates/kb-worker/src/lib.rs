//! `kb-worker` — Job execution subsystem for Knowledge Builder.
//!
//! Provides:
//! - [`pool`]      — Tokio semaphore-based bounded worker pool with atomic job claiming.
//! - [`processor`] — Subprocess spawning, JSON stdin/stdout, timeout + kill.
//! - [`validate`]  — Output path invariant enforcement (vault_root ⊃ output ⊄ sources_dir).

pub mod pool;
pub mod processor;
pub mod validate;

// Re-export the processor contract types at the crate root for convenience.
pub use processor::{invoke_processor, ProcessorError, ProcessorOutput};
