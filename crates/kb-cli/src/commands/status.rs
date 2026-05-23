//! `kb status` — aggregate queue summary.
//!
//! Displays the total file count broken down by status, plus derived metrics:
//! queue depth, age of the oldest pending entry, and the most recent error
//! message if any file is in a failed state.
//!
//! Operates in **offline mode**: opens the SQLite database directly (via the
//! config's `db_path`), so the command works whether or not the daemon is
//! currently running.
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

use anyhow::Result;

use super::db::{fmt_age, open_store};

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    let store = open_store().await?;
    let stats = store.stats().await?;

    // ── Header ────────────────────────────────────────────────────────────────
    println!("Knowledge Builder — Queue Status");
    println!("{}", "━".repeat(40));
    println!();

    // ── Status breakdown table ────────────────────────────────────────────────
    let rows: &[(&str, i64)] = &[
        ("seen",       stats.seen),
        ("queued",     stats.queued),
        ("processing", stats.processing),
        ("done",       stats.done),
        ("failed",     stats.failed),
        ("skipped",    stats.skipped),
    ];

    let total: i64 = rows.iter().map(|(_, c)| c).sum();

    const STATUS_W: usize = 11;
    const COUNT_W:  usize = 7;

    println!("  {:<STATUS_W$}  {:>COUNT_W$}", "Status", "Count");
    println!("  {}  {}", "─".repeat(STATUS_W), "─".repeat(COUNT_W));

    for (name, count) in rows {
        // Highlight non-zero entries for `failed` and `processing`.
        let marker = if (*name == "failed" || *name == "processing") && *count > 0 {
            " ◀"
        } else {
            ""
        };
        println!(
            "  {:<STATUS_W$}  {:>COUNT_W$}{marker}",
            name, count,
        );
    }

    println!("  {}  {}", "─".repeat(STATUS_W), "─".repeat(COUNT_W));
    println!("  {:<STATUS_W$}  {:>COUNT_W$}", "total", total);
    println!();

    // ── Derived metrics ───────────────────────────────────────────────────────
    println!("  Queue depth:      {}", stats.queue_depth);

    match stats.oldest_pending_age_secs {
        Some(age) => println!("  Oldest pending:   {}", fmt_age(age)),
        None      => println!("  Oldest pending:   —"),
    }

    match &stats.last_error {
        Some(err) => {
            // Truncate very long error strings so they don't wrap badly.
            let display = if err.len() > 80 {
                format!("{}…", &err[..77])
            } else {
                err.clone()
            };
            println!("  Last error:       {display}");
        }
        None => println!("  Last error:       —"),
    }

    println!();
    Ok(())
}
