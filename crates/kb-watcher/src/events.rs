//! FSEvents watcher with extension-allowlist and glob-ignore filtering.
//!
//! Full implementation: T9.  This scaffold defines the public types so that
//! dependent crates can reference them before the implementation lands.

use std::path::PathBuf;
use tokio::sync::mpsc;

/// A candidate file path that has passed the extension and glob filters.
#[derive(Debug, Clone)]
pub struct CandidatePath(pub PathBuf);

/// Start the FSEvents watcher on `sources_dir`.
///
/// Returns a receiver that yields [`CandidatePath`] values as files are
/// created or modified inside the watched tree.
///
/// # Errors
/// Returns an error if the watcher cannot be initialised (e.g. the directory
/// does not exist or `notify` cannot subscribe).
pub async fn start(
    _sources_dir:   PathBuf,
    _extensions:    Vec<String>,
    _ignore_globs:  Vec<String>,
    _event_tx:      mpsc::Sender<CandidatePath>,
) -> kb_core::Result<()> {
    // TODO (T9): wire up notify-debouncer-full, apply filters, forward paths.
    tracing::debug!("watcher stub — not yet implemented");
    Ok(())
}
