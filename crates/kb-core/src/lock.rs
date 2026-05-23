//! Singleton daemon lock — ensures only one `knowledge-builder` instance runs
//! at a time by holding an exclusive OS-level `flock` on a `.lock` file next
//! to the state database.
//!
//! # Usage
//! ```ignore
//! use std::path::Path;
//! use kb_core::lock::DaemonLock;
//!
//! let lock = DaemonLock::acquire(Path::new("/var/db/kb/state.db"))?;
//! // lock is now held; pass it into the main daemon struct so it lives as
//! // long as the process does.
//! ```

use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

use fs2::FileExt as _;
use thiserror::Error;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that can occur while acquiring the daemon singleton lock.
#[derive(Debug, Error)]
pub enum LockError {
    /// Another daemon process already holds the lock.
    #[error(
        "Another knowledge-builder daemon is already running (lock: {lock_path}). \
         Stop it first or run `kb status` to check."
    )]
    AlreadyRunning { lock_path: PathBuf },

    /// An unexpected I/O error occurred while creating or locking the file.
    #[error("I/O error on lock file {path}: {error}")]
    IoError {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
}

// ── DaemonLock ────────────────────────────────────────────────────────────────

/// An exclusive OS-level file lock that guarantees at most one daemon instance
/// is running at a time.
///
/// The lock is held for as long as this value is alive.  Dropping it releases
/// the lock so that a subsequent process (or test) can acquire it.
///
/// ```ignore
/// let lock = DaemonLock::acquire(db_path)?;
/// // hold `lock` in your top-level struct or main() binding.
/// ```
pub struct DaemonLock {
    /// The open lock file — the `flock` is tied to this file descriptor.
    /// We keep it in an `Option` so the `Drop` impl can take ownership and
    /// call `unlock` before the `File` is closed.
    file: Option<File>,
    /// Path kept for diagnostics / display only.
    lock_path: PathBuf,
}

impl std::fmt::Debug for DaemonLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonLock")
            .field("lock_path", &self.lock_path)
            .finish()
    }
}

impl DaemonLock {
    /// Compute the lock-file path from the database path.
    ///
    /// `state.db` → `state.db.lock`
    fn lock_path_for(db_path: &Path) -> PathBuf {
        let mut s = db_path.as_os_str().to_owned();
        s.push(".lock");
        PathBuf::from(s)
    }

    /// Try to acquire an exclusive, non-blocking `flock` on `<db_path>.lock`.
    ///
    /// # Errors
    /// - [`LockError::AlreadyRunning`] — another process holds the lock.
    /// - [`LockError::IoError`] — unexpected I/O failure (e.g. permissions).
    pub fn acquire(db_path: &Path) -> Result<Self, LockError> {
        let lock_path = Self::lock_path_for(db_path);

        // Create parent directories if they don't exist yet.
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| LockError::IoError {
                path: lock_path.clone(),
                error,
            })?;
        }

        // Open (or create) the lock file.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| LockError::IoError {
                path: lock_path.clone(),
                error,
            })?;

        // Non-blocking exclusive flock.
        match file.try_lock_exclusive() {
            Ok(()) => Ok(DaemonLock {
                file: Some(file),
                lock_path,
            }),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.raw_os_error() == Some(libc::EWOULDBLOCK) =>
            {
                Err(LockError::AlreadyRunning { lock_path })
            }
            Err(error) => Err(LockError::IoError {
                path: lock_path,
                error,
            }),
        }
    }

    /// The path of the lock file held by this instance.
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            // Best-effort unlock; ignore errors (the OS will release the lock
            // anyway when the file descriptor is closed).
            let _ = file.unlock();
            // `file` is dropped here, closing the fd.
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn db_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("state.db")
    }

    #[test]
    fn acquire_lock_succeeds() {
        let dir = tempdir().unwrap();
        let lock = DaemonLock::acquire(&db_path(&dir));
        assert!(lock.is_ok(), "first acquire must succeed: {lock:?}");
    }

    #[test]
    fn second_acquire_returns_already_running() {
        let dir = tempdir().unwrap();
        let path = db_path(&dir);

        let _first = DaemonLock::acquire(&path).expect("first acquire");
        let second = DaemonLock::acquire(&path);

        match second {
            Err(LockError::AlreadyRunning { lock_path }) => {
                assert!(
                    lock_path.to_string_lossy().ends_with(".lock"),
                    "lock_path should end with .lock"
                );
            }
            other => panic!("expected AlreadyRunning, got: {other:?}"),
        }
    }

    #[test]
    fn drop_first_lock_allows_reacquire() {
        let dir = tempdir().unwrap();
        let path = db_path(&dir);

        {
            let _first = DaemonLock::acquire(&path).expect("first acquire");
            // _first is dropped at end of this block — lock released.
        }

        let second = DaemonLock::acquire(&path);
        assert!(
            second.is_ok(),
            "acquire after drop must succeed: {second:?}"
        );
    }

    #[test]
    fn lock_path_has_dot_lock_suffix() {
        let path = Path::new("/some/dir/state.db");
        let lp = DaemonLock::lock_path_for(path);
        assert_eq!(lp, Path::new("/some/dir/state.db.lock"));
    }

    #[test]
    fn lock_path_accessor_returns_lock_path() {
        let dir = tempdir().unwrap();
        let path = db_path(&dir);
        let lock = DaemonLock::acquire(&path).expect("acquire");
        let expected = DaemonLock::lock_path_for(&path);
        assert_eq!(lock.lock_path(), expected);
    }
}
