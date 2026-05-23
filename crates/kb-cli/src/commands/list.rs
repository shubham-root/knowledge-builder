//! `kb list [--status …] [--limit N]` — list tracked source files.
//!
//! Full implementation: T14.

use clap::Args;
use anyhow::Result;

#[derive(Args)]
pub struct ListArgs {
    /// Filter by status (seen, queued, processing, done, failed, skipped).
    #[arg(long)]
    pub status: Option<String>,

    /// Maximum number of rows to show (default: 50).
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}

pub async fn run(args: ListArgs) -> Result<()> {
    // TODO (T14): query state store / HTTP API, format as table.
    println!("kb list (status={:?}, limit={}) — not yet implemented (T14)",
             args.status, args.limit);
    Ok(())
}
