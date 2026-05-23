//! `kb reset <path|id>` — delete a file's DB record so it can be re-processed.
//!
//! Accepts either a **numeric ID** (e.g. `kb reset 42`) or an
//! **absolute/relative file path** (e.g. `kb reset ~/Vault/Sources/paper.pdf`).
//!
//! # Behaviour
//! 1. Resolves the target to a `FileRow` (ID or path lookup).
//! 2. If not found: exits with a helpful error message.
//! 3. Issues a `ResetFile` op through the `StateStore` which:
//!    - Counts associated output rows (for the confirmation message).
//!    - Records a `reset` audit event (before deletion, so the FK is valid).
//!    - Deletes the `files` row — the `ON DELETE CASCADE` FK on `outputs`
//!      removes all associated output records automatically.
//!    - The `events` table FK uses `ON DELETE SET NULL`, so prior audit events
//!      are retained with `file_id = NULL`.
//! 4. Prints a confirmation:
//!    `"Reset: <path> - row and N outputs removed. File will be re-discovered on next scan."`
//!
//! **Important:** This command does **not** delete physical output files from
//! disk — only DB records are removed.  After a reset the file will be
//! re-discovered by the watcher or the next periodic scan and re-queued from
//! scratch.
//!
//! Works in **offline mode** (direct DB access) — the daemon does not need to
//! be running.
//!
//! # Example
//! ```text
//! $ kb reset 42
//! Reset: /Users/alice/Vault/Sources/paper.pdf - row and 2 outputs removed.
//! File will be re-discovered on next scan.
//!
//! $ kb reset ~/Vault/Sources/missing.pdf
//! error: no file found for '~/Vault/Sources/missing.pdf'.
//!        Use `kb list` to see all tracked files, or provide a numeric ID.
//! ```

use anyhow::Result;
use clap::Args;

use super::db::{open_store, resolve_target};

// ── Argument types ─────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ResetArgs {
    /// File path or numeric ID to reset.
    ///
    /// Accepts a numeric ID (e.g. `42`) or any path form that resolves to a
    /// file tracked in the database (absolute, relative, or `~`-prefixed).
    pub target: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(args: ResetArgs) -> Result<()> {
    let store    = open_store().await?;
    let file_row = resolve_target(&store, &args.target).await?;

    let path = file_row.path.clone();

    // Delegate to the state store actor — safe to call in offline mode.
    let (_, output_count) = store.reset_file(file_row.id).await?;

    let output_noun = if output_count == 1 { "output" } else { "outputs" };

    println!(
        "Reset: {} - row and {output_count} {output_noun} removed.\n\
         File will be re-discovered on next scan.",
        path.display(),
    );

    Ok(())
}
