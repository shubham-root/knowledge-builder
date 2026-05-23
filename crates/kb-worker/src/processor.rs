//! Processor subprocess invocation and JSON result parsing.
//!
//! Spawns the configured processor command, feeds it JSON on stdin, captures
//! stdout/stderr, enforces a hard timeout, and parses the last stdout line
//! as a [`ProcessResult`].
//!
//! Full implementation: T16 + T17.

use std::path::PathBuf;
use kb_core::types::{ProcessorInput, ProcessResult};

/// Invoke the processor for one job.
///
/// - `command`:      the processor executable / script path.
/// - `input`:        JSON payload written to stdin.
/// - `timeout_secs`: hard timeout; on expiry the process group receives
///                   `SIGTERM` then `SIGKILL` after a grace period.
///
/// Returns the parsed [`ProcessResult`] on success, or an error if the
/// process could not be spawned, timed out, or produced no parseable output.
pub async fn invoke(
    command:      &str,
    input:        &ProcessorInput,
    timeout_secs: u64,
    work_dir:     &PathBuf,
) -> kb_core::Result<ProcessResult> {
    // TODO (T16 + T17): spawn subprocess, write JSON stdin, read stdout,
    // apply timeout, parse last line.
    let _ = (command, input, timeout_secs, work_dir);
    tracing::debug!("processor stub — not yet implemented");
    Err(anyhow::anyhow!("processor not yet implemented"))
}
