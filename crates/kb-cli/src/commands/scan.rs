//! `kb scan` — trigger an immediate full-vault scan.
//!
//! Full implementation: T25 / T31.

use anyhow::Result;

pub async fn run() -> Result<()> {
    // TODO (T25): POST /scan to running daemon, or direct scan if offline.
    println!("kb scan — not yet implemented (T25)");
    Ok(())
}
