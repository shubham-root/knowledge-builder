//! `kb uninstall` — remove the launchd LaunchAgent registration.
//!
//! Full implementation: T33.

use anyhow::Result;

pub async fn run() -> Result<()> {
    // TODO (T33): launchctl bootout, remove plist file.
    println!("kb uninstall — not yet implemented (T33)");
    Ok(())
}
