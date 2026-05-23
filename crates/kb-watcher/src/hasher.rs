//! Streaming SHA-256 content hasher.
//!
//! Reads a file in configurable chunks, feeds each chunk to a SHA-256
//! digest, and returns the hash in `"sha256:<lowercase-hex>"` format.
//!
//! Full implementation: T11.

use std::path::Path;

/// Compute the SHA-256 hash of `path` using `chunk_bytes`-sized reads.
///
/// Returns `"sha256:<hex>"` on success.
pub async fn hash_file(
    path:        impl AsRef<Path>,
    chunk_bytes: usize,
) -> kb_core::Result<String> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;

    let path = path.as_ref();
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; chunk_bytes];

    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }

    let hex = format!("sha256:{:x}", hasher.finalize());
    tracing::debug!(path = %path.display(), hash = %hex, "file hashed");
    Ok(hex)
}
