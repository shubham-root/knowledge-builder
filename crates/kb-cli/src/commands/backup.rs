//! `kb backup` and `kb restore` — SQLite database backup and recovery.
//!
//! ## `kb backup [--output <path>]`
//! Uses SQLite `VACUUM INTO` to produce a consistent, compacted snapshot of
//! the state database without interfering with a running daemon (WAL mode
//! allows concurrent reads).  After writing, performs a `PRAGMA integrity_check`
//! on the new file and deletes it if the check fails.
//!
//! Default output: `<db_dir>/backups/state-<YYYY-MM-DD>.db`
//!
//! ## `kb restore <backup_path>`
//! Verifies the backup is healthy, requires confirmation (interactive or
//! `--force`), errors if the daemon is currently running, then atomically
//! swaps the live database with the backup (`current.db → current.db.bak`,
//! `backup → current.db`).

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::commands::db::fmt_bytes;

// ── Subcommand argument structs ────────────────────────────────────────────────

/// Arguments for `kb backup`.
#[derive(Args, Debug)]
pub struct BackupArgs {
    /// Output path for the backup file.
    /// Default: `<db_dir>/backups/state-<YYYY-MM-DD>.db`
    #[arg(long, short, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

/// Arguments for `kb restore`.
#[derive(Args, Debug)]
pub struct RestoreArgs {
    /// Path to the backup `.db` file to restore from.
    pub backup_path: PathBuf,

    /// Skip the interactive confirmation prompt.
    ///
    /// **Danger:** This overwrites the live database without asking.
    /// The current database is still preserved as `<db_path>.bak`.
    #[arg(long)]
    pub force: bool,
}

// ── kb backup ──────────────────────────────────────────────────────────────────

pub async fn run_backup(args: BackupArgs) -> Result<()> {
    let config = kb_core::config::load_raw().context("failed to load configuration")?;
    let db_path = PathBuf::from(&config.paths.db_path);

    if !db_path.exists() {
        bail!(
            "Database not found at '{}'.\n\
             Start the daemon at least once to create it, then re-run `kb backup`.",
            db_path.display()
        );
    }

    // Determine output path.
    let backup_path = match args.output {
        Some(p) => p,
        None => default_backup_path(&db_path),
    };

    // Ensure the backup directory exists.
    if let Some(parent) = backup_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("cannot create backup directory '{}'", parent.display())
        })?;
    }

    println!("Creating backup at {} …", backup_path.display());

    // --- VACUUM INTO (blocking; must not run on the tokio thread pool) ----------
    {
        // Convert paths to UTF-8 strings for the SQL statement.
        let db_str = path_to_utf8(&db_path)?;
        let bk_str = path_to_utf8(&backup_path)?;

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = rusqlite::Connection::open(&db_str)
                .with_context(|| format!("cannot open database '{db_str}'"))?;

            // Single-quote escape for embedded SQL literal.
            let escaped_bk = bk_str.replace('\'', "''");
            conn.execute_batch(&format!("VACUUM INTO '{escaped_bk}'"))
                .with_context(|| format!("VACUUM INTO '{}' failed", bk_str))?;
            Ok(())
        })
        .await
        .context("backup VACUUM INTO task panicked")??;
    }

    // --- Integrity check on the just-created backup ----------------------------
    let bp_check = backup_path.clone();
    let integrity = tokio::task::spawn_blocking(move || sqlite_integrity_check(&bp_check))
        .await
        .context("integrity check task panicked")??;

    if integrity.len() != 1 || integrity[0] != "ok" {
        // Delete the corrupt backup so we do not leave garbage behind.
        let _ = std::fs::remove_file(&backup_path);
        bail!(
            "Backup integrity check failed:\n  {}\n\
             The corrupt backup file has been deleted. \
             Re-run `kb backup` after investigating the database.",
            integrity.join("\n  ")
        );
    }

    let size = std::fs::metadata(&backup_path)
        .map(|m| m.len())
        .unwrap_or(0);

    println!(
        "Backup created: {} ({})",
        backup_path.display(),
        fmt_bytes(size as i64),
    );

    Ok(())
}

// ── kb restore ─────────────────────────────────────────────────────────────────

pub async fn run_restore(args: RestoreArgs) -> Result<()> {
    let config = kb_core::config::load_raw().context("failed to load configuration")?;
    let db_path = PathBuf::from(&config.paths.db_path);

    // --- Verify backup file exists --------------------------------------------
    if !args.backup_path.exists() {
        bail!(
            "Backup file not found: '{}'\n\
             Use `kb backup --output <path>` to create one.",
            args.backup_path.display()
        );
    }

    // --- Verify backup integrity before touching anything ---------------------
    let bp_check = args.backup_path.clone();
    let integrity = tokio::task::spawn_blocking(move || sqlite_integrity_check(&bp_check))
        .await
        .context("integrity check task panicked")??;

    if integrity.len() != 1 || integrity[0] != "ok" {
        bail!(
            "Backup integrity check failed — the backup file appears corrupt:\n  {}\n\
             Restore aborted. The live database was NOT modified.",
            integrity.join("\n  ")
        );
    }

    // --- Refuse to restore while the daemon is running -----------------------
    if daemon_lock_is_held(&db_path) {
        bail!(
            "The knowledge-builder daemon is currently running and holds the database lock.\n\
             Stop it first:\n\
             \n  \
               launchctl stop com.user.knowledge-builder\n\
             \n\
             Then retry `kb restore '{}'`.",
            args.backup_path.display()
        );
    }

    // --- Require confirmation -------------------------------------------------
    if !args.force {
        let bak_display = {
            let mut s = OsString::from(db_path.as_os_str());
            s.push(".bak");
            PathBuf::from(s).display().to_string()
        };

        eprintln!();
        eprintln!("⚠️  WARNING: This operation will REPLACE the current database.");
        eprintln!();
        eprintln!("  Current database : {}", db_path.display());
        eprintln!("  Will be moved to : {bak_display}");
        eprintln!("  Restoring from   : {}", args.backup_path.display());
        eprintln!();
        eprint!("Type 'yes' to continue, anything else to cancel: ");
        std::io::stderr().flush().ok();

        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .context("failed to read confirmation from stdin")?;

        if answer.trim() != "yes" {
            bail!("Restore cancelled. The live database was NOT modified.");
        }
    }

    // --- Atomic swap ----------------------------------------------------------

    // Build the .bak path using OsString so non-UTF-8 characters are preserved.
    let bak_path = {
        let mut s = OsString::from(db_path.as_os_str());
        s.push(".bak");
        PathBuf::from(s)
    };

    // Move the current DB out of the way (best-effort; it may not exist yet).
    if db_path.exists() {
        std::fs::rename(&db_path, &bak_path).with_context(|| {
            format!(
                "cannot move current database '{}' to backup location '{}'",
                db_path.display(),
                bak_path.display()
            )
        })?;
    }

    // Ensure the parent directory of the live DB exists.
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "cannot create database parent directory '{}'",
                parent.display()
            )
        })?;
    }

    // Copy the backup into the live location.
    if let Err(copy_err) = std::fs::copy(&args.backup_path, &db_path) {
        // Try to restore the original if the copy fails.
        if bak_path.exists() {
            let _ = std::fs::rename(&bak_path, &db_path);
        }
        return Err(copy_err).with_context(|| {
            format!(
                "failed to copy '{}' → '{}'; \
                 attempted to restore the original database",
                args.backup_path.display(),
                db_path.display()
            )
        });
    }

    println!(
        "Restored from: {}. Previous DB saved to {}",
        args.backup_path.display(),
        bak_path.display(),
    );

    Ok(())
}

// ── Shared helpers (pub(crate) so doctor.rs can use them) ─────────────────────

/// Compute the default backup output path.
///
/// Path: `<db_dir>/backups/state-<YYYY-MM-DD>.db`
pub(crate) fn default_backup_path(db_path: &Path) -> PathBuf {
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let db_dir = db_path.parent().unwrap_or(Path::new("."));
    db_dir.join("backups").join(format!("state-{date}.db"))
}

/// Return the canonical backups directory for a given `db_path`.
///
/// Does not create the directory — callers must create it if needed.
pub(crate) fn backups_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("backups")
}

/// Scan the backups directory and return the age (in whole days) of the most
/// recently-modified `.db` file, or `None` if no backups exist.
pub(crate) fn last_backup_age_days(db_path: &Path) -> Option<u64> {
    let dir = backups_dir(db_path);
    if !dir.is_dir() {
        return None;
    }

    let entries = std::fs::read_dir(&dir).ok()?;
    let mut newest: Option<SystemTime> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("db") {
            continue;
        }
        if let Ok(meta) = path.metadata() {
            if let Ok(modified) = meta.modified() {
                newest = Some(match newest {
                    None => modified,
                    Some(prev) => prev.max(modified),
                });
            }
        }
    }

    newest.map(|t| {
        SystemTime::now()
            .duration_since(t)
            .unwrap_or(Duration::ZERO)
            .as_secs()
            / 86_400
    })
}

// ── Private helpers ────────────────────────────────────────────────────────────

/// Run `PRAGMA integrity_check` on a SQLite database file.
///
/// Returns the vector of rows returned by the pragma.  A healthy database
/// returns exactly `["ok"]`.
fn sqlite_integrity_check(path: &Path) -> Result<Vec<String>> {
    let conn = rusqlite::Connection::open(path)
        .with_context(|| format!("cannot open '{}' for integrity check", path.display()))?;

    let mut stmt = conn
        .prepare("PRAGMA integrity_check")
        .context("cannot prepare PRAGMA integrity_check")?;

    let rows: rusqlite::Result<Vec<String>> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("integrity_check query failed")?
        .collect();

    rows.context("error collecting integrity_check rows")
}

/// Return `true` if another process is currently holding the daemon singleton
/// lock for the given `db_path`.
///
/// Implemented by attempting a non-blocking `flock`; success means no lock
/// holder (daemon not running).
fn daemon_lock_is_held(db_path: &Path) -> bool {
    match kb_core::DaemonLock::acquire(db_path) {
        Ok(_lock) => false, // we acquired the lock — daemon is not running
        Err(kb_core::LockError::AlreadyRunning { .. }) => true,
        Err(_) => false, // unexpected I/O error; err on the side of allowing restore
    }
}

/// Convert a `Path` to a `String`, returning an error for non-UTF-8 paths.
fn path_to_utf8(path: &Path) -> Result<String> {
    path.to_str()
        .with_context(|| format!("path '{}' contains non-UTF-8 characters", path.display()))
        .map(|s| s.to_owned())
}
