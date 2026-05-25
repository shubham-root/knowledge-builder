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
/// Iterates `outputs` in order.  For each entry, calls
/// [`kb_core::paths::validate_output`] which canonicalizes the path and
/// checks both invariants.
///
/// Errors are partitioned into two categories:
///
/// * **Fatal** — [`ValidationError::OutputOutsideVault`] and
///   [`ValidationError::OutputInsideSources`].  These indicate the
///   processor wrote (or claimed to write) somewhere it must not.  The
///   first such error aborts validation and is returned as `Err`.
///
/// * **Non-fatal** — [`ValidationError::CanonicalizeFailed`] and
///   [`ValidationError::OutputNotFile`].  These mean a claimed output
///   does not exist on disk (e.g. the agent typo'd a `file=` wikilink
///   that obsidian then resolved to nothing) or refers to a directory.
///   These are logged at WARN and the offending entry is *dropped*
///   from the returned vector — the rest are still recorded.  Missing-
///   file is a QA issue, not a security boundary; failing the entire
///   batch on it would lose the 9 valid outputs that did materialise.
///
/// # Errors
/// Returns the first fatal [`ValidationError`].
///
/// # Logging
/// Fatal violations log at `ERROR`; non-fatal ones log at `WARN`.
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
                match validation_err {
                    // Policy violations — always fatal.
                    ValidationError::OutputOutsideVault { .. }
                    | ValidationError::OutputInsideSources { .. } => {
                        tracing::error!(
                            path        = %path.display(),
                            vault_root  = %vault_root.display(),
                            sources_dir = %sources_dir.display(),
                            error       = %validation_err,
                            "processor output failed path invariant — marking failed (non-retryable)",
                        );
                        return Err(validation_err);
                    }
                    // Existence / regular-file issues — log warning and skip.
                    // Most common cause: the agent typo'd a `file=` wikilink in
                    // a property:set, the wrapper recorded `applied=true` based
                    // on obsidian's exit code, but no file actually exists at
                    // that path.  Dropping it is the right call — the other
                    // outputs in the batch are real and worth recording.
                    ValidationError::CanonicalizeFailed { .. }
                    | ValidationError::OutputNotFile { .. } => {
                        tracing::warn!(
                            path  = %path.display(),
                            error = %validation_err,
                            "claimed output not present on disk — dropping from outputs list, \
                             continuing with the remaining outputs in this batch",
                        );
                        continue;
                    }
                }
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
    fn fails_on_first_policy_violation_and_stops() {
        // Policy violations (OutsideVault / InsideSources) remain fatal
        // and abort the batch; only existence/file-type errors are
        // soft-dropped.
        let vault = make_vault();
        let sources = mk_dir(vault.path(), "sources");
        let notes = mk_dir(vault.path(), "notes");

        let good = mk(&notes, "good.md");
        let in_sources = mk(&sources, "forbidden.md");
        let good2 = mk(&notes, "good2.md");

        let outputs = vec![
            process_output(good),
            process_output(in_sources),
            process_output(good2),
        ];

        let result = validate_processor_outputs(&outputs, vault.path(), &sources);
        assert!(matches!(
            result,
            Err(ValidationError::OutputInsideSources { .. }),
        ));
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
    fn not_a_file_dropped_from_outputs() {
        // Directories and other non-file targets are now WARN-and-skip,
        // not fatal.  The result is a (smaller) Ok vec.
        let vault = make_vault();
        let sources = mk_dir(vault.path(), "sources");
        let subdir = mk_dir(vault.path(), "subdir");

        let outputs = vec![process_output(subdir)];
        let result = validate_processor_outputs(&outputs, vault.path(), &sources).unwrap();
        assert!(result.is_empty(), "non-file outputs should be dropped, not included");
    }

    #[test]
    fn nonexistent_path_dropped_from_outputs() {
        // The agent occasionally typos a wikilink in `property:set file=...`;
        // obsidian returns 0 (no-op) so the wrapper marks `applied=true` but
        // no file materialises at that path.  Drop it from outputs, keep
        // the rest.
        let vault = make_vault();
        let sources = mk_dir(vault.path(), "sources");
        let ghost = vault.path().join("does_not_exist.md");

        let outputs = vec![process_output(ghost)];
        let result = validate_processor_outputs(&outputs, vault.path(), &sources).unwrap();
        assert!(result.is_empty(), "non-existent paths should be dropped, not fatal");
    }

    #[test]
    fn good_outputs_kept_when_one_is_missing() {
        // The critical regression test: nine real outputs + one typo'd
        // wikilink should produce nine recorded outputs, not zero.
        let vault = make_vault();
        let sources = mk_dir(vault.path(), "sources");
        let notes = mk_dir(vault.path(), "notes");
        let real1 = mk(&notes, "real1.md");
        let real2 = mk(&notes, "real2.md");
        let ghost = vault.path().join("does_not_exist.md");

        let outputs = vec![
            process_output(real1),
            process_output(ghost),
            process_output(real2),
        ];
        let result =
            validate_processor_outputs(&outputs, vault.path(), &sources).unwrap();
        assert_eq!(result.len(), 2, "expected the two real outputs to survive");
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
