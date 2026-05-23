//! Output path invariant enforcement — worker-layer adapter.
//!
//! Delegates all path checking to [`kb_core::paths::validate_output`] and
//! wraps its errors with worker-specific context and structured logging.
//!
//! ## Invariant
//! Every output path returned by the processor must satisfy:
//! - `output ⊂ vault_root` (inside the vault)
//! - `output ⊄ sources_dir` (outside the sources directory)
//! - `output` refers to a regular file (not a directory or symlink target)
//!
//! A violation is always a **processor bug** — never a transient failure.
//! [`ValidationError::is_validation_retryable`] always returns `false`.

use std::path::{Path, PathBuf};

use kb_core::paths::OutputError;
use kb_core::types::ProcessOutput;
use thiserror::Error;

// ──────────────────────────────────────────────────────────────────────────────
// Error type
// ──────────────────────────────────────────────────────────────────────────────

/// Errors produced by [`validate_processor_outputs`].
///
/// Each variant mirrors one of the [`OutputError`] cases from `kb_core::paths`,
/// but carries copies of the relevant paths for structured logging and
/// downstream audit records.
#[derive(Debug, Error)]
pub enum ValidationError {
    /// The output path is not inside `vault_root`.
    #[error(
        "output path is outside the vault: path={path:?}, vault_root={vault_root:?}"
    )]
    OutputOutsideVault {
        path: PathBuf,
        vault_root: PathBuf,
    },

    /// The output path is inside `sources_dir`, which would trigger a
    /// reprocessing loop.
    #[error(
        "output path is inside sources directory: path={path:?}, sources_dir={sources_dir:?}"
    )]
    OutputInsideSources {
        path: PathBuf,
        sources_dir: PathBuf,
    },

    /// The canonicalized path does not refer to a regular file.
    #[error("output path is not a regular file: path={path:?}")]
    OutputNotFile { path: PathBuf },

    /// `std::fs::canonicalize` failed for the output path.
    #[error("failed to canonicalize output path: path={path:?}, error={error}")]
    CanonicalizeFailed { path: PathBuf, error: String },
}

impl ValidationError {
    /// Validation failures are **always processor bugs** — never transient.
    ///
    /// Always returns `false`.  The caller should mark the job as non-retryable
    /// failed rather than re-queuing it.
    #[inline]
    pub fn is_validation_retryable(&self) -> bool {
        false
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Public API
// ──────────────────────────────────────────────────────────────────────────────

/// Validate every output path returned by a processor invocation.
///
/// Iterates `outputs` in order.  For each entry:
///
/// 1. Calls [`kb_core::paths::validate_output`] which canonicalizes the path
///    and checks both invariants.
/// 2. On success, appends the canonical `PathBuf` to the result vector.
/// 3. On **first** failure, logs a [`tracing::error!`] and returns immediately
///    (**fail-fast** — subsequent paths are not checked).
///
/// # Errors
/// Returns the first [`ValidationError`] encountered.
///
/// # Logging
/// Every violation is logged at `ERROR` level with the offending path and the
/// relevant boundary path (`vault_root` or `sources_dir`).
pub fn validate_processor_outputs(
    outputs: &[ProcessOutput],
    vault_root: &Path,
    sources_dir: &Path,
) -> Result<Vec<PathBuf>, ValidationError> {
    let mut canonical_paths = Vec::with_capacity(outputs.len());

    for output in outputs {
        let path = &output.path;

        match kb_core::paths::validate_output(path, vault_root, sources_dir) {
            Ok(canon) => {
                canonical_paths.push(canon);
            }
            Err(e) => {
                let validation_err = map_output_error(e, path, vault_root, sources_dir);
                tracing::error!(
                    path          = %path.display(),
                    vault_root    = %vault_root.display(),
                    sources_dir   = %sources_dir.display(),
                    error         = %validation_err,
                    "processor output failed invariant check — non-retryable"
                );
                return Err(validation_err);
            }
        }
    }

    Ok(canonical_paths)
}

// ──────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Convert a [`kb_core::paths::OutputError`] into a [`ValidationError`],
/// supplementing each variant with the worker-level context paths.
fn map_output_error(
    err: OutputError,
    original_path: &Path,
    vault_root: &Path,
    sources_dir: &Path,
) -> ValidationError {
    match err {
        OutputError::OutsideVault { path, .. } => ValidationError::OutputOutsideVault {
            path,
            vault_root: vault_root.to_path_buf(),
        },
        OutputError::InsideSources { path, .. } => ValidationError::OutputInsideSources {
            path,
            sources_dir: sources_dir.to_path_buf(),
        },
        OutputError::NotRegularFile { path } => ValidationError::OutputNotFile { path },
        OutputError::CanonicalizeFailed { path, source } => ValidationError::CanonicalizeFailed {
            path,
            error: source.to_string(),
        },
        // Defensive fallback: if kb-core adds new variants in the future,
        // map them to CanonicalizeFailed with a descriptive string so the
        // worker still fails cleanly rather than panicking.
        #[allow(unreachable_patterns)]
        _ => ValidationError::CanonicalizeFailed {
            path: original_path.to_path_buf(),
            error: format!("unexpected error: {err}"),
        },
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_vault() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn mk(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, b"data").unwrap();
        p
    }

    fn mk_dir(parent: &Path, name: &str) -> PathBuf {
        let p = parent.join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn process_output(path: PathBuf) -> ProcessOutput {
        ProcessOutput {
            path,
            kind: "markdown".to_string(),
            bytes: 4,
        }
    }

    // ------------------------------------------------------------------
    // Happy-path
    // ------------------------------------------------------------------

    #[test]
    fn valid_outputs_returned_as_canonical_vec() {
        let vault = make_vault();
        let sources = mk_dir(vault.path(), "sources");
        let notes = mk_dir(vault.path(), "notes");
        let f1 = mk(&notes, "note1.md");
        let f2 = mk(&notes, "note2.md");

        let outputs = vec![process_output(f1.clone()), process_output(f2.clone())];

        let result =
            validate_processor_outputs(&outputs, vault.path(), &sources).unwrap();

        assert_eq!(result.len(), 2);
        // Canonical paths should have resolved any symlinks (e.g. /tmp → /private/tmp on macOS)
        for p in &result {
            assert!(p.exists());
        }
    }

    #[test]
    fn empty_outputs_returns_empty_vec() {
        let vault = make_vault();
        let sources = mk_dir(vault.path(), "sources");

        let result = validate_processor_outputs(&[], vault.path(), &sources).unwrap();
        assert!(result.is_empty());
    }

    // ------------------------------------------------------------------
    // Fail-fast: first error stops iteration
    // ------------------------------------------------------------------

    #[test]
    fn fails_on_first_bad_output_and_stops() {
        let vault = make_vault();
        let sources = mk_dir(vault.path(), "sources");
        let notes = mk_dir(vault.path(), "notes");

        // good, bad, good — should fail on the bad one, never touch the third
        let good = mk(&notes, "good.md");
        let outside = {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join("outside.md");
            fs::write(&p, b"x").unwrap();
            // We need to keep tmp alive so the file exists for canonicalize
            // but the path is outside the vault — it won't reach the IS-file check
            // because OutsideVault fires first.
            // Use a path that definitely doesn't exist inside vault:
            p.to_path_buf()
            // tmp drops here and removes the dir — that's fine; we want to test
            // the case where the path is simply outside vault_root, which is
            // caught before the file-exists check.
        };
        let good2 = mk(&notes, "good2.md");

        let outputs = vec![
            process_output(good),
            process_output(outside),
            process_output(good2),
        ];

        // The second entry (outside vault) should cause an error.
        // Because tmp was dropped the file no longer exists → CanonicalizeFailed.
        // Either way it must be an Err.
        let result = validate_processor_outputs(&outputs, vault.path(), &sources);
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // Error variant mapping
    // ------------------------------------------------------------------

    #[test]
    fn outside_vault_maps_correctly() {
        let vault = make_vault();
        let sources = mk_dir(vault.path(), "sources");

        // Create a file OUTSIDE the vault
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = mk(outside_dir.path(), "out.md");

        let outputs = vec![process_output(outside_file.clone())];
        let err =
            validate_processor_outputs(&outputs, vault.path(), &sources).unwrap_err();

        assert!(matches!(err, ValidationError::OutputOutsideVault { .. }));
        assert!(!err.is_validation_retryable());
    }

    #[test]
    fn inside_sources_maps_correctly() {
        let vault = make_vault();
        let sources = mk_dir(vault.path(), "sources");
        let file_in_sources = mk(&sources, "doc.md");

        let outputs = vec![process_output(file_in_sources)];
        let err =
            validate_processor_outputs(&outputs, vault.path(), &sources).unwrap_err();

        assert!(matches!(err, ValidationError::OutputInsideSources { .. }));
        assert!(!err.is_validation_retryable());
    }

    #[test]
    fn not_a_file_maps_correctly() {
        let vault = make_vault();
        let sources = mk_dir(vault.path(), "sources");
        // A directory inside the vault (not a regular file)
        let subdir = mk_dir(vault.path(), "subdir");

        let outputs = vec![process_output(subdir)];
        let err =
            validate_processor_outputs(&outputs, vault.path(), &sources).unwrap_err();

        assert!(matches!(err, ValidationError::OutputNotFile { .. }));
        assert!(!err.is_validation_retryable());
    }

    #[test]
    fn nonexistent_path_maps_to_canonicalize_failed() {
        let vault = make_vault();
        let sources = mk_dir(vault.path(), "sources");
        let ghost = vault.path().join("does_not_exist.md");

        let outputs = vec![process_output(ghost)];
        let err =
            validate_processor_outputs(&outputs, vault.path(), &sources).unwrap_err();

        assert!(matches!(err, ValidationError::CanonicalizeFailed { .. }));
        assert!(!err.is_validation_retryable());
    }

    // ------------------------------------------------------------------
    // is_validation_retryable always false
    // ------------------------------------------------------------------

    #[test]
    fn all_variants_are_not_retryable() {
        let p = PathBuf::from("/some/path");
        let variants = vec![
            ValidationError::OutputOutsideVault {
                path: p.clone(),
                vault_root: p.clone(),
            },
            ValidationError::OutputInsideSources {
                path: p.clone(),
                sources_dir: p.clone(),
            },
            ValidationError::OutputNotFile { path: p.clone() },
            ValidationError::CanonicalizeFailed {
                path: p.clone(),
                error: "test".to_string(),
            },
        ];
        for v in &variants {
            assert!(!v.is_validation_retryable(), "{v:?} should not be retryable");
        }
    }
}
