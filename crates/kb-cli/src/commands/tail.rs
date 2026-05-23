//! `kb tail [--level info|warn|error] [--kind <event_kind>]`
//!
//! Stream audit events in real time.
//!
//! **HTTP-first (SSE):** When the daemon is running, subscribes to
//! `GET /tail` (Server-Sent Events) for zero-polling, push-based delivery.
//!
//! **DB fallback (polling):** When offline, polls the `events` table every
//! 500 ms using the highest seen `id` as a cursor.
//!
//! # Output format
//! ```text
//! [2024-01-15 14:32:01] INFO  queued       | paper.pdf - Enqueued for processing
//! [2024-01-15 14:32:45] INFO  done         | paper.pdf - Processing complete (2 outputs)
//! [2024-01-15 14:33:01] ERROR failed       | report.docx - Processor timeout after 1800s
//! ```
//!
//! # Color mapping
//! - `INFO`  → green
//! - `WARN`  → yellow
//! - `ERROR` → red
//!
//! # Startup behaviour
//! Both modes show the last 20 events already in the database (backfill),
//! then stream new events.
//!
//! # DB-not-found handling
//! In DB mode, if the database does not exist yet, `kb tail` waits (retrying
//! every 2 s) until the daemon creates it.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;
use futures_util::StreamExt;
use kb_core::{AuditEvent, StateStore, config::load_raw};
use tokio::time::{sleep, Duration};

use crate::client::DaemonClient;

/// Level ordering (lower index = lower severity).
const LEVELS: &[&str] = &["info", "warn", "error"];

/// Number of backfill events to show on start.
const BACKFILL_COUNT: i64 = 20;

/// How often to poll the DB for new events in offline mode.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long to wait between retries when the DB is not available yet.
const DB_RETRY_INTERVAL: Duration = Duration::from_secs(2);

// ─── CLI args ─────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct TailArgs {
    /// Minimum event level to show: info, warn, or error.
    #[arg(long, default_value = "info")]
    pub level: String,

    /// Filter by exact event kind (e.g. queued, done, failed, recovered, …).
    #[arg(long)]
    pub kind: Option<String>,
}

// ─── Entry point ──────────────────────────────────────────────────────────────

pub async fn run(args: TailArgs) -> Result<()> {
    // Validate --level early so the user gets an actionable error.
    let min_level = args.level.to_lowercase();
    let min_level_idx = level_index(&min_level).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown level '{}'. Valid values: info, warn, error",
            args.level
        )
    })?;

    let config = load_raw().context("failed to load configuration")?;

    if let Some(client) = DaemonClient::try_connect(&config.ops.http_bind).await {
        // ── SSE mode (daemon running) ─────────────────────────────────────────
        run_sse_tail(&client, &args, min_level_idx).await
    } else {
        // ── DB polling mode (daemon offline) ──────────────────────────────────
        let db_path = PathBuf::from(&config.paths.db_path);
        let backoff  = config.worker.backoff_secs.clone();
        let store   = open_store_with_retry(&db_path, &backoff).await?;
        run_db_tail(store, &args, min_level_idx).await
    }
}

// ─── SSE mode ────────────────────────────────────────────────────────────────

/// Stream events via SSE when the daemon is running.
async fn run_sse_tail(
    client:        &DaemonClient,
    args:          &TailArgs,
    min_level_idx: usize,
) -> Result<()> {
    let stream = client.tail(args.kind.as_deref()).await?;
    tokio::pin!(stream);

    // ── Ctrl-C handler ────────────────────────────────────────────────────────
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut once = Some(shutdown_tx);
        ctrlc_once(move || {
            if let Some(tx) = once.take() {
                let _ = tx.send(());
            }
        });
    }

    eprintln!(
        "{}",
        "Connected to daemon — streaming live events (Ctrl-C to stop)…".dimmed()
    );

    loop {
        tokio::select! {
            item = stream.next() => {
                match item {
                    Some(Ok(event)) => {
                        if passes_level_filter(&event.level, min_level_idx) {
                            print_event(&event);
                        }
                    }
                    Some(Err(e)) => {
                        eprintln!("{}", format!("warn: SSE error: {e}").yellow());
                    }
                    None => {
                        // Server closed the connection.
                        eprintln!("{}", "SSE stream ended (daemon may have stopped).".yellow());
                        break;
                    }
                }
            }
            _ = &mut shutdown_rx => {
                break;
            }
        }
    }

    eprintln!("\nkb tail: exiting.");
    Ok(())
}

// ─── DB polling mode ──────────────────────────────────────────────────────────

/// Poll the `events` table every 500 ms when the daemon is not running.
async fn run_db_tail(
    store:         StateStore,
    args:          &TailArgs,
    min_level_idx: usize,
) -> Result<()> {
    // ── Ctrl-C handler ────────────────────────────────────────────────────────
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut once = Some(shutdown_tx);
        ctrlc_once(move || {
            if let Some(tx) = once.take() {
                let _ = tx.send(());
            }
        });
    }

    // ── Backfill ──────────────────────────────────────────────────────────────
    let backfill = store
        .get_events(None, None, args.kind.clone(), BACKFILL_COUNT)
        .await
        .unwrap_or_default();

    let mut last_id: i64 = backfill.iter().map(|e| e.id).max().unwrap_or(0);

    for event in backfill.iter().rev() {
        if passes_level_filter(&event.level, min_level_idx) {
            print_event(event);
        }
    }

    // ── Poll loop ─────────────────────────────────────────────────────────────
    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        tokio::select! {
            _ = sleep(POLL_INTERVAL) => {}
            _ = &mut shutdown_rx => { break; }
        }

        let events = match store
            .get_events(None, None, args.kind.clone(), 1000)
            .await
        {
            Ok(ev)  => ev,
            Err(e) => {
                eprintln!("{}", format!("warn: DB read error: {e}").yellow());
                continue;
            }
        };

        let mut new_events: Vec<&AuditEvent> = events
            .iter()
            .filter(|e| e.id > last_id)
            .collect();
        new_events.sort_by_key(|e| e.id);

        for event in new_events {
            if event.id > last_id {
                last_id = event.id;
            }
            if passes_level_filter(&event.level, min_level_idx) {
                print_event(event);
            }
        }
    }

    eprintln!("\nkb tail: exiting.");
    Ok(())
}

// ─── Shared helpers ───────────────────────────────────────────────────────────

/// Returns the numeric severity index of `level`, or `None` for unknown values.
fn level_index(level: &str) -> Option<usize> {
    LEVELS.iter().position(|&l| l == level)
}

/// Returns `true` if `event_level` is at or above `min_idx` in severity.
fn passes_level_filter(event_level: &str, min_idx: usize) -> bool {
    match level_index(&event_level.to_lowercase()) {
        Some(idx) => idx >= min_idx,
        None      => true, // Unknown level: always show (forward-compatible).
    }
}

/// Print a single [`AuditEvent`] as a formatted, coloured terminal line.
///
/// Format: `[2024-01-15 14:32:01] INFO  queued       | message`
fn print_event(event: &AuditEvent) {
    let ts_str      = fmt_event_ts(event.ts);
    let level_upper = event.level.to_uppercase();
    let kind_padded = format!("{:<12}", event.kind);
    let rhs         = event.message.trim();

    let line = format!(
        "[{ts_str}] {level:<5} {kind} | {rhs}",
        ts_str = ts_str,
        level  = level_upper,
        kind   = kind_padded,
        rhs    = rhs,
    );

    let colored_line = match event.level.to_lowercase().as_str() {
        "warn"  => line.yellow().to_string(),
        "error" => line.red().bold().to_string(),
        _       => line.green().to_string(),
    };

    println!("{colored_line}");
}

/// Format a Unix epoch-second timestamp for the event output line.
fn fmt_event_ts(unix_secs: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(unix_secs, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => format!("{unix_secs:<19}"),
    }
}

/// Open a [`StateStore`], retrying every [`DB_RETRY_INTERVAL`] when the
/// database file does not yet exist (daemon has never started).
async fn open_store_with_retry(db_path: &std::path::Path, backoff: &[u64]) -> Result<StateStore> {
    loop {
        if db_path.exists() {
            match StateStore::new(db_path, backoff).await {
                Ok(store) => return Ok(store),
                Err(e) => {
                    eprintln!(
                        "{}",
                        format!("warn: cannot open database: {e}  (retrying in 2s)").yellow()
                    );
                }
            }
        } else {
            eprintln!(
                "{}",
                format!(
                    "note: database not found at '{}'.  \
                     Waiting for daemon to create it…",
                    db_path.display()
                )
                .yellow()
            );
        }
        sleep(DB_RETRY_INTERVAL).await;
    }
}

/// Register a one-shot Ctrl-C handler using `tokio::signal`.
fn ctrlc_once(callback: impl FnOnce() + Send + 'static) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            if let Ok(mut sig) = signal(SignalKind::interrupt()) {
                sig.recv().await;
                callback();
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            callback();
        }
    });
}
