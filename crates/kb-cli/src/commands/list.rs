//! `kb list [--status <status>] [--limit N]` — list tracked source files.
//!
//! Displays a table of file rows, with optional filtering by status.
//!
//! **HTTP-first:** When the daemon is running, the file list is fetched via
//! `GET /files`.  When offline the SQLite database is read directly.
//!
//! # Column widths
//! ```text
//!   ID  Status       Path                                  Hash           Updated
//!  ──── ──────────── ───────────────────────────────────── ────────────── ───────────────────
//!     1 done         ~/Vault/Sources/paper.pdf             sha256:ab12cd… 2026-05-23 10:30:01
//!     2 queued       ~/Vault/Sources/data.xlsx             sha256:ff00aa… 2026-05-23 10:35:22
//!     3 failed ◀     ~/Vault/Sources/broken.docx           —              2026-05-23 10:40:11
//! ```

use std::str::FromStr;

use anyhow::{bail, Context, Result};
use clap::Args;
use kb_core::{config::load_raw, Status};

use crate::client::DaemonClient;
use super::db::{fmt_ts, open_store, short_hash, truncate_path};

// ── Argument types ─────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Filter by status (seen, queued, processing, done, failed, skipped).
    #[arg(long, short = 's')]
    pub status: Option<String>,

    /// Maximum number of rows to show (default: 20).
    #[arg(long, short = 'n', default_value_t = 20)]
    pub limit: usize,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(args: ListArgs) -> Result<()> {
    // Validate the status filter early, before any I/O.
    let status_filter: Option<Status> = match &args.status {
        Some(s) => {
            let parsed = Status::from_str(s.as_str()).map_err(|_| {
                anyhow::anyhow!(
                    "unknown status '{s}'. \
                     Valid values: seen, queued, processing, done, failed, skipped"
                )
            })?;
            Some(parsed)
        }
        None => None,
    };

    if args.limit == 0 {
        bail!("--limit must be at least 1");
    }

    let limit = args.limit as i64;

    // ── Fetch rows ────────────────────────────────────────────────────────────
    let config = load_raw().context("failed to load configuration")?;
    let rows = if let Some(client) = DaemonClient::try_connect(&config.ops.http_bind).await {
        // HTTP mode
        client.list_files(status_filter, limit, 0).await?
    } else {
        // DB fallback
        let store = open_store().await?;
        store.list_files(status_filter, limit, 0).await?
    };

    // ── Render ────────────────────────────────────────────────────────────────
    if rows.is_empty() {
        match &args.status {
            Some(s) => println!("No files with status '{s}'."),
            None    => println!("No files tracked yet."),
        }
        return Ok(());
    }

    const ID_W:     usize = 6;
    const STATUS_W: usize = 12;
    const PATH_W:   usize = 37;
    const HASH_W:   usize = 14;
    const TS_W:     usize = 19;

    println!(
        " {:>ID_W$}  {:<STATUS_W$}  {:<PATH_W$}  {:<HASH_W$}  {:<TS_W$}",
        "ID", "Status", "Path", "Hash", "Updated",
    );
    println!(
        " {}  {}  {}  {}  {}",
        "─".repeat(ID_W),
        "─".repeat(STATUS_W),
        "─".repeat(PATH_W),
        "─".repeat(HASH_W),
        "─".repeat(TS_W),
    );

    for row in &rows {
        let status_str = row.status.as_str();
        let status_display = match row.status {
            Status::Failed     => format!("{status_str} ◀"),
            Status::Processing => format!("{status_str} ●"),
            _                  => status_str.to_string(),
        };

        let path_str = row.path.to_string_lossy();
        let path_display = truncate_path(&path_str, PATH_W);

        let hash_display = match &row.content_hash {
            Some(h) => short_hash(h),
            None    => "—".to_string(),
        };

        let ts_display = fmt_ts(row.updated_at);

        println!(
            " {:>ID_W$}  {:<STATUS_W$}  {:<PATH_W$}  {:<HASH_W$}  {:<TS_W$}",
            row.id,
            status_display,
            path_display,
            hash_display,
            ts_display,
        );
    }

    println!();
    let shown = rows.len();
    if shown == args.limit {
        println!(
            "  Showing {shown} rows (limit). Use --limit N to see more, \
             or --status <status> to filter."
        );
    } else {
        println!("  {shown} row(s) total.");
    }

    Ok(())
}
