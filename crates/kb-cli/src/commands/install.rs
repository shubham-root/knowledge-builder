//! `kb install` — register the daemon as a launchd LaunchAgent.
//!
//! Full implementation: T33.

use anyhow::Result;

pub async fn run() -> Result<()> {
    // TODO (T33): run doctor, render plist, launchctl bootstrap.
    println!("kb install — not yet implemented (T33)");
    Ok(())
}
