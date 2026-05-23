//! `kb daemon [--foreground]` — start the Knowledge Builder daemon.
//!
//! Full implementation: T24 (startup orchestration).

use clap::Args;
use anyhow::Result;

#[derive(Args)]
pub struct DaemonArgs {
    /// Log to stderr in addition to the rotating log file.
    #[arg(long)]
    pub foreground: bool,
}

pub async fn run(args: DaemonArgs) -> Result<()> {
    // TODO (T24): singleton lock → crash recovery → initial scan →
    //             start watcher + workers + HTTP ops server.
    println!(
        "kb daemon (foreground={}) — not yet implemented",
        args.foreground
    );
    Ok(())
}
