//! `kb uninstall` — remove the launchd LaunchAgent registration.
//!
//! ## What this command does
//!
//! 1. Runs `launchctl bootout gui/<UID>/com.user.knowledge-builder` to stop the
//!    daemon and remove it from launchd's job database.  A failure here is
//!    treated as a warning (the service may already have been manually removed).
//! 2. Deletes `~/Library/LaunchAgents/com.user.knowledge-builder.plist`.
//! 3. Prints a confirmation message.
//!
//! Your data (SQLite database, processed notes in the vault) is **not** touched.
//! Run `kb install` at any time to re-register the service.

use anyhow::{Context, Result};

// Re-use the shared helpers and constants from install.rs.
use super::install::{current_uid, run_launchctl, LABEL, PLIST_FILENAME};

// ── Entry point ────────────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    let home = dirs::home_dir()
        .context("Cannot determine home directory ($HOME is unset)")?;
    let uid = current_uid();
    let plist_path = home
        .join("Library")
        .join("LaunchAgents")
        .join(PLIST_FILENAME);

    // ── Step 1: launchctl bootout ─────────────────────────────────────────────
    // Use the service target form `gui/<UID>/<label>` — this works even when
    // the plist file has already been modified or removed.
    let service = format!("gui/{uid}/{LABEL}");
    println!("  Stopping and unregistering service…");
    match run_launchctl(&["bootout", &service]) {
        Ok(()) => println!("  ✓  Booted out     : launchctl bootout {service}"),
        Err(e) => {
            // Not fatal — the service may have been previously unregistered
            // while the plist was still present on disk.
            println!("  (launchctl bootout returned an error — {e})");
            println!("  (Continuing; service may not have been registered)");
        }
    }

    // ── Step 2: remove the plist file ────────────────────────────────────────
    if plist_path.exists() {
        std::fs::remove_file(&plist_path)
            .with_context(|| format!("Cannot remove plist: {}", plist_path.display()))?;
        println!("  ✓  Removed plist  : {}", plist_path.display());
    } else {
        println!(
            "  (Plist not found at {} — nothing to remove)",
            plist_path.display()
        );
    }

    // ── Step 3: confirmation ──────────────────────────────────────────────────
    println!();
    println!("✓  Knowledge Builder uninstalled.");
    println!("   Your data (SQLite DB, processed vault notes) is untouched.");
    println!("   Run `kb install` to re-register the service at any time.");

    Ok(())
}
