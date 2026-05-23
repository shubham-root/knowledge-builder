//! Configuration types and loader for Knowledge Builder.
//!
//! Loaded from `~/.config/knowledge-builder/config.toml` with optional
//! environment-variable overrides using `figment` (prefix `KB__`).
//!
//! Example env override: `KB__PATHS__VAULT_ROOT=/Users/me/Vault`

use serde::{Deserialize, Serialize};

/// Top-level configuration, maps directly onto `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub paths:     PathsConfig,
    pub watch:     WatchConfig,
    pub worker:    WorkerConfig,
    pub processor: ProcessorConfig,
    pub ops:       OpsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathsConfig {
    /// Root of the Obsidian vault (must contain `sources_dir`).
    pub vault_root:  String,
    /// Subdirectory where source files are dropped.
    pub sources_dir: String,
    /// Path to the SQLite database file.
    pub db_path:     String,
    /// Directory for rotating log files.
    pub log_dir:     String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchConfig {
    /// File extensions to watch (lowercase, no leading dot).
    pub extensions:          Vec<String>,
    /// Glob patterns to ignore inside `sources_dir`.
    pub ignore_globs:        Vec<String>,
    /// How long (ms) size+mtime must be stable before hashing.
    pub stability_ms:        u64,
    /// Periodic full-scan interval in seconds.
    pub poll_interval_secs:  u64,
    /// Read chunk size for streaming SHA-256 hash.
    pub hash_chunk_bytes:    usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// Maximum simultaneous processor subprocesses.
    pub concurrency:  usize,
    /// Maximum attempts before a job is permanently failed.
    pub max_attempts: u32,
    /// Per-attempt backoff delays in seconds (length ≥ max_attempts - 1).
    pub backoff_secs: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorConfig {
    /// Path (or shell command) to invoke the processor script.
    pub command:      String,
    /// Hard timeout per processor invocation in seconds.
    pub timeout_secs: u64,
    /// Root directory for per-job working directories.
    pub work_dir_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpsConfig {
    /// Bind address for the local HTTP API.
    pub http_bind:  String,
    /// Minimum log level: trace | debug | info | warn | error.
    pub log_level:  String,
    /// Log format: json | pretty.
    pub log_format: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            paths: PathsConfig {
                vault_root:  "~/Vault".into(),
                sources_dir: "~/Vault/Sources".into(),
                db_path: "~/Library/Application Support/knowledge-builder/state.db".into(),
                log_dir: "~/Library/Logs/knowledge-builder".into(),
            },
            watch: WatchConfig {
                extensions:         vec![
                    "pdf".into(), "docx".into(), "xlsx".into(),
                    "ppt".into(), "pptx".into(), "jpg".into(),
                    "jpeg".into(), "png".into(),
                ],
                ignore_globs:       vec![
                    "**/.*".into(),
                    "**/~$*".into(),
                    "**/.obsidian/**".into(),
                    "**/*.icloud".into(),
                ],
                stability_ms:       2000,
                poll_interval_secs: 300,
                hash_chunk_bytes:   1_048_576,
            },
            worker: WorkerConfig {
                concurrency:  2,
                max_attempts: 3,
                backoff_secs: vec![30, 300, 1800],
            },
            processor: ProcessorConfig {
                command:      "processors/default/run.sh".into(),
                timeout_secs: 1800,
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

/// Load configuration from disk + environment variables.
///
/// Search order (highest priority first):
/// 1. `KB__*` environment variables
/// 2. `~/.config/knowledge-builder/config.toml`
/// 3. Built-in defaults
pub fn load() -> crate::Result<Config> {
    use figment::{
        providers::{Env, Format, Serialized, Toml},
        Figment,
    };

    let config_path = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.config"))
        .join("knowledge-builder")
        .join("config.toml");

    let config: Config = Figment::new()
        .merge(Serialized::defaults(Config::default()))
        .merge(Toml::file(config_path))
        .merge(Env::prefixed("KB__").split("__"))
        .extract()?;

    Ok(config)
}
