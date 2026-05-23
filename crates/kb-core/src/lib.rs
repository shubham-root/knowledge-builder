//! `kb-core` — Foundation crate for Knowledge Builder.
//!
//! Provides:
//! - Shared types (`FileRow`, `Status`, `ProcessResult`, `OutputRecord`, `Config`)
//! - Configuration loading via `figment` (TOML + env-var overrides)
//! - SQLite state store (single-writer tokio actor pattern)
//! - Path invariant utilities (`canonicalize`, `is_inside`, `validate_output`)
//! - Tracing / logging initialisation (JSON file layer + stderr human layer)
//! - SQLite schema migrations
//!
//! All other crates depend on this one; keep its public API stable.

pub mod config;
pub mod migrations;
pub mod paths;
pub mod state;
pub mod tracing_setup;
pub mod types;

/// Re-export the crate-level `Result` alias used throughout Knowledge Builder.
pub type Result<T, E = anyhow::Error> = std::result::Result<T, E>;
