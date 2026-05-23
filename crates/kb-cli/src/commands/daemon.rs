//! `kb daemon [--foreground]` — start the Knowledge Builder daemon.
//!
//! **Current implementation (T8 skeleton):**
//! - Loads and validates configuration.
//! - Initialises structured logging (JSON file layer + stderr pretty layer when
//!   `--foreground` is passed).
//! - Emits a "daemon idle" message and waits for `Ctrl-C` / `SIGTERM`.
//! - On signal: logs "daemon stopping" and exits cleanly.
//!
//! **Future (T24 — startup orchestration):**
//! singleton lock → crash recovery → initial full scan → watcher + workers +
//! HTTP ops server.

use anyhow::{Context, Result};
use clap::Args;
use tracing::{info, warn};

#[derive(Args, Debug)]
pub struct DaemonArgs {
    /// Log to stderr in addition to the rotating JSON log file.
    ///
    /// When omitted the daemon runs silently in the background (launchd
    /// redirects stdout/stderr to log files).  Pass `--foreground` when
    /// running from a terminal for interactive log output.
    #[arg(long)]
    pub foreground: bool,
}

pub async fn run(args: DaemonArgs) -> Result<()> {
    // ── 1. Load + validate configuration ─────────────────────────────────────
    let config = kb_core::Config::load().context(
        "Failed to start daemon: configuration is invalid.\n\
         Run `kb doctor` for a detailed diagnosis.",
    )?;

    // ── 2. Initialise logging ─────────────────────────────────────────────────
    // `_guard` must stay alive for the process lifetime; dropping it would
    // flush and close the file appender prematurely.
    let _guard = kb_core::init_logging(&config.paths.log_dir, &config.ops, args.foreground)
        .context("Failed to initialise logging")?;

    // ── 3. Emit startup telemetry ─────────────────────────────────────────────
    info!(
        foreground = args.foreground,
        vault_root  = %config.paths.vault_root,
        sources_dir = %config.paths.sources_dir,
        db_path     = %config.paths.db_path,
        "daemon started"
    );

    if args.foreground {
        eprintln!(
            "daemon idle  (vault={})  — Ctrl-C to stop",
            config.paths.vault_root
        );
    } else {
        println!("daemon idle");
    }

    // ── 4. Wait for shutdown signal ───────────────────────────────────────────
    // T24 will replace this with the full startup orchestration (watcher,
    // worker pool, HTTP server).  For now we just park until the user hits
    // Ctrl-C or launchd sends SIGTERM.
    wait_for_shutdown().await;

    // ── 5. Graceful shutdown ──────────────────────────────────────────────────
    info!("daemon stopping");
    if args.foreground {
        eprintln!("\ndaemon stopping");
    }

    Ok(())
}

// ── Signal handling ───────────────────────────────────────────────────────────

/// Park the current task until `SIGINT` (Ctrl-C) or `SIGTERM` is received.
///
/// On non-Unix platforms only `SIGINT` is available; that is fine for local
/// development.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                warn!("failed to install SIGINT handler: {e}; falling back to ctrl_c()");
                tokio::signal::ctrl_c()
                    .await
                    .expect("failed to listen for ctrl_c");
                return;
            }
        };

        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!("failed to install SIGTERM handler: {e}");
                // Still wait for SIGINT
                sigint.recv().await;
                return;
            }
        };

        tokio::select! {
            _ = sigint.recv()  => info!("received SIGINT"),
            _ = sigterm.recv() => info!("received SIGTERM"),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl_c");
    }
}
