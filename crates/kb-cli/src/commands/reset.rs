//! `kb reset <path|id>` — delete a file record so next discovery is fresh.
//!
//! Full implementation: T25.

use clap::Args;
use anyhow::Result;

#[derive(Args)]
pub struct ResetArgs {
    /// File path or numeric ID to reset.
    pub target: String,
}

pub async fn run(args: ResetArgs) -> Result<()> {
    // TODO (T25): POST /files/:id/reset or direct DB delete.
    println!("kb reset {:?} — not yet implemented (T25)", args.target);
    Ok(())
}
