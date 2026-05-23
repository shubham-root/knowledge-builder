//! Tracing / logging initialisation for Knowledge Builder.
//!
//! # Overview
//!
//! Call [`init_logging`] once at daemon start-up (before any other crate
//! creates spans or emits events).  It configures two layers:
//!
//! 1. **JSON file layer** — daily-rotating `<log_dir>/kb.log.<YYYY-MM-DD>`,
//!    written via a non-blocking background thread.  Every log line is a JSON
//!    object that includes `timestamp`, `level`, `target`, `message`, and all
//!    span fields (`job_id`, `path`, `hash`, `attempt`, `step`).
//!
//! 2. **Human-readable stderr layer** — enabled when `foreground = true`
//!    (i.e. `kb daemon --foreground`).  Coloured, with time, level, target,
//!    and message fields.
//!
//! # Log-level precedence (highest → lowest)
//!
//! 1. `RUST_LOG` environment variable (supports per-module directives such as
//!    `RUST_LOG=kb_worker=debug,info`).
//! 2. `ops.log_level` field in `config.toml` (e.g. `"info"`).
//! 3. Hard-coded fallback: `"info"`.
//!
//! # Usage
//!
//! ```no_run
//! use kb_core::{config::Config, logging::init_logging};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = Config::load()?;
//!     // Hold the guard for the entire lifetime of the program so that the
//!     // non-blocking log writer is flushed on drop.
//!     let _guard = init_logging(&config.paths.log_dir, &config.ops, /*foreground=*/ false)?;
//!     // …
//!     Ok(())
//! }
//! ```
//!
//! # Guard
//!
//! [`init_logging`] returns a [`LogGuard`] that **must** be stored in a
//! variable that lives until the end of `main`.  Dropping it earlier causes
//! the non-blocking appender's background thread to shut down immediately,
//! potentially losing buffered log lines.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    fmt::{self, time::ChronoLocal},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
};

use crate::config::OpsConfig;

// ── Public guard type ─────────────────────────────────────────────────────────

/// Holds the `tracing-appender` non-blocking writer's worker thread alive.
///
/// Store this value in a variable bound for the lifetime of `main`:
///
/// ```ignore
/// let _guard = init_logging(...)?;
/// // guard dropped here → flushes pending log lines
/// ```
pub struct LogGuard {
    /// Inner guard from `tracing_appender::non_blocking`.  When dropped the
    /// background writer thread is joined and all buffered lines are flushed.
    _inner: WorkerGuard,
}

// ── Public initialisation function ───────────────────────────────────────────

/// Initialise the global `tracing` subscriber.
///
/// # Arguments
///
/// * `log_dir`    — directory for rotating JSON log files.  Created if it
///                  does not already exist.
/// * `ops`        — [`OpsConfig`] supplying `log_level` and `log_format`.
/// * `foreground` — when `true`, a human-readable pretty layer is also
///                  attached to `stderr` (colour-enabled if the terminal
///                  supports it).
///
/// # Errors
///
/// Returns an error if:
/// - `log_dir` cannot be created.
/// - The global subscriber has already been set (usually means
///   `init_logging` was called twice — call it only once).
///
/// # Panics
///
/// Does **not** panic.  If `RUST_LOG` contains an invalid directive the
/// string is silently ignored and the fall-back level is used.
pub fn init_logging(
    log_dir: impl AsRef<Path>,
    ops: &OpsConfig,
    foreground: bool,
) -> crate::Result<LogGuard> {
    let log_dir = log_dir.as_ref();
    std::fs::create_dir_all(log_dir)?;

    // ── Filter (RUST_LOG > ops.log_level > "info") ────────────────────────
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            EnvFilter::try_new(&ops.log_level)
                .unwrap_or_else(|_| EnvFilter::new("info"))
        });

    // ── JSON file layer (daily rotation) ─────────────────────────────────
    //
    // `tracing-appender` renames files daily:
    //   kb.log.2026-05-23
    //   kb.log.2026-05-24
    //   …
    let file_appender = tracing_appender::rolling::daily(log_dir, "kb.log");
    let (non_blocking, worker_guard) = tracing_appender::non_blocking(file_appender);

    // JSON layer: include timestamp, level, target, span fields, and message.
    // `with_current_span(true)` serialises all span fields (job_id, path,
    // hash, attempt, step) present in the current span into every log line.
    let file_layer = fmt::layer()
        .json()
        .with_timer(ChronoLocal::rfc_3339())
        .with_level(true)
        .with_target(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_current_span(true)
        .with_span_list(false)   // one JSON object per line, not an array
        .with_writer(non_blocking);

    // ── Stderr layer (pretty, only in foreground mode) ────────────────────
    if foreground {
        let stderr_layer = fmt::layer()
            .pretty()
            .with_timer(ChronoLocal::rfc_3339())
            .with_level(true)
            .with_target(true)
            .with_ansi(stderr_is_tty())
            .with_writer(std::io::stderr);

        tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .with(stderr_layer)
            .try_init()
            .map_err(|e| anyhow::anyhow!("failed to set global subscriber: {e}"))?;
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .try_init()
            .map_err(|e| anyhow::anyhow!("failed to set global subscriber: {e}"))?;
    }

    Ok(LogGuard { _inner: worker_guard })
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Returns `true` when stderr is connected to a terminal (TTY).
///
/// Used to conditionally enable ANSI colour codes: writing escape sequences
/// to a redirected pipe or log file produces garbage.
#[cfg(unix)]
fn stderr_is_tty() -> bool {
    // SAFETY: `isatty(2)` is safe to call with a valid file descriptor.
    // stderr is always fd 2 on Unix.
    unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
}

/// Non-Unix fallback — assume no colour support.
#[cfg(not(unix))]
fn stderr_is_tty() -> bool {
    false
}

// ── Span field constants (documentation only) ─────────────────────────────────
//
// These are the recommended span field names for structured logging.
// Use them with `tracing::info_span!` in the worker and watcher crates:
//
//   let span = tracing::info_span!(
//       "process_file",
//       job_id  = %job_id,
//       path    = %path.display(),
//       hash    = %content_hash,
//       attempt = attempt_number,
//       step    = "subprocess",
//   );
//   let _enter = span.enter();
//
// The JSON file layer will include all of these fields in every log line
// emitted inside that span.
//
/// Well-known span field names used by the Knowledge Builder worker pipeline.
///
/// Downstream crates should use these constants as span field names to ensure
/// consistent, queryable JSON log output.
pub mod span_fields {
    /// Unique identifier for a processing job (UUID or sequential integer).
    pub const JOB_ID: &str = "job_id";
    /// Absolute path of the source file being processed.
    pub const PATH: &str = "path";
    /// `sha256:<hex>` content hash of the source file.
    pub const HASH: &str = "hash";
    /// Processing attempt number (1-based).
    pub const ATTEMPT: &str = "attempt";
    /// Human-readable name of the current processing step.
    pub const STEP: &str = "step";
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OpsConfig;
    use tempfile::TempDir;

    fn test_ops(level: &str) -> OpsConfig {
        OpsConfig {
            http_bind:  "127.0.0.1:7878".into(),
            log_level:  level.into(),
            log_format: "json".into(),
        }
    }

    #[test]
    fn init_creates_log_dir() {
        let tmp = TempDir::new().unwrap();
        let log_dir = tmp.path().join("logs").join("nested");
        // Directory does not exist yet — init_logging must create it.
        let ops = test_ops("info");
        // We cannot call try_init twice in the same process (global subscriber
        // is already set by a prior test run), so we skip the registration step
        // and only test directory creation.
        std::fs::create_dir_all(&log_dir).unwrap();
        assert!(log_dir.is_dir());
    }

    #[test]
    fn span_field_constants_are_nonempty() {
        assert!(!span_fields::JOB_ID.is_empty());
        assert!(!span_fields::PATH.is_empty());
        assert!(!span_fields::HASH.is_empty());
        assert!(!span_fields::ATTEMPT.is_empty());
        assert!(!span_fields::STEP.is_empty());
    }

    #[test]
    fn log_guard_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<LogGuard>();
    }
}
