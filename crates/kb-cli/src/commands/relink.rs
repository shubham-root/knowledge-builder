//! `kb relink` — recover wikilinks the legacy link-sweeper turned into
//! plain-text placeholders.
//!
//! Until 2026-05-26 the daemon's post-run sweeper rewrote every
//! unresolved `[[Target]]` to `Target [possible linkout - elaboration
//! needed]`.  That behaviour was hostile to Obsidian's graph view (an
//! unresolved wikilink is a click-to-create stub, not a bug) and has
//! been removed.  This command undoes the damage on existing notes
//! by walking `agent_root` for the placeholder string and rewriting
//! each match back to wikilink form.
//!
//! The recovery is **idempotent** — running it twice does nothing the
//! second time, since the placeholder pattern no longer matches.
//! Code-fenced and inline-code spans are preserved verbatim.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use kb_agent::link_sweeper::relink_files;
use walkdir::WalkDir;

#[derive(Args, Debug)]
pub struct RelinkArgs {
    /// Don't write anything; just report what would change.
    #[arg(long)]
    pub dry_run: bool,

    /// Override the vault's `agent_root`.  Defaults to the daemon's
    /// configured `paths.agent_root`.
    #[arg(long)]
    pub agent_root: Option<PathBuf>,

    /// Restrict the recovery to a specific subtree (must be inside
    /// `agent_root`).  Default: walk all of `agent_root`.
    #[arg(long)]
    pub root: Option<PathBuf>,
}

pub async fn run(args: RelinkArgs) -> Result<()> {
    let config = kb_core::config::load_raw()
        .context("Cannot load configuration (run `kb config show` to debug)")?;

    let agent_root: PathBuf = match args.agent_root {
        Some(p) => p,
        None    => PathBuf::from(&config.paths.agent_root),
    };
    if !agent_root.exists() {
        anyhow::bail!(
            "agent_root {} does not exist on disk; \
             pass --agent-root to override or fix paths.agent_root in config.toml.",
            agent_root.display(),
        );
    }

    let walk_root = match args.root {
        Some(p) if p.exists() => p,
        Some(p) => anyhow::bail!("--root {} does not exist", p.display()),
        None => agent_root.clone(),
    };

    println!(
        "kb relink: scanning {} (agent_root: {}){}",
        walk_root.display(),
        agent_root.display(),
        if args.dry_run { "  [dry-run]" } else { "" },
    );

    let mut candidates: Vec<PathBuf> = Vec::new();
    for e in WalkDir::new(&walk_root)
        .follow_links(false)
        .into_iter()
        .filter_map(|r| r.ok())
    {
        if !e.file_type().is_file() {
            continue;
        }
        let p = e.path();
        let is_md = p.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("md"))
            .unwrap_or(false);
        if !is_md {
            continue;
        }
        // Cheap pre-filter: only forward files that contain the
        // placeholder string anywhere in their bytes.  Avoids
        // re-reading the entire vault from inside the relinker.
        if let Ok(bytes) = std::fs::read(p) {
            if memmem(&bytes, b"[possible linkout - elaboration needed]") {
                candidates.push(p.to_path_buf());
            }
        }
    }

    if candidates.is_empty() {
        println!("  no files contain the legacy placeholder; nothing to do.");
        return Ok(());
    }
    println!("  {} file(s) carry the legacy placeholder", candidates.len());

    let stats = relink_files(candidates, &agent_root, args.dry_run);

    println!();
    println!("  files examined : {}", stats.files_examined);
    println!("  files modified : {}", stats.files_modified);
    println!("  links restored : {}", stats.links_restored);
    if !stats.examples.is_empty() {
        println!("  examples:");
        for ex in stats.examples.iter().take(20) {
            println!("    • {ex}");
        }
        if stats.examples.len() > 20 {
            println!("    … {} more", stats.examples.len() - 20);
        }
    }
    if args.dry_run {
        println!();
        println!("  Re-run without --dry-run to apply.");
    }
    Ok(())
}

/// Tiny `memmem` so we don't pull a regex compile per-file when the
/// vast majority of files contain no placeholder.
fn memmem(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}
