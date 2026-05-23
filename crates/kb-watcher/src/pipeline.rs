//! Detection pipeline — wires `FileWatcher → StabilityTracker → SHA-256 hasher
//! → StateStore` into a single, self-contained async pipeline.
//!
//! ## Pipeline flow
//! ```text
//! [FSEvents / cloud-sync]     [periodic scanner T22]
//!       │                               │
//!       ▼                               ▼
//! FileWatcher ──(WatchEvent)──▶ bridge task
//!                                       │
//!                              stability_sender (shared channel)
//!                                       │
//!                                       ▼
//!                             StabilityTracker
//!                                       │  (StableFile events)
//!                                       ▼
//!                             processor task
//!                                  │       │
//!                              hash_file   │
//!                                  │       │
//!                                  ▼       ▼
//!                         StateStore::process_stable_file()
//!                            → EnqueueOutcome (logged)
//! ```
//!
//! ## External path injection
//! The periodic scanner (T22) — and any other source that discovers candidate
//! paths without going through FSEvents — injects paths via the channel
//! returned by [`DetectionPipeline::path_sender`].  Those paths flow into the
//! same `StabilityTracker` instance as the watcher events, so stability
//! checking and deduplication are applied uniformly to all sources.
//!
//! ## Shutdown
//! Pass a [`CancellationToken`] to [`DetectionPipeline::run`].  Cancelling the
//! token causes all sub-tasks to exit on their next select-loop iteration and
//! then cleanly tears down the FSEvents subscription.

use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use kb_core::{Config, EnqueueOutcome, StateStore};

use crate::events::{FileWatcher, WatchEvent, WatcherError};
use crate::hasher::{hash_file, HashError};
use crate::stability::{StableFile, StabilityTracker};

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that can occur while **constructing** a [`DetectionPipeline`].
///
/// Note: watcher start-up errors (e.g. `sources_dir` does not exist) are
/// handled at runtime inside the spawned task, not at construction time,
/// so they are not represented here.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// A `FileWatcher` could not be constructed, typically because an
    /// ignore-glob pattern is syntactically invalid.
    #[error("failed to construct file watcher: {0}")]
    Watcher(#[from] WatcherError),
}

// ── DetectionPipeline ─────────────────────────────────────────────────────────

/// Wires the FSEvents watcher, stability tracker, SHA-256 hasher, and state
/// store into a single coherent detection pipeline.
///
/// # Lifecycle
///
/// ```text
/// 1.  pipeline  = DetectionPipeline::new(config, state_store)?
/// 2.  path_tx   = pipeline.path_sender()   // optional — for scanner / other sources
/// 3.  handle    = pipeline.run(shutdown)   // starts all background tasks
/// 4.  // … daemon is running …
/// 5.  shutdown.cancel()                    // initiate graceful shutdown
/// 6.  handle.await                         // wait for full teardown
/// ```
///
/// # External path injection
/// The scanner (T22) and any other source can call
/// [`DetectionPipeline::path_sender`] to obtain a [`tokio::sync::mpsc::Sender<PathBuf>`]
/// that feeds directly into the `StabilityTracker`.  The tracker
/// deduplicates concurrent submissions for the same path automatically.
/// **Obtain the sender before calling [`run`]** — `run` consumes `self`.
pub struct DetectionPipeline {
    /// Canonical path to the sources directory (for log messages).
    sources_dir: PathBuf,
    /// Read-buffer size (bytes) for the streaming SHA-256 hasher.
    hash_chunk_bytes: usize,
    /// Shared handle to the SQLite state-store actor.
    state_store: StateStore,
    /// Pre-obtained sender that feeds paths into the stability tracker.
    ///
    /// Clones of this are handed out via [`path_sender`].  One clone is also
    /// moved into the bridge task when [`run`] is called.
    stability_sender: mpsc::Sender<PathBuf>,
    /// The stability tracker, consumed by [`run`].
    tracker: StabilityTracker,
    /// The FSEvents watcher, consumed by [`run`] (started inside the spawned
    /// task so that start-up failures can be handled without panicking).
    watcher: FileWatcher,
    /// Receiver end of the FSEvents event channel, consumed by [`run`].
    watch_rx: mpsc::Receiver<WatchEvent>,
}

impl DetectionPipeline {
    /// Construct a new, **idle** pipeline.
    ///
    /// Creates all channels and builds the `FileWatcher` and
    /// `StabilityTracker`, but does **not** start any background tasks or
    /// subscribe to FSEvents.  Call [`run`] to activate the pipeline.
    ///
    /// # Arguments
    /// - `config`      — application configuration; paths must already be
    ///                   expanded (tilde-resolved) and validated.
    /// - `state_store` — cloneable handle to the state-store actor; the
    ///                   pipeline retains one clone for its processor task.
    ///
    /// # Errors
    /// Returns [`PipelineError::Watcher`] if any `ignore_globs` pattern is
    /// syntactically invalid (checked at construction time, not at runtime).
    pub fn new(config: &Config, state_store: StateStore) -> Result<Self, PipelineError> {
        // Config stores paths as `String`; convert once here.
        let sources_dir = PathBuf::from(&config.paths.sources_dir);

        // FSEvents event channel — 1 024-entry capacity absorbs burst drops
        // (e.g. many files copied at once) without blocking the notify thread.
        let (watch_tx, watch_rx) = mpsc::channel::<WatchEvent>(1_024);

        // Build the watcher (validates glob patterns; does NOT start FSEvents).
        let watcher = FileWatcher::new(
            sources_dir.clone(),
            config.watch.extensions.clone(),
            config.watch.ignore_globs.clone(),
            watch_tx,
        )?;

        // Stability tracker: polls `(size, mtime)` every 500 ms and emits a
        // `StableFile` once both values have been unchanged for `stability_ms`.
        // The 500 ms poll cadence is a sensible default; `stability_ms` (2 s by
        // default) controls the actual stability window.
        let tracker = StabilityTracker::new(config.watch.stability_ms, 500);

        // Pre-obtain a sender **before** `run()` consumes `self`.
        // Callers retrieve additional clones via `path_sender()`.
        let stability_sender = tracker.sender();

        Ok(Self {
            sources_dir,
            hash_chunk_bytes: config.watch.hash_chunk_bytes,
            state_store,
            stability_sender,
            tracker,
            watcher,
            watch_rx,
        })
    }

    /// Return a cloneable sender for injecting paths directly into the
    /// stability tracker.
    ///
    /// Use this to allow external sources — most importantly the periodic
    /// full-scanner (T22) — to submit candidate paths through the same
    /// stability machinery as the FSEvents watcher.
    ///
    /// The tracker deduplicates concurrent submissions for the same path:
    /// if the path is already being tracked, the duplicate is silently
    /// discarded and the existing tracking window continues uninterrupted.
    ///
    /// **Call this method before [`run`].**  `run` consumes `self`, so any
    /// handle you need must be obtained beforehand.
    ///
    /// ```no_run
    /// # use kb_watcher::pipeline::DetectionPipeline;
    /// # use kb_core::{Config, StateStore};
    /// # use tokio_util::sync::CancellationToken;
    /// # #[tokio::main] async fn main() -> anyhow::Result<()> {
    /// # let config = Config::default();
    /// # let store  = StateStore::new(std::path::Path::new("/tmp/t.db"), &[30]).await?;
    /// let pipeline   = DetectionPipeline::new(&config, store)?;
    /// let path_tx    = pipeline.path_sender(); // clone before run()
    /// let shutdown   = CancellationToken::new();
    /// let _handle    = pipeline.run(shutdown.clone());
    ///
    /// // Scanner / external source can now inject paths:
    /// path_tx.send(std::path::PathBuf::from("/vault/Sources/paper.pdf")).await.ok();
    /// # Ok(())
    /// # }
    /// ```
    pub fn path_sender(&self) -> mpsc::Sender<PathBuf> {
        self.stability_sender.clone()
    }

    /// Start all pipeline tasks and return a [`JoinHandle`] for the
    /// top-level orchestrator task.
    ///
    /// Internally this spawns:
    ///
    /// | Task | Role |
    /// |------|------|
    /// | **stability tracker** | Polls `(size, mtime)` and emits [`StableFile`] events. |
    /// | **bridge** | Translates `WatchEvent → PathBuf` and forwards to the tracker. |
    /// | **processor** | Receives [`StableFile`] events, hashes each file, and calls `StateStore::process_stable_file`. |
    ///
    /// All three tasks check `shutdown` and exit cleanly when it is cancelled.
    ///
    /// # Shutdown sequence
    /// 1. Caller cancels `shutdown`.
    /// 2. Bridge and processor tasks exit on their next `select!` iteration.
    /// 3. The stability tracker's `JoinHandle` is aborted.
    /// 4. The `FileWatcher` is dropped → FSEvents subscription is torn down.
    ///
    /// # Watcher start-up failures
    /// If `FileWatcher::start()` fails (e.g. `sources_dir` was deleted after
    /// `new()` was called), the error is logged and the pipeline shuts itself
    /// down gracefully.  It does **not** panic.
    pub fn run(self, shutdown: CancellationToken) -> JoinHandle<()> {
        let Self {
            sources_dir,
            hash_chunk_bytes,
            state_store,
            stability_sender,
            tracker,
            mut watcher,
            watch_rx,
        } = self;

        // Stable-file channel: connects tracker output → processor task.
        // 256 entries is sufficient for any realistic burst; the processor
        // is fast (async I/O + one DB write per file).
        let (stable_tx, stable_rx) = mpsc::channel::<StableFile>(256);

        // Start the stability tracker.  It holds its own keepalive sender
        // clone internally so the channel never closes spontaneously; we
        // must abort the handle explicitly on shutdown.
        let tracker_handle = tracker.run(stable_tx);

        tokio::spawn(async move {
            // ── Start FSEvents watcher ─────────────────────────────────────────
            // `FileWatcher::start()` is synchronous but spawns a blocking task
            // internally.  We call it here, inside the tokio runtime, so
            // `spawn_blocking` has an active runtime to attach to.
            if let Err(e) = watcher.start() {
                error!(
                    error         = %e,
                    sources_dir   = %sources_dir.display(),
                    "detection pipeline: failed to start file watcher; \
                     aborting pipeline"
                );
                tracker_handle.abort();
                return;
            }

            info!(
                sources_dir = %sources_dir.display(),
                "detection pipeline: started"
            );

            // ── Bridge task ────────────────────────────────────────────────────
            // Reads WatchEvents from the FSEvents channel and forwards each
            // path to the stability tracker.
            let bridge_handle = {
                let sd = shutdown.clone();
                tokio::spawn(bridge_task(watch_rx, stability_sender, sd))
            };

            // ── Processor task ─────────────────────────────────────────────────
            // Receives StableFile events, hashes them, and calls
            // `process_stable_file` to apply the §3.3 dedup rules.
            let processor_handle = {
                let sd = shutdown.clone();
                tokio::spawn(processor_task(
                    stable_rx,
                    state_store,
                    hash_chunk_bytes,
                    sd,
                ))
            };

            // ── Wait for shutdown ──────────────────────────────────────────────
            shutdown.cancelled().await;

            debug!("detection pipeline: shutdown signal received; stopping sub-tasks");

            // Abort sub-tasks.  They also check the shutdown token, but
            // aborting is a belt-and-suspenders measure for any task that is
            // blocked inside an `.await` (e.g. waiting on a full channel).
            bridge_handle.abort();
            processor_handle.abort();
            tracker_handle.abort();

            // Drop the watcher last: its Drop impl stops the debouncer and
            // closes the notify/FSEvents subscription cleanly.
            drop(watcher);

            info!("detection pipeline: all tasks stopped");
        })
    }
}

// ── Bridge task ───────────────────────────────────────────────────────────────

/// Translates raw [`WatchEvent`]s from the FSEvents watcher into
/// `PathBuf`s and forwards them to the stability tracker.
///
/// All event kinds (Created, Modified, Renamed) are forwarded: the stability
/// tracker internally deduplicates concurrent submissions for the same path and
/// polls the file's `(size, mtime)` independently — it does not need a fresh
/// event to reset its stability window.
///
/// Exits when:
/// - `shutdown` is cancelled.
/// - The watcher channel closes (watcher was dropped).
/// - The stability tracker channel closes (tracker was dropped / aborted).
async fn bridge_task(
    mut watch_rx: mpsc::Receiver<WatchEvent>,
    stability_tx: mpsc::Sender<PathBuf>,
    shutdown: CancellationToken,
) {
    debug!("bridge task: started");

    loop {
        tokio::select! {
            biased;

            _ = shutdown.cancelled() => {
                debug!("bridge task: shutdown signal received; exiting");
                break;
            }

            msg = watch_rx.recv() => {
                match msg {
                    None => {
                        debug!("bridge task: watcher channel closed; exiting");
                        break;
                    }
                    Some(event) => {
                        debug!(
                            path = %event.path.display(),
                            kind = ?event.kind,
                            "bridge task: forwarding path to stability tracker"
                        );
                        // Forward the destination path.  The stability tracker
                        // ignores the event kind — it only cares that this path
                        // should be watched until stable.
                        if stability_tx.send(event.path).await.is_err() {
                            warn!(
                                "bridge task: stability tracker channel closed \
                                 unexpectedly; exiting"
                            );
                            break;
                        }
                    }
                }
            }
        }
    }

    debug!("bridge task: exited");
}

// ── Processor task ────────────────────────────────────────────────────────────

/// Receives [`StableFile`] events from the stability tracker, hashes each
/// file, and records the result in the state store via the §3.3 dedup rules.
///
/// Exits when:
/// - `shutdown` is cancelled.
/// - The stable-file channel closes (tracker was dropped / aborted).
async fn processor_task(
    mut stable_rx: mpsc::Receiver<StableFile>,
    state_store: StateStore,
    hash_chunk_bytes: usize,
    shutdown: CancellationToken,
) {
    debug!("processor task: started");

    loop {
        tokio::select! {
            biased;

            _ = shutdown.cancelled() => {
                debug!("processor task: shutdown signal received; exiting");
                break;
            }

            msg = stable_rx.recv() => {
                match msg {
                    None => {
                        debug!("processor task: stable channel closed; exiting");
                        break;
                    }
                    Some(sf) => {
                        process_one(sf, &state_store, hash_chunk_bytes).await;
                    }
                }
            }
        }
    }

    debug!("processor task: exited");
}

// ── Single-file processor ─────────────────────────────────────────────────────

/// Hash one stable file and record it in the state store.
///
/// This function **never panics** and **never propagates errors** — all
/// failure modes are handled by logging and returning early so that the
/// caller continues processing subsequent files.
///
/// # Error handling
///
/// | Failure | Action |
/// |---------|--------|
/// | File deleted before hashing | `warn!` + skip |
/// | Permission denied on hash | `warn!` + skip |
/// | Other I/O error on hash | `warn!` + skip |
/// | State store error (transient) | `error!` + skip |
async fn process_one(sf: StableFile, state_store: &StateStore, hash_chunk_bytes: usize) {
    // ── Hash the file ─────────────────────────────────────────────────────────
    let content_hash = match hash_file(&sf.path, hash_chunk_bytes).await {
        Ok(hash) => {
            debug!(
                path = %sf.path.display(),
                hash = %hash,
                size = sf.size,
                "file hashed successfully"
            );
            hash
        }

        // File was deleted between the stability check and the hash attempt.
        // This is a normal race condition (the user deleted the file);
        // silently discard — there is nothing to enqueue.
        Err(HashError::NotFound { path }) => {
            warn!(
                path = %path.display(),
                "stable file deleted before hashing; skipping"
            );
            return;
        }

        // Process does not have read permission.  Log and skip; the user can
        // fix permissions and the periodic scanner will re-discover the file.
        Err(HashError::PermissionDenied { path }) => {
            warn!(
                path = %path.display(),
                "permission denied reading stable file for hashing; skipping"
            );
            return;
        }

        // Any other I/O error (disk error, NFS issue, …).
        Err(HashError::IoError { path, source }) => {
            warn!(
                path   = %path.display(),
                error  = %source,
                "I/O error hashing stable file; skipping"
            );
            return;
        }
    };

    // ── Apply §3.3 dedup rules via the state store ────────────────────────────
    //
    // `process_stable_file` executes all five rules in a single SQLite
    // transaction (T12):
    //   1. done + same hash   → AlreadyDone    (no-op)
    //   2. done + new hash    → RequeuedRevision
    //   3. dup done elsewhere → SkippedDuplicate
    //   4. queued/processing  → AlreadyPending (no-op)
    //   5. otherwise          → Queued
    let outcome = match state_store
        .process_stable_file(
            sf.path.clone(),
            // `size` is u64 in StableFile but i64 in the DB schema (SQL INTEGER).
            // Safe cast: real file sizes on macOS are well below i64::MAX.
            sf.size as i64,
            sf.mtime_ns,
            sf.inode,
            content_hash.clone(),
        )
        .await
    {
        Ok(o) => o,

        // Transient error (DB locked, actor channel full, …).  Log at error
        // level so operators can spot systemic issues, but continue running.
        Err(e) => {
            error!(
                path  = %sf.path.display(),
                error = %e,
                "state store error while recording stable file; \
                 file will be retried on next scan"
            );
            return;
        }
    };

    // ── Log the outcome ───────────────────────────────────────────────────────
    match outcome {
        EnqueueOutcome::Queued => {
            info!(
                path = %sf.path.display(),
                hash = %content_hash,
                "file enqueued for processing"
            );
        }
        EnqueueOutcome::RequeuedRevision => {
            info!(
                path = %sf.path.display(),
                hash = %content_hash,
                "file re-queued (content changed since last run)"
            );
        }
        EnqueueOutcome::SkippedDuplicate => {
            info!(
                path = %sf.path.display(),
                hash = %content_hash,
                "file skipped (duplicate content — another path was already processed)"
            );
        }
        EnqueueOutcome::AlreadyDone => {
            debug!(
                path = %sf.path.display(),
                "file already done with identical content; no action needed"
            );
        }
        EnqueueOutcome::AlreadyPending => {
            debug!(
                path = %sf.path.display(),
                "file already pending in queue; no action needed"
            );
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Build a minimal `Config` pointing at `sources_dir`.
    fn test_config(sources_dir: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.paths.sources_dir = sources_dir.to_string_lossy().into_owned();
        cfg.paths.vault_root  = sources_dir
            .parent()
            .unwrap_or(sources_dir)
            .to_string_lossy()
            .into_owned();
        cfg.watch.stability_ms       = 200;
        cfg.watch.hash_chunk_bytes   = 65_536;
        cfg
    }

    // ── construction tests ────────────────────────────────────────────────────

    /// `new()` succeeds with valid configuration.
    #[tokio::test]
    async fn new_succeeds_with_valid_config() {
        let tmp  = tempfile::TempDir::new().unwrap();
        let cfg  = test_config(tmp.path());
        let db   = tmp.path().join("test.db");
        let store = StateStore::new(&db, &[30, 300]).await.unwrap();
        let pipeline = DetectionPipeline::new(&cfg, store);
        assert!(pipeline.is_ok(), "expected Ok, got {:?}", pipeline.err());
    }

    /// `new()` fails when an invalid ignore-glob pattern is supplied.
    #[tokio::test]
    async fn new_fails_with_invalid_ignore_glob() {
        let tmp  = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config(tmp.path());
        cfg.watch.ignore_globs = vec!["[invalid-glob".to_string()];
        let db    = tmp.path().join("test.db");
        let store = StateStore::new(&db, &[30]).await.unwrap();
        let result = DetectionPipeline::new(&cfg, store);
        assert!(
            matches!(result, Err(PipelineError::Watcher(_))),
            "expected PipelineError::Watcher, got {:?}",
            result.err(),
        );
    }

    // ── path_sender tests ─────────────────────────────────────────────────────

    /// `path_sender()` returns a working sender.
    #[tokio::test]
    async fn path_sender_is_usable() {
        let tmp   = tempfile::TempDir::new().unwrap();
        let cfg   = test_config(tmp.path());
        let db    = tmp.path().join("test.db");
        let store = StateStore::new(&db, &[30]).await.unwrap();
        let pipeline = DetectionPipeline::new(&cfg, store).unwrap();

        let sender = pipeline.path_sender();
        // Should be able to clone the sender.
        let sender2 = sender.clone();
        drop(sender);
        drop(sender2);
    }

    // ── shutdown tests ────────────────────────────────────────────────────────

    /// `run()` spawns tasks and completes cleanly when shutdown is cancelled.
    ///
    /// Uses a `sources_dir` that exists (required for `watcher.start()` to
    /// succeed).
    #[tokio::test]
    async fn run_starts_and_shuts_down_cleanly() {
        let tmp   = tempfile::TempDir::new().unwrap();
        let cfg   = test_config(tmp.path());
        let db    = tmp.path().join("test.db");
        let store = StateStore::new(&db, &[30]).await.unwrap();

        let pipeline = DetectionPipeline::new(&cfg, store).unwrap();
        let shutdown = CancellationToken::new();
        let handle   = pipeline.run(shutdown.clone());

        // Give tasks a moment to start up.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Cancel and wait for clean shutdown.
        shutdown.cancel();
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            handle,
        )
        .await;

        assert!(result.is_ok(), "pipeline did not shut down within 5 s");
        assert!(result.unwrap().is_ok(), "pipeline task panicked");
    }

    // ── submit_path integration test ──────────────────────────────────────────

    /// Paths submitted via `path_sender()` eventually reach the stability
    /// tracker.  We verify by dropping the pipeline (so no watcher fires) and
    /// directly submitting a real file — the channel must be reachable.
    #[tokio::test]
    async fn submitted_paths_reach_stability_tracker() {
        use std::io::Write;

        let tmp   = tempfile::TempDir::new().unwrap();
        let cfg   = test_config(tmp.path());
        let db    = tmp.path().join("test.db");
        let store = StateStore::new(&db, &[30]).await.unwrap();

        let pipeline  = DetectionPipeline::new(&cfg, store.clone()).unwrap();
        let path_tx   = pipeline.path_sender();
        let shutdown  = CancellationToken::new();
        let _handle   = pipeline.run(shutdown.clone());

        // Create a real file so the stability tracker can stat it.
        let file_path = tmp.path().join("sample.pdf");
        {
            let mut f = std::fs::File::create(&file_path).unwrap();
            f.write_all(b"%PDF-1.4 sample content").unwrap();
            f.sync_all().unwrap();
        }

        // Submit the path via the external channel (simulating the scanner).
        path_tx.send(file_path.clone()).await
            .expect("stability tracker channel should be open");

        // Wait up to 3 s for the file to be hashed and recorded in the DB.
        // With stability_ms = 200 ms and a poll interval of 500 ms, the file
        // should be declared stable within ~700 ms.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            let row = store.find_by_path(file_path.clone()).await.unwrap();
            if let Some(r) = row {
                // File was picked up by the pipeline.
                assert!(
                    r.content_hash.is_some(),
                    "file should have a content hash by now"
                );
                break;
            }

            if std::time::Instant::now() > deadline {
                panic!("file was not recorded in the state store within 3 s");
            }
        }

        shutdown.cancel();
    }
}
