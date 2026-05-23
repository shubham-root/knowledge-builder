//! `kb status` — aggregate queue summary.
//!
//! Displays the total file count broken down by status, plus derived metrics:
//! queue depth, age of the oldest pending entry, and the most recent error
//! message if any file is in a failed state.
//!
//! **HTTP-first:** When the daemon is running, statistics are fetched live via
//! `GET /stats`.  When offline the SQLite database is read directly.
//!
//! # Example output
//! ```text
//! Knowledge Builder — Queue Status
//! ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
//!
//!  Status        Count
//!  ─────────── ───────
//!  seen              0
//!  queued            2
//!  processing        1
//!  done             42
//!  failed            1
//!  skipped           5
//!  ─────────── ───────
//!  total            51
//!
//!  Queue depth:      3
//!  Oldest pending:   1h 07m
//!  Last error:       processor timed out after 1800s
//! ```

use anyhow::{Context, Result};
use kb_core::config::load_raw;

use crate::client::DaemonClient;
use super::db::{fmt_age, open_store};

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    let config = load_raw().context("failed to load configuration")?;

    if let Some(client) = DaemonClient::try_connect(&config.ops.http_bind).await {
        // ── HTTP mode (daemon is running) ─────────────────────────────────────
        let stats = client.get_stats().await?;
        print_stats(
            stats.count("seen"),
            stats.count("queued"),
            stats.count("processing"),
            stats.count("done"),
            stats.count("failed"),
            stats.count("skipped"),
            stats.queue_depth,
            stats.oldest_pending_age_secs,
            stats.last_error.as_deref(),
        );
    } else {
        // ── DB mode (daemon not running — direct SQLite access) ───────────────
        let store = open_store().await?;
        let stats = store.stats().await?;
        print_stats(
            stats.seen,
            stats.queued,
            stats.processing,
            stats.done,
            stats.failed,
            stats.skipped,
            stats.queue_depth,
            stats.oldest_pending_age_secs,
            stats.last_error.as_deref(),
        );
    }

    Ok(())
}

// ── Rendering ─────────────────────────────────────────────────────────────────

/// Render the status table.  Called from both HTTP and DB code paths with the
/// same extracted values so the output is identical regardless of source.
fn print_stats(
    seen:       i64,
    queued:     i64,
    processing: i64,
    done:       i64,
    failed:     i64,
    skipped:    i64,
    queue_depth:             i64,
    oldest_pending_age_secs: Option<i64>,
    last_error:              Option<&str>,
) {
    let rows: &[(&str, i64)] = &[
        ("seen",       seen),
        ("queued",     queued),
        ("processing", processing),
        ("done",       done),
        ("failed",     failed),
        ("skipped",    skipped),
    ];
    let total: i64 = rows.iter().map(|(_, c)| c).sum();

    // ── Header ────────────────────────────────────────────────────────────────
    println!("Knowledge Builder — Queue Status");
    println!("{}", "━".repeat(40));
    println!();

    // ── Status breakdown table ────────────────────────────────────────────────
    const STATUS_W: usize = 11;
    const COUNT_W:  usize = 7;

    println!("  {:<STATUS_W$}  {:>COUNT_W$}", "Status", "Count");
    println!("  {}  {}", "─".repeat(STATUS_W), "─".repeat(COUNT_W));

    for (name, count) in rows {
        let marker = if (*name == "failed" || *name == "processing") && *count > 0 {
            " ◀"
        } else {
            ""
        };
        println!("  {:<STATUS_W$}  {:>COUNT_W$}{marker}", name, count);
    }

    println!("  {}  {}", "─".repeat(STATUS_W), "─".repeat(COUNT_W));
    println!("  {:<STATUS_W$}  {:>COUNT_W$}", "total", total);
    println!();

    // ── Derived metrics ───────────────────────────────────────────────────────
    println!("  Queue depth:      {queue_depth}");

    match oldest_pending_age_secs {
        Some(age) => println!("  Oldest pending:   {}", fmt_age(age)),
        None      => println!("  Oldest pending:   —"),
    }

    match last_error {
        Some(err) => {
            let display = if err.len() > 80 {
                format!("{}…", &err[..77])
            } else {
                err.to_string()
            };
            println!("  Last error:       {display}");
        }
        None => println!("  Last error:       —"),
    }

    println!();
}
