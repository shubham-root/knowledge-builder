//! `kb reset <path|id>` — delete a file's DB record so it can be re-processed.
//!
//! **HTTP-first:** When the daemon is running, issues `POST /files/:id/reset`
//! so the daemon's in-memory state stays consistent.  When offline, writes to
//! the SQLite database directly.
//!
//! # Behaviour
//! 1. Resolves the target to a file ID and path.
//! 2. If not found: exits with a helpful error message.
//! 3. Deletes the `files` row (CASCADE removes `outputs`; events retain a
//!    `NULL` `file_id`).
//! 4. Prints a confirmation message.
//!
//! **Important:** Physical output files on disk are NOT removed.
//!
//! # Example
//! ```text
//! $ kb reset 42
//! Reset: /Users/alice/Vault/Sources/paper.pdf - row and 2 outputs removed.
//! File will be re-discovered on next scan.
//! ```

use anyhow::{Context, Result};
use clap::Args;
use kb_core::config::load_raw;

use crate::client::DaemonClient;
use super::db::{open_store, resolve_target};

// ── Argument types ─────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ResetArgs {
    /// File path or numeric ID to reset.
    pub target: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(args: ResetArgs) -> Result<()> {
    let config = load_raw().context("failed to load configuration")?;

    if let Some(client) = DaemonClient::try_connect(&config.ops.http_bind).await {
        // ── HTTP mode ─────────────────────────────────────────────────────────
        let detail = client.resolve_target(&args.target).await?;
        let path         = detail.file.path.clone();
        let output_count = client.reset(detail.file.id).await?;

        let noun = if output_count == 1 { "output" } else { "outputs" };
        println!(
            "Reset: {} - row and {output_count} {noun} removed.\n\
             File will be re-discovered on next scan.",
            path.display(),
        );
    } else {
        // ── DB fallback ───────────────────────────────────────────────────────
        let store    = open_store().await?;
        let file_row = resolve_target(&store, &args.target).await?;
        let path     = file_row.path.clone();

        let (_, output_count) = store.reset_file(file_row.id).await?;
        let noun = if output_count == 1 { "output" } else { "outputs" };
        println!(
            "Reset: {} - row and {output_count} {noun} removed.\n\
             File will be re-discovered on next scan.",
            path.display(),
        );
    }

    Ok(())
}
