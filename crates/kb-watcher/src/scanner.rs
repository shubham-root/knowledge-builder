//! Periodic full-scan backstop for the Knowledge Builder daemon.
//!
//! ## Purpose
//! FSEvents is best-effort: it coalesces events, drops them during macOS sleep,
//! and may miss files that materialise via iCloud/Dropbox sync.
//! The periodic scanner performs a recursive `walkdir` over `sources_dir` on a
//! configurable interval, applying the same five-stage filter pipeline as the
//! FSEvents watcher, and submitting any new or changed file to the
//! [`StabilityTracker`] for further processing.
//!
//! ## Integration
//! The scanner is wired into the detection pipeline via the
//! [`DetectionPipeline::path_sender`] handle (a `tokio::sync::mpsc::Sender<PathBuf>`).
//! Files submitted through this sender are treated identically to files
//! discovered by the live watcher.
//!
//! ## Event loop (non-blocking)
//! All `walkdir` I/O runs inside `tokio::task::spawn_blocking` so the async
//! event loop is never stalled even when scanning a large vault.
//!
//! ## Usage
//! ```no_run
//! use std::{path::PathBuf, time::Duration};
//! use globset::GlobSetBuilder;
//! use tokio::sync::mpsc;
//! use tokio_util::sync::CancellationToken;
//! use kb_core::StateStore;
//! use kb_watcher::scanner::PeriodicScanner;
//!
//! # async fn example(state: StateStore) {
//! let (tx, _rx) = mpsc::channel(256);
//! let scanner = PeriodicScanner::new(
//!     PathBuf::from("/vault/Sources"),
//!     vec!["pdf".into(), "docx".into()],
//!     GlobSetBuilder::new().build().unwrap(),
//!     Duration::from_secs(300),
//!     state,
//!     tx,
//! );
//! let shutdown = CancellationToken::new();
//! let handle = scanner.run(shutdown.clone());
//! // …later…
//! shutdown.cancel();
//! handle.await.ok();
//! # }
//! ```

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use globset::GlobSet;
use kb_core::{StateStore, types::event_kind};
use serde_json::json;
use tokio::{sync::mpsc, task};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use walkdir::WalkDir;

use crate::events::is_allowed;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that can be returned by [`scan_once`].
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// The walkdir I/O task panicked.
    #[error("walkdir task panicked: {0}")]
    TaskPanic(String),
    /// The state store returned an unexpected error.
    #[error("state store error: {0}")]
    State(#[from] anyhow::Error),
}

// ── PeriodicScanner ───────────────────────────────────────────────────────────

/// Timer-driven backstop scanner that ensures all files in `sources_dir` are
/// eventually submitted to the detection pipeline, regardless of whether the
/// FSEvents watcher delivered a timely notification.
///
/// On construction the scanner does **not** run immediately; call [`run`] to
/// start it.  An initial scan fires before the first timer interval so that
/// files present on daemon startup are detected without waiting for
/// `poll_interval` to elapse.
///
/// [`run`]: PeriodicScanner::run
pub struct PeriodicScanner {
    /// Root directory to walk recursively.
    sources_dir: PathBuf,
    /// Dot-free, lower-case file extensions that are allowed (e.g. `"pdf"`).
    extensions: Vec<String>,
    /// Pre-compiled ignore-glob set (same patterns as the FSEvents watcher).
    ignore_globs: GlobSet,
    /// How often to perform a full scan after the initial run.
    poll_interval: Duration,
    /// State store handle — used to check whether a file is already known and
    /// whether its on-disk `(size, mtime_ns)` matches the stored values.
    state: StateStore,
    /// Sender end of the stability-tracker input channel.  The scanner submits
    /// candidate paths here and the stability tracker takes over from there.
    path_sender: mpsc::Sender<PathBuf>,
}

impl PeriodicScanner {
    /// Construct a new `PeriodicScanner`.
    ///
    /// # Parameters
    /// * `sources_dir`   — Absolute path of the directory to scan.
    /// * `extensions`    — Allowlisted extensions (dot-free, any case).
    /// * `ignore_globs`  — Pre-compiled glob set; paths matching any pattern
    ///                     are skipped.
    /// * `poll_interval` — Delay between successive scans after the first.
    /// * `state`         — Cloneable state-store handle.
    /// * `path_sender`   — Channel to the stability tracker's input queue.
    pub fn new(
        sources_dir: PathBuf,
        extensions: Vec<String>,
        ignore_globs: GlobSet,
        poll_interval: Duration,
        state: StateStore,
        path_sender: mpsc::Sender<PathBuf>,
    ) -> Self {
        Self {
            sources_dir,
            extensions,
            ignore_globs,
            poll_interval,
            state,
            path_sender,
        }
    }

    /// Start the periodic scanner and return its [`JoinHandle`].
    ///
    /// The scanner runs three phases in a loop:
    /// 1. Emit a `scan_started` audit event.
    /// 2. Walk `sources_dir` with `walkdir` (inside `spawn_blocking` to avoid
    ///    blocking the async event loop).
    /// 3. Emit a `scan_finished` audit event with the candidate count.
    ///
    /// The loop also handles graceful shutdown: when `shutdown` is cancelled,
    /// the current scan (if any) completes and then the task exits.
    ///
    /// [`JoinHandle`]: tokio::task::JoinHandle
    pub fn run(self, shutdown: CancellationToken) -> task::JoinHandle<()> {
        task::spawn(async move {
            let Self {
                sources_dir,
                extensions,
                ignore_globs,
                poll_interval,
                state,
                path_sender,
            } = self;

            // Run an initial scan immediately (before the first interval tick).
            if !shutdown.is_cancelled() {
                run_one_scan(
                    &sources_dir,
                    &extensions,
                    &ignore_globs,
                    &state,
                    &path_sender,
                )
                .await;
            }

            // Then fire on the configured interval for all subsequent scans.
            let mut interval = tokio::time::interval(poll_interval);
            // The first tick fires immediately; skip it because we already ran
            // the initial scan above.
            interval.tick().await;

            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {
                        debug!("periodic scanner shutting down");
                        break;
                    }
                    _ = interval.tick() => {
                        run_one_scan(
                            &sources_dir,
                            &extensions,
                            &ignore_globs,
                            &state,
                            &path_sender,
                        )
                        .await;
                    }
                }
            }
        })
    }
}

// ── Public one-shot API ───────────────────────────────────────────────────────

/// Perform a single full scan of `sources_dir` and return the number of
/// candidate paths submitted to the stability tracker.
///
/// This is the engine used by both [`PeriodicScanner::run`] and the
/// `kb scan` CLI command.
///
/// # Errors
/// Returns [`ScanError::TaskPanic`] if the blocking walkdir task panics.
/// Individual file-level I/O errors (stat failures, permission errors) are
/// logged as warnings and do not abort the scan.
pub async fn scan_once(
    sources_dir: &Path,
    extensions: &[String],
    ignore_globs: &GlobSet,
    state: &StateStore,
    path_sender: &mpsc::Sender<PathBuf>,
) -> Result<usize, ScanError> {
    // Collect candidates from the filesystem (blocking walkdir).
    let candidates = collect_candidates(sources_dir, extensions, ignore_globs).await?;

    let total = candidates.len();
    let mut submitted = 0usize;

    for (path, disk_size, disk_mtime_ns) in candidates {
        // Check the state store to decide whether to submit this path.
        let should_submit = match state.find_by_path(path.clone()).await {
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "scanner: state store lookup failed; submitting conservatively"
                );
                true // Submit conservatively on DB error.
            }
            Ok(None) => {
                // File not in DB at all — definitely a new candidate.
                true
            }
            Ok(Some(row)) => {
                // File is in DB. Submit only if size or mtime differs from
                // what was last recorded (file may have been modified since
                // last processing, or it may not have been processed yet).
                let stored_size   = row.size;
                let stored_mtime  = row.mtime_ns;

                let size_changed  = stored_size  != Some(disk_size);
                let mtime_changed = stored_mtime != Some(disk_mtime_ns);

                size_changed || mtime_changed
            }
        };

        if should_submit {
            match path_sender.send(path.clone()).await {
                Ok(()) => {
                    debug!(path = %path.display(), "scanner: submitted to stability tracker");
                    submitted += 1;
                }
                Err(_) => {
                    // Channel closed — pipeline is shutting down.
                    warn!("scanner: stability tracker channel closed; stopping scan early");
                    break;
                }
            }
        } else {
            debug!(path = %path.display(), "scanner: skipping unchanged known file");
        }
    }

    debug!(
        total_walked = total,
        submitted,
        "scanner: scan_once complete"
    );

    Ok(submitted)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Run a single full scan, emitting audit events around it.
async fn run_one_scan(
    sources_dir: &Path,
    extensions: &[String],
    ignore_globs: &GlobSet,
    state: &StateStore,
    path_sender: &mpsc::Sender<PathBuf>,
) {
    let scan_start = unix_now();

    // Emit scan_started audit event (non-fatal on failure).
    if let Err(e) = state
        .record_event(
            "info".into(),
            event_kind::SCAN_STARTED.into(),
            None,
            "periodic scan started".into(),
            Some(json!({"sources_dir": sources_dir.display().to_string()}).to_string()),
        )
        .await
    {
        warn!(error = %e, "scanner: failed to record scan_started event");
    }

    info!(sources_dir = %sources_dir.display(), "periodic scanner: starting scan");

    let candidate_count = match scan_once(sources_dir, extensions, ignore_globs, state, path_sender).await {
        Ok(n) => n,
        Err(e) => {
            error!(error = %e, "scanner: scan_once failed");
            0
        }
    };

    let elapsed_secs = unix_now().saturating_sub(scan_start);

    info!(
        candidates = candidate_count,
        elapsed_secs,
        "periodic scanner: scan complete"
    );

    // Emit scan_finished audit event (non-fatal on failure).
    if let Err(e) = state
        .record_event(
            "info".into(),
            event_kind::SCAN_FINISHED.into(),
            None,
            format!("periodic scan finished; {} new candidate(s)", candidate_count),
            Some(
                json!({
                    "candidates": candidate_count,
                    "elapsed_secs": elapsed_secs,
                })
                .to_string(),
            ),
        )
        .await
    {
        warn!(error = %e, "scanner: failed to record scan_finished event");
    }
}

/// Walk `sources_dir` in a blocking thread and collect all paths that pass
/// the filter pipeline together with their on-disk `(size, mtime_ns)`.
///
/// Returns `Vec<(path, size_bytes, mtime_ns)>`.
async fn collect_candidates(
    sources_dir: &Path,
    extensions: &[String],
    ignore_globs: &GlobSet,
) -> Result<Vec<(PathBuf, i64, i64)>, ScanError> {
    let sources_dir = sources_dir.to_owned();
    let extensions = extensions.to_vec();
    let ignore_globs = ignore_globs.clone();

    task::spawn_blocking(move || {
        let mut candidates = Vec::new();

        for entry in WalkDir::new(&sources_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|res| {
                match res {
                    Ok(e) => Some(e),
                    Err(e) => {
                        warn!(error = %e, "scanner: walkdir error; skipping entry");
                        None
                    }
                }
            })
        {
            // Only consider regular files (not directories, symlinks, etc.).
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();

            // Apply the same five-stage filter as the FSEvents watcher.
            if !is_allowed(path, &extensions, &ignore_globs) {
                continue;
            }

            // Stat the file to obtain (size, mtime_ns) for change detection.
            let (size_bytes, mtime_ns) = match entry.metadata() {
                Ok(meta) => {
                    let size = meta.len() as i64;
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);
                    (size, mtime)
                }
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "scanner: failed to stat file; skipping"
                    );
                    continue;
                }
            };

            candidates.push((path.to_owned(), size_bytes, mtime_ns));
        }

        candidates
    })
    .await
    .map_err(|e| ScanError::TaskPanic(e.to_string()))
}

/// Returns the current Unix time in whole seconds.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── is_candidate helper (re-exported for testing) ─────────────────────────────

/// Returns `true` if `path` passes all scanner filters.
///
/// This is a thin wrapper around [`is_allowed`] from the events module,
/// exposed here so that callers can pre-filter a list of paths without
/// spinning up a full scanner.
pub fn passes_filter(path: &Path, extensions: &[String], ignore_globs: &GlobSet) -> bool {
    is_allowed(path, extensions, ignore_globs)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use globset::GlobSetBuilder;
    use kb_core::{StateStore, db_open};
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    use super::*;

    // ── helpers ───────────────────────────────────────────────────────────────

    fn empty_glob_set() -> GlobSet {
        GlobSetBuilder::new().build().unwrap()
    }

    /// Create a TempDir whose name does NOT start with '.' so that the
    /// `is_allowed` dotfile filter lets paths inside it through.
    /// (The default `TempDir::new()` uses a `.tmp` prefix which IS rejected.)
    fn tempdir() -> TempDir {
        tempfile::Builder::new()
            .prefix("kb_test_")
            .tempdir()
            .expect("tempdir")
    }

    async fn make_store(dir: &TempDir) -> StateStore {
        let db_path = dir.path().join("state.db");
        let conn = db_open(&db_path).unwrap();
        drop(conn); // migrations applied; StateStore will reopen
        StateStore::new(&db_path, &[30, 300, 1800])
            .await
            .expect("StateStore::new")
    }

    // ── passes_filter ─────────────────────────────────────────────────────────

    #[test]
    fn passes_filter_allowed_extension() {
        let p = PathBuf::from("/vault/Sources/doc.pdf");
        assert!(passes_filter(&p, &["pdf".into()], &empty_glob_set()));
    }

    #[test]
    fn passes_filter_disallowed_extension() {
        let p = PathBuf::from("/vault/Sources/readme.md");
        assert!(!passes_filter(&p, &["pdf".into(), "docx".into()], &empty_glob_set()));
    }

    #[test]
    fn passes_filter_rejects_dotfile() {
        let p = PathBuf::from("/vault/Sources/.hidden.pdf");
        assert!(!passes_filter(&p, &["pdf".into()], &empty_glob_set()));
    }

    #[test]
    fn passes_filter_rejects_office_lockfile() {
        let p = PathBuf::from("/vault/Sources/~$document.docx");
        assert!(!passes_filter(&p, &["docx".into()], &empty_glob_set()));
    }

    #[test]
    fn passes_filter_rejects_icloud_placeholder() {
        let p = PathBuf::from("/vault/Sources/report.pdf.icloud");
        // .icloud suffix means the extension IS icloud, not pdf — extension
        // filter alone would reject it; the explicit check is defence-in-depth.
        assert!(!passes_filter(&p, &["pdf".into(), "icloud".into()], &empty_glob_set()));
    }

    #[test]
    fn passes_filter_case_insensitive_extension() {
        let p = PathBuf::from("/vault/Sources/document.PDF");
        assert!(passes_filter(&p, &["pdf".into()], &empty_glob_set()));
    }

    // ── scan_once ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn scan_once_finds_new_files() {
        let dir = tempdir();
        let sources = dir.path().join("sources");
        std::fs::create_dir_all(&sources).unwrap();

        let pdf = sources.join("test.pdf");
        std::fs::write(&pdf, b"fake pdf content").unwrap();

        let store = make_store(&dir).await;
        let (tx, mut rx) = mpsc::channel(64);

        let count = scan_once(&sources, &["pdf".into()], &empty_glob_set(), &store, &tx)
            .await
            .expect("scan_once");

        assert_eq!(count, 1);
        // The path should have been forwarded.
        let received = rx.recv().await.expect("should receive path");
        assert_eq!(received, pdf);
    }

    #[tokio::test]
    async fn scan_once_skips_unchanged_known_files() {
        let dir = tempdir();
        let sources = dir.path().join("sources");
        std::fs::create_dir_all(&sources).unwrap();

        let pdf = sources.join("known.pdf");
        std::fs::write(&pdf, b"known content").unwrap();

        let store = make_store(&dir).await;
        let (tx, mut rx) = mpsc::channel(64);

        // Pre-register the file with the correct size+mtime so the scanner
        // should consider it unchanged and skip it.
        let meta = std::fs::metadata(&pdf).unwrap();
        let size = meta.len() as i64;
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);

        store
            .register_seen(pdf.clone(), Some(size), Some(mtime_ns), None)
            .await
            .expect("register_seen");

        let count = scan_once(&sources, &["pdf".into()], &empty_glob_set(), &store, &tx)
            .await
            .expect("scan_once");

        assert_eq!(count, 0, "unchanged file should be skipped");
        // Channel should be empty.
        rx.close();
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn scan_once_resubmits_when_size_changes() {
        let dir = tempdir();
        let sources = dir.path().join("sources");
        std::fs::create_dir_all(&sources).unwrap();

        let pdf = sources.join("modified.pdf");
        std::fs::write(&pdf, b"v1 content").unwrap();

        let store = make_store(&dir).await;
        let (tx, mut rx) = mpsc::channel(64);

        // Register with a different (stale) size.
        let meta = std::fs::metadata(&pdf).unwrap();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let stale_size: i64 = 999; // deliberately wrong

        store
            .register_seen(pdf.clone(), Some(stale_size), Some(mtime_ns), None)
            .await
            .expect("register_seen");

        let count = scan_once(&sources, &["pdf".into()], &empty_glob_set(), &store, &tx)
            .await
            .expect("scan_once");

        assert_eq!(count, 1, "size-changed file should be resubmitted");
        let received = rx.recv().await.expect("path");
        assert_eq!(received, pdf);
    }

    #[tokio::test]
    async fn scan_once_ignores_non_matching_extensions() {
        let dir = tempdir();
        let sources = dir.path().join("sources");
        std::fs::create_dir_all(&sources).unwrap();

        std::fs::write(sources.join("readme.md"), b"markdown").unwrap();
        std::fs::write(sources.join("note.txt"), b"text").unwrap();

        let store = make_store(&dir).await;
        let (tx, _rx) = mpsc::channel(64);

        let count = scan_once(&sources, &["pdf".into(), "docx".into()], &empty_glob_set(), &store, &tx)
            .await
            .expect("scan_once");

        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn scan_once_returns_zero_for_empty_dir() {
        let dir = tempdir();
        let sources = dir.path().join("sources");
        std::fs::create_dir_all(&sources).unwrap();

        let store = make_store(&dir).await;
        let (tx, _rx) = mpsc::channel(64);

        let count = scan_once(&sources, &["pdf".into()], &empty_glob_set(), &store, &tx)
            .await
            .expect("scan_once");

        assert_eq!(count, 0);
    }

    // ── PeriodicScanner::run ──────────────────────────────────────────────────

    #[tokio::test]
    async fn periodic_scanner_runs_initial_scan_and_shuts_down() {
        let dir = tempdir();
        let sources = dir.path().join("sources");
        std::fs::create_dir_all(&sources).unwrap();

        // Place one file so we know the initial scan fires.
        std::fs::write(sources.join("startup.pdf"), b"content").unwrap();

        let store = make_store(&dir).await;
        let (tx, mut rx) = mpsc::channel(64);

        let scanner = PeriodicScanner::new(
            sources.clone(),
            vec!["pdf".into()],
            empty_glob_set(),
            Duration::from_secs(300), // long interval — won't fire in this test
            store,
            tx,
        );

        let shutdown = CancellationToken::new();
        let handle = scanner.run(shutdown.clone());

        // The initial scan should submit the file quickly.
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout waiting for initial scan")
            .expect("channel closed unexpectedly");

        assert!(received.file_name().unwrap().to_str().unwrap().ends_with(".pdf"));

        // Shut down cleanly.
        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("scanner did not shut down in time")
            .expect("scanner task panicked");
    }

    #[tokio::test]
    async fn periodic_scanner_honours_shutdown_before_first_scan() {
        let dir = tempdir();
        let sources = dir.path().join("sources");
        std::fs::create_dir_all(&sources).unwrap();

        let store = make_store(&dir).await;
        let (tx, _rx) = mpsc::channel(64);

        let scanner = PeriodicScanner::new(
            sources,
            vec!["pdf".into()],
            empty_glob_set(),
            Duration::from_secs(300),
            store,
            tx,
        );

        // Cancel BEFORE calling run — the scanner should not block.
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let handle = scanner.run(shutdown);

        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("scanner should exit quickly when pre-cancelled")
            .expect("scanner task panicked");
    }

    #[tokio::test]
    async fn periodic_scanner_walks_subdirectories() {
        let dir = tempdir();
        let sources = dir.path().join("sources");
        let subdir = sources.join("papers").join("2024");
        std::fs::create_dir_all(&subdir).unwrap();

        std::fs::write(subdir.join("deep.pdf"), b"deep content").unwrap();

        let store = make_store(&dir).await;
        let (tx, mut rx) = mpsc::channel(64);

        let count = scan_once(&sources, &["pdf".into()], &empty_glob_set(), &store, &tx)
            .await
            .expect("scan_once");

        assert_eq!(count, 1);
        let received = rx.recv().await.unwrap();
        assert!(received.to_str().unwrap().contains("deep.pdf"));
    }
}
