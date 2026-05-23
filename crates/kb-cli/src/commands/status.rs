//! `kb status` — aggregate queue summary.
//!
//! Full implementation: T14.

use anyhow::Result;

pub async fn run() -> Result<()> {
    // TODO (T14): query state store / HTTP API for Stats, format output.
    println!("kb status — not yet implemented (T14)");
    Ok(())
}
