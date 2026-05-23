//! `kb` — Knowledge Builder CLI binary.
//!
//! Single entrypoint for both the background daemon and all operational
//! subcommands.  Delegates to per-subcommand modules in `commands/`.
//!
//! # Subcommands
//! ```text
//! kb daemon   [--foreground]          Start the background daemon
//! kb status                           Aggregate queue summary
//! kb list     [--status …] [--limit N]
//! kb show     <path|id>
//! kb requeue  <path|id>
//! kb reset    <path|id>
//! kb scan                             Force immediate full scan
//! kb tail     [--level …] [--kind …]  Stream audit events (SSE)
//! kb prune    [--before <date>] [--status done] [--dry-run]
//! kb storage                          Byte-usage report
//! kb install                          Register with launchd
//! kb uninstall
//! kb config   [show|path|validate]
//! kb doctor                           Validate configuration
//! kb backup   [--output <path>]       Create a database backup
//! kb restore  <backup_path>           Restore from a backup
//! ```

use anyhow::Result;
use clap::{Parser, Subcommand};

mod client;
mod commands;

#[derive(Parser)]
#[command(
    name    = "kb",
    version = env!("CARGO_PKG_VERSION"),
    about   = "Knowledge Builder — Obsidian vault file processor daemon & CLI",
    propagate_version = true,
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the Knowledge Builder daemon.
    Daemon(commands::daemon::DaemonArgs),

    /// Show aggregate queue status.
    Status,

    /// List tracked source files.
    List(commands::list::ListArgs),

    /// Show details for a single source file.
    Show(commands::show::ShowArgs),

    /// Re-queue a file for processing (resets attempt counter).
    Requeue(commands::requeue::RequeueArgs),

    /// Delete a file record so the next discovery treats it as new.
    Reset(commands::reset::ResetArgs),

    /// Trigger an immediate full-vault scan.
    Scan,

    /// Stream audit events in real time.
    Tail(commands::tail::TailArgs),

    /// Prune completed or old records from the database.
    Prune(commands::prune::PruneArgs),

    /// Show storage usage grouped by output kind.
    Storage,

    /// Register the daemon as a launchd LaunchAgent.
    Install(commands::install::InstallArgs),

    /// Remove the launchd LaunchAgent registration.
    Uninstall,

    /// Configuration introspection.
    Config(commands::config::ConfigArgs),

    /// Validate configuration and environment prerequisites.
    Doctor,

    /// Create a compact, consistent backup of the state database (VACUUM INTO).
    Backup(commands::backup::BackupArgs),

    /// Restore the state database from a backup file.
    Restore(commands::backup::RestoreArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon(args)   => commands::daemon::run(args).await,
        Commands::Status         => commands::status::run().await,
        Commands::List(args)     => commands::list::run(args).await,
        Commands::Show(args)     => commands::show::run(args).await,
        Commands::Requeue(args)  => commands::requeue::run(args).await,
        Commands::Reset(args)    => commands::reset::run(args).await,
        Commands::Scan           => commands::scan::run().await,
        Commands::Tail(args)     => commands::tail::run(args).await,
        Commands::Prune(args)    => commands::prune::run(args).await,
        Commands::Storage        => commands::storage::run().await,
        Commands::Install(args)  => commands::install::run(args).await,
        Commands::Uninstall      => commands::uninstall::run().await,
        Commands::Config(args)   => commands::config::run(args).await,
        Commands::Doctor         => commands::doctor::run().await,
        Commands::Backup(args)   => commands::backup::run_backup(args).await,
        Commands::Restore(args)  => commands::backup::run_restore(args).await,
    }
}
