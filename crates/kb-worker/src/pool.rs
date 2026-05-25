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

// Metric names (same constants as kb_ops::metrics — duplicated here so
// kb-worker does not need to depend on kb-ops, keeping the dependency graph
// acyclic: kb-ops → kb-worker, NOT kb-worker → kb-ops).
const METRIC_PROCESSED_TOTAL: &str = "kb_processed_total";
const METRIC_FAILED_TOTAL: &str = "kb_failed_total";
const METRIC_PROCESSOR_DURATION: &str = "kb_processor_duration_seconds";

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
//
// SECURITY: `extra_env` and `cancel_token` are explicitly skipped from span
// fields.  `extra_env` carries credentials (OPENROUTER_API_KEY, etc.) loaded
// from `~/.config/knowledge-builder/secrets.env`; without this skip the
// `tracing` JSON formatter would serialise the entire map into every span
// entry written to disk.  `cancel_token` is skipped because its Debug impl
// adds no operational value.
#[instrument(
    skip(state, config, extra_env, cancel_token),
    fields(
        job_id = job.id,
        path   = %job.path.display(),
    )
)]
pub async fn process_job(
    job:          FileRow,
    state:        StateStore,
    config:       &ProcessorConfig,
    extra_env:    &std::collections::BTreeMap<String, String>,
    vault_root:   &Path,
    sources_dir:  &Path,
    agent_root:   &Path,
    cancel_token: CancellationToken,
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
        agent_root:   agent_root.to_path_buf(),
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
    let processor_start = std::time::Instant::now();
    let invoke_result = invoke_processor(
        &config.command,
        &input,
        &work_dir,
        config.timeout_secs,
        extra_env,
        agent_root,
        cancel_token.clone(),
    )
    .await;
    let processor_elapsed = processor_start.elapsed().as_secs_f64();
    // Record processor wall-clock time regardless of outcome.
    metrics::histogram!(METRIC_PROCESSOR_DURATION).record(processor_elapsed);

    // ── (e–g) Handle the outcome ──────────────────────────────────────────
    // Default: clean work_dir on success.  The processor can override
    // this by setting `retain_work_dir: true` in its result metadata
    // (e.g. shadow-mode runs preserve the .kb-plan.jsonl for `kb show`).
    let mut retain_work_dir_on_success: bool = false;

    let job_succeeded: bool = match invoke_result {
        // ─── Subprocess completed — inspect the ProcessResult ─────────────
        Ok(processor_output) => {
            match processor_output.result {
                // ── (e) ProcessResult::Ok — validate outputs then mark done ─
                ProcessResult::Ok { outputs, metadata } => {
                    // Extract retain-flag BEFORE `metadata` is moved into
                    // `state.mark_done(…)` below.
                    if let Some(meta) = metadata.as_ref() {
                        if let Some(flag) = meta.get("retain_work_dir").and_then(|v| v.as_bool()) {
                            retain_work_dir_on_success = flag;
                        }
                    }
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
                                    metrics::counter!(METRIC_PROCESSED_TOTAL).increment(1);
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
                                    metrics::counter!(METRIC_FAILED_TOTAL).increment(1);
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
                            metrics::counter!(METRIC_FAILED_TOTAL).increment(1);
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
                    metrics::counter!(METRIC_FAILED_TOTAL).increment(1);
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
            metrics::counter!(METRIC_FAILED_TOTAL).increment(1);
            false
        }

        // ── Cancelled by daemon shutdown ──────────────────────────────────
        //
        // The subprocess has already been signalled and reaped inside
        // `invoke_processor`.  Leave the row in `processing` and return
        // early; daemon shutdown calls `recover_in_flight_with_config`
        // which resets it to `queued` for retry on next start, so no
        // `attempts` increment is consumed for shutdown-induced
        // cancellations.
        Err(ProcessorError::Cancelled) => {
            tracing::info!(
                job_id = job.id,
                "processor cancelled by daemon shutdown — row left in `processing` for recovery",
            );
            // Return early: skip the cleanup + final audit event paths so
            // the post-mortem work_dir is retained for inspection.
            return Ok(());
        }

        // ── (g) Other invocation errors — retryable by default ───────────
        Err(e) => {
            tracing::error!(
                job_id = job.id,
                error  = %e,
                "processor invocation error — marking failed (retryable)",
            );
            let _ = state.mark_failed(job.id, e.to_string(), true).await;
            metrics::counter!(METRIC_FAILED_TOTAL).increment(1);
            false
        }
    };

    // ── (h) Clean work_dir on success; retain on failure or when the
    //          processor explicitly asked for retention (shadow mode) ────
    if job_succeeded && !retain_work_dir_on_success {
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
    } else if retain_work_dir_on_success {
        tracing::debug!(
            job_id = job.id,
            path   = %work_dir.display(),
            "retaining work_dir on success per processor's retain_work_dir flag",
        );
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
    /// Extra environment variables (loaded from the secrets file by the
    /// daemon) that are forwarded into every processor subprocess via
    /// `Command::envs`.  Holds credentials such as `KB_LLM_MODEL` and
    /// `OPENROUTER_API_KEY` so they reach the processor regardless of
    /// whether the daemon is run by `launchd` or via
    /// `kb daemon --foreground`.
    extra_env:   Arc<std::collections::BTreeMap<String, String>>,
    shutdown:    CancellationToken,
    notify:      Arc<Notify>,
    vault_root:  Arc<PathBuf>,
    sources_dir: Arc<PathBuf>,
    /// Agent's mutation root.  Forwarded to the processor subprocess as
    /// the ``KB_AGENT_ROOT`` env var; the kb-obsidian wrapper rejects any
    /// write whose target path resolves outside this tree.
    agent_root:  Arc<PathBuf>,
}

impl WorkerPool {
    /// Create a new pool.
    ///
    /// # Arguments
    ///
    /// * `concurrency`  — Maximum number of simultaneous processor subprocesses.
    /// * `state`        — Shared state store handle (cloned for each task).
    /// * `config`       — Processor configuration shared across all workers.
    /// * `extra_env`    — Key/value pairs to inject into every processor
    ///                    subprocess (typically the contents of
    ///                    `~/.config/knowledge-builder/secrets.env`).
    /// * `shutdown`     — Token; when cancelled the claim loop exits cleanly.
    /// * `vault_root`   — Canonical absolute path to the vault root directory.
    ///                    Should already have had `~` expanded and been
    ///                    canonicalized by the config loader.
    /// * `sources_dir`  — Canonical absolute path to the sources directory.
    pub fn new(
        concurrency:  usize,
        state:        StateStore,
        config:       ProcessorConfig,
        extra_env:    std::collections::BTreeMap<String, String>,
        shutdown:     CancellationToken,
        vault_root:   PathBuf,
        sources_dir:  PathBuf,
        agent_root:   PathBuf,
    ) -> Self {
        Self {
            semaphore:   Arc::new(Semaphore::new(concurrency)),
            state,
            config:      Arc::new(config),
            extra_env:   Arc::new(extra_env),
            shutdown,
            notify:      Arc::new(Notify::new()),
            vault_root:  Arc::new(vault_root),
            sources_dir: Arc::new(sources_dir),
            agent_root:  Arc::new(agent_root),
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
                let agent_root  = (*self.agent_root).clone();
                // Clone the daemon-wide shutdown token so the spawned task
                // can react to Ctrl-C / SIGTERM while a processor subprocess
                // is in-flight (kills the child's process group, preventing
                // orphan Python processes after `kb daemon` exits).
                let shutdown    = self.shutdown.clone();
                // Cheap Arc clone — extra_env is shared read-only across
                // every job in the pool's lifetime.
                let extra_env   = Arc::clone(&self.extra_env);

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
                        &extra_env,
                        &vault_root,
                        &sources_dir,
                        &agent_root,
                        shutdown.clone(),
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

// ── Regression tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod secrets_leak_tests {
    //! Defends against re-introducing the credential-leak bug where the
    //! `#[instrument]` macro on `process_job` captured `extra_env` (which
    //! holds OPENROUTER_API_KEY etc.) into every span field, writing
    //! credentials to disk on every job.

    /// Read this file and assert the `#[instrument]` block on `process_job`
    /// continues to skip `extra_env` and `cancel_token`.  This is a
    /// source-level invariant — the only foolproof way to enforce it is to
    /// inspect the source.
    #[test]
    fn instrument_skips_extra_env_and_cancel_token() {
        let src = include_str!("pool.rs");

        // Find the `#[instrument(` block immediately preceding `pub async fn process_job`.
        let fn_idx = src
            .find("pub async fn process_job(")
            .expect("process_job must exist");
        let head  = &src[..fn_idx];
        let inst_idx = head.rfind("#[instrument(")
            .expect("#[instrument(...)] must precede process_job");
        let inst = &head[inst_idx..];

        // Pull out the `skip(...)` clause.
        let skip_start = inst.find("skip(").expect("instrument must have skip(...)");
        let skip_end   = inst[skip_start..].find(')').expect("skip(...) must close");
        let skip       = &inst[skip_start..skip_start + skip_end];

        for needle in ["extra_env", "cancel_token"] {
            assert!(
                skip.contains(needle),
                "process_job's #[instrument(skip(...))] must include `{needle}` to \
                 prevent credentials / sensitive state from being written to log \
                 files.  Current skip clause: {skip}"
            );
        }
    }
}
