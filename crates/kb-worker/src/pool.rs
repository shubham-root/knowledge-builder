//! Bounded worker pool with atomic job claiming.
//!
//! Uses a [`tokio::sync::Semaphore`] to cap parallelism at `concurrency`
//! simultaneous subprocesses.  Workers atomically claim `queued` rows from
//! the [`StateStore`] to prevent double-processing.
//!
//! # Lifecycle
//!
//! 1. [`WorkerPool::new`] — build the pool (does not start any background work).
//! 2. [`WorkerPool::run`] — launch the claim-process loop; returns a `JoinHandle`
//!    the daemon can `.await` for graceful exit.
//! 3. [`WorkerPool::notify_new_work`] — wake the loop immediately when a new job
//!    is enqueued, bypassing the 100 ms polling delay.
//!
//! # Notification mechanism
//!
//! The claim loop sleeps for 100 ms between polls when no work is available.
//! External callers (e.g. the watcher pipeline after `process_stable_file`)
//! can call [`WorkerPool::notify_new_work`] to instantly wake the loop.
//! This uses a [`tokio::sync::Notify`] — a single notification collapses
//! multiple rapid calls into one wakeup.
//!
//! # Job processing
//!
//! The per-job [`process_job`] function implements the full pipeline per
//! PLAN.md §9.6: claim → work_dir → spawn → parse → validate → record →
//! mark_done/failed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kb_core::{
    config::ProcessorConfig,
    event_kind,
    state::StateStore,
    FileRow, ProcessOutput, ProcessResult, ProcessorInput,
};
use tokio::{
    sync::{Notify, Semaphore},
    task::JoinHandle,
    time::{sleep, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing::instrument;

use crate::{
    processor::{invoke_processor, ProcessorError},
    validate::validate_processor_outputs,
};

// ── process_job ───────────────────────────────────────────────────────────────

/// Process a single claimed job end-to-end.
///
/// Implements PLAN.md §9.6 step by step:
///
/// 1. **work_dir** — `<work_dir_root>/<hash12>-<job_id>/` (created by
///    [`invoke_processor`] if absent).
/// 2. **Build input** — serialises [`ProcessorInput`] for the subprocess.
/// 3. **Audit** — records a `processor_started` event in the state store.
/// 4. **Invoke** — spawns the processor with a hard `timeout_secs` limit.
/// 5. **Ok result** — validates all output paths against the vault invariant,
///    then calls [`StateStore::mark_done`] (or `mark_failed` on violation).
/// 6. **Error result** — calls `mark_failed` with the processor's own
///    `retryable` flag.
/// 7. **Timeout** — calls `mark_failed(retryable = true)`.
/// 8. **Other errors** — calls `mark_failed(retryable = true)`.
/// 9. **Cleanup** — removes `work_dir` on success; retains it on failure for
///    debugging.
/// 10. **Final audit** — records a `done` or `failed` event.
///
/// This function **never panics**.  Every error path results in an appropriate
/// `mark_failed` call so the job never gets stuck in the `processing` state.
///
/// # Arguments
///
/// * `job`         — The [`FileRow`] claimed from the state store.
/// * `state`       — A cloneable handle to the state-store actor.
/// * `config`      — Processor configuration (command, timeout, work-dir root).
/// * `vault_root`  — Canonical absolute path to the vault root.
/// * `sources_dir` — Canonical absolute path to the sources directory.
///
/// # Errors
///
/// Returns `Err` only if an unexpected internal error occurs *after* the job
/// has already been transitioned to a terminal state.  Under normal operation
/// (including processor failures) the function returns `Ok(())`.
#[instrument(
    skip(state, config),
    fields(
        job_id = job.id,
        path   = %job.path.display(),
    )
)]
pub async fn process_job(
    job:         FileRow,
    state:       StateStore,
    config:      &ProcessorConfig,
    vault_root:  &Path,
    sources_dir: &Path,
) -> kb_core::Result<()> {
    // ── (a) Build the per-job work directory path ─────────────────────────
    //
    // Format: <work_dir_root>/<first12-hex-of-hash>-<job_id>/
    //
    // Using the leading 12 hex characters of the content hash (or "nohash"
    // when absent — which should not happen for a claimed job) plus the row
    // ID makes the directory name unique and human-readable for debugging.
    let hash_slug = job
        .content_hash
        .as_deref()
        .unwrap_or("nohash")
        .trim_start_matches("sha256:")
        .chars()
        .take(12)
        .collect::<String>();

    let dir_name = format!("{hash_slug}-{}", job.id);
    let work_dir = PathBuf::from(&config.work_dir_root).join(&dir_name);

    tracing::debug!(
        job_id   = job.id,
        work_dir = %work_dir.display(),
        command  = %config.command,
        "process_job: beginning pipeline",
    );

    // ── (b) Build ProcessorInput ──────────────────────────────────────────
    let input = ProcessorInput {
        input_path:   job.path.clone(),
        content_hash: job.content_hash.clone().unwrap_or_default(),
        vault_root:   vault_root.to_path_buf(),
        sources_dir:  sources_dir.to_path_buf(),
        work_dir:     work_dir.clone(),
        job_id:       job.id,
        attempt:      job.attempts,
    };

    // ── (c) Audit event: processor_started ───────────────────────────────
    {
        let detail = serde_json::json!({
            "command":  &config.command,
            "work_dir": work_dir.display().to_string(),
            "attempt":  job.attempts,
            "hash":     job.content_hash.as_deref().unwrap_or("<none>"),
        })
        .to_string();

        let file_name = job
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| job.path.display().to_string());

        if let Err(e) = state
            .record_event(
                "info".to_string(),
                event_kind::PROCESSOR_STARTED.to_string(),
                Some(job.id),
                format!("processor starting for {file_name:?} (attempt {})", job.attempts),
                Some(detail),
            )
            .await
        {
            // Audit failure is non-fatal — log and continue.
            tracing::warn!(
                job_id = job.id,
                error  = %e,
                "failed to record processor_started audit event (continuing)",
            );
        }
    }

    // ── (d) Invoke the processor subprocess ──────────────────────────────
    //
    // `invoke_processor` handles:
    //   • creating the work_dir,
    //   • serialising `input` as JSON on stdin,
    //   • enforcing `timeout_secs` with SIGTERM → (5 s grace) → SIGKILL,
    //   • capturing stdout line-by-line,
    //   • parsing the last non-empty stdout line as a `ProcessResult`.
    let invoke_result = invoke_processor(
        &config.command,
        &input,
        &work_dir,
        config.timeout_secs,
    )
    .await;

    // ── (e–g) Handle the outcome ──────────────────────────────────────────
    let job_succeeded: bool = match invoke_result {
        // ─── Subprocess completed — inspect the ProcessResult ─────────────
        Ok(processor_output) => {
            match processor_output.result {
                // ── (e) ProcessResult::Ok — validate outputs then mark done ─
                ProcessResult::Ok { outputs, metadata } => {
                    match validate_processor_outputs(&outputs, vault_root, sources_dir) {
                        Ok(canonical_paths) => {
                            // Rebuild output records with canonical paths but
                            // preserve the original kind/bytes from the processor.
                            let records: Vec<ProcessOutput> = outputs
                                .iter()
                                .zip(canonical_paths.iter())
                                .map(|(orig, canon)| ProcessOutput {
                                    path:  canon.clone(),
                                    kind:  orig.kind.clone(),
                                    bytes: orig.bytes,
                                })
                                .collect();

                            let outputs_count = records.len();

                            match state.mark_done(job.id, records, metadata).await {
                                Ok(()) => {
                                    tracing::info!(
                                        job_id        = job.id,
                                        outputs_count = outputs_count,
                                        "job completed successfully",
                                    );
                                    true
                                }
                                Err(e) => {
                                    // mark_done failed (e.g. DB error) — the
                                    // row may still be in 'processing'; fall
                                    // back to mark_failed so it can be retried.
                                    tracing::error!(
                                        job_id = job.id,
                                        error  = %e,
                                        "mark_done failed after successful processing \
                                         — falling back to mark_failed (retryable)",
                                    );
                                    let _ = state
                                        .mark_failed(job.id, e.to_string(), true)
                                        .await;
                                    false
                                }
                            }
                        }

                        // Output path invariant violated — processor bug,
                        // non-retryable.
                        Err(validation_err) => {
                            tracing::error!(
                                job_id = job.id,
                                error  = %validation_err,
                                "processor output failed path invariant — \
                                 marking failed (non-retryable)",
                            );
                            let _ = state
                                .mark_failed(
                                    job.id,
                                    validation_err.to_string(),
                                    false, // non-retryable
                                )
                                .await;
                            false
                        }
                    }
                }

                // ── (e) ProcessResult::Error — use processor's retryable flag ─
                ProcessResult::Error { error, retryable, .. } => {
                    tracing::warn!(
                        job_id    = job.id,
                        error     = %error,
                        retryable = retryable,
                        "processor reported error result",
                    );
                    let _ = state.mark_failed(job.id, error, retryable).await;
                    false
                }
            }
        }

        // ── (f) Timeout — always retryable ───────────────────────────────
        Err(ProcessorError::Timeout { elapsed_secs }) => {
            tracing::warn!(
                job_id       = job.id,
                elapsed_secs = elapsed_secs,
                "processor timed out — marking failed (retryable)",
            );
            let _ = state
                .mark_failed(
                    job.id,
                    format!("processor timed out after {elapsed_secs}s"),
                    true,
                )
                .await;
            false
        }

        // ── (g) Other invocation errors — retryable by default ───────────
        Err(e) => {
            tracing::error!(
                job_id = job.id,
                error  = %e,
                "processor invocation error — marking failed (retryable)",
            );
            let _ = state.mark_failed(job.id, e.to_string(), true).await;
            false
        }
    };

    // ── (h) Clean work_dir on success; retain on failure ─────────────────
    if job_succeeded {
        // Best-effort removal — a failure here is non-fatal.
        if work_dir.exists() {
            match tokio::fs::remove_dir_all(&work_dir).await {
                Ok(()) => {
                    tracing::debug!(
                        job_id = job.id,
                        path   = %work_dir.display(),
                        "work_dir cleaned after successful job",
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        job_id = job.id,
                        path   = %work_dir.display(),
                        error  = %e,
                        "failed to remove work_dir after successful job (non-fatal)",
                    );
                }
            }
        }
    } else {
        tracing::debug!(
            job_id = job.id,
            path   = %work_dir.display(),
            "retaining work_dir for post-mortem debugging after job failure",
        );
    }

    // ── (i) Final audit event: 'done' or 'failed' ────────────────────────
    let (level, audit_kind, message) = if job_succeeded {
        (
            "info",
            event_kind::DONE,
            format!("job {} completed successfully", job.id),
        )
    } else {
        (
            "warn",
            event_kind::FAILED,
            format!("job {} failed", job.id),
        )
    };

    if let Err(e) = state
        .record_event(
            level.to_string(),
            audit_kind.to_string(),
            Some(job.id),
            message,
            None,
        )
        .await
    {
        tracing::warn!(
            job_id = job.id,
            error  = %e,
            "failed to record final audit event (non-fatal)",
        );
    }

    Ok(())
}

// ── WorkerPool ────────────────────────────────────────────────────────────────

/// A bounded pool of processing workers.
///
/// Internally holds:
/// - a [`Semaphore`] that caps the number of concurrent subprocesses;
/// - a [`StateStore`] handle for claiming jobs;
/// - the [`ProcessorConfig`] shared with each spawned task;
/// - a [`CancellationToken`] for coordinated shutdown;
/// - a [`Notify`] handle for instant wakeup when new work arrives;
/// - the vault root and sources directory paths (shared via [`Arc`]).
///
/// Cheaply cloneable: call [`WorkerPool::notify_new_work`] from the watcher
/// pipeline after enqueuing new work to bypass the 100 ms polling delay.
#[derive(Clone)]
pub struct WorkerPool {
    semaphore:   Arc<Semaphore>,
    state:       StateStore,
    config:      Arc<ProcessorConfig>,
    shutdown:    CancellationToken,
    notify:      Arc<Notify>,
    vault_root:  Arc<PathBuf>,
    sources_dir: Arc<PathBuf>,
}

impl WorkerPool {
    /// Create a new pool.
    ///
    /// # Arguments
    ///
    /// * `concurrency`  — Maximum number of simultaneous processor subprocesses.
    /// * `state`        — Shared state store handle (cloned for each task).
    /// * `config`       — Processor configuration shared across all workers.
    /// * `shutdown`     — Token; when cancelled the claim loop exits cleanly.
    /// * `vault_root`   — Canonical absolute path to the vault root directory.
    ///                    Should already have had `~` expanded and been
    ///                    canonicalized by the config loader.
    /// * `sources_dir`  — Canonical absolute path to the sources directory.
    pub fn new(
        concurrency:  usize,
        state:        StateStore,
        config:       ProcessorConfig,
        shutdown:     CancellationToken,
        vault_root:   PathBuf,
        sources_dir:  PathBuf,
    ) -> Self {
        Self {
            semaphore:   Arc::new(Semaphore::new(concurrency)),
            state,
            config:      Arc::new(config),
            shutdown,
            notify:      Arc::new(Notify::new()),
            vault_root:  Arc::new(vault_root),
            sources_dir: Arc::new(sources_dir),
        }
    }

    /// Wake the claim loop immediately.
    ///
    /// Call this after enqueuing new work (i.e., after
    /// [`StateStore::process_stable_file`] returns [`EnqueueOutcome::Queued`]
    /// or [`EnqueueOutcome::RequeuedRevision`]) to avoid the 100 ms polling
    /// delay.
    ///
    /// ```ignore
    /// pool.notify_new_work();   // instant wakeup; safe to call from any task
    /// ```
    pub fn notify_new_work(&self) {
        self.notify.notify_one();
    }

    /// Start the bounded claim-process loop.
    ///
    /// The loop runs until the [`CancellationToken`] is cancelled.  Each
    /// iteration:
    ///
    /// 1. Acquire a semaphore **permit** (blocks if all slots are in use).
    /// 2. Call [`StateStore::claim_next`] to atomically take a `queued` row.
    /// 3. If a job was claimed → spawn a task that calls [`process_job`] and
    ///    drops the permit when done (whether success or failure).
    /// 4. If no job was available → release the permit and wait 100 ms (or
    ///    until [`notify_new_work`] wakes us, whichever comes first) before
    ///    retrying.
    ///
    /// # Returns
    ///
    /// A `JoinHandle` for the background task.  Await it in `daemon` startup
    /// to wait for the loop to exit cleanly after shutdown.
    pub fn run(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            let concurrency = self.semaphore.available_permits();
            tracing::info!(concurrency, "worker pool started");

            loop {
                // ── Check for shutdown before doing any work ──────────────
                if self.shutdown.is_cancelled() {
                    tracing::info!("worker pool: shutdown signal received — exiting");
                    break;
                }

                // ── Acquire a permit (may wait if all slots are busy) ─────
                //
                // We select! across permit acquisition and shutdown so the
                // pool exits promptly even when all permits are taken.
                let permit = tokio::select! {
                    biased;

                    // Shutdown takes priority.
                    _ = self.shutdown.cancelled() => {
                        tracing::info!("worker pool: cancelled while waiting for permit — exiting");
                        break;
                    }

                    // Wait for a free slot.
                    permit = self.semaphore.clone().acquire_owned() => {
                        match permit {
                            Ok(p)  => p,
                            Err(_) => {
                                // Semaphore was closed — treat as shutdown.
                                tracing::warn!("worker pool: semaphore closed — exiting");
                                break;
                            }
                        }
                    }
                };

                // ── Atomically claim the next queued job ──────────────────
                let job = match self.state.claim_next().await {
                    Ok(Some(row)) => row,
                    Ok(None) => {
                        // No work right now.  Drop the permit immediately so
                        // other workers (or the next loop iteration) can use
                        // it, then wait briefly before retrying.
                        drop(permit);

                        tokio::select! {
                            biased;
                            _ = self.shutdown.cancelled() => {
                                tracing::info!(
                                    "worker pool: cancelled during idle wait — exiting"
                                );
                                break;
                            }
                            // Woken by notify_new_work() — go round immediately.
                            _ = self.notify.notified() => {}
                            // Fallback 100 ms poll.
                            _ = sleep(Duration::from_millis(100)) => {}
                        }

                        continue;
                    }
                    Err(e) => {
                        // Transient DB error; release the permit and retry
                        // after a short delay rather than crashing the pool.
                        tracing::error!(error = %e, "worker pool: claim_next failed");
                        drop(permit);
                        sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                };

                // ── Spawn a task to process the claimed job ───────────────
                let state       = self.state.clone();
                let config      = Arc::clone(&self.config);
                // Clone the PathBufs once per spawned task (O(n) in path
                // length ≈ trivial) to give the task full ownership without
                // lifetime ties to the pool struct.
                let vault_root  = (*self.vault_root).clone();
                let sources_dir = (*self.sources_dir).clone();

                tracing::debug!(
                    job_id = job.id,
                    path   = %job.path.display(),
                    "worker pool: spawning task for job",
                );

                tokio::spawn(async move {
                    // `permit` is moved into this task; it is dropped when
                    // the task completes — whether success or failure —
                    // automatically releasing the semaphore slot.
                    let _permit = permit;

                    let job_id = job.id;
                    match process_job(
                        job,
                        state.clone(),
                        &config,
                        &vault_root,
                        &sources_dir,
                    )
                    .await
                    {
                        Ok(()) => {
                            tracing::debug!(job_id, "worker task: completed");
                        }
                        Err(e) => {
                            // process_job handles all error paths internally
                            // and always returns Ok(()).  An Err here means
                            // an unexpected internal failure occurred *after*
                            // the job was already transitioned — log it and
                            // do a best-effort mark_failed in case the row is
                            // somehow still in 'processing'.
                            tracing::error!(
                                job_id,
                                error = %e,
                                "worker task: process_job returned unexpected error",
                            );
                            if let Err(mark_err) = state
                                .mark_failed(job_id, e.to_string(), true)
                                .await
                            {
                                tracing::error!(
                                    job_id,
                                    error = %mark_err,
                                    "worker task: fallback mark_failed also failed",
                                );
                            }
                        }
                    }
                });
            }

            tracing::info!("worker pool: loop exited");
        })
    }
}
