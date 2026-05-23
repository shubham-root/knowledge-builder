//! `kb scan` — trigger an immediate full-vault scan.
//!
//! **HTTP-only:** This command requires a running daemon, because the scan is
//! executed by the daemon's periodic-scanner component.  If the daemon is not
//! reachable, the command prints a clear message and exits.
//!
//! # Behaviour
//! - If daemon is running: `POST /scan` and print confirmation.
//! - If daemon is not running: print "daemon not running" and exit 0.
//!
//! # Example
//! ```text
//! $ kb scan
//! Scan triggered. The daemon will process any new files shortly.
//!
//! $ kb scan   # (daemon not running)
//! Daemon is not running — cannot trigger a scan.
//! Start the daemon with `kb daemon --foreground` or `kb install`.
//! ```

use anyhow::{Context, Result};
use kb_core::config::load_raw;

use crate::client::DaemonClient;

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    let config = load_raw().context("failed to load configuration")?;

    if let Some(client) = DaemonClient::try_connect(&config.ops.http_bind).await {
        // ── HTTP mode ─────────────────────────────────────────────────────────
        client.trigger_scan().await?;
        println!("Scan triggered. The daemon will process any new files shortly.");
    } else {
        // ── Daemon not running ────────────────────────────────────────────────
        println!(
            "Daemon is not running — cannot trigger a scan.\n\
             Start the daemon with `kb daemon --foreground` or `kb install`."
        );
    }

    Ok(())
}
