//! `kb doctor` — validate configuration and environment prerequisites.
//!
//! Runs the 8-point configuration validation plus additional runtime checks:
//!
//! | Check | Description |
//! |---|---|
//! | 1–8 | Config fields (vault/sources paths, processor, DB, log dir, backoff) |
//! | 9   | SQLite database integrity (`PRAGMA integrity_check`) if DB exists |
//! | 10  | Log directory is writable |
//! | 11  | Backup health (warns if no backups or last backup > 7 days old) |
//!
//! Exits 0 when all checks pass; exits 1 with a diagnostic listing otherwise.

use anyhow::Result;

use super::backup;

// ── Entry point ────────────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    println!("kb doctor — running pre-flight checks\n");

    let mut checker = Checker::new();

    // ── Step 1: parse + tilde-expand the config file ──────────────────────────
    let config_path = kb_core::config::config_file_path();
    if config_path.exists() {
        checker.pass(&format!(
            "Config file found at {}",
            config_path.display()
        ));
    } else {
        checker.warn(format!(
            "No config file at {} — using built-in defaults. \
             Create this file to override.",
            config_path.display()
        ));
    }

    let config = match kb_core::config::load_raw() {
        Ok(c) => {
            checker.pass("Config parsed successfully.");
            c
        }
        Err(e) => {
            checker.fail(format!("Cannot parse configuration: {e}"));
            checker.print_summary();
            std::process::exit(1);
        }
    };

    // ── Step 2: run the 8-point validation ────────────────────────────────────
    match config.validate() {
        Ok(()) => {
            checker.pass("All 8 configuration checks passed.");
        }
        Err(errs) => {
            let count = errs.len();
            for e in &errs {
                checker.fail(format!("{e}"));
            }
            eprintln!(
                "\n  → {count} configuration error(s). Run `kb config validate` for full detail."
            );
        }
    }

    // ── Step 3: SQLite integrity check (only if DB file already exists) ───────
    let db_path_str = config.paths.db_path.clone();
    let db_path     = std::path::PathBuf::from(&db_path_str);
    if db_path.exists() {
        let check_result = tokio::task::spawn_blocking(move || {
            let conn   = kb_core::migrations::db_open(&db_path)?;
            let result: String =
                conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            Ok::<String, anyhow::Error>(result)
        })
        .await;

        match check_result {
            Ok(Ok(ref s)) if s == "ok" => {
                checker.pass("SQLite integrity check passed.");
            }
            Ok(Ok(ref s)) => {
                checker.fail(format!(
                    "SQLite integrity check failed: {s}\n\
                     \t   Backup the DB immediately; consider `VACUUM INTO backup.db`."
                ));
            }
            Ok(Err(e)) => {
                checker.fail(format!(
                    "Cannot open database '{db_path_str}': {e}\n\
                     \t   Ensure the path is writable and not locked by another process."
                ));
            }
            Err(e) => {
                checker.fail(format!("SQLite integrity check task panicked: {e}"));
            }
        }
    } else {
        checker.pass(&format!(
            "Database will be created on first daemon start.\n\
             \t   (planned path: {})",
            config.paths.db_path
        ));
    }

    // ── Step 4: log directory writable ────────────────────────────────────────
    match std::fs::create_dir_all(&config.paths.log_dir) {
        Ok(()) => checker.pass(&format!(
            "Log directory is writable: {}",
            config.paths.log_dir
        )),
        Err(e) => checker.fail(format!(
            "Log directory '{}' cannot be created: {e}",
            config.paths.log_dir
        )),
    }

    // ── Step 5: backup health ─────────────────────────────────────────────────
    let db_path_for_backup = std::path::PathBuf::from(&config.paths.db_path);
    match backup::last_backup_age_days(&db_path_for_backup) {
        None => {
            checker.warn(
                "No database backups found. \
                 Run `kb backup` to create one."
                .to_string(),
            );
        }
        Some(days) if days > 7 => {
            checker.warn(format!(
                "Last backup is {days} day(s) old (threshold: 7 days). \
                 Run `kb backup` to refresh."
            ));
        }
        Some(days) => {
            checker.pass(&format!(
                "Last backup is {days} day(s) old — within the 7-day window."
            ));
        }
    }

    // ── Step 6: secrets file health ──────────────────────────────────────
    //
    // `~/.config/knowledge-builder/secrets.env` is where credentials such as
    // `OPENROUTER_API_KEY` live.  We print only the keys (never the values),
    // so the operator can confirm what is in scope without leaking secrets
    // to the terminal scrollback.
    match kb_core::load_secrets() {
        Ok(s) if !s.loaded => {
            let dir = s.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
            checker.warn(format!(
                "No secrets file at {}.\n\
                 \t   The processor will only see the daemon's inherited environment.\n\
                 \t   To add credentials, run:\n\
                 \t     mkdir -p {dir} && touch {p} && chmod 600 {p}",
                s.path.display(),
                dir = dir,
                p   = s.path.display(),
            ));
        }
        Ok(s) => {
            if s.insecure_perms {
                checker.fail(format!(
                    "Secrets file '{}' has permissive mode {:o}.\n\
                     \t   Run: chmod 600 '{}'",
                    s.path.display(),
                    s.mode.unwrap_or(0),
                    s.path.display(),
                ));
            } else if s.entries.is_empty() {
                checker.warn(format!(
                    "Secrets file '{}' exists but is empty.",
                    s.path.display(),
                ));
            } else {
                checker.pass(&format!(
                    "Secrets file readable with {} key(s): {:?}",
                    s.entries.len(),
                    s.keys(),
                ));
            }
        }
        Err(e) => {
            checker.fail(format!("Cannot read secrets file: {e}"));
        }
    }

    // ── Step 7: pi-coding-agent reachability ────────────────────────────
    //
    // The processor spawns `pi --mode rpc` for every job.  When the
    // daemon runs under launchd, version-manager shims (fnm, nvm) on
    // the operator's shell PATH are invisible because launchd does not
    // execute shell init scripts.  This check resolves `pi` against
    // the current shell's PATH, follows symlinks, and warns when the
    // resolution lands on a per-session shim that won't be reachable
    // from a daemon-managed process.
    match crate::pi_resolver::locate_pi() {
        crate::pi_resolver::PiLocation::Stable(p) => {
            checker.pass(&format!(
                "`pi` resolves to a stable path: {}", p.display(),
            ));
        }
        crate::pi_resolver::PiLocation::PerShellShim { shim, stable, manager } => {
            // Has the operator already pinned KB_PI_BIN somehow?  If
            // they have, the warning is purely informational; if not,
            // it's actionable.
            let pinned = std::env::var_os("KB_PI_BIN").is_some()
                      || secrets_has_kb_pi_bin();
            if pinned {
                checker.pass(&format!(
                    "`pi` is at a {manager} per-shell shim ({}) but \
                     KB_PI_BIN is already set; daemon will use the pinned path.",
                    shim.display(),
                ));
            } else {
                checker.warn(format!(
                    "`pi` resolves to a {manager} per-shell shim ({}).\n\
                     \t   This path is invisible to launchd-managed processes.\n\
                     \t   `kb install` will auto-bake the stable path into the\n\
                     \t   plist; if you run `kb daemon --foreground` from a non-\n\
                     \t   shell context (e.g. a service manager other than\n\
                     \t   launchd) you should also add to secrets.env:\n\
                     \t     KB_PI_BIN={}",
                    shim.display(), stable.display(),
                ));
            }
        }
        crate::pi_resolver::PiLocation::NotFound => {
            checker.fail(format!(
                "`pi` not found on PATH.  Install pi-coding-agent:\n\
                 \t   npm install -g @earendil-works/pi-coding-agent"
            ));
        }
    }

    // ── Final verdict ─────────────────────────────────────────────────────────
    checker.print_summary();
    if !checker.all_passed {
        std::process::exit(1);
    }

    Ok(())
}

// ── Checker helper ────────────────────────────────────────────────────────────

/// Tracks per-check results and formats them as a numbered list.
struct Checker {
    count:      u8,
    all_passed: bool,
}

impl Checker {
    fn new() -> Self {
        Self {
            count:      0,
            all_passed: true,
        }
    }

    fn pass(&mut self, msg: &str) {
        self.count += 1;
        println!("  [{}] \u{2713}  {}", self.count, msg);
    }

    /// Non-fatal warning — does not set `all_passed = false`.
    fn warn(&mut self, msg: String) {
        self.count += 1;
        println!("  [{}] \u{26a0}\u{fe0f}  {msg}", self.count);
    }

    fn fail(&mut self, msg: String) {
        self.count += 1;
        self.all_passed = false;
        eprintln!("  [{}] \u{2717}  {}", self.count, msg);
    }

    fn print_summary(&self) {
        println!();
        if self.all_passed {
            println!("\u{2713}  All checks passed \u{2014} daemon is ready to start.");
            println!("   Run: kb daemon --foreground");
        } else {
            eprintln!(
                "\u{2717}  One or more checks failed. Fix the issues above and re-run `kb doctor`."
            );
        }
    }
}

// ── Internal: read secrets.env and check for KB_PI_BIN ────────────────────────

/// Return `true` when the operator has set `KB_PI_BIN` in their
/// `secrets.env` file.  Used by the pi-resolver check so that an
/// operator who has already pinned the path doesn't see an actionable
/// warning every time they run `kb doctor`.
fn secrets_has_kb_pi_bin() -> bool {
    match kb_core::load_secrets() {
        Ok(s) => s.entries.contains_key("KB_PI_BIN"),
        Err(_) => false,
    }
}
