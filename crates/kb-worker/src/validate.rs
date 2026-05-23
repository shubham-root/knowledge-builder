//! Output path invariant enforcement.
//!
//! Every path returned by the processor is routed through [`validate_outputs`]
//! before any DB records are written.  A single violation aborts the job as a
//! non-retryable failure.
//!
//! Delegates to [`kb_core::paths::validate_output`] — this module is the
//! worker-layer adapter that formats errors and collects results.
//!
//! Full implementation: T18.

use std::path::{Path, PathBuf};
use kb_core::types::ProcessOutput;

/// Validate every output entry returned by the processor.
///
/// Returns a `Vec<PathBuf>` of canonicalized output paths on success, or the
/// first [`kb_core::paths::PathError`] encountered.
pub fn validate_outputs(
    outputs:     &[ProcessOutput],
    vault_root:  &Path,
    sources_dir: &Path,
) -> kb_core::Result<Vec<PathBuf>> {
    let mut canonical_paths = Vec::with_capacity(outputs.len());
    for output in outputs {
        let canon = kb_core::paths::validate_output(&output.path, vault_root, sources_dir)
            .map_err(|e| anyhow::anyhow!("invalid_output_path: {e}"))?;
        canonical_paths.push(canon);
    }
    Ok(canonical_paths)
}
