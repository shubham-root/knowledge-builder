//! Configuration types and loader for Knowledge Builder.
//!
//! Loaded from `~/.config/knowledge-builder/config.toml` with optional
//! environment-variable overrides using `figment` (prefix `KB__`, separator `__`).
//!
//! ## Precedence (highest → lowest)
//! 1. `KB__*` environment variables  (e.g. `KB__PATHS__VAULT_ROOT=/my/vault`)
//! 2. `~/.config/knowledge-builder/config.toml`
//! 3. Built-in defaults
//!
//! ## Path expansion
//! All path-valued fields (`vault_root`, `sources_dir`, `db_path`, `log_dir`,
//! `processor.command`, `processor.work_dir_root`) undergo `~` expansion
//! immediately after the `figment` merge, before any validation.
//!
//! ## Validation
//! `Config::validate()` runs 8 checks and collects *all* failures before
//! returning.  Each error message is actionable — it names the problematic
//! value and tells the operator what to do.
//!
//! `Config::load()` is the primary entry point: it loads, expands, validates,
//! and returns a ready-to-use `Config` or a combined error listing all failures.
//!
//! For contexts that only need the raw config (e.g. `kb config show` printing
//! the resolved TOML without checking whether the vault exists), use the
//! free-standing `load_raw()` function instead.

use std::{
    fmt,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Error type ────────────────────────────────────────────────────────────────

/// A single configuration validation failure with an actionable error message.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// vault_root does not exist, is not a directory, or is not readable/writable.
    #[error(
        "vault_root '{path}': {detail}\n\
         Action: create the directory and ensure the current user has read+write access."
    )]
    VaultRoot { path: String, detail: String },

    /// sources_dir does not exist, is not a directory, or is not readable.
    #[error(
        "sources_dir '{path}': {detail}\n\
         Action: create the directory and ensure the current user has read access."
    )]
    SourcesDir { path: String, detail: String },

    /// sources_dir is not a subdirectory of vault_root (after canonicalization).
    #[error(
        "sources_dir '{sources_dir}' is not inside vault_root '{vault_root}'.\n\
         Action: set sources_dir to a path that is nested under vault_root."
    )]
    SourcesNotInsideVault { sources_dir: String, vault_root: String },

    /// sources_dir resolves to the exact same path as vault_root.
    #[error(
        "sources_dir '{sources_dir}' resolves to the same path as vault_root '{vault_root}'.\n\
         Action: set sources_dir to a strict subdirectory of vault_root (e.g. vault_root/Sources)."
    )]
    SourcesSameAsVault { sources_dir: String, vault_root: String },

    /// processor.command could not be found or is not executable.
    #[error(
        "processor.command '{command}': {detail}\n\
         Action: verify the script path is correct, that the file exists, \
         and that it has execute permission (`chmod +x`)."
    )]
    ProcessorCommand { command: String, detail: String },

    /// db_path parent directory cannot be created or SQLite open failed.
    #[error(
        "db_path '{db_path}': {detail}\n\
         Action: ensure the parent directory exists or can be created, \
         and that the current user can write there."
    )]
    DbPath { db_path: String, detail: String },

    /// log_dir cannot be created.
    #[error(
        "log_dir '{log_dir}': {detail}\n\
         Action: ensure the parent directory exists or can be created, \
         and that the current user can write there."
    )]
    LogDir { log_dir: String, detail: String },

    /// backoff_secs is too short relative to max_attempts.
    #[error(
        "worker.backoff_secs has {len} entr{len_plural} but worker.max_attempts={max_attempts} \
         requires at least {needed} (one entry per retry, i.e. max_attempts − 1).\n\
         Action: add more entries to [[worker].backoff_secs] in config.toml."
    )]
    BackoffTooShort {
        len: usize,
        len_plural: &'static str,
        needed: usize,
        max_attempts: u32,
    },
}

/// Wrapper that formats a slice of `ConfigError`s as a numbered list.
pub struct ConfigErrors(pub Vec<ConfigError>);

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Configuration validation failed with {} error(s):",
            self.0.len()
        )?;
        for (i, e) in self.0.iter().enumerate() {
            writeln!(f, "\n  [{}] {}", i + 1, e)?;
        }
        Ok(())
    }
}

// ── Config structs ─────────────────────────────────────────────────────────────

/// Top-level configuration — maps 1-to-1 onto `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub paths:     PathsConfig,
    pub watch:     WatchConfig,
    pub worker:    WorkerConfig,
    pub processor: ProcessorConfig,
    pub ops:       OpsConfig,
}

/// Filesystem paths used by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathsConfig {
    /// Root of the Obsidian vault; every output must live inside this tree.
    pub vault_root: String,
    /// Subdirectory of `vault_root` where source files are dropped.
    pub sources_dir: String,
    /// SQLite database file path.
    pub db_path: String,
    /// Directory for rotating log files.
    pub log_dir: String,
}

/// File-watching parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchConfig {
    /// Lower-case, dot-free extensions to admit (e.g. `["pdf", "docx"]`).
    pub extensions: Vec<String>,
    /// Glob patterns that suppress an otherwise-admitted path.
    pub ignore_globs: Vec<String>,
    /// Milliseconds that size + mtime must remain stable before hashing.
    pub stability_ms: u64,
    /// Full-scan backstop interval in seconds.
    pub poll_interval_secs: u64,
    /// Read-chunk size for streaming SHA-256 hash (bytes).
    pub hash_chunk_bytes: usize,
}

/// Worker-pool parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// Maximum simultaneous processor subprocesses.
    pub concurrency: usize,
    /// Max processing attempts before a job is permanently failed.
    pub max_attempts: u32,
    /// Per-retry backoff delays (seconds).  Length must be ≥ `max_attempts − 1`.
    pub backoff_secs: Vec<u64>,
}

/// Subprocess processor parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorConfig {
    /// Path (absolute, relative, or bare name on `$PATH`) to the processor.
    pub command: String,
    /// Hard wall-clock timeout per invocation (seconds).
    pub timeout_secs: u64,
    /// Root directory under which per-job working directories are created.
    pub work_dir_root: String,
}

/// HTTP ops server and logging parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpsConfig {
    /// TCP bind address for the local HTTP API (loopback only by default).
    pub http_bind: String,
    /// Minimum log level: `trace` | `debug` | `info` | `warn` | `error`.
    pub log_level: String,
    /// Log format: `json` | `pretty`.
    pub log_format: String,
}

// ── Defaults ──────────────────────────────────────────────────────────────────

impl Default for Config {
    fn default() -> Self {
        Self {
            paths: PathsConfig {
                vault_root:  "~/Vault".into(),
                sources_dir: "~/Vault/Sources".into(),
                db_path:     "~/Library/Application Support/knowledge-builder/state.db".into(),
                log_dir:     "~/Library/Logs/knowledge-builder".into(),
            },
            watch: WatchConfig {
                extensions: vec![
                    "pdf".into(),
                    "docx".into(),
                    "xlsx".into(),
                    "ppt".into(),
                    "pptx".into(),
                    "jpg".into(),
                    "jpeg".into(),
                    "png".into(),
                ],
                ignore_globs: vec![
                    "**/.*".into(),
                    "**/~$*".into(),
                    "**/.obsidian/**".into(),
                    "**/*.icloud".into(),
                ],
                stability_ms:       2_000,
                poll_interval_secs: 300,
                hash_chunk_bytes:   1_048_576,
            },
            worker: WorkerConfig {
                concurrency:  2,
                max_attempts: 3,
                backoff_secs: vec![30, 300, 1_800],
            },
            processor: ProcessorConfig {
                command:       "processors/default/run.sh".into(),
                timeout_secs:  1_800,
                work_dir_root: "~/Library/Caches/knowledge-builder/jobs".into(),
            },
            ops: OpsConfig {
                http_bind:  "127.0.0.1:7878".into(),
                log_level:  "info".into(),
                log_format: "json".into(),
            },
        }
    }
}

// ── Tilde expansion ────────────────────────────────────────────────────────────

/// Expand a leading `~` to the user's home directory.
///
/// Handles `~` alone, `~/…`, and leaves everything else unchanged.
/// If `dirs::home_dir()` returns `None` the original string is returned as-is.
fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return dirs::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_owned());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_owned()
}

/// Apply `expand_tilde` to every path-valued field in the config.
fn expand_all_paths(mut cfg: Config) -> Config {
    cfg.paths.vault_root        = expand_tilde(&cfg.paths.vault_root);
    cfg.paths.sources_dir       = expand_tilde(&cfg.paths.sources_dir);
    cfg.paths.db_path           = expand_tilde(&cfg.paths.db_path);
    cfg.paths.log_dir           = expand_tilde(&cfg.paths.log_dir);
    cfg.processor.command       = expand_tilde(&cfg.processor.command);
    cfg.processor.work_dir_root = expand_tilde(&cfg.processor.work_dir_root);
    cfg
}

// ── Config implementation ─────────────────────────────────────────────────────

impl Config {
    // ── Loading ──────────────────────────────────────────────────────────────

    /// **Primary entry point.** Loads config from disk + environment, expands
    /// all path fields, and runs the 8-point startup validation.
    ///
    /// Returns `Err` if *any* validation check fails.  The error message lists
    /// all failures so the operator can fix them all in one edit.
    pub fn load() -> crate::Result<Self> {
        let mut cfg = Self::load_raw()?;
        cfg = expand_all_paths(cfg);
        cfg.validate().map_err(|errs| {
            anyhow::anyhow!("{}", ConfigErrors(errs))
        })?;
        Ok(cfg)
    }

    // ── Validation ───────────────────────────────────────────────────────────

    /// Run all 8 startup validation checks and collect *every* failure.
    ///
    /// The caller should format `Vec<ConfigError>` with `ConfigErrors` for a
    /// human-readable listing.
    pub fn validate(&self) -> Result<(), Vec<ConfigError>> {
        let mut errors: Vec<ConfigError> = Vec::new();

        // ── 1. vault_root: exists, is dir, readable + writable ──────────────
        let vault_canon = check_vault_root(&self.paths.vault_root, &mut errors);

        // ── 2. sources_dir: exists, is dir, readable ────────────────────────
        let sources_canon = check_sources_dir(&self.paths.sources_dir, &mut errors);

        // ── 3 & 4. Containment checks (require both canonicalized paths) ─────
        if let (Some(ref vc), Some(ref sc)) = (vault_canon, sources_canon) {
            // 3. sources_dir must be inside vault_root
            if !sc.starts_with(vc) {
                errors.push(ConfigError::SourcesNotInsideVault {
                    sources_dir: sc.display().to_string(),
                    vault_root:  vc.display().to_string(),
                });
            }
            // 4. sources_dir must be a *strict* subdirectory (not equal to vault_root)
            else if sc == vc {
                errors.push(ConfigError::SourcesSameAsVault {
                    sources_dir: sc.display().to_string(),
                    vault_root:  vc.display().to_string(),
                });
            }
        }

        // ── 5. processor.command: exists and is executable ───────────────────
        check_processor_command(&self.processor.command, &mut errors);

        // ── 6. db_path: parent creatable; SQLite opens ───────────────────────
        check_db_path(&self.paths.db_path, &mut errors);

        // ── 7. log_dir: creatable ────────────────────────────────────────────
        check_log_dir(&self.paths.log_dir, &mut errors);

        // ── 8. backoff_secs.len() >= max_attempts - 1 ────────────────────────
        check_backoff(&self.worker, &mut errors);

        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }
}

// ── Free-standing helpers (public for `kb config show` / tests) ───────────────

/// Load configuration from disk + environment **without** running validation.
///
/// Suitable for `kb config show`, `kb config path`, and any context where the
/// vault or processor may not yet exist.  All path fields are tilde-expanded.
pub fn load_raw() -> crate::Result<Config> {
    use figment::{
        providers::{Env, Format, Serialized, Toml},
        Figment,
    };

    let config_path = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("knowledge-builder")
        .join("config.toml");

    let cfg: Config = Figment::new()
        .merge(Serialized::defaults(Config::default()))
        .merge(Toml::file(config_path))          // missing file is silently skipped
        .merge(Env::prefixed("KB__").split("__"))
        .extract()?;

    Ok(expand_all_paths(cfg))
}

/// Backward-compatible alias: loads raw config (no validation).
///
/// Callers that want validation should use `Config::load()`.
pub fn load() -> crate::Result<Config> {
    load_raw()
}

// ── Per-check helper functions ────────────────────────────────────────────────

/// Check 1: vault_root exists, is a directory, is readable + writable.
/// Returns `Some(canonical_path)` on success, `None` on any failure.
fn check_vault_root(path: &str, errors: &mut Vec<ConfigError>) -> Option<PathBuf> {
    let p = Path::new(path);

    // Existence + is-directory
    match fs::metadata(p) {
        Err(e) => {
            errors.push(ConfigError::VaultRoot {
                path:   path.to_owned(),
                detail: format!("does not exist or cannot be accessed: {e}"),
            });
            return None;
        }
        Ok(m) if !m.is_dir() => {
            errors.push(ConfigError::VaultRoot {
                path:   path.to_owned(),
                detail: "path exists but is not a directory".to_owned(),
            });
            return None;
        }
        Ok(_) => {}
    }

    // Readability: list entries
    if let Err(e) = fs::read_dir(p) {
        errors.push(ConfigError::VaultRoot {
            path:   path.to_owned(),
            detail: format!("directory is not readable: {e}"),
        });
        return None;
    }

    // Writability: probe-file create + remove
    let probe = p.join(".kb-write-probe");
    match fs::write(&probe, b"") {
        Err(e) => {
            errors.push(ConfigError::VaultRoot {
                path:   path.to_owned(),
                detail: format!("directory is not writable: {e}"),
            });
            return None;
        }
        Ok(_) => {
            let _ = fs::remove_file(&probe); // best-effort cleanup; ignore error
        }
    }

    // Canonicalize
    match fs::canonicalize(p) {
        Ok(c)  => Some(c),
        Err(e) => {
            errors.push(ConfigError::VaultRoot {
                path:   path.to_owned(),
                detail: format!("cannot canonicalize path: {e}"),
            });
            None
        }
    }
}

/// Check 2: sources_dir exists, is a directory, is readable.
/// Returns `Some(canonical_path)` on success, `None` on any failure.
fn check_sources_dir(path: &str, errors: &mut Vec<ConfigError>) -> Option<PathBuf> {
    let p = Path::new(path);

    // Existence + is-directory
    match fs::metadata(p) {
        Err(e) => {
            errors.push(ConfigError::SourcesDir {
                path:   path.to_owned(),
                detail: format!("does not exist or cannot be accessed: {e}"),
            });
            return None;
        }
        Ok(m) if !m.is_dir() => {
            errors.push(ConfigError::SourcesDir {
                path:   path.to_owned(),
                detail: "path exists but is not a directory".to_owned(),
            });
            return None;
        }
        Ok(_) => {}
    }

    // Readability: list entries
    if let Err(e) = fs::read_dir(p) {
        errors.push(ConfigError::SourcesDir {
            path:   path.to_owned(),
            detail: format!("directory is not readable: {e}"),
        });
        return None;
    }

    // Canonicalize
    match fs::canonicalize(p) {
        Ok(c)  => Some(c),
        Err(e) => {
            errors.push(ConfigError::SourcesDir {
                path:   path.to_owned(),
                detail: format!("cannot canonicalize path: {e}"),
            });
            None
        }
    }
}

/// Check 5: processor.command exists and is executable.
///
/// Handles three command styles:
/// - Absolute path (`/usr/local/bin/python3`)
/// - Relative path with a `/` component (`processors/default/run.sh`)
/// - Bare name looked up on `$PATH` (`python3`)
fn check_processor_command(command: &str, errors: &mut Vec<ConfigError>) {
    let resolved = if command.contains('/') {
        // Explicit path
        let p = PathBuf::from(command);
        if p.exists() { Some(p) } else { None }
    } else {
        // Bare name — search $PATH
        find_in_path(command)
    };

    let resolved = match resolved {
        Some(p) => p,
        None => {
            errors.push(ConfigError::ProcessorCommand {
                command: command.to_owned(),
                detail:  if command.contains('/') {
                    format!("file not found at '{command}'")
                } else {
                    format!("'{command}' not found in $PATH or filesystem")
                },
            });
            return;
        }
    };

    // Must be a regular file (not a directory) and have at least one execute bit set
    match fs::metadata(&resolved) {
        Err(e) => {
            errors.push(ConfigError::ProcessorCommand {
                command: command.to_owned(),
                detail:  format!("cannot stat '{}': {e}", resolved.display()),
            });
        }
        Ok(m) if m.is_dir() => {
            errors.push(ConfigError::ProcessorCommand {
                command: command.to_owned(),
                detail:  format!("'{}' is a directory, not an executable file", resolved.display()),
            });
        }
        Ok(m) => {
            let mode = m.permissions().mode();
            if mode & 0o111 == 0 {
                errors.push(ConfigError::ProcessorCommand {
                    command: command.to_owned(),
                    detail: format!(
                        "'{}' exists but has no execute permission (mode {mode:o}); \
                         run: chmod +x '{}'",
                        resolved.display(),
                        resolved.display(),
                    ),
                });
            }
        }
    }
}

/// Search each directory in `$PATH` for a file named `name`.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Check 6: db_path parent directory is creatable; SQLite opens successfully.
fn check_db_path(db_path: &str, errors: &mut Vec<ConfigError>) {
    let p = PathBuf::from(db_path);

    // Parent must exist or be creatable
    let parent = match p.parent() {
        Some(par) if !par.as_os_str().is_empty() => par.to_path_buf(),
        _ => {
            errors.push(ConfigError::DbPath {
                db_path: db_path.to_owned(),
                detail:  "path has no parent directory component".to_owned(),
            });
            return;
        }
    };

    if let Err(e) = fs::create_dir_all(&parent) {
        errors.push(ConfigError::DbPath {
            db_path: db_path.to_owned(),
            detail:  format!("cannot create parent directory '{}': {e}", parent.display()),
        });
        return;
    }

    // Try opening the SQLite database (creates the file if it does not exist)
    match rusqlite::Connection::open(&p) {
        Err(e) => {
            errors.push(ConfigError::DbPath {
                db_path: db_path.to_owned(),
                detail:  format!("SQLite cannot open database: {e}"),
            });
        }
        Ok(conn) => {
            // Quick smoke-test: a trivial pragma to confirm the connection is live
            if let Err(e) = conn.execute_batch("PRAGMA journal_mode;") {
                errors.push(ConfigError::DbPath {
                    db_path: db_path.to_owned(),
                    detail:  format!("SQLite connection smoke-test failed: {e}"),
                });
            }
            // `conn` drops here, closing the file handle cleanly
        }
    }
}

/// Check 7: log_dir can be created (or already exists).
fn check_log_dir(log_dir: &str, errors: &mut Vec<ConfigError>) {
    if let Err(e) = fs::create_dir_all(log_dir) {
        errors.push(ConfigError::LogDir {
            log_dir: log_dir.to_owned(),
            detail:  format!("cannot create directory: {e}"),
        });
    }
}

/// Check 8: `backoff_secs.len() >= max_attempts - 1`.
fn check_backoff(worker: &WorkerConfig, errors: &mut Vec<ConfigError>) {
    // max_attempts == 0 means the job never runs; no retries, no backoff required.
    if worker.max_attempts == 0 {
        return;
    }
    let needed = (worker.max_attempts as usize).saturating_sub(1);
    let len    = worker.backoff_secs.len();
    if len < needed {
        errors.push(ConfigError::BackoffTooShort {
            len,
            len_plural: if len == 1 { "y" } else { "ies" },
            needed,
            max_attempts: worker.max_attempts,
        });
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::OpenOptionsExt;
    use tempfile::TempDir;

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Build a minimal valid Config pointing entirely inside `tmp`.
    fn valid_config(tmp: &TempDir) -> Config {
        let vault   = tmp.path().join("vault");
        let sources = tmp.path().join("vault").join("Sources");
        let db      = tmp.path().join("db").join("state.db");
        let logs    = tmp.path().join("logs");
        let cmd     = tmp.path().join("proc.sh");

        fs::create_dir_all(&sources).unwrap();
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        fs::create_dir_all(&logs).unwrap();

        // Write a minimal executable shell script
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .mode(0o755)
            .open(&cmd)
            .unwrap();

        Config {
            paths: PathsConfig {
                vault_root:  vault.to_str().unwrap().to_owned(),
                sources_dir: sources.to_str().unwrap().to_owned(),
                db_path:     db.to_str().unwrap().to_owned(),
                log_dir:     logs.to_str().unwrap().to_owned(),
            },
            watch: WatchConfig {
                extensions:         vec!["pdf".into()],
                ignore_globs:       vec![],
                stability_ms:       2_000,
                poll_interval_secs: 300,
                hash_chunk_bytes:   1_048_576,
            },
            worker: WorkerConfig {
                concurrency:  2,
                max_attempts: 3,
                backoff_secs: vec![30, 300],     // len == max_attempts - 1 == 2 ✓
            },
            processor: ProcessorConfig {
                command:       cmd.to_str().unwrap().to_owned(),
                timeout_secs:  1_800,
                work_dir_root: tmp.path().join("jobs").to_str().unwrap().to_owned(),
            },
            ops: OpsConfig {
                http_bind:  "127.0.0.1:7878".into(),
                log_level:  "info".into(),
                log_format: "json".into(),
            },
        }
    }

    // ── expand_tilde ─────────────────────────────────────────────────────────

    #[test]
    fn tilde_alone_expands() {
        let result = expand_tilde("~");
        // On any CI/dev machine dirs::home_dir() should succeed
        if dirs::home_dir().is_some() {
            assert!(!result.starts_with('~'), "tilde was not expanded: {result}");
        }
    }

    #[test]
    fn tilde_slash_path_expands() {
        let result = expand_tilde("~/Vault");
        if dirs::home_dir().is_some() {
            assert!(result.ends_with("/Vault"), "unexpected result: {result}");
            assert!(!result.starts_with('~'), "tilde was not expanded: {result}");
        }
    }

    #[test]
    fn non_tilde_path_is_unchanged() {
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
        assert_eq!(expand_tilde("relative/path"),  "relative/path");
    }

    // ── Defaults ─────────────────────────────────────────────────────────────

    #[test]
    fn default_config_deserializes() {
        let cfg = Config::default();
        assert_eq!(cfg.watch.stability_ms, 2_000);
        assert_eq!(cfg.worker.max_attempts, 3);
        assert_eq!(cfg.worker.backoff_secs.len(), 3);
        assert_eq!(cfg.ops.http_bind, "127.0.0.1:7878");
    }

    // ── Validation checks ────────────────────────────────────────────────────

    #[test]
    fn valid_config_passes_all_checks() {
        let tmp = TempDir::new().unwrap();
        let cfg = valid_config(&tmp);
        assert!(
            cfg.validate().is_ok(),
            "valid config failed: {:?}",
            cfg.validate().unwrap_err()
        );
    }

    #[test]
    fn check1_vault_root_missing() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = valid_config(&tmp);
        cfg.paths.vault_root = "/nonexistent/vault/path".into();
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| matches!(e, ConfigError::VaultRoot { .. })),
            "expected VaultRoot error, got: {errs:?}"
        );
    }

    #[test]
    fn check2_sources_dir_missing() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = valid_config(&tmp);
        cfg.paths.sources_dir = "/nonexistent/sources".into();
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| matches!(e, ConfigError::SourcesDir { .. })),
            "expected SourcesDir error, got: {errs:?}"
        );
    }

    #[test]
    fn check3_sources_not_inside_vault() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = valid_config(&tmp);
        // Create a sibling directory outside the vault
        let sibling = tmp.path().join("elsewhere");
        fs::create_dir_all(&sibling).unwrap();
        cfg.paths.sources_dir = sibling.to_str().unwrap().to_owned();
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| matches!(e, ConfigError::SourcesNotInsideVault { .. })),
            "expected SourcesNotInsideVault error, got: {errs:?}"
        );
    }

    #[test]
    fn check4_sources_same_as_vault() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = valid_config(&tmp);
        // Point sources_dir at the vault root itself
        cfg.paths.sources_dir = cfg.paths.vault_root.clone();
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| matches!(e, ConfigError::SourcesSameAsVault { .. })),
            "expected SourcesSameAsVault error, got: {errs:?}"
        );
    }

    #[test]
    fn check5_processor_command_missing() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = valid_config(&tmp);
        cfg.processor.command = "/nonexistent/run.sh".into();
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| matches!(e, ConfigError::ProcessorCommand { .. })),
            "expected ProcessorCommand error, got: {errs:?}"
        );
    }

    #[test]
    fn check5_processor_command_not_executable() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = valid_config(&tmp);
        // Create a non-executable file
        let non_exec = tmp.path().join("notexec.sh");
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .mode(0o644)          // no execute bit
            .open(&non_exec)
            .unwrap();
        cfg.processor.command = non_exec.to_str().unwrap().to_owned();
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| matches!(e, ConfigError::ProcessorCommand { .. })),
            "expected ProcessorCommand error, got: {errs:?}"
        );
    }

    #[test]
    fn check8_backoff_too_short() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = valid_config(&tmp);
        cfg.worker.max_attempts = 5;
        cfg.worker.backoff_secs = vec![30]; // need 4 entries, only have 1
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| matches!(e, ConfigError::BackoffTooShort { .. })),
            "expected BackoffTooShort error, got: {errs:?}"
        );
    }

    #[test]
    fn check8_backoff_zero_max_attempts() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = valid_config(&tmp);
        cfg.worker.max_attempts = 0;
        cfg.worker.backoff_secs = vec![]; // no retries needed for 0 max_attempts
        // Checks 1-7 still pass; check 8 must not fire
        let errs_opt = cfg.validate();
        // may have other errors (unchanged from valid_config), but NOT BackoffTooShort
        if let Err(errs) = errs_opt {
            assert!(
                !errs.iter().any(|e| matches!(e, ConfigError::BackoffTooShort { .. })),
                "unexpected BackoffTooShort with max_attempts=0: {errs:?}"
            );
        }
    }

    #[test]
    fn multiple_errors_collected() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = valid_config(&tmp);
        // Break two checks simultaneously
        cfg.paths.vault_root      = "/no/vault".into();
        cfg.paths.sources_dir     = "/no/sources".into();
        cfg.processor.command     = "/no/cmd".into();
        cfg.worker.max_attempts   = 4;
        cfg.worker.backoff_secs   = vec![];
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.len() >= 2,
            "expected ≥2 errors when multiple checks fail, got {}: {errs:?}",
            errs.len()
        );
    }

    // ── load_raw ─────────────────────────────────────────────────────────────

    #[test]
    fn load_raw_succeeds_with_defaults_only() {
        // Should succeed even with no config file and no env vars
        let cfg = load_raw();
        assert!(cfg.is_ok(), "load_raw failed: {:?}", cfg.err());
    }

    #[test]
    fn load_raw_expands_tilde_in_vault_root() {
        let cfg = load_raw().unwrap();
        if dirs::home_dir().is_some() {
            assert!(
                !cfg.paths.vault_root.starts_with('~'),
                "vault_root still contains tilde: {}",
                cfg.paths.vault_root
            );
        }
    }

    // ── ConfigErrors display ──────────────────────────────────────────────────

    #[test]
    fn config_errors_display_has_all_items() {
        let errs = vec![
            ConfigError::VaultRoot {
                path:   "/bad/path".into(),
                detail: "not found".into(),
            },
            ConfigError::BackoffTooShort {
                len:         1,
                len_plural:  "y",
                needed:      3,
                max_attempts: 4,
            },
        ];
        let display = ConfigErrors(errs).to_string();
        assert!(display.contains("[1]"), "missing item 1: {display}");
        assert!(display.contains("[2]"), "missing item 2: {display}");
    }
}
