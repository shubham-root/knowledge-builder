//! `kb prune [--before <date>] [--status done] [--dry-run]`
//!
//! Full implementation: T26.

use clap::Args;
use anyhow::Result;

#[derive(Args)]
pub struct PruneArgs {
    /// Delete rows older than this ISO-8601 date (e.g. 2025-01-01).
    #[arg(long)]
    pub before: Option<String>,

    /// Only prune rows with this status (default: done).
    #[arg(long, default_value = "done")]
    pub status: String,

    /// Print what would be deleted without deleting.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: PruneArgs) -> Result<()> {
    // TODO (T26): build DELETE query, respect --dry-run.
    println!(
        "kb prune (before={:?}, status={}, dry_run={}) — not yet implemented (T26)",
        args.before, args.status, args.dry_run
    );
    Ok(())
}
