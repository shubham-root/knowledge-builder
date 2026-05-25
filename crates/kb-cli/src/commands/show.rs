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

    // ── Agent plan (when a `plan_file` is present in processor_meta) ───────────────
    //
    // The processor records the path to the agent's `.kb-plan.jsonl` in
    // `processor_meta`.  Read it here so the operator can inspect what the
    // agent decided (or proposed in shadow mode) without having to dig into
    // ~/Library/Caches/knowledge-builder/jobs/<...>/.kb-plan.jsonl manually.
    print_agent_plan_section(file_row.processor_meta.as_deref());

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

// ── Agent plan section ───────────────────────────────────────────────────────

/// Render the agent-produced plan section.
///
/// Reads `processor_meta` (a JSON string column on `files`), pulls out the
/// agent-related keys, and if there is a `plan_file` reference also tries
/// to read+parse the `.kb-plan.jsonl` file the wrapper wrote during the
/// run.  Prints nothing when the file row has no agent metadata at all
/// (e.g. legacy rows from before the agent was wired in, or rows where
/// the agent never ran).
fn print_agent_plan_section(processor_meta: Option<&str>) {
    use serde_json::Value;
    use std::path::Path;

    const LBL: usize = 16;
    const SEP: &str = "───────────────────────────────────────────────────────────────────────";

    // No metadata → nothing to render.  This is the common case for pre-agent
    // jobs and we deliberately stay silent rather than printing a header that
    // would be empty.
    let raw = match processor_meta {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };

    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return, // unparseable; not our problem to surface here
    };

    // Only render when at least one agent_* field is present.
    let mode  = v.get("agent_mode").and_then(Value::as_str);
    let plan  = v.get("plan_file").and_then(Value::as_str);
    let summary = v.get("plan_summary").and_then(Value::as_str);
    let turns = v.get("agent_turns").and_then(Value::as_u64);
    let elapsed = v.get("agent_elapsed_secs").and_then(Value::as_f64);
    let aborted = v.get("agent_aborted").and_then(Value::as_bool).unwrap_or(false);

    if mode.is_none() && plan.is_none() && summary.is_none() {
        return;
    }

    println!("  Agent plan:");
    println!("  {SEP}");

    if let Some(m) = mode {
        let badge = match m {
            "shadow" => "shadow (no vault writes)",
            "apply"  => "apply  (writes committed)",
            other    => other,
        };
        println!("  {:<LBL$}  {}", "Mode:", badge);
    }
    if aborted {
        println!("  {:<LBL$}  yes (budget exhausted before completion)", "Aborted:");
    }
    if let (Some(t), Some(e)) = (turns, elapsed) {
        println!("  {:<LBL$}  {} turns, {:.1}s", "LLM activity:", t, e);
    }
    if let Some(s) = summary {
        println!("  {:<LBL$}  {}", "Summary:", s);
    }
    if let Some(p) = plan {
        println!("  {:<LBL$}  {}", "Plan file:", p);
    }

    // Loud rogue-write warning when the audit found anything.
    let rogue_count = v.get("rogue_writes_count").and_then(Value::as_u64).unwrap_or(0);
    if rogue_count > 0 {
        println!("  {:<LBL$}  ⚠️  {} rogue write(s) (agent bypassed kb-obsidian)",
                 "Audit:", rogue_count);
        if let Some(arr) = v.get("rogue_writes").and_then(Value::as_array) {
            for entry in arr.iter().take(10) {
                if let Some(s) = entry.as_str() {
                    println!("    ⚠  {s}");
                }
            }
        }
    }
    if let Some(s) = v.get("agent_final_text").and_then(Value::as_str) {
        // Indent the final assistant message so it's visibly separate.
        println!("  Final summary from the agent:");
        for line in s.lines().take(20) {
            println!("    {line}");
        }
        if s.lines().count() > 20 {
            println!("    … ({} more lines elided; cat the plan file for full)",
                     s.lines().count() - 20);
        }
    }

    // Render plan entries when we can read the file.
    if let Some(p) = plan {
        let path = Path::new(p);
        match std::fs::read_to_string(path) {
            Ok(content) => {
                let entries: Vec<&str> = content
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .collect();
                if !entries.is_empty() {
                    println!("  Plan entries ({}):", entries.len());
                    for (i, line) in entries.iter().enumerate().take(20) {
                        match serde_json::from_str::<Value>(line) {
                            Ok(j) => {
                                let cmd = j.get("cmd").and_then(Value::as_str).unwrap_or("?");
                                let mode_e = j.get("mode").and_then(Value::as_str).unwrap_or("?");
                                let applied = j.get("applied").and_then(Value::as_bool).unwrap_or(false);
                                let args: Vec<String> = j.get("args")
                                    .and_then(Value::as_array)
                                    .map(|a| a.iter()
                                        .filter_map(|x| x.as_str().map(|s| {
                                            // Truncate very long content= values to keep the
                                            // table readable.
                                            if s.len() > 80 { format!("{}…", &s[..80]) } else { s.to_string() }
                                        }))
                                        .collect())
                                    .unwrap_or_default();
                                let badge = if applied { "✓" } else { "·" };
                                println!(
                                    "    {} {:>3}. [{}] {}  {}",
                                    badge,
                                    i + 1,
                                    mode_e,
                                    cmd,
                                    args.join("  "),
                                );
                            }
                            Err(_) => {
                                println!("    · {:>3}. (unparseable line: {})", i + 1, line);
                            }
                        }
                    }
                    if entries.len() > 20 {
                        println!("    … {} more entries (read the plan file for full)",
                                 entries.len() - 20);
                    }
                }
            }
            Err(e) => {
                println!("  (plan file unreadable: {e})");
            }
        }
    }

    println!();
}
