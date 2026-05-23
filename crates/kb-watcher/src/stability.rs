//! Per-path stability tracker.
//!
//! After a file-system event arrives for a candidate path, the stability
//! tracker polls `(size, mtime)` every 500 ms until both values are
//! unchanged for `stability_ms` milliseconds.  Only then does it hand the
//! path to the hasher.
//!
//! Full implementation: T10.

use std::path::PathBuf;

/// Outcome of a stability check.
#[derive(Debug)]
pub enum StabilityOutcome {
    /// File is stable; proceed to hash.
    Stable { path: PathBuf, size: u64, mtime_ns: i64, inode: u64 },
    /// File disappeared before it became stable.
    Disappeared,
    /// Stability window exceeded maximum wait time; treated as disappeared.
    Timeout,
}

/// Poll `path` until stable (or gone/timeout).
///
/// - `stability_ms`: milliseconds both `size` and `mtime` must be
///   unchanged.
/// - `max_wait_ms`:  hard ceiling (typically `5 * stability_ms`).
pub async fn wait_for_stable(
    path:          PathBuf,
    stability_ms:  u64,
    max_wait_ms:   u64,
) -> StabilityOutcome {
    // TODO (T10): implement polling loop.
    let _ = (stability_ms, max_wait_ms);
    tracing::debug!(?path, "stability check stub — not yet implemented");
    StabilityOutcome::Timeout
}
