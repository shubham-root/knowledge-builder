//! `kb show <path|id>` — detailed view of a single source file.
//!
//! Full implementation: T14.

use clap::Args;
use anyhow::Result;

#[derive(Args)]
pub struct ShowArgs {
    /// File path or numeric ID to look up.
    pub target: String,
}

pub async fn run(args: ShowArgs) -> Result<()> {
    // TODO (T14): lookup by path or id, print row + outputs + recent events.
    println!("kb show {:?} — not yet implemented (T14)", args.target);
    Ok(())
}
