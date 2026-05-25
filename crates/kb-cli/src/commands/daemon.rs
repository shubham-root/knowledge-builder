//! `kb daemon [--foreground]` — start the Knowledge Builder daemon.
//!
//! Implements the full startup and shutdown orchestration described in
//! PLAN.md §9.6 and §12.
//!
//! # Startup sequence
//!
//! 1. Load and validate configuration.
//! 2. Initialise structured logging (foreground mode adds stderr layer).
//! 3. Acquire singleton process lock ([`kb_core::DaemonLock::acquire`]).
//! 4. Open the SQLite state store.
//! 5. Record `daemon_started` audit event.
//! 6. Run crash recovery ([`kb_core::StateStore::recover_in_flight_with_config`]).
//! 7. Build and start the detection pipeline (FSEvents + stability + hasher).
//! 8. Build and start the periodic scanner (initial scan fires immediately).
//! 9. Start the worker pool.
//! 10. (HTTP server deferred to T28.)
//! 11. Log "Knowledge Builder daemon started successfully".
//!
//! # Main loop
//!
//! Awaits SIGTERM, SIGINT, or SIGHUP (Unix).  SIGHUP logs
//! "config reload not yet supported" and continues running without stopping the
//! daemon.
//!
//! # Shutdown sequence
//!
//! On SIGTERM/SIGINT:
//!
//! a. Log "Shutting down..."
//! b. Record `daemon_stopping` audit event.
//! c. Cancel the [`CancellationToken`] (propagates to pipeline, scanner, pool,
//!    **and every in-flight processor subprocess**).  Each running
//!    `invoke_processor` reacts by sending `SIGTERM` to its child's process
//!    group, waiting 5 s, then `SIGKILL` — guaranteeing no Python
//!    subprocesses survive as orphans (PPID=1) after the daemon exits.
//! d. Wait up to 5 s for the worker-pool claim loop to exit, then poll the
//!    DB until all in-flight processor jobs complete (or the 30 s budget
//!    runs out).  Cancelled jobs leave their row in `processing` (no
//!    `attempts` increment) so step g resets them to `queued` for retry.
//! e. If the budget runs out, log a warning — recovery (step g) handles
//!    stale rows.
//! f. Wait up to 5 s for the detection pipeline and scanner tasks to exit.
//! g. Call `recover_in_flight_with_config` to reset any remaining
//!    `processing` rows to `queued` so the next daemon start retries them.
//! h. Drop the singleton lock (explicit drop for deterministic release).
//! i. Log "Daemon stopped cleanly".

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use globset::GlobSetBuilder;
use tracing::{info, warn};

use kb_core::{event_kind, DaemonLock, StateStore};
use kb_watcher::{CancellationToken, DetectionPipeline, PeriodicScanner};
use kb_worker::WorkerPool;

// ── CLI args ──────────────────────────────────────────────────────────────────

/// Arguments for the `kb daemon` subcommand.
#[derive(Args, Debug)]
pub struct DaemonArgs {
    /// Log to stderr in addition to the rotating JSON log file.
    ///
    /// When omitted the daemon runs silently in the background (launchd
    /// redirects stdout/stderr to configured log files).  Pass `--foreground`
    /// when running from a terminal for interactive log output.
    #[arg(long)]
    pub foreground: bool,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(args: DaemonArgs) -> Result<()> {
    // ── 1. Load + validate configuration ─────────────────────────────────────
    //
    // `Config::load()` runs all 8 startup validation checks (§6 of PLAN.md).
    // If any check fails it prints an actionable error and returns Err; the
    // caller should then exit with a non-zero code so launchd backs off.
    let config = kb_core::Config::load().context(
        "Failed to start daemon: configuration is invalid.\n\
         Run `kb doctor` for a detailed diagnosis.",
    )?;

    // ── 2. Initialise logging ─────────────────────────────────────────────────
    //
    // `_guard` MUST stay alive for the entire process lifetime; dropping it
    // would flush and close the rotating file appender prematurely, silencing
    // all subsequent log output.
    let _guard = kb_core::init_logging(&config.paths.log_dir, &config.ops, args.foreground)
        .context("Failed to initialise logging")?;

    info!(
        foreground  = args.foreground,
        vault_root  = %config.paths.vault_root,
        sources_dir = %config.paths.sources_dir,
        db_path     = %config.paths.db_path,
        "daemon initialising",
    );

    // ── 3. Acquire singleton lock ─────────────────────────────────────────────
    //
    // The `flock`-based lock ensures only one daemon instance runs at a time
    // (PLAN.md §3.13 / T23).  `_lock` must be a *named* binding — not `let _`
    // (which drops immediately) — so that the OS-level lock is held for the
    // entire daemon runtime.  It is explicitly released via `drop(_lock)` near
    // the end of the shutdown sequence for deterministic ordering.
    let _lock = DaemonLock::acquire(Path::new(&config.paths.db_path))
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    info!(
        lock_path = %_lock.lock_path().display(),
        "singleton lock acquired",
    );

    // ── 4. Open state store ───────────────────────────────────────────────────
    //
    // The actor starts a dedicated OS thread that owns the `rusqlite::Connection`
    // for the lifetime of the process.  All state operations go through it.
    let state = StateStore::new(Path::new(&config.paths.db_path), &config.worker.backoff_secs)
        .await
        .context("Failed to open state store")?;

    // ── 5. Record daemon_started audit event ─────────────────────────────────
    if let Err(e) = state
        .record_event(
            "info".to_string(),
            event_kind::DAEMON_STARTED.to_string(),
            None,
            "daemon started".to_string(),
            None,
        )
        .await
    {
        // Audit failure is non-fatal — the daemon can still run without it.
        warn!(error = %e, "Failed to record daemon_started audit event (non-fatal)");
    }

    // ── 6. Crash recovery ─────────────────────────────────────────────────────
    //
    // Resets rows that were left in `processing` by a previous crash back to
    // `queued` so they are retried.  Must complete before workers start
    // claiming (§3.10).
    let recovered = state
        .recover_in_flight_with_config(config.worker.max_attempts as i32)
        .await
        .context("Crash recovery failed")?;

    if recovered > 0 {
        info!(
            recovered,
            "Recovered in-flight jobs from previous crash",
        );
    } else {
        info!("No in-flight jobs to recover on startup");
    }

    // ── Shared cancellation token ─────────────────────────────────────────────
    //
    // All long-running daemon tasks share this token.  Cancelling it signals a
    // graceful shutdown across the pipeline, periodic scanner, and worker pool.
    let shutdown = CancellationToken::new();

    // ── 7. Start the detection pipeline ──────────────────────────────────────
    //
    // The pipeline wires:
    //   FSEvents watcher → StabilityTracker → SHA-256 hasher → StateStore
    // It applies the §3.3 dedup rules to every stable file it discovers.
    let pipeline = DetectionPipeline::new(&config, state.clone())
        .context("Failed to build detection pipeline")?;

    // The scanner injects paths into the same `StabilityTracker` used by the
    // FSEvents watcher.  The sender MUST be obtained before `run()` consumes
    // `pipeline`.
    let scanner_path_tx = pipeline.path_sender();

    let pipeline_handle = pipeline.run(shutdown.clone());

    // ── 8. Build and start the periodic scanner ───────────────────────────────
    //
    // The scanner runs an initial full scan of `sources_dir` immediately,
    // then repeats every `poll_interval_secs`.  This is the correctness
    // backstop for events missed during macOS sleep, iCloud materialisation,
    // and any FSEvents coalescing edge cases (PLAN.md §3.4 / §9.5).
    let ignore_globs = build_ignore_glob_set(&config.watch.ignore_globs)
        .context("Failed to compile ignore_glob patterns from config")?;

    let scanner = PeriodicScanner::new(
        PathBuf::from(&config.paths.sources_dir),
        config.watch.extensions.clone(),
        ignore_globs,
        Duration::from_secs(config.watch.poll_interval_secs),
        state.clone(),
        scanner_path_tx,
    );

    let scanner_handle = scanner.run(shutdown.clone());

    // ── 8b. Load processor secrets (single source of truth) ──────────────────
    //
    // `~/.config/knowledge-builder/secrets.env` holds the env vars that the
    // processor subprocess needs (e.g. `KB_LLM_MODEL`, `OPENROUTER_API_KEY`).
    // Loading them here — inside the daemon — means foreground execution and
    // launchd-managed execution behave identically, and the LaunchAgent
    // plist no longer needs to carry secrets in plaintext.
    //
    // Missing file → silent success (empty map).  Permissive perms (group/
    // world readable) trigger a warning but do not block startup.
    let secrets = match kb_core::load_secrets() {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "Failed to load secrets file (continuing without it)");
            kb_core::SecretsLoad::default()
        }
    };
    if secrets.loaded {
        if secrets.insecure_perms {
            warn!(
                path = %secrets.path.display(),
                mode = format!("{:o}", secrets.mode.unwrap_or(0)),
                "secrets file is group/world-accessible — run: chmod 600 <path>",
            );
        }
        info!(
            path = %secrets.path.display(),
            keys = ?secrets.keys(),
            "loaded {} key(s) from secrets file",
            secrets.entries.len(),
        );
    } else {
        info!(
            path = %secrets.path.display(),
            "no secrets file found (processor will use only inherited env)",
        );
    }

    // ── 9. Start the worker pool ──────────────────────────────────────────────
    //
    // Workers atomically claim `queued` rows and process them via the
    // configured processor subprocess.  The pool is bounded by
    // `config.worker.concurrency` permits.
    let pool = WorkerPool::new(
        config.worker.concurrency,
        state.clone(),
        config.processor.clone(),
        secrets.entries,
        shutdown.clone(),
        PathBuf::from(&config.paths.vault_root),
        PathBuf::from(&config.paths.sources_dir),
        PathBuf::from(&config.paths.agent_root),
    );

    let pool_handle = pool.run();

    // ── 10. HTTP server (deferred) ────────────────────────────────────────────
    //
    // The `kb-ops` HTTP server (axum, T28) will be started here once it is
    // ready.  For now the daemon is fully functional without HTTP; `kb`
    // commands read the DB directly in offline mode.

    // ── Startup complete ───────────────────────────────────────────────────────
    info!("Knowledge Builder daemon started successfully");

    if args.foreground {
        eprintln!(
            "Knowledge Builder daemon started  (vault={})  — Ctrl-C to stop",
            config.paths.vault_root,
        );
    }

    // ── Main loop: wait for shutdown signal ───────────────────────────────────
    wait_for_shutdown().await;

    // ═════════════════════════════════════════════════════════════════════════
    //                         SHUTDOWN SEQUENCE
    // ═════════════════════════════════════════════════════════════════════════

    // a. Log.
    info!("Shutting down...");
    if args.foreground {
        eprintln!("\nShutting down...");
    }

    // b. Record daemon_stopping audit event.
    if let Err(e) = state
        .record_event(
            "info".to_string(),
            event_kind::DAEMON_STOPPING.to_string(),
            None,
            "daemon stopping".to_string(),
            None,
        )
        .await
    {
        warn!(error = %e, "Failed to record daemon_stopping audit event (non-fatal)");
    }

    // c. Cancel the CancellationToken.
    //
    // This propagates to:
    //   • the detection pipeline (bridge + processor tasks exit on next select)
    //   • the periodic scanner (exits on next interval or immediately if idle)
    //   • the worker-pool claim loop (breaks out of the select immediately)
    shutdown.cancel();

    // d. Wait for the worker-pool claim loop to exit (fast after cancellation),
    //    then poll the DB until all in-flight processor jobs complete.
    //
    // Note: the pool's JoinHandle tracks the *claim loop* task only.  Per-job
    // tasks are spawned independently inside the pool and complete on their own.
    // We use the DB's `processing` count as the ground truth for job completion.
    match tokio::time::timeout(Duration::from_secs(5), pool_handle).await {
        Ok(Ok(())) => info!("Worker pool claim loop stopped cleanly"),
        Ok(Err(e)) => warn!(error = %e, "Worker pool task panicked"),
        Err(_) => warn!("Worker pool claim loop did not exit within 5s"),
    }

    // Now wait up to 30 s for any in-flight jobs to drain.
    wait_for_jobs_to_drain(&state, Duration::from_secs(30)).await;

    // e. Each in-flight processor subprocess has been signalled by step (c)
    //    via its job's `CancellationToken`-aware `invoke_processor` call,
    //    which performs SIGTERM → 5 s grace → SIGKILL on the child's process
    //    group.  After the 30 s drain window any rows still in `processing`
    //    (e.g. because a processor swallowed SIGTERM) are reset by recovery
    //    in step (g) without consuming an `attempts` slot.

    // f. Wait for the detection pipeline and scanner to tear down.
    let _ = tokio::time::timeout(Duration::from_secs(5), pipeline_handle).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), scanner_handle).await;
    info!("Detection pipeline and scanner stopped");

    // g. Mark any remaining in-flight rows back to `queued`.
    //
    // This is the same crash-recovery path used at startup.  Any row still
    // in `processing` here (because a subprocess outlived the drain window)
    // will be retried on the next daemon start.
    match state
        .recover_in_flight_with_config(config.worker.max_attempts as i32)
        .await
    {
        Ok(0) => info!("No lingering in-flight jobs at shutdown"),
        Ok(n) => info!(n, "Reset lingering in-flight jobs to queued for next startup"),
        Err(e) => warn!(error = %e, "Failed to reset lingering in-flight jobs (non-fatal)"),
    }

    // h. Drop the singleton lock (explicit for deterministic release).
    //
    // The `_lock` binding keeps the lock alive until here; `drop` moves it and
    // triggers the `Drop` impl which calls `fs2::FileExt::unlock()` then closes
    // the file descriptor.  A second daemon instance can acquire the lock after
    // this point.
    drop(_lock);

    // i. Done.
    info!("Daemon stopped cleanly");
    if args.foreground {
        eprintln!("Daemon stopped cleanly");
    }

    Ok(())
}

// ── Helper: compile ignore-glob patterns ─────────────────────────────────────

/// Compile the configured `ignore_globs` patterns into a [`globset::GlobSet`].
///
/// Checked at daemon start-up so that misconfigured patterns produce an
/// immediate, actionable error rather than silently failing to filter files.
fn build_ignore_glob_set(patterns: &[String]) -> Result<globset::GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = globset::Glob::new(pattern)
            .with_context(|| format!("Invalid ignore_glob pattern: {pattern:?}"))?;
        builder.add(glob);
    }
    builder.build().context("Failed to build ignore_glob set")
}

// ── Helper: drain in-flight jobs ─────────────────────────────────────────────

/// Poll the state store until `stats.processing == 0` or `timeout` expires.
///
/// This is the daemon's signal that all actively running processor subprocesses
/// have finished and their rows have been transitioned out of `processing`.
/// We use the DB as ground truth rather than tracking task handles because the
/// worker pool spawns per-job tasks independently.
async fn wait_for_jobs_to_drain(state: &StateStore, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        match state.stats().await {
            Ok(stats) if stats.processing == 0 => {
                info!("All in-flight jobs completed before shutdown timeout");
                return;
            }
            Ok(stats) => {
                if tokio::time::Instant::now() >= deadline {
                    // e. Timeout warning (PLAN §12 step e).
                    warn!(
                        in_flight = stats.processing,
                        "Shutdown timeout reached; {} job(s) still in-flight. \
                         They will be reset to queued and retried on next startup.",
                        stats.processing,
                    );
                    return;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "Failed to query in-flight job count during shutdown; proceeding",
                );
                return;
            }
        }
    }
}

// ── Signal handling ───────────────────────────────────────────────────────────

/// Park the current task until `SIGINT` (Ctrl-C) or `SIGTERM` is received.
///
/// `SIGHUP` is caught in the select loop and logs
/// "config reload not yet supported" without stopping the daemon.
///
/// On non-Unix platforms only `SIGINT` (via `ctrl_c()`) is available.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        // Install SIGINT handler (Ctrl-C).
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to install SIGINT handler: {e}; falling back to ctrl_c()");
                tokio::signal::ctrl_c()
                    .await
                    .expect("Failed to listen for ctrl_c");
                return;
            }
        };

        // Install SIGTERM handler (sent by launchd on service stop / system
        // shutdown, and by `kill <pid>`).
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to install SIGTERM handler: {e}; waiting for SIGINT only");
                sigint.recv().await;
                return;
            }
        };

        // Install SIGHUP handler (config-reload convention).
        // If installation fails we continue with just SIGINT + SIGTERM.
        let mut sighup_opt: Option<tokio::signal::unix::Signal> =
            match signal(SignalKind::hangup()) {
                Ok(s) => Some(s),
                Err(e) => {
                    warn!("Failed to install SIGHUP handler: {e}; SIGHUP will not be handled");
                    None
                }
            };

        if let Some(ref mut sighup) = sighup_opt {
            // Full three-signal loop.
            loop {
                tokio::select! {
                    biased;

                    _ = sigterm.recv() => {
                        info!("received SIGTERM");
                        break;
                    }
                    _ = sigint.recv() => {
                        info!("received SIGINT");
                        break;
                    }
                    _ = sighup.recv() => {
                        warn!("received SIGHUP — config reload not yet supported");
                        // Do not break; the daemon continues running.
                    }
                }
            }
        } else {
            // Fallback: SIGINT + SIGTERM only.
            tokio::select! {
                biased;
                _ = sigterm.recv() => info!("received SIGTERM"),
                _ = sigint.recv()  => info!("received SIGINT"),
            }
        }
    }

    #[cfg(not(unix))]
    {
        // On Windows / WASM: only Ctrl-C is available.
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for ctrl_c");
    }
}
