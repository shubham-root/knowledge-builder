//! FSEvents watcher with extension-allowlist and glob-ignore filtering.
//!
//! ## Event flow
//! ```text
//! macOS FSEvents
//!   └─► notify-debouncer-full (200 ms debounce window)
//!         └─► std::sync::mpsc::Sender<DebounceEventResult>  [OS thread]
//!               └─► tokio::task::spawn_blocking bridge        [blocking pool thread]
//!                     └─► is_allowed() filter
//!                           └─► tokio::sync::mpsc::Sender<WatchEvent>  [caller's task]
//! ```
//!
//! ## Filter pipeline (all conditions must pass)
//! 1. No dotfile path components (any segment starting with `.`).
//! 2. No `~$…` Microsoft Office lock-files.
//! 3. No `*.icloud` iCloud placeholder files.
//! 4. File extension must be in the configured allowlist (case-insensitive).
//! 5. Full path must not match any ignore-glob pattern.
//!
//! ## Events forwarded
//! | `notify` event | Maps to |
//! |---|---|
//! | `Create(_)` | `WatchEventKind::Created` |
//! | `Modify(Data(_))` | `WatchEventKind::Modified` |
//! | `Modify(Name(Both))` | `WatchEventKind::Renamed { from: Some(..) }` |
//! | `Modify(Name(To))` | `WatchEventKind::Renamed { from: None }` |
//! | `Modify(Name(Any))` where path exists | `WatchEventKind::Renamed { from: None }` |
//! | `Modify(_)` (other) | `WatchEventKind::Modified` |
//! | All other kinds | *dropped* |

use std::{
    path::{Component, Path, PathBuf},
    sync::mpsc as std_mpsc,
    time::Duration,
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::Watcher;
use notify::{RecommendedWatcher, RecursiveMode};
use notify::event::{ModifyKind, RenameMode};
use notify_debouncer_full::{new_debouncer, Debouncer, FileIdMap};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

/// Debounce window — coalesces rapid bursts on the same path before delivery.
const DEBOUNCE_MS: u64 = 200;

// ── Public types ──────────────────────────────────────────────────────────────

/// A filtered file-system event produced by [`FileWatcher`].
#[derive(Debug, Clone)]
pub struct WatchEvent {
    /// Absolute path of the file that triggered the event.
    pub path: PathBuf,
    /// The type of change that was detected.
    pub kind: WatchEventKind,
}

/// The category of file-system change detected.
#[derive(Debug, Clone)]
pub enum WatchEventKind {
    /// A new file appeared in the sources directory tree.
    Created,
    /// An existing file's content was modified in place.
    Modified,
    /// A file was renamed or moved into scope.
    ///
    /// `from` is the previous absolute path when the debouncer was able to
    /// stitch the rename pair together; `None` when only the destination
    /// event was observed (common on macOS FSEvents).
    Renamed { from: Option<PathBuf> },
}

/// Errors that can occur while constructing or starting a [`FileWatcher`].
#[derive(Debug, thiserror::Error)]
pub enum WatcherError {
    /// One of the supplied ignore-glob patterns is syntactically invalid.
    #[error("failed to compile ignore glob pattern '{pattern}': {source}")]
    GlobBuild {
        pattern: String,
        #[source]
        source: globset::Error,
    },
    /// `notify` could not initialise the platform watcher (FSEvents, kqueue …).
    #[error("failed to initialise notify watcher: {0}")]
    Notify(#[from] notify::Error),
    /// `sources_dir` does not exist or is not a directory.
    #[error("sources_dir does not exist or is not a directory: {0}")]
    SourcesDir(PathBuf),
    /// [`FileWatcher::start`] was called more than once.
    #[error("watcher has already been started")]
    AlreadyStarted,
}

// ── FileWatcher ───────────────────────────────────────────────────────────────

/// Watches `sources_dir` recursively using FSEvents and forwards filtered
/// [`WatchEvent`]s through a [`tokio::sync::mpsc`] channel.
///
/// # Lifecycle
/// 1. Construct with [`FileWatcher::new`].
/// 2. Call [`FileWatcher::start`] once to activate watching.
/// 3. Keep the `FileWatcher` alive — dropping it tears down the watcher thread.
///
/// # Example
/// ```no_run
/// use std::path::PathBuf;
/// use tokio::sync::mpsc;
/// use kb_watcher::events::{FileWatcher, WatchEvent};
///
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// let (tx, mut rx) = mpsc::channel::<WatchEvent>(256);
/// let mut watcher = FileWatcher::new(
///     PathBuf::from("/tmp/vault/sources"),
///     vec!["pdf".into(), "docx".into()],
///     vec!["**/.obsidian/**".into()],
///     tx,
/// )?;
/// watcher.start()?;
///
/// while let Some(event) = rx.recv().await {
///     println!("{event:?}");
/// }
/// # Ok(())
/// # }
/// ```
pub struct FileWatcher {
    sources_dir: PathBuf,
    extensions: Vec<String>,
    ignore_set: GlobSet,
    sender: mpsc::Sender<WatchEvent>,
    /// Keeps the debouncer alive; `None` until [`start`][Self::start] is called.
    _debouncer: Option<Debouncer<RecommendedWatcher, FileIdMap>>,
}

impl std::fmt::Debug for FileWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileWatcher")
            .field("sources_dir", &self.sources_dir)
            .field("extensions", &self.extensions)
            .field("active", &self._debouncer.is_some())
            .finish_non_exhaustive()
    }
}

impl FileWatcher {
    /// Construct a new `FileWatcher`.
    ///
    /// # Parameters
    /// * `sources_dir`  — root directory to watch recursively.
    /// * `extensions`   — lower-case, dot-free extensions to admit
    ///                    (e.g. `["pdf", "docx", "xlsx"]`).  Comparison is
    ///                    case-insensitive.
    /// * `ignore_globs` — glob patterns for paths to reject
    ///                    (e.g. `["**/.obsidian/**", "**/~$*"]`).
    /// * `sender`       — tokio channel through which filtered events are sent.
    ///
    /// # Errors
    /// Returns [`WatcherError::GlobBuild`] if any ignore-glob pattern is
    /// syntactically invalid.
    pub fn new(
        sources_dir: PathBuf,
        extensions: Vec<String>,
        ignore_globs: Vec<String>,
        sender: mpsc::Sender<WatchEvent>,
    ) -> Result<Self, WatcherError> {
        let ignore_set = build_glob_set(&ignore_globs)?;
        Ok(Self {
            sources_dir,
            extensions,
            ignore_set,
            sender,
            _debouncer: None,
        })
    }

    /// Start the FSEvents watcher.
    ///
    /// Internally this:
    /// 1. Validates that `sources_dir` exists and is a directory.
    /// 2. Creates a `notify-debouncer-full` debouncer with a 200 ms window.
    /// 3. Registers a recursive watch on `sources_dir`.
    /// 4. Seeds the file-ID cache for rename stitching (macOS FSEvents).
    /// 5. Spawns a `tokio::task::spawn_blocking` task that drains the
    ///    `std::sync::mpsc` bridge and forwards filtered events to the
    ///    tokio sender.
    ///
    /// The watch remains active until `self` is dropped.
    ///
    /// # Errors
    /// * [`WatcherError::SourcesDir`]     — path does not exist / not a directory.
    /// * [`WatcherError::Notify`]         — platform watcher could not subscribe.
    /// * [`WatcherError::AlreadyStarted`] — `start` called more than once.
    pub fn start(&mut self) -> Result<(), WatcherError> {
        if self._debouncer.is_some() {
            return Err(WatcherError::AlreadyStarted);
        }
        if !self.sources_dir.exists() || !self.sources_dir.is_dir() {
            return Err(WatcherError::SourcesDir(self.sources_dir.clone()));
        }

        // Bridge: connects the notify OS thread to the tokio blocking task.
        // `std::sync::mpsc::Sender<DebounceEventResult>` implements
        // `DebounceEventHandler` natively in notify-debouncer-full >= 0.3.
        let (bridge_tx, bridge_rx) = std_mpsc::channel();

        let mut debouncer = new_debouncer(
            Duration::from_millis(DEBOUNCE_MS),
            None,
            bridge_tx,
        )?;

        // Register recursive watch on sources_dir.
        debouncer
            .watcher()
            .watch(&self.sources_dir, RecursiveMode::Recursive)?;

        // Seed the file-ID cache so the debouncer can stitch rename pairs.
        // FSEvents and Windows deliver From+To as two separate events; the
        // cache lets notify-debouncer-full match them into RenameMode::Both.
        debouncer
            .cache()
            .add_root(&self.sources_dir, RecursiveMode::Recursive);

        // Clone state needed by the background task.
        let extensions = self.extensions.clone();
        let ignore_set = self.ignore_set.clone();
        let sender     = self.sender.clone();

        // Drain the bridge on a dedicated blocking thread so we never block
        // the tokio executor.
        tokio::task::spawn_blocking(move || {
            debug!("watcher bridge task started");
            for result in &bridge_rx {
                match result {
                    Ok(debounced_events) => {
                        for debounced in debounced_events {
                            dispatch_event(
                                debounced.event,
                                &extensions,
                                &ignore_set,
                                &sender,
                            );
                        }
                    }
                    Err(errors) => {
                        for e in errors {
                            error!(error = %e, "notify watcher error");
                        }
                    }
                }
            }
            debug!("watcher bridge channel closed — watcher has stopped");
        });

        self._debouncer = Some(debouncer);
        Ok(())
    }
}

// ── Filtering ─────────────────────────────────────────────────────────────────

/// Build a [`GlobSet`] from a list of glob pattern strings.
pub(crate) fn build_glob_set(patterns: &[String]) -> Result<GlobSet, WatcherError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|e| WatcherError::GlobBuild {
            pattern: pattern.clone(),
            source:  e,
        })?;
        builder.add(glob);
    }
    // GlobSetBuilder::build() is infallible after valid Globs are added, but
    // the error type is the same so we map it for completeness.
    builder.build().map_err(|e| WatcherError::GlobBuild {
        pattern: "<set>".into(),
        source:  e,
    })
}

/// Returns `true` if `path` should be forwarded to the consumer.
///
/// All five filter stages must pass:
/// 1. No dotfile path components (any `Normal` segment starting with `.`).
/// 2. No `~$…` Office lock-files.
/// 3. No `*.icloud` placeholder files.
/// 4. Extension in the allowlist (case-insensitive).
/// 5. Full path does not match any ignore-glob.
pub(crate) fn is_allowed(path: &Path, extensions: &[String], ignore_set: &GlobSet) -> bool {
    // 1. Reject dotfiles — any Normal component whose name starts with '.'
    for component in path.components() {
        if let Component::Normal(os_str) = component {
            if let Some(s) = os_str.to_str() {
                if s.starts_with('.') {
                    return false;
                }
            }
        }
    }

    let filename = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None    => return false,
    };

    // 2. Reject Office lock-files (~$<filename>).
    if filename.starts_with("~$") {
        return false;
    }

    // 3. Reject iCloud placeholder files (<filename>.icloud).
    //    Also caught by the default ignore_globs but we check explicitly for
    //    clarity and defence-in-depth.
    if filename.ends_with(".icloud") {
        return false;
    }

    // 4. Extension must appear in the allowlist (case-insensitive).
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if extensions.iter().any(|a| a.eq_ignore_ascii_case(ext)) => {}
        _ => return false,
    }

    // 5. Reject paths that match any configured ignore glob.
    if ignore_set.is_match(path) {
        return false;
    }

    true
}

// ── Event dispatch ────────────────────────────────────────────────────────────

/// Translate a single [`notify::Event`] into zero or more [`WatchEvent`]s
/// and forward any that pass the filter through `sender`.
fn dispatch_event(
    event:      notify::Event,
    extensions: &[String],
    ignore_set: &GlobSet,
    sender:     &mpsc::Sender<WatchEvent>,
) {
    use notify::EventKind;

    match &event.kind {
        // ── New file appeared ─────────────────────────────────────────────────
        EventKind::Create(_) => {
            for path in &event.paths {
                if is_allowed(path, extensions, ignore_set) {
                    emit(sender, WatchEvent {
                        path: path.clone(),
                        kind: WatchEventKind::Created,
                    });
                }
            }
        }

        // ── Data modification (content written) ───────────────────────────────
        EventKind::Modify(ModifyKind::Data(_)) => {
            for path in &event.paths {
                if is_allowed(path, extensions, ignore_set) {
                    emit(sender, WatchEvent {
                        path: path.clone(),
                        kind: WatchEventKind::Modified,
                    });
                }
            }
        }

        // ── Rename: debouncer stitched From+To into one event ─────────────────
        // Convention from notify-debouncer-full: paths[0] = source,
        //                                         paths[1] = destination.
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            if event.paths.len() >= 2 {
                let from = &event.paths[0];
                let to   = &event.paths[1];
                if is_allowed(to, extensions, ignore_set) {
                    emit(sender, WatchEvent {
                        path: to.clone(),
                        kind: WatchEventKind::Renamed { from: Some(from.clone()) },
                    });
                }
            }
        }

        // ── Rename destination only (no matching From in the debounce window) ─
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            for path in &event.paths {
                if is_allowed(path, extensions, ignore_set) {
                    emit(sender, WatchEvent {
                        path: path.clone(),
                        kind: WatchEventKind::Renamed { from: None },
                    });
                }
            }
        }

        // ── FSEvents / kqueue: individual Any events for each rename path ──────
        // The debouncer may have already stitched these into Both above;
        // handle residual Any events by checking whether the path exists
        // (destination is present; source has been moved away).
        EventKind::Modify(ModifyKind::Name(RenameMode::Any)) => {
            for path in &event.paths {
                if path.exists() && is_allowed(path, extensions, ignore_set) {
                    emit(sender, WatchEvent {
                        path: path.clone(),
                        kind: WatchEventKind::Renamed { from: None },
                    });
                }
            }
        }

        // ── Other Modify variants (metadata, ownership, …) ────────────────────
        // Some FSEvents back-ends surface content changes as Modify(Other).
        // Forward as Modified rather than silently discard.
        EventKind::Modify(_) => {
            for path in &event.paths {
                if is_allowed(path, extensions, ignore_set) {
                    emit(sender, WatchEvent {
                        path: path.clone(),
                        kind: WatchEventKind::Modified,
                    });
                }
            }
        }

        // ── Everything else (Access, Remove, …) — not interesting ─────────────
        _ => {}
    }
}

/// Forward a [`WatchEvent`] through the tokio channel from a blocking context.
///
/// `blocking_send` is safe here because this function is called exclusively
/// from `tokio::task::spawn_blocking`, never from within an async task.
///
/// A `Send` error means the receiver has been dropped (normal during daemon
/// shutdown); we log at `warn` and discard the event.
#[inline]
fn emit(sender: &mpsc::Sender<WatchEvent>, event: WatchEvent) {
    let path = event.path.clone();
    if let Err(_e) = sender.blocking_send(event) {
        warn!(path = %path.display(), "watcher: consumer channel closed; discarding event");
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn no_ignores() -> GlobSet {
        GlobSetBuilder::new().build().unwrap()
    }

    fn exts(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    // ── is_allowed: extension allowlist ───────────────────────────────────────

    #[test]
    fn allows_matching_extension() {
        assert!(is_allowed(
            Path::new("/vault/sources/doc.pdf"),
            &exts(&["pdf", "docx"]),
            &no_ignores(),
        ));
    }

    #[test]
    fn extension_comparison_is_case_insensitive() {
        assert!(is_allowed(
            Path::new("/vault/sources/doc.PDF"),
            &exts(&["pdf"]),
            &no_ignores(),
        ));
    }

    #[test]
    fn rejects_unlisted_extension() {
        assert!(!is_allowed(
            Path::new("/vault/sources/note.md"),
            &exts(&["pdf"]),
            &no_ignores(),
        ));
    }

    #[test]
    fn rejects_path_without_extension() {
        assert!(!is_allowed(
            Path::new("/vault/sources/README"),
            &exts(&["pdf"]),
            &no_ignores(),
        ));
    }

    // ── is_allowed: dotfile rejection ─────────────────────────────────────────

    #[test]
    fn rejects_dotfile_directory_component() {
        assert!(!is_allowed(
            Path::new("/vault/sources/.hidden/doc.pdf"),
            &exts(&["pdf"]),
            &no_ignores(),
        ));
    }

    #[test]
    fn rejects_dotfile_name() {
        assert!(!is_allowed(
            Path::new("/vault/sources/.secret.pdf"),
            &exts(&["pdf"]),
            &no_ignores(),
        ));
    }

    #[test]
    fn rejects_obsidian_config_dir() {
        assert!(!is_allowed(
            Path::new("/vault/.obsidian/plugins/doc.pdf"),
            &exts(&["pdf"]),
            &no_ignores(),
        ));
    }

    // ── is_allowed: Office lock-file rejection ────────────────────────────────

    #[test]
    fn rejects_office_lockfile() {
        assert!(!is_allowed(
            Path::new("/vault/sources/~$document.docx"),
            &exts(&["docx"]),
            &no_ignores(),
        ));
    }

    // ── is_allowed: iCloud placeholder rejection ──────────────────────────────

    #[test]
    fn rejects_icloud_placeholder() {
        assert!(!is_allowed(
            Path::new("/vault/sources/doc.pdf.icloud"),
            &exts(&["pdf"]),
            &no_ignores(),
        ));
    }

    // ── is_allowed: ignore-glob rejection ────────────────────────────────────

    #[test]
    fn rejects_path_matching_ignore_glob() {
        let gs = build_glob_set(&["**/.obsidian/**".to_string()]).unwrap();
        assert!(!is_allowed(
            Path::new("/vault/.obsidian/plugins/doc.pdf"),
            &exts(&["pdf"]),
            &gs,
        ));
    }

    #[test]
    fn allows_path_not_matching_glob() {
        let gs = build_glob_set(&["**/.obsidian/**".to_string()]).unwrap();
        assert!(is_allowed(
            Path::new("/vault/sources/paper.pdf"),
            &exts(&["pdf"]),
            &gs,
        ));
    }

    // ── build_glob_set ────────────────────────────────────────────────────────

    #[test]
    fn empty_glob_set_matches_nothing() {
        let gs = build_glob_set(&[]).unwrap();
        assert!(!gs.is_match("/any/path/file.pdf"));
    }

    #[test]
    fn invalid_glob_pattern_returns_error() {
        let result = build_glob_set(&["[invalid".to_string()]);
        assert!(
            matches!(result, Err(WatcherError::GlobBuild { .. })),
            "expected GlobBuild error, got {result:?}",
        );
    }

    // ── FileWatcher::new ──────────────────────────────────────────────────────

    #[test]
    fn new_succeeds_with_valid_parameters() {
        let (tx, _rx) = mpsc::channel::<WatchEvent>(8);
        let result = FileWatcher::new(
            PathBuf::from("/tmp"),
            vec!["pdf".into()],
            vec![],
            tx,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn new_fails_with_invalid_glob() {
        let (tx, _rx) = mpsc::channel::<WatchEvent>(8);
        let result = FileWatcher::new(
            PathBuf::from("/tmp"),
            vec!["pdf".into()],
            vec!["[bad-glob".into()],
            tx,
        );
        assert!(
            matches!(result, Err(WatcherError::GlobBuild { .. })),
            "expected GlobBuild error",
        );
    }

    // ── FileWatcher::start (negative cases, no FS access needed) ─────────────

    #[test]
    fn start_fails_when_sources_dir_nonexistent() {
        let (tx, _rx) = mpsc::channel::<WatchEvent>(8);
        let mut watcher = FileWatcher::new(
            PathBuf::from("/nonexistent/path/that/cannot/exist"),
            vec!["pdf".into()],
            vec![],
            tx,
        ).unwrap();
        let err = watcher.start().unwrap_err();
        assert!(
            matches!(err, WatcherError::SourcesDir(_)),
            "expected SourcesDir error, got {err:?}",
        );
    }
}
