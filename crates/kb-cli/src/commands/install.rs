//! `kb install` — render the launchd plist template and register the daemon as
//! a LaunchAgent.
//!
//! ## What this command does
//!
//! 1. Runs `kb doctor` — aborts on any failure.
//! 2. Determines the absolute path of the `kb` binary (`current_exe`).
//! 3. Loads the config to obtain `LOG_DIR`.
//! 4. Reads the plist template (embedded at compile time via `include_str!`).
//! 5. Substitutes `{{KB_BIN}}`, `{{HOME}}`, `{{LOG_DIR}}` in the template.
//! 6. Writes the rendered plist to `~/Library/LaunchAgents/com.user.knowledge-builder.plist`.
//! 7. Runs `launchctl bootstrap gui/<UID> <plist>`.
//! 8. Runs `launchctl enable gui/<UID>/com.user.knowledge-builder`.
//! 9. Runs `launchctl kickstart gui/<UID>/com.user.knowledge-builder`.
//! 10. Prints a success message with helpful follow-up commands.
//!
//! Pass `--force` to overwrite an existing plist and re-register the service.

use anyhow::{bail, Context, Result};

// ── Compile-time embedded plist template ──────────────────────────────────────

/// The plist template, embedded at compile time from the repo's
/// `installer/com.user.knowledge-builder.plist`.
const PLIST_TEMPLATE: &str =
    include_str!("../../../../installer/com.user.knowledge-builder.plist");

// ── Shared constants (also used by uninstall.rs) ──────────────────────────────

/// launchd service label — must match the `<key>Label</key>` in the plist.
pub(crate) const LABEL: &str = "com.user.knowledge-builder";
/// Filename written to `~/Library/LaunchAgents/`.
pub(crate) const PLIST_FILENAME: &str = "com.user.knowledge-builder.plist";

// ── CLI args ──────────────────────────────────────────────────────────────────

/// Arguments for `kb install`.
#[derive(clap::Args, Debug)]
pub struct InstallArgs {
    /// Overwrite an existing plist and re-register the launchd service.
    ///
    /// Without this flag, `kb install` aborts if the plist already exists at
    /// `~/Library/LaunchAgents/com.user.knowledge-builder.plist`.
    #[arg(long)]
    pub force: bool,
}

// ── Entry point ────────────────────────────────────────────────────────────────

pub async fn run(args: InstallArgs) -> Result<()> {
    // ── Step 1: doctor pre-flight ─────────────────────────────────────────────
    // `doctor::run()` calls `std::process::exit(1)` on any failure, so if
    // execution reaches the next line, all checks passed.
    println!("Running pre-flight checks (kb doctor)…\n");
    super::doctor::run().await?;
    println!();

    // ── Step 2: KB_BIN — canonicalized path of this binary ───────────────────
    let kb_bin = std::env::current_exe()
        .context("Cannot determine path to the kb binary")?;
    // Resolve symlinks (e.g. when invoked from a Cargo `target/debug/` symlink).
    let kb_bin = std::fs::canonicalize(&kb_bin).unwrap_or(kb_bin);
    let kb_bin_str = kb_bin.to_string_lossy().into_owned();

    // ── Step 3: HOME and LOG_DIR ──────────────────────────────────────────────
    let home = dirs::home_dir()
        .context("Cannot determine home directory ($HOME is unset)")?;
    let home_str = home.to_string_lossy().into_owned();

    let config = kb_core::config::load_raw()
        .context("Cannot load configuration (run `kb config show` to debug)")?;
    let log_dir = config.paths.log_dir.clone();

    // ── Step 4: current user ID ───────────────────────────────────────────────
    let uid = current_uid();

    // ── Step 5: plist destination ─────────────────────────────────────────────
    let la_dir = home.join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&la_dir)
        .context("Cannot create ~/Library/LaunchAgents/ — check permissions")?;
    let plist_path = la_dir.join(PLIST_FILENAME);

    // ── Step 6: handle existing plist ────────────────────────────────────────
    if plist_path.exists() {
        if !args.force {
            bail!(
                "Plist already exists at {path}.\n\
                 \n\
                 Options:\n\
                 • Run `kb install --force` to overwrite and re-register.\n\
                 • Run `kb uninstall` first, then `kb install`.",
                path = plist_path.display()
            );
        }
        // --force: try to bootout the existing registration before overwriting.
        // We intentionally ignore failure — the service may have been manually
        // removed from launchd's database while the plist remained on disk.
        println!("  (--force: removing existing service registration…)");
        let domain_target = format!("gui/{uid}/{LABEL}");
        if let Err(e) = run_launchctl(&["bootout", &domain_target]) {
            println!("  (Note: bootout returned: {e} — continuing anyway)");
        }
    }

    // ── Step 7: render the plist template ────────────────────────────────────
    let rendered = PLIST_TEMPLATE
        .replace("{{KB_BIN}}", &kb_bin_str)
        .replace("{{HOME}}", &home_str)
        .replace("{{LOG_DIR}}", &log_dir);

    // ── Step 8: ensure LOG_DIR exists before launchd tries to open log files ──
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("Cannot create log directory: {log_dir}"))?;

    // ── Step 9: write the rendered plist ─────────────────────────────────────
    std::fs::write(&plist_path, rendered.as_bytes())
        .with_context(|| format!("Cannot write plist to {}", plist_path.display()))?;
    println!("  ✓  Plist written      → {}", plist_path.display());

    // ── Step 10: launchctl bootstrap ─────────────────────────────────────────
    let domain = format!("gui/{uid}");
    let plist_str = plist_path.to_string_lossy().into_owned();
    run_launchctl(&["bootstrap", &domain, &plist_str]).with_context(|| {
        format!(
            "launchctl bootstrap failed.\n\
             Hint: if the service is already registered, run `kb install --force`."
        )
    })?;
    println!("  ✓  Bootstrapped       : launchctl bootstrap {domain}");

    // ── Step 11: launchctl enable ─────────────────────────────────────────────
    let service = format!("{domain}/{LABEL}");
    run_launchctl(&["enable", &service])
        .with_context(|| format!("launchctl enable failed for {service}"))?;
    println!("  ✓  Enabled            : launchctl enable {service}");

    // ── Step 12: launchctl kickstart ──────────────────────────────────────────
    run_launchctl(&["kickstart", &service])
        .with_context(|| format!("launchctl kickstart failed for {service}"))?;
    println!("  ✓  Kickstarted        : launchctl kickstart {service}");

    // ── Step 13: success message ──────────────────────────────────────────────
    println!();
    println!(
        "✓  Knowledge Builder is installed and running as a launchd LaunchAgent."
    );
    println!();
    println!("   Check service   :  launchctl list {LABEL}");
    println!("   View stderr log :  tail -f {log_dir}/stderr.log");
    println!("   Queue status    :  kb status");
    println!("   Stop service    :  kb uninstall");

    Ok(())
}

// ── Shared helpers (pub(crate) so uninstall.rs can import them) ───────────────

/// Return the real UID of the calling process.
///
/// On Unix this calls `libc::getuid()`.  On non-Unix platforms (not targeted
/// by this project) it falls back to parsing `id -u` output.
pub(crate) fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: getuid() has no preconditions and never fails.
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        // Fallback for non-Unix platforms — not a production target.
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(501)
    }
}

/// Invoke `launchctl <args…>` and return `Ok(())` on exit code 0.
///
/// On non-zero exit, the combined stdout + stderr and exit code are included in
/// the returned `Err`.
pub(crate) fn run_launchctl(args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("launchctl")
        .args(args)
        .output()
        .context("Failed to spawn `launchctl` — is it on PATH?")?;

    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code().unwrap_or(-1);
    let subcmd = args.first().copied().unwrap_or("?");

    bail!(
        "`launchctl {subcmd}` exited with code {code}.\n\
         stdout: {stdout}\n\
         stderr: {stderr}"
    )
}
