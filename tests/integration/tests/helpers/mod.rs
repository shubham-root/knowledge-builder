//! Integration-test harness for Knowledge Builder.
//!
//! This module provides [`TestVault`], a self-contained test fixture that
//! combines a temporary directory, a live [`StateStore`] actor, and a set of
//! helpers for enqueueing files, waiting on status transitions, and starting
//! the worker pool and detection pipeline.
//!
//! # Stub processor locations
//!
//! All stub scripts live in `processors/stub/` at the workspace root.  The
//! [`stub_path`] helper resolves them relative to `CARGO_MANIFEST_DIR` at
//! compile time.

#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use globset::{GlobSet, GlobSetBuilder};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use kb_core::{
    config::{OpsConfig, PathsConfig, ProcessorConfig, WatchConfig, WorkerConfig},
    Config, EnqueueOutcome, FileRow, OutputRecord, StateStore, Status,
};
use kb_watcher::{DetectionPipeline};
use kb_worker::WorkerPool;

// ── Stub-script paths ─────────────────────────────────────────────────────────

/// Absolute path to the processors/stub/ directory.
///
/// `CARGO_MANIFEST_DIR` resolves to `<workspace>/tests/integration` so we go
/// up two levels to reach the workspace root.
pub fn stub_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests/")
        .parent()
        .expect("workspace root")
        .join("processors")
        .join("stub")
}

/// Absolute path to a named stub script inside `processors/stub/`.
///
/// # Example
/// ```ignore
/// let cmd = stub_path("run.sh");
/// ```
pub fn stub_path(name: &str) -> String {
    stub_dir().join(name).to_string_lossy().to_string()
}

// ── TestVault ─────────────────────────────────────────────────────────────────

/// An isolated, fully self-contained test vault.
///
/// Each test should create its own `TestVault` so there is no shared state
/// between tests.  The underlying `TempDir` is deleted when this struct drops.
///
/// # Directory layout
/// ```text
/// <TempDir>/
///   vault/
///     Sources/     ← watched directory, files are dropped here
///     Notes/       ← stub processor writes outputs here
///   state.db       ← SQLite state store
///   work/          ← per-job working directories
/// ```
pub struct TestVault {
    /// Keep the TempDir alive for the life of the test.
    pub _dir: TempDir,
    /// Root of the simulated Obsidian vault.
    pub vault_root: PathBuf,
    /// Watched sources directory where test files are dropped.
    pub sources_dir: PathBuf,
    /// Notes directory where the stub processor writes outputs.
    pub notes_dir: PathBuf,
    /// SQLite database path.
    pub db_path: PathBuf,
    /// Root for per-job working directories.
    pub work_dir_root: PathBuf,
    /// Handle to the single-writer state store actor.
    pub store: StateStore,
}

impl TestVault {
    /// Create a new, isolated test vault with its own temp directory and DB.
    /// Uses default short backoffs `[1, 2, 5]`.
    pub async fn new() -> Result<Self> {
        Self::with_backoffs(&[1_u64, 2_u64, 5_u64]).await
    }

    /// Create a vault with custom backoff schedule.
    ///
    /// The backoff schedule is owned by the `StateStore` actor and drives
    /// `mark_failed` retry logic.  Length determines max attempts:
    /// `max_attempts = backoff_secs.len() + 1`.
    pub async fn with_backoffs(backoffs: &[u64]) -> Result<Self> {
        // Prefix must NOT start with '.' — the scanner's dotfile filter would
        // reject all paths inside a dot-prefixed TempDir.
        let dir = tempfile::Builder::new()
            .prefix("kb_inttest_")
            .tempdir()
            .context("create temp dir")?;

        let vault_root = dir.path().join("vault");
        let sources_dir = vault_root.join("Sources");
        let notes_dir = vault_root.join("Notes");
        let db_path = dir.path().join("state.db");
        let work_dir_root = dir.path().join("work");

        std::fs::create_dir_all(&sources_dir).context("create sources_dir")?;
        std::fs::create_dir_all(&notes_dir).context("create notes_dir")?;
        std::fs::create_dir_all(&work_dir_root).context("create work_dir_root")?;

        let store = StateStore::new(&db_path, backoffs)
            .await
            .context("open state store")?;

        Ok(Self {
            _dir: dir,
            vault_root,
            sources_dir,
            notes_dir,
            db_path,
            work_dir_root,
            store,
        })
    }

    // ── File helpers ──────────────────────────────────────────────────────────

    /// Write `content` to `sources_dir/<name>` and return the absolute path.
    pub fn drop_file(&self, name: &str, content: &[u8]) -> PathBuf {
        let path = self.sources_dir.join(name);
        std::fs::write(&path, content).expect("drop test file");
        path
    }

    /// Write `content` to `sources_dir/<subdir>/<name>` (creates subdir).
    pub fn drop_file_in(&self, subdir: &str, name: &str, content: &[u8]) -> PathBuf {
        let dir = self.sources_dir.join(subdir);
        std::fs::create_dir_all(&dir).expect("create subdir");
        let path = dir.join(name);
        std::fs::write(&path, content).expect("drop test file");
        path
    }

    // ── DB query helpers ──────────────────────────────────────────────────────

    /// Fetch the current `FileRow` for `path`, if any.
    pub async fn get_file_row(&self, path: &Path) -> Option<FileRow> {
        self.store
            .find_by_path(path.to_path_buf())
            .await
            .ok()
            .flatten()
    }

    /// Fetch just the `Status` for `path`, if any.
    pub async fn get_file_status(&self, path: &Path) -> Option<Status> {
        self.get_file_row(path).await.map(|r| r.status)
    }

    /// Poll until `path` reaches `expected` status, or until `timeout_ms`
    /// milliseconds elapse.  Returns the final `FileRow` on success.
    pub async fn wait_for_status(
        &self,
        path: &Path,
        expected: Status,
        timeout_ms: u64,
    ) -> Option<FileRow> {
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(row) = self.get_file_row(path).await {
                if row.status == expected {
                    return Some(row);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Poll until `path` reaches any of `expected` statuses, or timeout.
    pub async fn wait_for_any_status(
        &self,
        path: &Path,
        expected: &[Status],
        timeout_ms: u64,
    ) -> Option<FileRow> {
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(row) = self.get_file_row(path).await {
                if expected.contains(&row.status) {
                    return Some(row);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Get all outputs recorded for a file.
    pub async fn get_outputs(&self, file_id: i64) -> Vec<OutputRecord> {
        self.store
            .get_outputs_for_file(file_id)
            .await
            .unwrap_or_default()
    }

    // ── Pipeline helpers ──────────────────────────────────────────────────────

    /// Build a `Config` for this vault.
    ///
    /// * `processor_cmd` — path to the stub script to use
    /// * `timeout_secs`  — processor hard timeout
    /// * `stability_ms`  — stability window before hashing
    /// * `backoff_secs`  — per-retry backoff (length = max_attempts - 1)
    /// * `concurrency`   — max simultaneous workers
    pub fn make_config(
        &self,
        processor_cmd: &str,
        timeout_secs: u64,
        stability_ms: u64,
        backoff_secs: Vec<u64>,
        concurrency: usize,
    ) -> Config {
        let max_attempts = (backoff_secs.len() + 1) as u32;
        Config {
            paths: PathsConfig {
                vault_root:  self.vault_root.to_string_lossy().to_string(),
                sources_dir: self.sources_dir.to_string_lossy().to_string(),
                db_path:     self.db_path.to_string_lossy().to_string(),
                log_dir:     self.work_dir_root.to_string_lossy().to_string(),
            },
            watch: WatchConfig {
                extensions: vec![
                    "pdf".into(),
                    "docx".into(),
                    "xlsx".into(),
                    "ppt".into(),
                    "pptx".into(),
                    "jpg".into(),
                    "jpeg".into(),
                    "png".into(),
                ],
                ignore_globs: vec![
                    "**/.*".into(),
                    "**/~$*".into(),
                    "**/.obsidian/**".into(),
                    "**/*.icloud".into(),
                ],
                stability_ms,
                poll_interval_secs: 300, // very long — tests trigger scans manually
                hash_chunk_bytes: 1_048_576,
            },
            worker: WorkerConfig {
                concurrency,
                max_attempts,
                backoff_secs,
            },
            processor: ProcessorConfig {
                command:       processor_cmd.to_string(),
                timeout_secs,
                work_dir_root: self.work_dir_root.to_string_lossy().to_string(),
            },
            ops: OpsConfig {
                http_bind:  "127.0.0.1:7878".into(),
                log_level:  "info".into(),
                log_format: "json".into(),
            },
        }
    }

    /// Enqueue a file directly — hashes it and calls `process_stable_file`.
    ///
    /// This bypasses the watcher/stability tracker, which is correct for tests
    /// that want to verify worker/processor behaviour without FSEvents.
    pub async fn enqueue_direct(&self, path: &Path) -> Result<EnqueueOutcome> {
        let hash = kb_watcher::hasher::hash_file(path, 1_048_576)
            .await
            .with_context(|| format!("hash_file: {}", path.display()))?;

        let meta = std::fs::metadata(path)
            .with_context(|| format!("stat: {}", path.display()))?;

        let size = meta.len() as i64;
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.subsec_nanos() as i64)
            .unwrap_or(0);

        self.store
            .process_stable_file(path.to_path_buf(), size, mtime_ns, 0, hash)
            .await
            .context("process_stable_file")
    }

    /// Start a `WorkerPool` with the given config and cancellation token.
    ///
    /// Returns the `JoinHandle` (pool consumes self on `run()`).
    pub fn start_worker_pool(
        &self,
        config: &Config,
        shutdown: CancellationToken,
    ) -> JoinHandle<()> {
        let pool = WorkerPool::new(
            config.worker.concurrency,
            self.store.clone(),
            config.processor.clone(),
            shutdown,
            PathBuf::from(&config.paths.vault_root),
            PathBuf::from(&config.paths.sources_dir),
        );
        pool.run()
    }

    /// Build the glob-ignore set from a config's `ignore_globs` list.
    pub fn build_ignore_set(config: &Config) -> GlobSet {
        let mut builder = GlobSetBuilder::new();
        for pattern in &config.watch.ignore_globs {
            if let Ok(g) = globset::Glob::new(pattern) {
                builder.add(g);
            }
        }
        builder.build().unwrap_or_else(|_| GlobSetBuilder::new().build().unwrap())
    }
}

// ── FullSystem ────────────────────────────────────────────────────────────────

/// A fully running detection + processing system.
///
/// Wires together `TestVault`, `DetectionPipeline`, `PeriodicScanner`, and
/// `WorkerPool` under a single `CancellationToken`.
///
/// For tests that only need the state store + worker pool, use `TestVault`
/// directly and call `vault.enqueue_direct(path)` instead.
pub struct FullSystem {
    /// The underlying vault (state store, paths).
    pub vault: TestVault,
    /// Shutdown token — cancel to tear down all background tasks.
    pub shutdown: CancellationToken,
    /// Send paths directly into the stability tracker (bypasses FSEvents).
    /// Useful for injecting paths deterministically without relying on
    /// kernel event delivery timing.
    pub path_tx: mpsc::Sender<PathBuf>,
    /// The resolved config used to build all components.
    pub config: Config,
    pipeline_handle: JoinHandle<()>,
    pool_handle: JoinHandle<()>,
}

impl FullSystem {
    /// Stand up a fully running system.
    ///
    /// * `processor_cmd` — stub processor script to use
    /// * `timeout_secs`  — processor hard timeout
    /// * `stability_ms`  — stability window (use 300–500 for fast tests)
    /// * `backoff_secs`  — per-retry backoff delays
    /// * `concurrency`   — worker pool concurrency
    pub async fn new(
        processor_cmd: &str,
        timeout_secs: u64,
        stability_ms: u64,
        backoff_secs: Vec<u64>,
        concurrency: usize,
    ) -> Result<Self> {
        let vault = TestVault::new().await?;
        let config = vault.make_config(
            processor_cmd,
            timeout_secs,
            stability_ms,
            backoff_secs,
            concurrency,
        );
        let shutdown = CancellationToken::new();

        // Detection pipeline: FSEvents watcher + stability tracker + hasher +
        // state store.  We keep the path_sender so tests can inject paths
        // without depending on FSEvents timing.
        let pipeline =
            DetectionPipeline::new(&config, vault.store.clone())
                .context("build DetectionPipeline")?;
        let path_tx = pipeline.path_sender();
        let pipeline_handle = pipeline.run(shutdown.clone());

        // Worker pool.
        let pool_handle = vault.start_worker_pool(&config, shutdown.clone());

        Ok(Self {
            vault,
            shutdown,
            path_tx,
            config,
            pipeline_handle,
            pool_handle,
        })
    }

    /// Convenience: happy-path stub, 500 ms stability, short backoffs.
    pub async fn default_happy() -> Result<Self> {
        Self::new(&stub_path("run.sh"), 30, 500, vec![1_u64, 2_u64], 2).await
    }

    /// Inject a path directly into the stability tracker.
    pub async fn inject_path(&self, path: PathBuf) {
        let _ = self.path_tx.send(path).await;
    }

    /// Gracefully shut down all background tasks (10-second deadline each).
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            self.pipeline_handle,
        )
        .await;
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            self.pool_handle,
        )
        .await;
    }
}
