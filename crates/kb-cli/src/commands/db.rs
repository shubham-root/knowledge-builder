//! Shared helper for opening a [`kb_core::StateStore`] from the loaded config.
//!
//! All three read-only commands (`status`, `list`, `show`) need a live
//! `StateStore`.  For now every CLI command opens the DB directly (offline
//! mode); a future task will add HTTP-first fallback when the daemon is
//! running.

use anyhow::{Context, Result};
use kb_core::{config::load_raw, StateStore};

/// Open the state DB and return a ready-to-use [`StateStore`].
///
/// Loads the config via [`load_raw`] (no validation) so the command works
/// even when the vault or processor are not yet configured.
///
/// # Errors
/// - Config cannot be loaded (missing or malformed TOML).
/// - Database cannot be opened or migrated (bad path, permissions, etc.).
pub async fn open_store() -> Result<StateStore> {
    let config = load_raw().context("failed to load configuration")?;

    let db_path = std::path::PathBuf::from(&config.paths.db_path);
    let backoff: Vec<u64> = config.worker.backoff_secs.clone();

    StateStore::new(&db_path, &backoff)
        .await
        .with_context(|| format!("cannot open database at '{}'", config.paths.db_path))
}

// ── Timestamp helpers ─────────────────────────────────────────────────────────

/// Format a Unix epoch-second timestamp as a local-time string.
///
/// Output: `2026-05-23 10:30:01` (always 19 chars wide).
pub fn fmt_ts(unix_secs: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(unix_secs, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => format!("{unix_secs}"),
    }
}

/// Format an optional Unix epoch-second timestamp, defaulting to `"—"`.
pub fn fmt_ts_opt(unix_secs: Option<i64>) -> String {
    match unix_secs {
        Some(ts) => fmt_ts(ts),
        None => "—".to_string(),
    }
}

/// Format a duration in seconds as a human-readable string.
///
/// Examples: `"5s"`, `"2m 05s"`, `"1h 03m"`, `"2d 05h"`.
pub fn fmt_age(secs: i64) -> String {
    if secs < 0 {
        return "0s".to_string();
    }
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let d = secs / 86400;
    if d > 0 {
        format!("{d}d {h:02}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Format byte count as a human-readable size.
///
/// Examples: `"512 B"`, `"1.5 KB"`, `"3.2 MB"`.
pub fn fmt_bytes(bytes: i64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Truncate a path string to `max_chars`, showing `…` in the middle if needed.
///
/// Attempts to preserve the filename by keeping as much of the tail as
/// possible after the ellipsis.
pub fn truncate_path(path: &str, max_chars: usize) -> String {
    if path.len() <= max_chars {
        return path.to_string();
    }
    if max_chars < 7 {
        return path[..max_chars].to_string();
    }
    // Keep the tail (filename) and as much prefix as fits.
    let ellipsis = "…";
    let tail_len = max_chars / 2;
    let head_len = max_chars - tail_len - ellipsis.len();
    let head = &path[..head_len];
    let tail = &path[path.len() - tail_len..];
    format!("{head}{ellipsis}{tail}")
}

/// Shorten a content hash like `sha256:<64-hex>` to `sha256:<12-hex>…`.
pub fn short_hash(hash: &str) -> String {
    // Expected: "sha256:abcdef..."
    if let Some(hex) = hash.strip_prefix("sha256:") {
        let prefix = &hex[..hex.len().min(12)];
        if hex.len() > 12 {
            format!("sha256:{prefix}…")
        } else {
            hash.to_string()
        }
    } else {
        // Unknown format — truncate generically.
        if hash.len() > 20 {
            format!("{}…", &hash[..17])
        } else {
            hash.to_string()
        }
    }
}
