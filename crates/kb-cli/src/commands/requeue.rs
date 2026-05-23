//! `kb requeue <path|id>` — reset a file to `queued` status.
//!
//! Full implementation: T25.

use clap::Args;
use anyhow::Result;

#[derive(Args)]
pub struct RequeueArgs {
    /// File path or numeric ID to re-queue.
    pub target: String,
}

pub async fn run(args: RequeueArgs) -> Result<()> {
    // TODO (T25): POST /files/:id/requeue or direct DB update.
    println!("kb requeue {:?} — not yet implemented (T25)", args.target);
    Ok(())
}
