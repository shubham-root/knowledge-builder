//! Prometheus metrics for Knowledge Builder.
//!
//! ## Architecture
//!
//! This module uses the [`metrics`] crate as a thin facade over the global
//! metrics registry.  The [`metrics`] macros (`counter!`, `gauge!`,
//! `histogram!`) can be called from any crate that has `metrics` as a
//! dependency.  When the daemon starts, [`init_metrics`] installs a
//! Prometheus recorder globally, so all subsequent calls automatically flow
//! into it.
//!
//! When no recorder is installed (e.g. in unit tests), the macro calls are
//! no-ops — no panics, no allocations.
//!
//! ## Metric catalogue
//!
//! | Name | Kind | Unit | Description |
//! |------|------|------|-------------|
//! | [`QUEUE_DEPTH`]          | gauge     | files    | Status=queued count (point-in-time) |
//! | [`IN_FLIGHT`]            | gauge     | files    | Status=processing count (point-in-time) |
//! | [`PROCESSED_TOTAL`]      | counter   | files    | Successful completions since daemon start |
//! | [`FAILED_TOTAL`]         | counter   | files    | Failed completions since daemon start |
//! | [`PROCESSOR_DURATION`]   | histogram | seconds  | Subprocess wall-clock time |
//! | [`HASH_DURATION`]        | histogram | seconds  | SHA-256 hashing wall-clock time |
//! | [`SCAN_DURATION`]        | histogram | seconds  | Full-scan cycle wall-clock time |
//!
//! ## Usage
//!
//! ```no_run
//! // In daemon startup (before starting the HTTP server):
//! let handle = kb_ops::metrics::init_metrics().expect("metrics init");
//!
//! // Embed the handle in AppState so the /metrics endpoint can render:
//! use kb_ops::AppState;
//! // state.metrics_handle = Some(handle);
//! ```

pub use metrics_exporter_prometheus::PrometheusHandle;

use metrics_exporter_prometheus::PrometheusBuilder;

// ── Metric name constants ─────────────────────────────────────────────────────

/// Gauge: current number of files with `status = 'queued'`.
///
/// Updated on every `GET /metrics` request by querying the state store.
pub const QUEUE_DEPTH: &str = "kb_queue_depth";

/// Gauge: current number of files with `status = 'processing'`.
///
/// Updated on every `GET /metrics` request by querying the state store.
pub const IN_FLIGHT: &str = "kb_in_flight";

/// Counter: total files that completed processing successfully.
///
/// Incremented by the worker pool each time a job transitions to `done`.
pub const PROCESSED_TOTAL: &str = "kb_processed_total";

/// Counter: total files that failed processing (terminal or exhausted retries).
///
/// Incremented by the worker pool each time a job transitions to `failed`.
pub const FAILED_TOTAL: &str = "kb_failed_total";

/// Histogram: wall-clock time spent inside the processor subprocess, in seconds.
///
/// Recorded by the worker pool for every `invoke_processor` call, whether
/// it succeeds or fails.
pub const PROCESSOR_DURATION: &str = "kb_processor_duration_seconds";

/// Histogram: wall-clock time spent computing the SHA-256 hash of a file,
/// in seconds.
///
/// Recorded by the hasher for every `hash_file` call.
pub const HASH_DURATION: &str = "kb_hash_duration_seconds";

/// Histogram: wall-clock time spent in a single full vault-scan cycle,
/// in seconds.
///
/// Recorded by the periodic scanner for every complete scan pass.
pub const SCAN_DURATION: &str = "kb_scan_duration_seconds";

// ── Initialisation ────────────────────────────────────────────────────────────

/// Install the Prometheus metrics recorder as the global handler and return a
/// rendering handle.
///
/// **Call this exactly once**, before the HTTP server starts.  Subsequent
/// calls will return an error because a global recorder is already installed.
///
/// The returned [`PrometheusHandle`] is `Clone` and `Send + Sync` — store it
/// in [`crate::AppState`] and call [`PrometheusHandle::render`] in the
/// `/metrics` handler.
///
/// # Errors
///
/// Returns an error if another metrics recorder has already been installed
/// for this process (e.g. double-init in integration tests).
///
/// # Example
///
/// ```no_run
/// let handle = kb_ops::metrics::init_metrics().expect("metrics init failed");
/// println!("{}", handle.render()); // Prometheus text format
/// ```
pub fn init_metrics() -> anyhow::Result<PrometheusHandle> {
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| anyhow::anyhow!("failed to install Prometheus recorder: {e}"))?;

    tracing::info!("Prometheus metrics recorder installed");
    Ok(handle)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_name_constants_are_nonempty() {
        assert!(!QUEUE_DEPTH.is_empty());
        assert!(!IN_FLIGHT.is_empty());
        assert!(!PROCESSED_TOTAL.is_empty());
        assert!(!FAILED_TOTAL.is_empty());
        assert!(!PROCESSOR_DURATION.is_empty());
        assert!(!HASH_DURATION.is_empty());
        assert!(!SCAN_DURATION.is_empty());
    }

    #[test]
    fn metric_names_have_kb_prefix() {
        for name in &[
            QUEUE_DEPTH,
            IN_FLIGHT,
            PROCESSED_TOTAL,
            FAILED_TOTAL,
            PROCESSOR_DURATION,
            HASH_DURATION,
            SCAN_DURATION,
        ] {
            assert!(
                name.starts_with("kb_"),
                "metric '{name}' should start with 'kb_'"
            );
        }
    }

    #[test]
    fn duration_metrics_end_in_seconds() {
        for name in &[PROCESSOR_DURATION, HASH_DURATION, SCAN_DURATION] {
            assert!(
                name.ends_with("_seconds"),
                "histogram '{name}' should end with '_seconds'"
            );
        }
    }
}
