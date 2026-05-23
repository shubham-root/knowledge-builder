//! `kb-core` — Foundation crate for Knowledge Builder.
//!
//! Provides:
//! - Shared types ([`FileRow`], [`Status`], [`ProcessResult`], [`OutputRecord`],
//!   [`AuditEvent`], [`ProcessorInput`], [`ProcessOutput`], [`EnqueueOutcome`],
//!   [`Stats`]) — all re-exported at the crate root for ergonomic imports.
//! - Configuration loading via `figment` (TOML + `KB__` env-var overrides).
//! - SQLite state store (single-writer tokio actor pattern).
//! - Path invariant utilities ([`paths::canonicalize`], [`paths::is_inside`],
//!   [`paths::validate_output`]).
//! - Tracing / logging initialisation (JSON file layer + stderr human layer).
//! - SQLite schema migrations (idempotent, append-only).
//!
//! All other crates depend on this one; keep the public API stable.

pub mod config;
pub mod lock;
pub mod logging;
pub mod migrations;
pub mod paths;
pub mod state;
pub mod tracing_setup;
pub mod types;

// ── Crate-level re-exports ────────────────────────────────────────────────────

// Lock
pub use lock::{DaemonLock, LockError};

// Logging
pub use logging::{init_logging, LogGuard};
//
// Import the types that every downstream crate will use most often directly
// from `kb_core::` rather than `kb_core::types::`.

pub use types::{
    // Status state machine
    Status,

    // Database row types
    AuditEvent,
    FileRow,
    OutputRecord,

    // Processor contract
    ProcessOutput,
    ProcessResult,
    ProcessorInput,

    // Queue outcome
    EnqueueOutcome,

    // Aggregate stats
    Stats,

    // Well-known event kind string constants
    event_kind,
};

/// Crate-level `Result` alias: all fallible functions return
/// `kb_core::Result<T>` which is `anyhow::Result<T>` under the hood.
pub type Result<T, E = anyhow::Error> = std::result::Result<T, E>;

// ── Config re-exports ─────────────────────────────────────────────────────────
//
// The most-used config surface is available directly from `kb_core::` so
// callers can write `kb_core::Config::load()` without the full module path.

// ── State store re-export ────────────────────────────────────────────────────
//
// The actor handle is available as `kb_core::StateStore` so downstream crates
// need only one import.
pub use state::StateStore;

// ── Migrations re-export ─────────────────────────────────────────────────────
pub use migrations::{db_open, run_migrations};

pub use config::{
    Config,
    ConfigError,
    ConfigErrors,
    PathsConfig,
    WatchConfig,
    WorkerConfig,
    ProcessorConfig,
    OpsConfig,
    load_raw,
};
