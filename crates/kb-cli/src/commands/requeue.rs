//! `kb requeue <path|id>` — reset a file back to `queued` status.
//!
//! Accepts either a **numeric ID** (e.g. `kb requeue 42`) or an
//! **absolute/relative file path** (e.g. `kb requeue ~/Vault/Sources/paper.pdf`).
//!
//! # Behaviour
//! 1. Resolves the target to a `FileRow` (ID or path lookup).
//! 2. If not found: exits with a helpful error message.
//! 3. Issues a `Requeue` op through the `StateStore` which:
//!    - Sets `status = 'queued'`
//!    - Resets `attempts = 0`
//!    - Clears `last_error` and `next_attempt_at`
//!    - Records a `requeued` audit event in the `events` table
//! 4. Prints a confirmation: `"Requeued: <path> (was: <old_status>)"`
//!
//! **Edge cases:**
//! - File already `queued`: the command succeeds but prints a note so the
//!   operator knows the state did not actually change meaningfully.
//! - Works in **offline mode** (direct DB access) — the daemon does not need
//!   to be running.
//!
//! # Example
//! ```text
//! $ kb requeue 42
//! Requeued: /Users/alice/Vault/Sources/paper.pdf (was: failed)
//!
//! $ kb requeue ~/Vault/Sources/paper.pdf
//! Note: /Users/alice/Vault/Sources/paper.pdf is already queued.
//! Requeued: /Users/alice/Vault/Sources/paper.pdf (was: queued)
//! ```

use anyhow::Result;
use clap::Args;

use kb_core::Status;

use super::db::{open_store, resolve_target};

// ── Argument types ─────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct RequeueArgs {
    /// File path or numeric ID to re-queue.
    ///
    /// Accepts a numeric ID (e.g. `42`) or any path form that resolves to a
    /// file tracked in the database (absolute, relative, or `~`-prefixed).
    pub target: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(args: RequeueArgs) -> Result<()> {
    let store    = open_store().await?;
    let file_row = resolve_target(&store, &args.target).await?;

    let path       = file_row.path.clone();
    let old_status = file_row.status.clone();

    // Inform the user if the file is already queued so they know the reset
    // still executed (attempts → 0, next_attempt_at cleared), even though the
    // status itself did not change.
    if old_status == Status::Queued {
        eprintln!(
            "Note: {} is already queued (attempts and backoff window will still be reset).",
            path.display()
        );
    }

    // Delegate to the state store actor — this is safe to call offline.
    let returned_status = store.requeue(file_row.id).await?;

    println!(
        "Requeued: {} (was: {})",
        path.display(),
        returned_status,
    );

    Ok(())
}
