//! `kb requeue <path|id>` — reset a file back to `queued` status.
//!
//! **HTTP-first:** When the daemon is running, issues `POST /files/:id/requeue`
//! so the daemon picks it up immediately.  When offline, writes to the
//! SQLite database directly.
//!
//! # Behaviour
//! 1. Resolves the target to a file ID and path.
//! 2. If not found: exits with a helpful error message.
//! 3. Issues the requeue operation which:
//!    - Sets `status = 'queued'`
//!    - Resets `attempts = 0`
//!    - Clears `last_error` and `next_attempt_at`
//!    - Records a `requeued` audit event
//! 4. Prints a confirmation: `"Requeued: <path> (was: <old_status>)"`
//!
//! # Example
//! ```text
//! $ kb requeue 42
//! Requeued: /Users/alice/Vault/Sources/paper.pdf (was: failed)
//! ```

use anyhow::{Context, Result};
use clap::Args;
use kb_core::{Status, config::load_raw};

use crate::client::DaemonClient;
use super::db::{open_store, resolve_target};

// ── Argument types ─────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct RequeueArgs {
    /// File path or numeric ID to re-queue.
    pub target: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(args: RequeueArgs) -> Result<()> {
    let config = load_raw().context("failed to load configuration")?;

    if let Some(client) = DaemonClient::try_connect(&config.ops.http_bind).await {
        // ── HTTP mode ─────────────────────────────────────────────────────────
        let detail = client.resolve_target(&args.target).await?;
        let path       = detail.file.path.clone();
        let old_status = detail.file.status.clone();

        if old_status == Status::Queued {
            eprintln!(
                "Note: {} is already queued (attempts and backoff window will still be reset).",
                path.display()
            );
        }

        let previous_status = client.requeue(detail.file.id).await?;
        println!(
            "Requeued: {} (was: {})",
            path.display(),
            previous_status,
        );
    } else {
        // ── DB fallback ───────────────────────────────────────────────────────
        let store    = open_store().await?;
        let file_row = resolve_target(&store, &args.target).await?;

        let path       = file_row.path.clone();
        let old_status = file_row.status.clone();

        if old_status == Status::Queued {
            eprintln!(
                "Note: {} is already queued (attempts and backoff window will still be reset).",
                path.display()
            );
        }

        let returned_status = store.requeue(file_row.id).await?;
        println!(
            "Requeued: {} (was: {})",
            path.display(),
            returned_status,
        );
    }

    Ok(())
}
