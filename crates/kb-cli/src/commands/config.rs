//! `kb config [show|path|validate]` — configuration introspection.
//!
//! Full implementation: T8.

use clap::{Args, Subcommand};
use anyhow::Result;

#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: Option<ConfigAction>,
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Print the resolved configuration.
    Show,
    /// Print the path to the config file.
    Path,
    /// Validate the configuration and exit.
    Validate,
}

pub async fn run(args: ConfigArgs) -> Result<()> {
    let config = kb_core::config::load()?;

    match args.action.unwrap_or(ConfigAction::Show) {
        ConfigAction::Show => {
            let json = serde_json::to_string_pretty(&config)?;
            println!("{json}");
        }
        ConfigAction::Path => {
            let path = dirs::config_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("~/.config"))
                .join("knowledge-builder")
                .join("config.toml");
            println!("{}", path.display());
        }
        ConfigAction::Validate => {
            // TODO (T8): full 8-point startup validation.
            println!("config loaded successfully (full validation: T8)");
            let _ = config;
        }
    }
    Ok(())
}
