//! Periodic full-scan backstop.
//!
//! A `tokio::time::interval`-driven `walkdir` over `sources_dir`.  Each
//! entry is subject to the same extension and glob filters as the FSEvents
//! watcher, ensuring files missed during sleep or cloud-sync materialisation
//! are eventually enqueued.
//!
//! Full implementation: T22.

use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::events::WatchEvent;

/// Start the periodic scanner.
///
/// Fires once immediately (pre-worker drain), then every
/// `poll_interval_secs` thereafter.
pub async fn start(
    _sources_dir:        PathBuf,
    _extensions:         Vec<String>,
    _ignore_globs:       Vec<String>,
    _poll_interval_secs: u64,
    _event_tx:           mpsc::Sender<WatchEvent>,
) -> kb_core::Result<()> {
    // TODO (T22): implement walkdir loop.
    tracing::debug!("scanner stub — not yet implemented");
    Ok(())
}
