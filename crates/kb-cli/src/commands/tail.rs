//! `kb tail [--level …] [--kind …]` — stream audit events.
//!
//! Full implementation: T27 (DB-polled) then T30 (SSE via HTTP).

use clap::Args;
use anyhow::Result;

#[derive(Args)]
pub struct TailArgs {
    /// Minimum event level to show (info, warn, error).
    #[arg(long, default_value = "info")]
    pub level: String,

    /// Filter by event kind (discovered, queued, done, failed, …).
    #[arg(long)]
    pub kind: Option<String>,
}

pub async fn run(args: TailArgs) -> Result<()> {
    // TODO (T27): poll events table; T30: switch to SSE when daemon running.
    println!(
        "kb tail (level={}, kind={:?}) — not yet implemented (T27)",
        args.level, args.kind
    );
    Ok(())
}
