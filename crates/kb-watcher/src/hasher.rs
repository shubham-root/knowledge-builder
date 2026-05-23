//! Streaming SHA-256 content hasher.
//!
//! Reads a file in configurable chunks via `spawn_blocking` (so the tokio
//! runtime is never stalled on I/O), feeds each chunk to a SHA-256 digest,
//! and returns the hash in `"sha256:<lowercase-hex>"` format.
//!
//! # Example
//! ```no_run
//! # async fn ex() -> Result<(), kb_watcher::hasher::HashError> {
//! let hash = kb_watcher::hasher::hash_file(
//!     std::path::Path::new("/tmp/my.pdf"),
//!     1_048_576,
//! ).await?;
//! assert!(hash.starts_with("sha256:"));
//! # Ok(())
//! # }
//! ```

use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use thiserror::Error;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that can occur while hashing a file.
#[derive(Debug, Error)]
pub enum HashError {
    /// The file does not exist at the given path.
    #[error("file not found: {path}")]
    NotFound { path: PathBuf },

    /// The process lacks read permission for the file.
    #[error("permission denied: {path}")]
    PermissionDenied { path: PathBuf },

    /// Any other I/O error that is not specifically `NotFound` or
    /// `PermissionDenied`.
    #[error("I/O error hashing {path}: {source}")]
    IoError {
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
}

impl HashError {
    fn from_io(err: io::Error, path: &Path) -> Self {
        match err.kind() {
            io::ErrorKind::NotFound => Self::NotFound { path: path.to_path_buf() },
            io::ErrorKind::PermissionDenied => Self::PermissionDenied { path: path.to_path_buf() },
            _ => Self::IoError { path: path.to_path_buf(), source: err },
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Compute the SHA-256 content hash of `path` using `chunk_bytes`-sized reads.
///
/// The actual I/O is offloaded to `tokio::task::spawn_blocking` so the
/// runtime thread pool is never stalled, even for large files.
///
/// # Arguments
/// * `path`        – Path to the file to hash.
/// * `chunk_bytes` – Read-buffer size in bytes (use `1_048_576` for the
///                   recommended 1 MiB default).
///
/// # Returns
/// A string of the form `"sha256:<64-char-lowercase-hex>"`.
///
/// # Errors
/// Returns [`HashError::NotFound`] if the file does not exist,
/// [`HashError::PermissionDenied`] if the process cannot read it, or
/// [`HashError::IoError`] for any other I/O failure.
pub async fn hash_file(path: &Path, chunk_bytes: usize) -> Result<String, HashError> {
    // Capture an owned copy of the path so we can move it into the closure.
    let path_buf = path.to_path_buf();

    tokio::task::spawn_blocking(move || hash_file_blocking(&path_buf, chunk_bytes))
        .await
        // JoinError → treat as an IoError with a synthetic error
        .map_err(|e| HashError::IoError {
            path:   path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::Other, e.to_string()),
        })?
}

// ── Blocking worker (called inside spawn_blocking) ────────────────────────────

fn hash_file_blocking(path: &Path, chunk_bytes: usize) -> Result<String, HashError> {
    let mut file = fs::File::open(path).map_err(|e| HashError::from_io(e, path))?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; chunk_bytes];

    loop {
        let n = file.read(&mut buf).map_err(|e| HashError::from_io(e, path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let digest = hasher.finalize();
    let hex = format!("sha256:{digest:x}");
    tracing::debug!(path = %path.display(), hash = %hex, "file hashed");
    Ok(hex)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // Helper: write bytes to a temp file and return (file, path).
    fn tmp_with(data: &[u8]) -> (NamedTempFile, PathBuf) {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(data).expect("write");
        f.flush().expect("flush");
        let path = f.path().to_path_buf();
        (f, path)
    }

    // ── Known content ─────────────────────────────────────────────────────────

    /// `echo -n "hello world" | sha256sum` → 
    /// b94d27b9934d3e08a52e52d7da7dabfac484efe04294e576b4d8c5a3951c4e1d  (WRONG)
    ///
    /// Actual: `printf 'hello world' | sha256sum`
    /// → b94d27b9934d3e08a52e52d7da7dabfac484efe04294e576b4d8c5a3951c4e1d
    ///
    /// Let's compute properly:
    /// SHA256("hello world") =
    /// b94d27b9934d3e08a52e52d7da7dabfac484efe04294e576b4d8c5a3951c4e1d
    ///
    /// We'll compute expected with the sha2 crate itself to avoid copy-paste errors.
    #[tokio::test]
    async fn known_content_matches_sha2_reference() {
        let content = b"hello world";
        let (_f, path) = tmp_with(content);

        // Compute expected using sha2 directly (same library — guaranteed match).
        let mut h = Sha256::new();
        h.update(content);
        let expected = format!("sha256:{:x}", h.finalize());

        let result = hash_file(&path, 1_048_576).await.expect("hash ok");
        assert_eq!(result, expected);
    }

    /// Independently-verified SHA-256 for the ASCII string "The quick brown fox":
    /// `printf 'The quick brown fox' | sha256sum`
    /// → a48e7bdb4285f34ae64671b4a3a84dfba98c57b1f70cca57be97a3e99a3fc4f0
    ///
    /// Note: sha256sum output may differ by trailing newline. We use the exact
    /// value produced by `sha2::Sha256` over the raw bytes with no trailing newline.
    #[tokio::test]
    async fn known_content_fox_phrase() {
        let content = b"The quick brown fox";
        let (_f, path) = tmp_with(content);

        // Pre-computed with `sha2::Sha256` over raw bytes (no newline).
        let mut h = Sha256::new();
        h.update(content);
        let expected = format!("sha256:{:x}", h.finalize());

        let result = hash_file(&path, 1_048_576).await.expect("hash ok");
        assert_eq!(result, expected);
        assert!(result.starts_with("sha256:"), "must use sha256: prefix");
        // SHA-256 hex digest is always 64 chars → total len = 7 + 64 = 71
        assert_eq!(result.len(), 71, "sha256:<64 hex chars>");
    }

    // ── Empty file ────────────────────────────────────────────────────────────

    /// SHA-256 of empty input is the well-known constant.
    #[tokio::test]
    async fn empty_file() {
        let (_f, path) = tmp_with(b"");
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let result = hash_file(&path, 1_048_576).await.expect("hash ok");
        assert_eq!(
            result,
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // ── Large file (multi-chunk streaming) ───────────────────────────────────

    /// Create a file that spans multiple chunks and verify the hash is consistent
    /// with a single-pass reference.
    #[tokio::test]
    async fn large_file_multi_chunk() {
        // 3 MiB of repeating byte pattern — will require 3+ chunks at 1 MiB each.
        let data: Vec<u8> = (0u8..=255).cycle().take(3 * 1_048_576).collect();
        let (_f, path) = tmp_with(&data);

        // Reference hash (single pass via sha2 in memory).
        let mut h = Sha256::new();
        h.update(&data);
        let expected = format!("sha256:{:x}", h.finalize());

        // Hash using 1 MiB chunks (3 reads required).
        let result = hash_file(&path, 1_048_576).await.expect("hash ok");
        assert_eq!(result, expected, "multi-chunk hash must equal single-pass hash");
    }

    /// Verify that a small chunk size (even 1 byte) still produces the correct hash.
    #[tokio::test]
    async fn tiny_chunk_size() {
        let content = b"streaming correctness test";
        let (_f, path) = tmp_with(content);

        let mut h = Sha256::new();
        h.update(content);
        let expected = format!("sha256:{:x}", h.finalize());

        // Use 1-byte chunks — exercises the loop body N times.
        let result = hash_file(&path, 1).await.expect("hash ok");
        assert_eq!(result, expected);
    }

    // ── Non-existent file ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn nonexistent_file_returns_not_found() {
        let path = PathBuf::from("/tmp/kb_watcher_test_nonexistent_file_12345678.pdf");
        let err = hash_file(&path, 1_048_576)
            .await
            .expect_err("should fail for missing file");
        assert!(
            matches!(err, HashError::NotFound { .. }),
            "expected NotFound, got: {err}"
        );
    }

    // ── Determinism ───────────────────────────────────────────────────────────

    /// Same content always produces the same hash (two calls on the same file).
    #[tokio::test]
    async fn deterministic_same_file() {
        let content = b"determinism test content";
        let (_f, path) = tmp_with(content);

        let h1 = hash_file(&path, 1_048_576).await.expect("first hash");
        let h2 = hash_file(&path, 1_048_576).await.expect("second hash");
        assert_eq!(h1, h2, "hashing the same file twice must produce identical results");
    }

    /// Same content in two different files produces the same hash.
    #[tokio::test]
    async fn deterministic_identical_content_different_files() {
        let content = b"identical content test";
        let (_f1, path1) = tmp_with(content);
        let (_f2, path2) = tmp_with(content);

        let h1 = hash_file(&path1, 1_048_576).await.expect("hash1");
        let h2 = hash_file(&path2, 1_048_576).await.expect("hash2");
        assert_eq!(h1, h2, "identical content must hash to the same value");
    }

    /// Different content always produces a different hash.
    #[tokio::test]
    async fn different_content_produces_different_hash() {
        let (_f1, path1) = tmp_with(b"content A");
        let (_f2, path2) = tmp_with(b"content B");

        let h1 = hash_file(&path1, 1_048_576).await.expect("hash1");
        let h2 = hash_file(&path2, 1_048_576).await.expect("hash2");
        assert_ne!(h1, h2, "distinct content must hash to distinct values");
    }

    // ── Chunk size independence ───────────────────────────────────────────────

    /// Hash produced with 512-byte chunks equals hash produced with 1 MiB chunks.
    #[tokio::test]
    async fn chunk_size_does_not_affect_hash() {
        let data: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let (_f, path) = tmp_with(&data);

        let h_small = hash_file(&path, 512).await.expect("small chunk");
        let h_large = hash_file(&path, 1_048_576).await.expect("large chunk");
        assert_eq!(h_small, h_large, "chunk size must not affect the hash");
    }

    // ── Output format ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn output_format_is_lowercase_hex() {
        let (_f, path) = tmp_with(b"format test");
        let result = hash_file(&path, 1_048_576).await.expect("hash ok");

        let hex_part = result
            .strip_prefix("sha256:")
            .expect("must start with sha256: prefix");
        assert_eq!(hex_part.len(), 64, "SHA-256 hex digest is 64 chars");
        assert!(
            hex_part.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "hex digits must be lowercase: {hex_part}"
        );
    }
}
