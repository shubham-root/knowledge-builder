//! `kb-worker` — Job execution subsystem for Knowledge Builder.
//!
//! Provides:
//! - [`pool`]      — Tokio semaphore-based bounded worker pool with atomic job claiming.
//! - [`processor`] — Subprocess spawning, JSON stdin/stdout, timeout + kill.
//! - [`parser`]    — JSON result parser: extracts the last stdout line and deserialises it.
//! - [`validate`]  — Output path invariant enforcement (vault_root ⊃ output ⊄ sources_dir).

pub mod parser;
pub mod pool;
pub mod processor;
pub mod validate;

// Re-export the two most-used items at the crate root for convenience.
pub use parser::{parse_processor_output, ParseError};

// Re-export the processor contract types at the crate root for convenience.
pub use processor::{invoke_processor, ProcessorError, ProcessorOutput};

// Re-export output validation at the crate root for convenience.
pub use validate::{validate_processor_outputs, ValidationError};
