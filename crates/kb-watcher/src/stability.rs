//! Per-path stability tracker.
//!
//! After a file-system event arrives for a candidate path the tracker polls
//! `(size, mtime)` every `poll_interval_ms` milliseconds.  Once both values
//! have remained **unchanged** for at least `stability_ms` milliseconds the
//! file is declared stable and a [`StableFile`] is emitted on the output
//! channel.
//!
//! # Design
//!
//! [`StabilityTracker`] runs as a **single background tokio task** so that
//! all state is owned by one thread and there is no mutex contention.
//! New paths are fed into the task through an `mpsc` channel, meaning callers
//! in any other task can enqueue paths concurrently.
//!
//! # Usage
//!
//! ```no_run
//! use std::path::PathBuf;
//! use tokio::sync::mpsc;
//! use kb_watcher::stability::{StabilityTracker, StableFile};
//!
//! #[tokio::main]
//! async fn main() {
//!     let (out_tx, mut out_rx) = mpsc::channel::<StableFile>(64);
//!     let tracker = StabilityTracker::new(2_000, 500);
//!     let sender  = tracker.sender(); // clone before consuming
//!     let _handle = tracker.run(out_tx);
//!
//!     sender.send(PathBuf::from("/tmp/example.pdf")).await.unwrap();
//!
//!     if let Some(sf) = out_rx.recv().await {
//!         println!("stable: {:?}  size={}", sf.path, sf.size);
//!     }
//! }
//! ```

use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use tokio::{sync::mpsc, task::JoinHandle, time};

// ── Public types ──────────────────────────────────────────────────────────────

/// A file that has been observed as stable (size + mtime unchanged for
/// `stability_ms`).  Ready to be handed to the SHA-256 hasher.
#[derive(Debug, Clone)]
pub struct StableFile {
    /// Canonical absolute path to the file.
    pub path: PathBuf,
    /// File size in bytes at the moment stability was declared.
    pub size: u64,
    /// Last-modification time as nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Inode number (macOS / Linux).  `0` on platforms without inode support.
    pub inode: u64,
}

// ── Internal state ────────────────────────────────────────────────────────────

/// Per-path state machine entry.
#[derive(Debug)]
struct FileState {
    /// Size observed on the most recent stat.
    last_size: u64,
    /// Mtime (ns) observed on the most recent stat.
    last_mtime: i64,
    /// Wall-clock instant when this path was first submitted for tracking.
    first_seen: Instant,
    /// Wall-clock instant of the most recent `(size, mtime)` change.
    last_changed: Instant,
    /// True after the file has been declared stable (row is pending removal).
    stable: bool,
}

// ── StabilityTracker ──────────────────────────────────────────────────────────

/// Manages per-path stability state machines.
///
/// Create via [`StabilityTracker::new`], optionally call
/// [`StabilityTracker::track`] to seed initial paths, retrieve a shareable
/// [`mpsc::Sender<PathBuf>`] via [`StabilityTracker::sender`], then call
/// [`StabilityTracker::run`] to start the background task.
pub struct StabilityTracker {
    /// How long (ms) `(size, mtime)` must be unchanged to declare stable.
    stability_ms: u64,
    /// How often (ms) to re-stat tracked files.
    poll_interval_ms: u64,
    /// Sender half — kept here so callers can clone it via [`sender()`].
    track_tx: mpsc::Sender<PathBuf>,
    /// Receiver half — consumed by [`run()`].
    track_rx: mpsc::Receiver<PathBuf>,
}

impl StabilityTracker {
    /// Create a new tracker.
    ///
    /// # Arguments
    /// - `stability_ms`     — milliseconds `(size, mtime)` must be stable.
    ///   Typical: `2_000`.
    /// - `poll_interval_ms` — polling cadence.  Typical: `500`.
    pub fn new(stability_ms: u64, poll_interval_ms: u64) -> Self {
        let (track_tx, track_rx) = mpsc::channel(1_024);
        Self {
            stability_ms,
            poll_interval_ms,
            track_tx,
            track_rx,
        }
    }

    /// Queue `path` for stability tracking.
    ///
    /// Can be called before [`run`] is started.  For concurrent post-run
    /// submissions use the [`sender`] method to obtain a shareable
    /// [`mpsc::Sender<PathBuf>`].
    ///
    /// Uses a non-blocking send; logs a warning and drops the path if the
    /// internal channel is at capacity (1 024 entries).
    pub fn track(&mut self, path: PathBuf) {
        if let Err(e) = self.track_tx.try_send(path) {
            tracing::warn!(error = %e, "stability: input channel full; path dropped");
        }
    }

    /// Return a cloneable [`mpsc::Sender<PathBuf>`] for submitting paths
    /// from other tasks **after** [`run`] has consumed `self`.
    ///
    /// ```no_run
    /// # use kb_watcher::stability::StabilityTracker;
    /// # use tokio::sync::mpsc;
    /// # use kb_watcher::stability::StableFile;
    /// # tokio_test::block_on(async {
    /// let (out_tx, _out_rx) = mpsc::channel::<StableFile>(64);
    /// let tracker = StabilityTracker::new(2_000, 500);
    /// let sender  = tracker.sender();          // clone before run()
    /// let _handle = tracker.run(out_tx);       // self consumed
    /// sender.send(std::path::PathBuf::from("/tmp/foo.pdf")).await.ok();
    /// # });
    /// ```
    pub fn sender(&self) -> mpsc::Sender<PathBuf> {
        self.track_tx.clone()
    }

    /// Start the background stability-tracking task.
    ///
    /// Consumes `self` and spawns a single tokio task.  That task:
    /// - Receives new paths from the internal channel.
    /// - Polls every `poll_interval_ms` ms (re-stats all tracked files).
    /// - Emits a [`StableFile`] on `output` for each file that stabilises.
    /// - Drops paths whose file has disappeared (logged at `DEBUG`).
    /// - Gives up on paths that are still unstable after `5 × stability_ms`
    ///   (logged at `WARN`).
    ///
    /// The task exits when the input channel is closed (all [`Sender`] handles
    /// are dropped).
    pub fn run(self, output: mpsc::Sender<StableFile>) -> JoinHandle<()> {
        let stability_dur = Duration::from_millis(self.stability_ms);
        let max_wait_dur = Duration::from_millis(self.stability_ms.saturating_mul(5));
        let poll_dur = Duration::from_millis(self.poll_interval_ms);
        let mut track_rx = self.track_rx;

        tokio::spawn(async move {
            let mut states: HashMap<PathBuf, FileState> = HashMap::new();
            let mut ticker = time::interval(poll_dur);
            ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    // Polling tick: re-stat all currently tracked files.
                    _ = ticker.tick() => {
                        // Drain any buffered new-path messages before polling
                        // so a freshly-added file isn't missed this tick.
                        loop {
                            match track_rx.try_recv() {
                                Ok(path)  => Self::on_new_path(path, &mut states),
                                Err(_)    => break,
                            }
                        }

                        Self::poll_all(
                            &mut states,
                            stability_dur,
                            max_wait_dur,
                            &output,
                        ).await;
                    }

                    // New path to begin tracking.
                    msg = track_rx.recv() => {
                        match msg {
                            None => {
                                tracing::debug!(
                                    "stability tracker: all senders dropped; \
                                     shutting down"
                                );
                                break;
                            }
                            Some(path) => Self::on_new_path(path, &mut states),
                        }
                    }
                }
            }
        })
    }

    // ── Private helpers ───────────────────────────────────────────────────

    /// Handle a newly submitted path.
    fn on_new_path(path: PathBuf, states: &mut HashMap<PathBuf, FileState>) {
        if states.contains_key(&path) {
            tracing::trace!(?path, "stability: already tracking; ignoring duplicate");
            return;
        }

        let (init_size, init_mtime_ns) = Self::stat_path(&path).unwrap_or((0, 0));
        let now = Instant::now();

        tracing::debug!(
            ?path,
            size = init_size,
            mtime_ns = init_mtime_ns,
            "stability: began tracking"
        );

        states.insert(
            path,
            FileState {
                last_size: init_size,
                last_mtime: init_mtime_ns,
                first_seen: now,
                last_changed: now,
                stable: false,
            },
        );
    }

    /// Stat a file and return `(size, mtime_ns)`.  Returns `None` on error.
    fn stat_path(path: &PathBuf) -> Option<(u64, i64)> {
        let meta = std::fs::metadata(path).ok()?;
        let size = meta.len();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        Some((size, mtime_ns))
    }

    /// Poll every tracked path and advance the per-path state machine.
    ///
    /// Collects all changes first, then removes finished/dropped entries,
    /// then emits stable events — avoiding borrow conflicts during iteration.
    async fn poll_all(
        states: &mut HashMap<PathBuf, FileState>,
        stability_dur: Duration,
        max_wait_dur: Duration,
        output: &mpsc::Sender<StableFile>,
    ) {
        let mut to_remove: Vec<PathBuf> = Vec::new();
        let mut to_emit: Vec<StableFile> = Vec::new();

        for (path, state) in states.iter_mut() {
            if state.stable {
                // Already emitted; pending removal.
                to_remove.push(path.clone());
                continue;
            }

            // Hard timeout: 5× stability_ms.
            if state.first_seen.elapsed() > max_wait_dur {
                tracing::warn!(
                    ?path,
                    max_wait_ms = max_wait_dur.as_millis(),
                    "stability: max-wait exceeded; giving up on path"
                );
                to_remove.push(path.clone());
                continue;
            }

            // Re-stat the file.
            match std::fs::metadata(path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(?path, "stability: file disappeared; dropping");
                    to_remove.push(path.clone());
                }

                Err(e) => {
                    // Transient error (e.g. permission, busy) — keep tracking.
                    tracing::warn!(
                        ?path,
                        error = %e,
                        "stability: stat error; will retry next tick"
                    );
                }

                Ok(meta) => {
                    let size = meta.len();
                    let mtime_ns = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);

                    if size != state.last_size || mtime_ns != state.last_mtime {
                        // File changed: reset the stability timer.
                        tracing::trace!(
                            ?path,
                            old_size = state.last_size,
                            new_size = size,
                            "stability: size/mtime changed; resetting timer"
                        );
                        state.last_size = size;
                        state.last_mtime = mtime_ns;
                        state.last_changed = Instant::now();
                    } else if state.last_changed.elapsed() >= stability_dur {
                        // Unchanged long enough → stable.
                        state.stable = true;

                        #[cfg(unix)]
                        let inode: u64 = {
                            use std::os::unix::fs::MetadataExt;
                            meta.ino()
                        };
                        #[cfg(not(unix))]
                        let inode: u64 = 0;

                        tracing::info!(
                            ?path,
                            size,
                            mtime_ns,
                            inode,
                            "stability: file declared stable"
                        );

                        to_emit.push(StableFile {
                            path: path.clone(),
                            size,
                            mtime_ns,
                            inode,
                        });
                        to_remove.push(path.clone());
                    }
                    // else: unchanged but not long enough yet — keep waiting.
                }
            }
        }

        // Purge finished / dropped entries.
        for path in to_remove {
            states.remove(&path);
        }

        // Emit stable events.  We do this after the loop to avoid holding a
        // mutable borrow on `states` across `.await` points.
        for sf in to_emit {
            if output.send(sf).await.is_err() {
                tracing::warn!("stability: output channel closed; stable event dropped");
            }
        }
    }
}

// ── Backward-compat stubs from T1 scaffold ────────────────────────────────────
//
// These types were defined in the T1 skeleton.  They are retained here so that
// any code referencing them continues to compile during the transition to the
// full `StabilityTracker` API.

/// Outcome of a per-file stability poll (legacy API — prefer
/// [`StabilityTracker`]).
#[deprecated(note = "Use StabilityTracker instead")]
#[derive(Debug)]
pub enum StabilityOutcome {
    /// File is stable; proceed to hash.
    Stable {
        path: PathBuf,
        size: u64,
        mtime_ns: i64,
        inode: u64,
    },
    /// File disappeared before it became stable.
    Disappeared,
    /// Stability window exceeded maximum wait time.
    Timeout,
}

/// Legacy one-shot stability poll (T1 scaffold stub — prefer
/// [`StabilityTracker`]).
#[deprecated(note = "Use StabilityTracker instead")]
#[allow(deprecated)]
pub async fn wait_for_stable(
    path: PathBuf,
    stability_ms: u64,
    max_wait_ms: u64,
) -> StabilityOutcome {
    let _ = (stability_ms, max_wait_ms);
    tracing::debug!(?path, "wait_for_stable: delegating to StabilityTracker");

    let (out_tx, mut out_rx) = mpsc::channel::<StableFile>(1);
    let mut tracker = StabilityTracker::new(stability_ms, 500);
    tracker.track(path);
    let _handle = tracker.run(out_tx);

    match tokio::time::timeout(
        Duration::from_millis(max_wait_ms),
        out_rx.recv(),
    )
    .await
    {
        Ok(Some(sf)) => StabilityOutcome::Stable {
            path: sf.path,
            size: sf.size,
            mtime_ns: sf.mtime_ns,
            inode: sf.inode,
        },
        Ok(None) => StabilityOutcome::Disappeared,
        Err(_)   => StabilityOutcome::Timeout,
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;
    use std::time::Duration;
    use tempfile::NamedTempFile;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    /// Helper: create a tracker with faster timing for tests.
    ///
    /// - stability_ms = 400  (file must be unchanged for 400 ms)
    /// - poll_interval_ms = 80
    fn fast_tracker() -> StabilityTracker {
        StabilityTracker::new(400, 80)
    }

    // ── Test 1: a quiescent file becomes stable ───────────────────────────

    /// Create a file, track it, verify a [`StableFile`] is emitted.
    #[tokio::test]
    async fn file_becomes_stable_after_quiet_period() {
        let mut tmp = NamedTempFile::new().expect("tempfile");
        writeln!(tmp.as_file_mut(), "hello stable world").unwrap();
        tmp.as_file().sync_all().unwrap();

        let path = tmp.path().to_path_buf();

        let (out_tx, mut out_rx) = mpsc::channel::<StableFile>(4);
        let mut tracker = fast_tracker();
        tracker.track(path.clone());
        let _handle = tracker.run(out_tx);

        // Wait up to 4 s — should arrive well within stability_ms + a few ticks.
        let result = timeout(Duration::from_secs(4), out_rx.recv()).await;
        assert!(result.is_ok(), "timed out waiting for stable event");

        let sf = result.unwrap().expect("channel closed unexpectedly");
        assert_eq!(sf.path, path, "wrong path in StableFile");
        assert!(sf.size > 0, "size should be non-zero");
        assert!(sf.mtime_ns > 0, "mtime_ns should be non-zero");
        #[cfg(unix)]
        assert!(sf.inode > 0, "inode should be non-zero on unix");
    }

    // ── Test 2: an actively-written file does NOT stabilise until writes stop

    /// Keep writing to a file every 150 ms.  Verify no stable event is emitted
    /// during the write window, then one arrives after writes stop.
    #[tokio::test]
    async fn actively_written_file_does_not_stabilise_until_writes_stop() {
        let tmp = NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();

        let (out_tx, mut out_rx) = mpsc::channel::<StableFile>(4);
        let mut tracker = fast_tracker();
        tracker.track(path.clone());
        let _handle = tracker.run(out_tx);

        // Write to the file every 100 ms for 600 ms (1.5× stability_ms = 600 ms).
        let write_path = path.clone();
        let writer = tokio::spawn(async move {
            for i in 0u8..6 {
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&write_path)
                    .unwrap();
                writeln!(f, "write {i}").unwrap();
                f.sync_all().unwrap();
                drop(f);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });

        // During the write window no stable event should arrive.
        let no_event = timeout(Duration::from_millis(550), out_rx.recv()).await;
        assert!(
            no_event.is_err(),
            "got premature stable event while file was still being written"
        );

        // Wait for writes to finish.
        writer.await.unwrap();

        // Now the file should stabilise.
        let result = timeout(Duration::from_secs(4), out_rx.recv()).await;
        assert!(result.is_ok(), "timed out waiting for stable event after writes stopped");
        let sf = result.unwrap().expect("channel closed unexpectedly");
        assert_eq!(sf.path, path);
    }

    // ── Test 3: a deleted file is silently dropped ────────────────────────

    /// Track a file, then immediately delete it.  The tracker should NOT emit
    /// a [`StableFile`]; it should drop the entry after detecting disappearance.
    #[tokio::test]
    async fn deleted_file_is_dropped_without_emitting_stable() {
        let tmp = NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();

        let (out_tx, mut out_rx) = mpsc::channel::<StableFile>(4);
        let mut tracker = fast_tracker();
        tracker.track(path.clone());
        let _handle = tracker.run(out_tx);

        // Delete the file almost immediately (before any stability window).
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(tmp); // NamedTempFile removes the file on drop

        // Wait well past the stability window — no event should arrive.
        let result = timeout(Duration::from_millis(1_500), out_rx.recv()).await;
        assert!(
            result.is_err(),
            "unexpected stable event emitted for a deleted file"
        );
    }

    // ── Test 4: concurrent track() calls via sender ───────────────────────

    /// Add paths via the cloned sender after `run()` has consumed the tracker.
    #[tokio::test]
    async fn concurrent_track_via_sender() {
        let mut tmp1 = NamedTempFile::new().unwrap();
        writeln!(tmp1.as_file_mut(), "file one").unwrap();
        tmp1.as_file().sync_all().unwrap();

        let mut tmp2 = NamedTempFile::new().unwrap();
        writeln!(tmp2.as_file_mut(), "file two").unwrap();
        tmp2.as_file().sync_all().unwrap();

        let path1 = tmp1.path().to_path_buf();
        let path2 = tmp2.path().to_path_buf();

        let (out_tx, mut out_rx) = mpsc::channel::<StableFile>(8);
        let tracker = fast_tracker();
        let sender  = tracker.sender();
        let _handle = tracker.run(out_tx);

        // Submit both paths via the sender (simulates concurrent callers).
        sender.send(path1.clone()).await.unwrap();
        sender.send(path2.clone()).await.unwrap();

        let mut received: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();

        // Expect two stable events.
        for _ in 0..2 {
            let result = timeout(Duration::from_secs(4), out_rx.recv()).await;
            assert!(result.is_ok(), "timed out waiting for stable event");
            let sf = result.unwrap().expect("channel unexpectedly closed");
            received.insert(sf.path);
        }

        assert!(received.contains(&path1), "path1 not stabilised");
        assert!(received.contains(&path2), "path2 not stabilised");
    }

    // ── Test 5: duplicate track submissions are idempotent ────────────────

    /// Submitting the same path twice should produce exactly one StableFile.
    #[tokio::test]
    async fn duplicate_track_produces_one_stable_event() {
        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp.as_file_mut(), "dup test").unwrap();
        tmp.as_file().sync_all().unwrap();

        let path = tmp.path().to_path_buf();

        let (out_tx, mut out_rx) = mpsc::channel::<StableFile>(4);
        let mut tracker = fast_tracker();
        tracker.track(path.clone());
        tracker.track(path.clone()); // duplicate
        let _handle = tracker.run(out_tx);

        // First event.
        let result = timeout(Duration::from_secs(4), out_rx.recv()).await;
        assert!(result.is_ok());
        result.unwrap().expect("channel closed");

        // Second event should NOT arrive.
        let second = timeout(Duration::from_millis(500), out_rx.recv()).await;
        assert!(second.is_err(), "duplicate track produced a second stable event");
    }
}
