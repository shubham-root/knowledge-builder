//! Path invariant utilities for Knowledge Builder.
//!
//! This module is the **SINGLE SOURCE OF TRUTH** for the hard structural guarantee:
//!
//! > Every processor output MUST reside inside `vault_root` and MUST NOT
//! > reside inside `sources_dir`.
//!
//! Violating this invariant would cause an infinite reprocessing loop — a
//! processor output inside `sources_dir` would be detected as a new source
//! file, re-enqueued, processed again, and so on indefinitely.
//!
//! # Usage
//!
//! All path validation in the codebase **must** route through [`validate_output`].
//! The worker crate calls it for every path in a processor result before recording
//! any output.  The config validator calls [`safe_canonicalize`] to resolve `~`
//! and symlinks before storing config paths.
//!
//! # Canonicalization contract
//!
//! Both [`safe_canonicalize`] and [`validate_output`] rely on
//! [`std::fs::canonicalize`], which:
//! - Resolves all `..` and `.` components.
//! - Follows every symlink in the path (recursively).
//! - Returns an absolute path.
//! - **Requires all path components to exist on disk** (unlike `Path::clean`).
//!
//! The last point means that symlink escapes — where a symlink inside the vault
//! points to a file outside it — are caught automatically: the canonical form
//! of the symlink resolves to the real location, which will be outside the vault.

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

// ── Error types ───────────────────────────────────────────────────────────────

/// Low-level error returned by [`safe_canonicalize`].
///
/// Provides a specific, actionable diagnostic for each failure mode rather than
/// surfacing a bare [`std::io::Error`].
#[derive(Debug, Error)]
pub enum PathError {
    /// The path (or one of its components / symlink targets) does not exist.
    ///
    /// Most common cause: the file was deleted between detection and processing,
    /// or the config references a directory that has not been created yet.
    #[error("path not found: {path}")]
    NotFound { path: PathBuf },

    /// The current process lacks read or execute permission on the path or one
    /// of its ancestor directories.
    #[error("permission denied: {path}")]
    PermissionDenied { path: PathBuf },

    /// Any other I/O error encountered during [`std::fs::canonicalize`].
    ///
    /// This is the catch-all variant for OS-level errors that do not have a
    /// dedicated variant (e.g., `ELOOP` from a symlink cycle, `EIO`, `ENOMEM`).
    #[error("I/O error while resolving {path}: {source}")]
    IoError {
        path:   PathBuf,
        source: io::Error,
    },
}

/// High-level error returned by [`validate_output`].
///
/// Each variant pinpoints exactly which part of the core invariant was violated,
/// providing full context for both logging and non-retryable failure marking.
#[derive(Debug, Error)]
pub enum OutputError {
    /// The output path (after full canonicalization) does not reside under
    /// `vault_root`.  Accepting this output would silently write files outside
    /// the user's vault, which is both a data-loss risk and a security concern.
    #[error(
        "output path is outside vault_root\n  \
         path:       {path}\n  \
         vault_root: {vault_root}"
    )]
    OutsideVault {
        /// The canonical form of the rejected output path.
        path:       PathBuf,
        /// The canonical form of the configured vault root.
        vault_root: PathBuf,
    },

    /// The output path (after full canonicalization) resides under `sources_dir`.
    /// Accepting this output would cause the file to be re-enqueued, processed
    /// again, and trigger an infinite reprocessing loop.
    #[error(
        "output path is inside sources_dir (infinite-loop hazard)\n  \
         path:        {path}\n  \
         sources_dir: {sources_dir}"
    )]
    InsideSources {
        /// The canonical form of the rejected output path.
        path:        PathBuf,
        /// The canonical form of the configured sources directory.
        sources_dir: PathBuf,
    },

    /// The path exists but is not a regular file.  The processor must write
    /// concrete files, not directories, sockets, FIFOs, or device nodes.
    #[error("output path is not a regular file: {path}")]
    NotRegularFile {
        /// The canonical form of the rejected path.
        path: PathBuf,
    },

    /// Canonicalization of one of the input paths (`path`, `vault_root`, or
    /// `sources_dir`) failed.  The `source` field carries the specific
    /// [`PathError`] from [`safe_canonicalize`].
    #[error("failed to canonicalize {path}: {source}")]
    CanonicalizeFailed {
        /// The raw (pre-canonicalization) path that failed to resolve.
        path:   PathBuf,
        /// The underlying canonicalization error.
        source: PathError,
    },
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Resolve `p` to a canonical absolute path, following all symlinks.
///
/// This is a thin, ergonomic wrapper around [`std::fs::canonicalize`] that maps
/// the generic [`std::io::Error`] to the specific [`PathError`] variant that
/// best describes the failure.
///
/// # Errors
///
/// - [`PathError::NotFound`] — path or a component does not exist.
/// - [`PathError::PermissionDenied`] — insufficient permissions.
/// - [`PathError::IoError`] — any other OS-level failure (symlink cycle, etc.).
///
/// # Example
///
/// ```no_run
/// use kb_core::paths::safe_canonicalize;
/// use std::path::Path;
///
/// let canon = safe_canonicalize(Path::new("/Users/me/Vault")).unwrap();
/// ```
pub fn safe_canonicalize(p: &Path) -> Result<PathBuf, PathError> {
    std::fs::canonicalize(p).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => PathError::NotFound {
            path: p.to_path_buf(),
        },
        io::ErrorKind::PermissionDenied => PathError::PermissionDenied {
            path: p.to_path_buf(),
        },
        _ => PathError::IoError {
            path:   p.to_path_buf(),
            source: e,
        },
    })
}

/// Return `true` if `child` resides inside (or is equal to) `parent`.
///
/// Uses Rust's component-wise [`Path::starts_with`], so `/a/bc` does **not**
/// start with `/a/b` — only a true path prefix (or exact match) qualifies.
///
/// # Precondition
///
/// **Both paths must already be canonicalized** (via [`safe_canonicalize`] or
/// equivalent).  Passing non-canonical paths — ones that contain `..`, `.`,
/// or unresolved symlinks — may produce incorrect results.
///
/// # Examples
///
/// ```
/// use kb_core::paths::is_inside;
/// use std::path::Path;
///
/// assert!( is_inside(Path::new("/a/b/c"), Path::new("/a/b")));  // child of parent
/// assert!( is_inside(Path::new("/a/b"),   Path::new("/a/b")));  // exact match
/// assert!(!is_inside(Path::new("/a/bc"),  Path::new("/a/b")));  // sibling, not child
/// assert!(!is_inside(Path::new("/a"),     Path::new("/a/b")));  // parent, not child
/// ```
#[inline]
pub fn is_inside(child: &Path, parent: &Path) -> bool {
    // `Path::starts_with` is component-wise: it checks that every component of
    // `parent` appears at the start of `child`.  An exact match returns `true`,
    // matching the semantics of "child is inside (or equal to) parent".
    child.starts_with(parent)
}

/// Validate that `path` satisfies the core output-path invariant.
///
/// This is the **single enforcement gate** that every output path returned by
/// a processor subprocess must pass.  Callers **must not** bypass it.
///
/// # Invariant checked
///
/// ```text
/// forall output:
///   canonical(output)  starts_with  canonical(vault_root)     [output is inside vault]
///   canonical(output) !starts_with  canonical(sources_dir)    [output is not in sources]
///   canonical(output) is a regular file
/// ```
///
/// # Validation order
///
/// Steps are performed in this order so that the most informative error is
/// returned first:
///
/// 1. Canonicalize `path` (catches non-existent paths early).
/// 2. Canonicalize `vault_root` and `sources_dir`.
/// 3. Check `canonical_path` starts with `canonical_vault` → [`OutputError::OutsideVault`].
/// 4. Check `canonical_path` does NOT start with `canonical_sources` → [`OutputError::InsideSources`].
/// 5. Check `canonical_path` is a regular file → [`OutputError::NotRegularFile`].
/// 6. Return `canonical_path` on success.
///
/// # Symlink safety
///
/// Because all three inputs are canonicalized, a symlink inside the vault that
/// points outside it will be resolved to its real location and caught at step 3.
/// No additional symlink-checking logic is required.
///
/// # Arguments
///
/// - `path` — the output path as returned by the processor. May be
///   non-canonical; may contain `..` or symlinks.
/// - `vault_root` — the configured vault root directory.  Also canonicalized
///   internally, so it need not be canonical on entry (though it normally will
///   be after config startup validation).
/// - `sources_dir` — the configured sources directory.  Same canonicalization
///   note as `vault_root`.
///
/// # Returns
///
/// `Ok(canonical_path)` if all checks pass, or the first [`OutputError`]
/// encountered.
pub fn validate_output(
    path:        &Path,
    vault_root:  &Path,
    sources_dir: &Path,
) -> Result<PathBuf, OutputError> {
    // ── Step a: canonicalize the output path ─────────────────────────────────
    let canon_path = safe_canonicalize(path).map_err(|e| OutputError::CanonicalizeFailed {
        path:   path.to_path_buf(),
        source: e,
    })?;

    // ── Step b: canonicalize vault_root and sources_dir ──────────────────────
    let canon_vault = safe_canonicalize(vault_root).map_err(|e| OutputError::CanonicalizeFailed {
        path:   vault_root.to_path_buf(),
        source: e,
    })?;

    let canon_sources =
        safe_canonicalize(sources_dir).map_err(|e| OutputError::CanonicalizeFailed {
            path:   sources_dir.to_path_buf(),
            source: e,
        })?;

    // ── Step c: must be inside vault_root ────────────────────────────────────
    if !canon_path.starts_with(&canon_vault) {
        return Err(OutputError::OutsideVault {
            path:       canon_path,
            vault_root: canon_vault,
        });
    }

    // ── Step d: must NOT be inside sources_dir ───────────────────────────────
    // `starts_with` covers both exact match (path == sources_dir) and any
    // proper descendant (path is inside sources_dir).
    if canon_path.starts_with(&canon_sources) {
        return Err(OutputError::InsideSources {
            path:        canon_path,
            sources_dir: canon_sources,
        });
    }

    // ── Step e: must be a regular file ───────────────────────────────────────
    // `is_file()` on a canonicalized path returns `true` only for regular
    // files; directories, sockets, FIFOs, and device nodes all return `false`.
    if !canon_path.is_file() {
        return Err(OutputError::NotRegularFile { path: canon_path });
    }

    // ── Step f: return the canonical path ────────────────────────────────────
    Ok(canon_path)
}

// ── Backward-compatibility alias ──────────────────────────────────────────────

/// Alias for [`safe_canonicalize`]; retained for internal backwards compatibility.
///
/// Prefer [`safe_canonicalize`] in new code.
#[inline]
pub fn canonical(p: &Path) -> Result<PathBuf, PathError> {
    safe_canonicalize(p)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Create a regular file at `dir/name` with dummy content.
    /// Supports nested names like `"a/b/c.txt"` by ensuring parent dirs exist.
    fn make_file(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("make_file: create_dir_all failed");
        }
        fs::write(&p, b"knowledge-builder test content").expect("make_file: write failed");
        p
    }

    /// Create a directory at `parent/name` (including all parents).
    fn make_dir(parent: &Path, name: &str) -> PathBuf {
        let p = parent.join(name);
        fs::create_dir_all(&p).expect("make_dir: create_dir_all failed");
        p
    }

    /// Standard vault layout:
    /// ```text
    /// <tmp>/          <- vault root  (= tmp.path())
    ///   Sources/      <- sources dir
    /// ```
    fn setup_vault() -> (TempDir, PathBuf, PathBuf) {
        let tmp     = TempDir::new().expect("TempDir::new failed");
        let vault   = tmp.path().to_path_buf();
        let sources = vault.join("Sources");
        fs::create_dir_all(&sources).expect("create Sources failed");
        (tmp, vault, sources)
    }

    /// Nested vault layout for tests that need a sibling "outside" directory:
    /// ```text
    /// <tmp>/           <- TempDir root (NOT the vault)
    ///   vault/         <- vault root
    ///     Sources/     <- sources dir
    /// ```
    fn setup_nested_vault() -> (TempDir, PathBuf, PathBuf) {
        let tmp     = TempDir::new().expect("TempDir::new failed");
        let vault   = tmp.path().join("vault");
        let sources = vault.join("Sources");
        fs::create_dir_all(&sources).expect("create nested Sources failed");
        (tmp, vault, sources)
    }

    // =========================================================================
    // is_inside
    // =========================================================================

    #[test]
    fn is_inside_true_for_deep_child() {
        assert!(is_inside(Path::new("/a/b/c/d"), Path::new("/a/b")));
    }

    #[test]
    fn is_inside_true_for_direct_child() {
        assert!(is_inside(Path::new("/a/b/c"), Path::new("/a/b")));
    }

    #[test]
    fn is_inside_true_for_exact_match() {
        // Exact match is considered "inside" per the spec's starts_with semantics.
        assert!(is_inside(Path::new("/a/b"), Path::new("/a/b")));
    }

    #[test]
    fn is_inside_false_for_sibling() {
        // "/a/bc" shares a prefix string but is NOT a path-component descendant of "/a/b".
        assert!(!is_inside(Path::new("/a/bc"), Path::new("/a/b")));
    }

    #[test]
    fn is_inside_false_for_parent() {
        // The parent is not inside its own child.
        assert!(!is_inside(Path::new("/a"), Path::new("/a/b")));
    }

    #[test]
    fn is_inside_false_for_unrelated() {
        assert!(!is_inside(Path::new("/x/y/z"), Path::new("/a/b")));
    }

    #[test]
    fn is_inside_handles_root_parent() {
        assert!(is_inside(Path::new("/a"), Path::new("/")));
    }

    // Note: empty paths are not valid canonical paths per the `is_inside` precondition
    // (canonicalized paths are always absolute), so that edge case is not tested here.

    // =========================================================================
    // safe_canonicalize
    // =========================================================================

    #[test]
    fn safe_canonicalize_existing_file() {
        let tmp = TempDir::new().unwrap();
        let f   = make_file(tmp.path(), "test.txt");
        let result = safe_canonicalize(&f);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        // Returned path must exist and be absolute.
        let canon = result.unwrap();
        assert!(canon.is_absolute());
        assert!(canon.exists());
    }

    #[test]
    fn safe_canonicalize_existing_directory() {
        let tmp = TempDir::new().unwrap();
        let d   = make_dir(tmp.path(), "subdir");
        let result = safe_canonicalize(&d);
        assert!(result.is_ok(), "expected Ok for existing dir, got: {result:?}");
    }

    #[test]
    fn safe_canonicalize_not_found() {
        let result = safe_canonicalize(Path::new(
            "/tmp/__kb_core_tests_this_path_must_not_exist_deadbeef/file.txt",
        ));
        assert!(
            matches!(result, Err(PathError::NotFound { .. })),
            "expected NotFound, got: {result:?}",
        );
    }

    #[test]
    fn safe_canonicalize_resolves_dot_dot() {
        // Create /tmp/<X>/a/b/  and canonicalize /tmp/<X>/a/b/../  -> /tmp/<X>/a
        let tmp = TempDir::new().unwrap();
        let a   = make_dir(tmp.path(), "a");
        let _b  = make_dir(&a, "b");
        let dotdot = a.join("b").join("..");
        let result = safe_canonicalize(&dotdot);
        assert!(result.is_ok(), "expected Ok for .., got: {result:?}");
        assert_eq!(
            result.unwrap(),
            std::fs::canonicalize(&a).unwrap(),
            ".. should resolve to parent",
        );
    }

    // =========================================================================
    // validate_output -- VALID cases
    // =========================================================================

    /// Valid: file inside vault, outside sources -> Ok(canonical_path).
    #[test]
    fn valid_inside_vault_outside_sources() {
        let (_tmp, vault, sources) = setup_vault();
        let notes  = make_dir(&vault, "Notes");
        let output = make_file(&notes, "note.md");

        let result = validate_output(&output, &vault, &sources);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        // The returned path must equal the canonical form.
        assert_eq!(
            result.unwrap(),
            fs::canonicalize(&output).unwrap(),
        );
    }

    /// Valid: output is a sibling of sources_dir (both direct children of vault) -> Ok.
    #[test]
    fn valid_sibling_of_sources_dir() {
        let (_tmp, vault, sources) = setup_vault();
        // Notes/ is a sibling of Sources/ -- both live directly under vault.
        let notes  = make_dir(&vault, "Notes");
        let output = make_file(&notes, "sibling_note.md");

        let result = validate_output(&output, &vault, &sources);
        assert!(result.is_ok(), "sibling of sources should be valid; got: {result:?}");
    }

    /// Valid: output placed directly in vault root (not in sources) -> Ok.
    #[test]
    fn valid_direct_child_of_vault_root() {
        let (_tmp, vault, sources) = setup_vault();
        let output = make_file(&vault, "root_note.md");

        let result = validate_output(&output, &vault, &sources);
        assert!(result.is_ok(), "direct child of vault root should be valid; got: {result:?}");
    }

    /// Valid: deeply nested output inside vault, outside sources -> Ok.
    #[test]
    fn valid_deeply_nested_inside_vault() {
        let (_tmp, vault, sources) = setup_vault();
        let output = make_file(&vault, "a/b/c/d/deep_note.md");

        let result = validate_output(&output, &vault, &sources);
        assert!(result.is_ok(), "deeply nested valid path failed: {result:?}");
    }

    /// Edge: vault_root == sources_dir's direct parent -- the most common real
    /// config (`~/Vault` + `~/Vault/Sources`) -> still works correctly.
    #[test]
    fn valid_vault_root_is_sources_direct_parent() {
        let (_tmp, vault, sources) = setup_vault();
        // vault = <tmp>, sources = <tmp>/Sources  (direct parent relationship)
        let output = make_file(&vault, "Notes/direct_parent_note.md");

        let result = validate_output(&output, &vault, &sources);
        assert!(result.is_ok(), "direct-parent edge case failed: {result:?}");
    }

    /// Valid: non-canonical path (contains `.`) is accepted after canonicalization.
    #[test]
    fn valid_non_canonical_path_with_dot_component() {
        let (_tmp, vault, sources) = setup_vault();
        let notes  = make_dir(&vault, "Notes");
        let output = make_file(&notes, "note.md");

        // Inject a harmless `.` component -- canonicalize should remove it.
        let non_canon = vault.join("Notes").join(".").join("note.md");
        let result = validate_output(&non_canon, &vault, &sources);
        assert!(result.is_ok(), "dot component should be normalised; got: {result:?}");
        assert_eq!(result.unwrap(), fs::canonicalize(&output).unwrap());
    }

    // =========================================================================
    // validate_output -- INVALID cases
    // =========================================================================

    /// Invalid: output is entirely outside the vault -> OutsideVault.
    #[test]
    fn invalid_outside_vault_entirely() {
        let (tmp, vault, sources) = setup_nested_vault();
        // File lives in <tmp>/ but vault is <tmp>/vault/ -- completely outside.
        let outside_file = make_file(tmp.path(), "outside.txt");

        let result = validate_output(&outside_file, &vault, &sources);
        assert!(
            matches!(result, Err(OutputError::OutsideVault { .. })),
            "expected OutsideVault, got: {result:?}",
        );
    }

    /// Invalid: output uses `..` to escape the vault -> OutsideVault.
    ///
    /// The path `<vault>/../outside.txt` resolves (after canonicalization)
    /// to `<tmp>/outside.txt`, which is outside the vault.
    #[test]
    fn invalid_dotdot_escapes_vault() {
        let (tmp, vault, sources) = setup_nested_vault();
        // Create the actual target so canonicalize can resolve it.
        let outside_file = make_file(tmp.path(), "dotdot_escape.txt");

        // Construct the malicious path: vault/../dotdot_escape.txt
        let escape_path = vault.join("..").join("dotdot_escape.txt");

        let result = validate_output(&escape_path, &vault, &sources);
        assert!(
            matches!(result, Err(OutputError::OutsideVault { .. })),
            "expected OutsideVault for .. escape, got: {result:?}",
        );
        // Verify the escape target actually existed (test validity check).
        assert!(outside_file.exists());
    }

    /// Invalid: symlink inside vault that resolves to a file outside vault -> OutsideVault.
    ///
    /// `canonicalize` follows the symlink to its real target, which lies outside
    /// the vault; the resolved path then fails the vault-containment check.
    #[test]
    #[cfg(unix)]
    fn invalid_symlink_escapes_vault() {
        let (tmp, vault, sources) = setup_nested_vault();
        let notes = make_dir(&vault, "Notes");

        // Real file sits OUTSIDE the vault (but inside <tmp>/).
        let real_file = make_file(tmp.path(), "outside_real_file.txt");

        // Symlink lives INSIDE the vault but points to the outside file.
        let symlink_path = notes.join("malicious_link.md");
        symlink(&real_file, &symlink_path)
            .expect("failed to create symlink -- requires Unix");

        let result = validate_output(&symlink_path, &vault, &sources);
        assert!(
            matches!(result, Err(OutputError::OutsideVault { .. })),
            "expected OutsideVault for symlink escape, got: {result:?}",
        );
    }

    /// Invalid: output is inside sources_dir -> InsideSources.
    #[test]
    fn invalid_inside_sources_dir() {
        let (_tmp, vault, sources) = setup_vault();
        let source_file = make_file(&sources, "document.pdf");

        let result = validate_output(&source_file, &vault, &sources);
        assert!(
            matches!(result, Err(OutputError::InsideSources { .. })),
            "expected InsideSources, got: {result:?}",
        );
    }

    /// Invalid: output nested inside a subdirectory of sources_dir -> InsideSources.
    #[test]
    fn invalid_nested_inside_sources_dir() {
        let (_tmp, vault, sources) = setup_vault();
        let nested = make_dir(&sources, "subfolder");
        let output = make_file(&nested, "nested_bad.txt");

        let result = validate_output(&output, &vault, &sources);
        assert!(
            matches!(result, Err(OutputError::InsideSources { .. })),
            "expected InsideSources for nested path, got: {result:?}",
        );
    }

    /// Invalid: path IS sources_dir exactly (not just inside it) -> InsideSources.
    ///
    /// The `starts_with` check covers exact equality, so passing sources_dir
    /// itself as an output path is rejected before the `is_file()` check.
    #[test]
    fn invalid_path_equals_sources_dir() {
        let (_tmp, vault, sources) = setup_vault();

        // sources is a directory; per spec order, InsideSources is checked
        // (step d) before NotRegularFile (step e), so we expect InsideSources.
        let result = validate_output(&sources, &vault, &sources);
        assert!(
            matches!(result, Err(OutputError::InsideSources { .. })),
            "expected InsideSources when path == sources_dir, got: {result:?}",
        );
    }

    /// Invalid: output is a directory inside vault, outside sources -> NotRegularFile.
    #[test]
    fn invalid_path_is_directory() {
        let (_tmp, vault, sources) = setup_vault();
        let dir_in_vault = make_dir(&vault, "NotesDir");

        let result = validate_output(&dir_in_vault, &vault, &sources);
        assert!(
            matches!(result, Err(OutputError::NotRegularFile { .. })),
            "expected NotRegularFile for directory, got: {result:?}",
        );
    }

    /// Invalid: output path does not exist -> CanonicalizeFailed(NotFound).
    #[test]
    fn invalid_nonexistent_path() {
        let (_tmp, vault, sources) = setup_vault();
        let missing = vault.join("Notes").join("does_not_exist.md");

        let result = validate_output(&missing, &vault, &sources);
        assert!(
            matches!(
                result,
                Err(OutputError::CanonicalizeFailed {
                    source: PathError::NotFound { .. },
                    ..
                })
            ),
            "expected CanonicalizeFailed(NotFound), got: {result:?}",
        );
    }

    /// Invalid: vault_root itself does not exist -> CanonicalizeFailed.
    #[test]
    fn invalid_nonexistent_vault_root() {
        let tmp          = TempDir::new().unwrap();
        let fake_vault   = PathBuf::from("/tmp/__kb_nonexistent_vault_9999abcd");
        let fake_sources = fake_vault.join("Sources");
        let some_file    = make_file(tmp.path(), "real_file.txt");

        // The vault does not exist, so safe_canonicalize(vault_root) will fail.
        let result = validate_output(&some_file, &fake_vault, &fake_sources);
        assert!(
            matches!(result, Err(OutputError::CanonicalizeFailed { .. })),
            "expected CanonicalizeFailed for missing vault, got: {result:?}",
        );
    }

    // =========================================================================
    // Regression / error-message tests
    // =========================================================================

    /// OutsideVault error message should mention "outside vault_root".
    #[test]
    fn outside_vault_error_contains_paths() {
        let (tmp, vault, sources) = setup_nested_vault();
        let outside_file = make_file(tmp.path(), "outside.txt");

        let err = validate_output(&outside_file, &vault, &sources).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("outside vault_root"),
            "error message should mention 'outside vault_root'; got: {msg}",
        );
    }

    /// InsideSources error message should mention "sources_dir".
    #[test]
    fn inside_sources_error_contains_paths() {
        let (_tmp, vault, sources) = setup_vault();
        let bad_output = make_file(&sources, "bad.txt");

        let err = validate_output(&bad_output, &vault, &sources).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("sources_dir"),
            "error message should mention 'sources_dir'; got: {msg}",
        );
    }

    /// NotRegularFile error message should mention "not a regular file".
    #[test]
    fn not_regular_file_error_contains_path() {
        let (_tmp, vault, sources) = setup_vault();
        let dir = make_dir(&vault, "SomeDir");

        let err = validate_output(&dir, &vault, &sources).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not a regular file"),
            "error message should mention 'not a regular file'; got: {msg}",
        );
    }

    // =========================================================================
    // canonical() alias
    // =========================================================================

    #[test]
    fn canonical_alias_works() {
        let tmp  = TempDir::new().unwrap();
        let file = make_file(tmp.path(), "alias_test.txt");
        // Both the alias and the primary function must return identical results.
        assert_eq!(
            canonical(&file).unwrap(),
            safe_canonicalize(&file).unwrap(),
        );
    }
}
