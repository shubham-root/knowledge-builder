//! `kb doctor` — validate configuration and environment prerequisites.
//!
//! Runs the 8-point configuration validation plus additional runtime checks:
//!
//! | Check | Description |
//! |---|---|
//! | 1–8 | Config fields (vault/sources paths, processor, DB, log dir, backoff) |
//! | 9   | SQLite database integrity (`PRAGMA integrity_check`) if DB exists |
//! | 10  | Log directory is writable |
//!
//! Exits 0 when all checks pass; exits 1 with a diagnostic listing otherwise.

use anyhow::Result;

// ── Entry point ────────────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    println!("kb doctor — running pre-flight checks\n");

    let mut checker = Checker::new();

    // ── Step 1: parse + tilde-expand the config file ──────────────────────────
    let config = match kb_core::config::load_raw() {
        Ok(c) => {
            checker.pass("Config file parsed successfully.");
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
        println!("  [{}] ✓  {}", self.count, msg);
    }

    fn fail(&mut self, msg: String) {
        self.count += 1;
        self.all_passed = false;
        eprintln!("  [{}] ✗  {}", self.count, msg);
    }

    fn print_summary(&self) {
        println!();
        if self.all_passed {
            println!("✓  All checks passed — daemon is ready to start.");
            println!("   Run: kb daemon --foreground");
        } else {
            eprintln!(
                "✗  One or more checks failed. Fix the issues above and re-run `kb doctor`."
            );
        }
    }
}
