//! `kb show <path|id>` — detailed view of a single source file.
//!
//! Accepts either a **numeric ID** (e.g. `kb show 42`) or an **absolute/
//! relative file path** (e.g. `kb show ~/Vault/Sources/paper.pdf`).
//! Path arguments are canonicalized before the database lookup, so relative
//! paths and `~`-prefixed paths both work.
//!
//! Displays three sections:
//! 1. **File details** — all columns from the `files` table.
//! 2. **Outputs** — every artifact recorded in the `outputs` table for this
//!    source file.
//! 3. **Recent events** — the last 10 audit-log entries whose `file_id` column
//!    matches this file.
//!
//! Operates in **offline mode**: opens the SQLite database directly so the
//! command works whether or not the daemon is running.
//!
//! # Example output
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────┐
//! │  File #42 — paper.pdf                                                │
//! └──────────────────────────────────────────────────────────────────────┘
//!
//!  Path:           /Users/alice/Vault/Sources/paper.pdf
//!  Status:         done
//!  Content hash:   sha256:abcdef1234567890abcdef1234567890abcdef1234567890abcdef12
//!  Size:           2.3 MB
//!  Attempts:       1
//!  First seen:     2026-05-23 10:30:01
//!  Updated:        2026-05-23 10:32:15
//!  Processed:      2026-05-23 10:32:14
//!  Next attempt:   —
//!  Last error:     —
//!
//!  Outputs (2):
//!  ─────────────────────────────────────────────────────────────────────
//!    1. /Users/alice/Vault/Notes/paper.md            [markdown,  15.2 KB]
//!    2. /Users/alice/Vault/Assets/fig1.png           [asset,    234.5 KB]
//!
//!  Recent events (last 10):
//!  ─────────────────────────────────────────────────────────────────────
//!  2026-05-23 10:32:14  info   done              File processed successfully
//!  2026-05-23 10:30:05  info   claimed           Worker claimed job
//!  2026-05-23 10:30:01  info   queued            File queued for processing
//! ```

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::Args;

use super::db::{fmt_age, fmt_bytes, fmt_ts, fmt_ts_opt, open_store};

// ── Argument types ─────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ShowArgs {
    /// File path or numeric ID to look up.
    pub target: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(args: ShowArgs) -> Result<()> {
    let store = open_store().await?;

    // ── Resolve target to a FileRow ───────────────────────────────────────────
    let file_row = resolve_target(&store, &args.target).await?;

    // ── Section separator constant ────────────────────────────────────────────
    const SEP: &str =
        "───────────────────────────────────────────────────────────────────────";

    // ── Box header ────────────────────────────────────────────────────────────
    let filename = file_row
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| file_row.path.to_string_lossy().to_string());

    let title = format!("  File #{} — {}", file_row.id, filename);
    let box_width = SEP.len();
    let title_padded = format!("{:<width$}", title, width = box_width - 2);

    println!("┌{}┐", "─".repeat(box_width));
    println!("│{title_padded}│");
    println!("└{}┘", "─".repeat(box_width));
    println!();

    // ── File details ──────────────────────────────────────────────────────────
    const LBL: usize = 16; // label column width

    println!(
        "  {:<LBL$}  {}",
        "Path:", file_row.path.display()
    );
    println!(
        "  {:<LBL$}  {}",
        "Status:", file_row.status.as_str()
    );

    let hash_display = file_row
        .content_hash
        .as_deref()
        .unwrap_or("—");
    println!("  {:<LBL$}  {}", "Content hash:", hash_display);

    let size_display = match file_row.size {
        Some(b) => fmt_bytes(b),
        None    => "—".to_string(),
    };
    println!("  {:<LBL$}  {}", "Size:", size_display);
    println!("  {:<LBL$}  {}", "Attempts:", file_row.attempts);

    println!(
        "  {:<LBL$}  {}",
        "First seen:", fmt_ts(file_row.first_seen_at)
    );
    println!(
        "  {:<LBL$}  {}",
        "Updated:", fmt_ts(file_row.updated_at)
    );
    println!(
        "  {:<LBL$}  {}",
        "Processed:", fmt_ts_opt(file_row.processed_at)
    );

    // Next-attempt: show a countdown if in the future, else "—".
    let next_attempt_display = match file_row.next_attempt_at {
        Some(ts) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let wait = ts - now;
            if wait > 0 {
                format!("{} (in {})", fmt_ts(ts), fmt_age(wait))
            } else {
                format!("{} (overdue)", fmt_ts(ts))
            }
        }
        None => "—".to_string(),
    };
    println!("  {:<LBL$}  {}", "Next attempt:", next_attempt_display);

    let error_display = file_row.last_error.as_deref().unwrap_or("—");
    println!("  {:<LBL$}  {}", "Last error:", error_display);

    println!();

    // ── Outputs ───────────────────────────────────────────────────────────────
    let outputs = store.get_outputs_for_file(file_row.id).await?;

    println!("  Outputs ({}):", outputs.len());
    println!("  {SEP}");

    if outputs.is_empty() {
        println!("  (none)");
    } else {
        for (i, out) in outputs.iter().enumerate() {
            let kind_str = out.kind.as_deref().unwrap_or("unknown");
            let bytes_str = match out.bytes {
                Some(b) => fmt_bytes(b),
                None    => "?".to_string(),
            };
            println!(
                "  {}. {}  [{}, {}]",
                i + 1,
                out.path.display(),
                kind_str,
                bytes_str,
            );
        }
    }

    println!();

    // ── Recent events for this file ───────────────────────────────────────────
    // Filter events by file_id.  The StateStore's `get_events` does not
    // support a file_id filter in its public API (filter is by level/kind/since),
    // so we fetch the last N events and then filter client-side.
    // We fetch 10× more than needed (max 200) to get a full 10 file-specific entries.
    let all_events = store
        .get_events(None, None, None, 200)
        .await?;

    let file_events: Vec<_> = all_events
        .into_iter()
        .filter(|e| e.file_id == Some(file_row.id))
        .take(10)
        .collect();

    println!("  Recent events (last 10):");
    println!("  {SEP}");

    if file_events.is_empty() {
        println!("  (no events recorded for this file)");
    } else {
        const TS_W:    usize = 19;
        const LEVEL_W: usize = 6;
        const KIND_W:  usize = 18;

        for ev in &file_events {
            let ts_str    = fmt_ts(ev.ts);
            let level_str = &ev.level;
            let kind_str  = &ev.kind;
            let msg_str   = &ev.message;

            println!(
                "  {:<TS_W$}  {:<LEVEL_W$}  {:<KIND_W$}  {}",
                ts_str, level_str, kind_str, msg_str,
            );
        }
    }

    println!();
    Ok(())
}

// ── Target resolution ─────────────────────────────────────────────────────────

/// Resolve the `target` argument to a `FileRow`.
///
/// Strategy:
/// 1. If `target` parses as a positive integer → look up by ID.
/// 2. Otherwise treat as a path:
///    a. Expand a leading `~` to the home directory.
///    b. Make absolute (prepend `cwd` if relative).
///    c. Look up in the DB by path string.
///    d. If not found, also try `std::fs::canonicalize` in case the path
///       contains symlinks (matching the stored canonical path).
async fn resolve_target(
    store: &kb_core::StateStore,
    target: &str,
) -> Result<kb_core::FileRow> {
    // ── Try numeric ID first ──────────────────────────────────────────────────
    if let Ok(id) = target.parse::<i64>() {
        if id > 0 {
            return match store.get_file_by_id(id).await? {
                Some(row) => Ok(row),
                None      => bail!("no file found with ID {id}"),
            };
        }
    }

    // ── Treat as path ─────────────────────────────────────────────────────────
    let expanded = expand_tilde(target);
    let path     = if expanded.is_absolute() {
        expanded.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&expanded)
    };

    // First attempt: look up with the path as-is (may already be canonical).
    if let Some(row) = store.find_by_path(path.clone()).await? {
        return Ok(row);
    }

    // Second attempt: canonicalize (resolves `.` / `..` / symlinks) and retry.
    if let Ok(canon) = std::fs::canonicalize(&path) {
        if canon != path {
            if let Some(row) = store.find_by_path(canon).await? {
                return Ok(row);
            }
        }
    }

    bail!(
        "no file found for '{}'. \
         Use `kb list` to see tracked files, or provide a numeric ID.",
        target
    );
}

/// Expand a leading `~` in `s` to the user's home directory.
///
/// Returns a [`PathBuf`] with the substitution applied, or the original path
/// unchanged if it does not start with `~`.
fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(s)
}
