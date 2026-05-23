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
//! # Placeholder processing
//!
//! The per-job [`process_job`] function is a stub to be filled in by **T19**.
//! It has the correct signature and is public so T19 can replace the body
//! without changing any call-site signatures.

use std::sync::Arc;

use kb_core::{config::ProcessorConfig, state::StateStore, FileRow};
use tokio::{
    sync::{Notify, Semaphore},
    task::JoinHandle,
    time::{sleep, Duration},
};
use tokio_util::sync::CancellationToken;

// ── Public process_job placeholder ───────────────────────────────────────────

/// Process a single claimed job.
///
/// **This function is a placeholder** — the full implementation will be
/// provided in T19 (`kb-worker::worker`).  For now it logs the job and
/// returns `Ok(())` so that the worker pool can be built and `cargo check`
/// passes.
///
/// # Arguments
/// * `job`    — The file row claimed from the state store.
/// * `state`  — A cloneable handle to the state store actor.
/// * `config` — Processor configuration (command, timeout, work-dir root).
///
/// # Errors
/// Returns an error if the job could not be processed.  The caller is
/// responsible for calling [`StateStore::mark_failed`] or
/// [`StateStore::mark_done`] based on the outcome.
pub async fn process_job(
    job: FileRow,
    state: StateStore,
    config: &ProcessorConfig,
) -> kb_core::Result<()> {
    // T19 will replace this body with the full pipeline:
    //   work_dir creation → invoke_processor → parse → validate → record_outputs → mark_done/failed
    tracing::info!(
        job_id  = job.id,
        path    = %job.path.display(),
        hash    = job.content_hash.as_deref().unwrap_or("<none>"),
        command = %config.command,
        "process_job: placeholder — full implementation in T19",
    );
    let _ = state; // suppress unused-variable warning until T19
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
/// - a [`Notify`] handle for instant wakeup when new work arrives.
#[derive(Clone)]
pub struct WorkerPool {
    semaphore: Arc<Semaphore>,
    state:     StateStore,
    config:    Arc<ProcessorConfig>,
    shutdown:  CancellationToken,
    notify:    Arc<Notify>,
}

impl WorkerPool {
    /// Create a new pool.
    ///
    /// # Arguments
    /// * `concurrency` — Maximum number of simultaneous processor subprocesses.
    /// * `state`       — Shared state store handle (cloned for each task).
    /// * `config`      — Processor configuration shared across all workers.
    /// * `shutdown`    — Token; when cancelled the claim loop exits cleanly.
    pub fn new(
        concurrency: usize,
        state: StateStore,
        config: ProcessorConfig,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(concurrency)),
            state,
            config: Arc::new(config),
            shutdown,
            notify: Arc::new(Notify::new()),
        }
    }

    /// Returns a [`Notify`]-based handle that immediately wakes the claim loop.
    ///
    /// Call this after enqueuing new work to avoid the 100 ms polling delay.
    ///
    /// ```ignore
    /// pool.notify_new_work();   // instant wakeup; safe to call from any task
    /// ```
    pub fn notify_new_work(&self) {
        self.notify.notify_one();
    }

    /// Start the bounded claim-process loop.
    ///
    /// The loop runs until [`CancellationToken`] is cancelled.  Each
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
                        // other workers (or the next loop iteration) can use it,
                        // then wait briefly before retrying.
                        drop(permit);

                        tokio::select! {
                            biased;
                            _ = self.shutdown.cancelled() => {
                                tracing::info!("worker pool: cancelled during idle wait — exiting");
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
                let state  = self.state.clone();
                let config = Arc::clone(&self.config);

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
                    match process_job(job, state.clone(), &config).await {
                        Ok(()) => {
                            tracing::debug!(job_id, "worker task: completed successfully");
                        }
                        Err(e) => {
                            // process_job is expected to call mark_failed
                            // internally (T19).  Log any error that escapes.
                            tracing::error!(
                                job_id,
                                error = %e,
                                "worker task: process_job returned error",
                            );
                            // Best-effort: mark as failed if the job somehow
                            // escaped without being transitioned (guards
                            // against T19 implementation bugs leaving rows
                            // stuck in 'processing').
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
