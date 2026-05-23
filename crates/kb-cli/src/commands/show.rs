//! `kb show <path|id>` — detailed view of a single source file.
//!
//! Accepts either a **numeric ID** or an **absolute/relative file path**.
//!
//! **HTTP-first:** When the daemon is running, all data is fetched live via
//! `GET /files/:id`.  When offline the SQLite database is read directly.
//!
//! Displays three sections:
//! 1. **File details** — all columns from the `files` table.
//! 2. **Outputs** — every artifact in the `outputs` table for this source.
//! 3. **Recent events** — the last 10 audit-log entries for this file.
//!
//! # Example output
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────┐
//! │  File #42 — paper.pdf                                                │
//! └──────────────────────────────────────────────────────────────────────┘
//!
//!  Path:           /Users/alice/Vault/Sources/paper.pdf
//!  Status:         done
//!  ...
//! ```

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Args;
use kb_core::{AuditEvent, FileRow, OutputRecord, config::load_raw};

use crate::client::DaemonClient;
use super::db::{fmt_age, fmt_bytes, fmt_ts, fmt_ts_opt, open_store, resolve_target};

// ── Argument types ─────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ShowArgs {
    /// File path or numeric ID to look up.
    pub target: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(args: ShowArgs) -> Result<()> {
    let config = load_raw().context("failed to load configuration")?;

    if let Some(client) = DaemonClient::try_connect(&config.ops.http_bind).await {
        // ── HTTP mode ─────────────────────────────────────────────────────────
        let detail = client.resolve_target(&args.target).await?;
        render_file_detail(&detail.file, &detail.outputs, &detail.events);
    } else {
        // ── DB fallback ───────────────────────────────────────────────────────
        let store = open_store().await?;
        let file_row = resolve_target(&store, &args.target).await?;
        let outputs = store.get_outputs_for_file(file_row.id).await?;

        // Fetch recent events and filter by file_id client-side.
        let all_events = store.get_events(None, None, None, 200).await?;
        let file_events: Vec<AuditEvent> = all_events
            .into_iter()
            .filter(|e| e.file_id == Some(file_row.id))
            .take(10)
            .collect();

        render_file_detail(&file_row, &outputs, &file_events);
    }

    Ok(())
}

// ── Rendering ─────────────────────────────────────────────────────────────────

/// Render the full file-detail view.  Called identically from the HTTP and
/// DB code paths so the output is byte-for-byte identical regardless of source.
pub(crate) fn render_file_detail(
    file_row: &FileRow,
    outputs:  &[OutputRecord],
    events:   &[AuditEvent],
) {
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
    const LBL: usize = 16;

    println!("  {:<LBL$}  {}", "Path:", file_row.path.display());
    println!("  {:<LBL$}  {}", "Status:", file_row.status.as_str());

    let hash_display = file_row.content_hash.as_deref().unwrap_or("—");
    println!("  {:<LBL$}  {}", "Content hash:", hash_display);

    let size_display = match file_row.size {
        Some(b) => fmt_bytes(b),
        None    => "—".to_string(),
    };
    println!("  {:<LBL$}  {}", "Size:", size_display);
    println!("  {:<LBL$}  {}", "Attempts:", file_row.attempts);

    println!("  {:<LBL$}  {}", "First seen:", fmt_ts(file_row.first_seen_at));
    println!("  {:<LBL$}  {}", "Updated:",    fmt_ts(file_row.updated_at));
    println!("  {:<LBL$}  {}", "Processed:",  fmt_ts_opt(file_row.processed_at));

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
    println!("  Outputs ({}):", outputs.len());
    println!("  {SEP}");

    if outputs.is_empty() {
        println!("  (none)");
    } else {
        for (i, out) in outputs.iter().enumerate() {
            let kind_str  = out.kind.as_deref().unwrap_or("unknown");
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

    // ── Recent events ─────────────────────────────────────────────────────────
    // The HTTP endpoint already returns file-scoped events; the DB fallback
    // pre-filters them before calling this function.
    let display_events: Vec<&AuditEvent> = events.iter().take(10).collect();

    println!("  Recent events (last 10):");
    println!("  {SEP}");

    if display_events.is_empty() {
        println!("  (no events recorded for this file)");
    } else {
        const TS_W:    usize = 19;
        const LEVEL_W: usize = 6;
        const KIND_W:  usize = 18;

        for ev in &display_events {
            println!(
                "  {:<TS_W$}  {:<LEVEL_W$}  {:<KIND_W$}  {}",
                fmt_ts(ev.ts),
                ev.level,
                ev.kind,
                ev.message,
            );
        }
    }

    println!();
}

// ── Target resolution (kept for DB fallback path) ─────────────────────────────

/// Expand a leading `~` in `s` to the user's home directory.
fn _expand_tilde(s: &str) -> PathBuf {
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

/// Resolve `target` to a `FileRow` using DB fallback resolution.
///
/// This mirrors the logic in `db::resolve_target` but is kept here for
/// historical compatibility.  New code should call `db::resolve_target`.
async fn _resolve_target_db(
    store: &kb_core::StateStore,
    target: &str,
) -> Result<kb_core::FileRow> {
    if let Ok(id) = target.parse::<i64>() {
        if id > 0 {
            return match store.get_file_by_id(id).await? {
                Some(row) => Ok(row),
                None      => bail!("no file found with ID {id}"),
            };
        }
    }

    let expanded = _expand_tilde(target);
    let path = if expanded.is_absolute() {
        expanded.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&expanded)
    };

    if let Some(row) = store.find_by_path(path.clone()).await? {
        return Ok(row);
    }

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
    )
}
