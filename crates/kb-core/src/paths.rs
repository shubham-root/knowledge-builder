//! Path invariant utilities for Knowledge Builder.
//!
//! This module is the single source of truth for the hard structural guarantee:
//!
//! > Every processor output MUST reside inside `vault_root` and MUST NOT
//! > reside inside `sources_dir`.
//!
//! All path validation in the codebase routes through [`validate_output`].

use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors produced by path invariant checks.
#[derive(Debug, Error)]
pub enum PathError {
    #[error("path does not exist or cannot be canonicalized: {path}: {source}")]
    Canonicalize { path: PathBuf, source: std::io::Error },

    #[error("output path is not inside vault_root\n  path:       {path}\n  vault_root: {vault_root}")]
    OutsideVault { path: PathBuf, vault_root: PathBuf },

    #[error("output path is inside sources_dir (would cause reprocessing loop)\n  path:        {path}\n  sources_dir: {sources_dir}")]
    InsideSources { path: PathBuf, sources_dir: PathBuf },

    #[error("output path is not a regular file: {0}")]
    NotAFile(PathBuf),
}

/// Canonicalize `p`, returning a helpful error on failure.
pub fn canonical(p: impl AsRef<Path>) -> Result<PathBuf, PathError> {
    let p = p.as_ref();
    std::fs::canonicalize(p).map_err(|source| PathError::Canonicalize {
        path: p.to_path_buf(),
        source,
    })
}

/// Return `true` if `child` is strictly inside `parent` (both already
/// canonicalized).  An exact match returns `false`.
pub fn is_inside(child: &Path, parent: &Path) -> bool {
    // `starts_with` on `PathBuf` does component-wise comparison, so
    // `/a/bc` does NOT start_with `/a/b` — exactly what we want.
    child != parent && child.starts_with(parent)
}

/// The single validation gate for the output-path invariant.
///
/// Canonicalizes `output_path`, then checks:
/// 1. It exists and is a regular file.
/// 2. It is inside `vault_root`.
/// 3. It is NOT inside `sources_dir`.
///
/// `vault_root` and `sources_dir` must already be canonicalized (they come
/// from config, which is validated at startup).
pub fn validate_output(
    output_path: impl AsRef<Path>,
    vault_root:  &Path,
    sources_dir: &Path,
) -> Result<PathBuf, PathError> {
    let canon = canonical(output_path)?;

    // Must be a regular file (not symlink, not dir, not socket).
    if !canon.is_file() {
        return Err(PathError::NotAFile(canon));
    }

    // Must live inside vault_root.
    if !canon.starts_with(vault_root) {
        return Err(PathError::OutsideVault {
            path:       canon,
            vault_root: vault_root.to_path_buf(),
        });
    }

    // Must NOT live inside sources_dir.
    if canon.starts_with(sources_dir) {
        return Err(PathError::InsideSources {
            path:        canon,
            sources_dir: sources_dir.to_path_buf(),
        });
    }

    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // is_inside ----------------------------------------------------------------

    #[test]
    fn is_inside_true_for_child() {
        assert!(is_inside(Path::new("/a/b/c"), Path::new("/a/b")));
    }

    #[test]
    fn is_inside_false_for_exact_match() {
        assert!(!is_inside(Path::new("/a/b"), Path::new("/a/b")));
    }

    #[test]
    fn is_inside_false_for_sibling() {
        assert!(!is_inside(Path::new("/a/bc"), Path::new("/a/b")));
    }

    #[test]
    fn is_inside_false_for_parent() {
        assert!(!is_inside(Path::new("/a"), Path::new("/a/b")));
    }

    #[test]
    fn is_inside_false_for_unrelated() {
        assert!(!is_inside(Path::new("/x/y"), Path::new("/a/b")));
    }

    #[test]
    fn is_inside_handles_root() {
        assert!(is_inside(Path::new("/a"), Path::new("/")));
    }
}
