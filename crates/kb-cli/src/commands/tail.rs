//! `kb tail [--level info|warn|error] [--kind <event_kind>]`
//!
//! Polls the `events` table every 500 ms and streams formatted, coloured audit
//! events to the terminal.  Runs until the user presses Ctrl-C.
//!
//! # Output format
//! ```text
//! [2024-01-15 14:32:01] INFO  queued      | paper.pdf - Enqueued for processing
//! [2024-01-15 14:32:45] INFO  done        | paper.pdf - Processing complete (2 outputs)
//! [2024-01-15 14:33:01] ERROR failed      | report.docx - Processor timeout after 1800s
//! ```
//!
//! # Color mapping
//! - `INFO`  → green
//! - `WARN`  → yellow
//! - `ERROR` → red
//!
//! # Startup behaviour
//! Shows the last 20 events already in the database (backfill), then polls
//! for new events every 500 ms.  Uses the highest seen event `id` as a
//! cursor so events are never duplicated or missed.
//!
//! # DB-not-found handling
//! If the database file does not exist yet (daemon has never run), `kb tail`
//! prints a friendly warning and waits for the database to appear, retrying
//! every 2 s.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;
use kb_core::{config::load_raw, AuditEvent, StateStore};
use tokio::time::{sleep, Duration};

/// Level ordering (lower index = lower severity).
const LEVELS: &[&str] = &["info", "warn", "error"];

/// Number of backfill events to show on start.
const BACKFILL_COUNT: i64 = 20;

/// How often to poll the database for new events.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long to wait between retries when the DB is not yet available.
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
    // Validate --level argument.
    let min_level = args.level.to_lowercase();
    let min_level_idx = level_index(&min_level).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown level '{}'. Valid values: info, warn, error",
            args.level
        )
    })?;

    // Load config (no validation — vault / processor may not be set up yet).
    let config = load_raw().context("failed to load configuration")?;
    let db_path = PathBuf::from(&config.paths.db_path);
    let backoff  = config.worker.backoff_secs.clone();

    // Open the state store, retrying if the DB doesn't exist yet.
    let store = open_store_with_retry(&db_path, &backoff).await?;

    // ── Ctrl-C handler ────────────────────────────────────────────────────────
    // We use a simple tokio oneshot to signal shutdown.  The signal handler
    // fires at most once (Ctrl-C), after which we flush and exit.
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
    // Fetch the last N events (already in DB) and print them oldest-first.
    // The `get_events` query returns rows in DESC order (newest first) so we
    // reverse before printing.
    let backfill = store
        .get_events(None, None, args.kind.clone(), BACKFILL_COUNT)
        .await
        .unwrap_or_default();

    // Determine the cursor (highest id seen so far).
    // We must inspect all backfill rows even if they are filtered out by level,
    // so we iterate the raw backfill list.
    let mut last_id: i64 = backfill.iter().map(|e| e.id).max().unwrap_or(0);

    // Print backfill events that pass the level filter (oldest first).
    for event in backfill.iter().rev() {
        if passes_level_filter(&event.level, min_level_idx) {
            print_event(event);
        }
    }

    // ── Poll loop ─────────────────────────────────────────────────────────────
    loop {
        // Check for shutdown signal (non-blocking).
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        // Wait for the poll interval, but wake up early on Ctrl-C.
        tokio::select! {
            _ = sleep(POLL_INTERVAL) => {}
            _ = &mut shutdown_rx => { break; }
        }

        // Fetch new events since last_id.
        // We pass `since = Some(last_ts)` via the ts column — but actually
        // the StateStore API filters by `ts`, not `id`.  To avoid duplicates
        // we fetch all recent events and skip any we've already printed.
        //
        // Strategy: fetch up to 1000 events with no time filter; the actor
        // returns them newest-first (DESC ts).  We collect only those with
        // id > last_id, then sort ascending before printing.
        let events = match store
            .get_events(None, None, args.kind.clone(), 1000)
            .await
        {
            Ok(ev) => ev,
            Err(e) => {
                eprintln!("{}", format!("warn: DB read error: {e}").yellow());
                continue;
            }
        };

        // Collect events newer than our cursor, in ascending order.
        let mut new_events: Vec<&AuditEvent> = events
            .iter()
            .filter(|e| e.id > last_id)
            .collect();
        new_events.sort_by_key(|e| e.id);

        for event in new_events {
            // Update cursor regardless of level filter so we don't replay.
            if event.id > last_id {
                last_id = event.id;
            }
            if passes_level_filter(&event.level, min_level_idx) {
                print_event(event);
            }
        }
    }

    // Print a clean exit message so the user knows the loop ended intentionally.
    eprintln!("\nkb tail: exiting.");
    Ok(())
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Returns the numeric index of `level` in the severity order, or `None`.
fn level_index(level: &str) -> Option<usize> {
    LEVELS.iter().position(|&l| l == level)
}

/// Returns `true` if `event_level` is at least as severe as the minimum.
fn passes_level_filter(event_level: &str, min_idx: usize) -> bool {
    match level_index(&event_level.to_lowercase()) {
        Some(idx) => idx >= min_idx,
        // Unknown level: always show (forward-compatible with future levels).
        None => true,
    }
}

/// Print a single `AuditEvent` as a formatted, coloured line.
///
/// Format:
/// ```text
/// [2024-01-15 14:32:01] INFO  queued      | paper.pdf - Enqueued for processing
/// ```
fn print_event(event: &AuditEvent) {
    let ts_str = fmt_event_ts(event.ts);
    let level_upper = event.level.to_uppercase();
    let kind_padded = format!("{:<12}", event.kind);

    // Build the right-hand side: just the message (detail omitted for brevity).
    let rhs = event.message.trim();

    let line = format!(
        "[{ts_str}] {level:<5} {kind} | {rhs}",
        ts_str = ts_str,
        level  = level_upper,
        kind   = kind_padded,
        rhs    = rhs,
    );

    // Apply colour based on level.
    let colored_line = match event.level.to_lowercase().as_str() {
        "warn"  => line.yellow().to_string(),
        "error" => line.red().bold().to_string(),
        _       => line.green().to_string(), // "info" and anything else
    };

    println!("{colored_line}");
}

/// Format a Unix epoch-second timestamp for event output.
///
/// Returns `"2024-01-15 14:32:01"` (always 19 chars).
fn fmt_event_ts(unix_secs: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(unix_secs, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => format!("{unix_secs:<19}"),
    }
}

/// Open a `StateStore`, retrying every `DB_RETRY_INTERVAL` if the database
/// file does not yet exist (i.e. the daemon has never started).
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
///
/// The callback is invoked at most once.  After the first Ctrl-C the default
/// OS behaviour is restored (a second Ctrl-C will hard-kill the process).
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
