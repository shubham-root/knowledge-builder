//! `kb config [show|path|validate]` — configuration introspection.
//!
//! | Sub-action | Behaviour |
//! |---|---|
//! | `show` (default) | Load config without validation and pretty-print as JSON. |
//! | `path`           | Print the resolved path to the TOML config file. |
//! | `validate`       | Load raw config and run all 8 validation checks; exit 1 on failure. |

use anyhow::Result;
use clap::{Args, Subcommand};

// ── Argument types ─────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: Option<ConfigAction>,
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Print the resolved configuration as pretty-printed JSON.
    Show,
    /// Print the path to the config file (may not exist yet).
    Path,
    /// Run configuration validation and report all issues; exit 1 on failure.
    Validate,
}

// ── Entry point ────────────────────────────────────────────────────────────────

pub async fn run(args: ConfigArgs) -> Result<()> {
    match args.action.unwrap_or(ConfigAction::Show) {
        ConfigAction::Show     => show().await,
        ConfigAction::Path     => path().await,
        ConfigAction::Validate => validate().await,
    }
}

// ── Sub-action implementations ────────────────────────────────────────────────

/// `kb config show` — load the raw (unexpanded) config and pretty-print it.
///
/// Uses [`kb_core::config::load_raw`] so that the command succeeds even when
/// the vault, sources directory, or processor script do not yet exist.
async fn show() -> Result<()> {
    let config = kb_core::config::load_raw()?;
    let json = serde_json::to_string_pretty(&config)?;
    println!("{json}");
    Ok(())
}

/// `kb config path` — print the filesystem path of the TOML config file.
///
/// The file need not exist; this command only reports where Knowledge Builder
/// *expects* to find it.
async fn path() -> Result<()> {
    let config_path = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.config"))
        .join("knowledge-builder")
        .join("config.toml");

    println!("{}", config_path.display());
    Ok(())
}

/// `kb config validate` — run all 8 startup validation checks and report results.
///
/// Exits with status 0 if every check passes; status 1 otherwise.
/// All failures are printed before exiting so the user can fix them in one go.
async fn validate() -> Result<()> {
    let config = match kb_core::config::load_raw() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to load configuration: {e}");
            std::process::exit(1);
        }
    };

    match config.validate() {
        Ok(()) => {
            println!("✓ Configuration is valid.");
        }
        Err(errs) => {
            let count = errs.len();
            let display = kb_core::ConfigErrors(errs).to_string();
            eprintln!("{display}");
            eprintln!("Fix the {count} error(s) above, then re-run `kb config validate`.");
            std::process::exit(1);
        }
    }

    Ok(())
}
