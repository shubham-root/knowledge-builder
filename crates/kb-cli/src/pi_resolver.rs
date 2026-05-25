//! Locating the `pi-coding-agent` binary on the operator's machine.
//!
//! The processor spawns `pi --mode rpc` for every job.  Resolving the
//! `pi` binary is non-trivial on macOS for two reasons:
//!
//! 1. **Per-shell version managers** — `fnm`, `nvm`, and `volta` all
//!    install Node into a versioned directory and create a *per-shell-
//!    session* symlink that gets put on `$PATH` by shell init scripts.
//!    These shims look like:
//!      * fnm   — `~/.local/state/fnm_multishells/<pid>_<ts>/bin/pi`
//!      * nvm   — `~/.nvm/versions/node/<v>/bin/pi`
//!      * volta — `~/.volta/bin/pi`
//!    Only the volta path is stable; fnm and nvm shims are tied to the
//!    spawning shell.  When `kb daemon` runs under `launchd` it does
//!    NOT execute shell init, so it sees only the plist's static
//!    `PATH` and the per-session shim is invisible.
//! 2. **Apple-Silicon dual prefixes** — `/usr/local/bin` (Intel) vs
//!    `/opt/homebrew/bin` (Apple Silicon).  Either may contain `pi`
//!    (when installed via `brew install -g …`) or neither.
//!
//! This module gives the rest of the codebase one place to:
//!
//! * resolve the **stable absolute path** to `pi` from the operator's
//!   shell-time `PATH` (following any symlink chain to its real home);
//! * detect whether the resolved path is a per-session shim that
//!   `launchd` will not be able to see;
//! * surface the result as a structured `PiLocation` so `kb doctor`
//!   can warn and `kb install` can bake the absolute path into the
//!   LaunchAgent plist's `EnvironmentVariables`.

use std::path::{Path, PathBuf};

/// Result of `locate_pi()`.
#[derive(Debug, Clone)]
pub enum PiLocation {
    /// `pi` was resolved to a stable absolute path that any process can
    /// invoke regardless of `PATH` decoration.  Suitable for baking
    /// into the LaunchAgent plist.
    Stable(PathBuf),

    /// `pi` was resolved, but the path is a per-shell-session shim
    /// (fnm / nvm style) that will be invisible to `launchd`.  The
    /// shim's real target was followed and is returned alongside the
    /// shim path so the operator (and `kb install`) can use the
    /// stable form.
    PerShellShim {
        /// What `which pi` returned.
        shim:   PathBuf,
        /// Where the shim ultimately points (after following all
        /// symlinks).  Use this in the plist.
        stable: PathBuf,
        /// The version manager we detected — `"fnm"`, `"nvm"`, or
        /// `"unknown"`.
        manager: &'static str,
    },

    /// `pi` is not on `PATH` at all.
    NotFound,
}

impl PiLocation {
    /// The path to put in the launchd plist (or pass via `KB_PI_BIN`).
    /// Returns `None` only for `NotFound`.
    pub fn stable_path(&self) -> Option<&Path> {
        match self {
            PiLocation::Stable(p)                          => Some(p.as_path()),
            PiLocation::PerShellShim { stable, .. }        => Some(stable.as_path()),
            PiLocation::NotFound                           => None,
        }
    }

    /// `true` when the operator should set `KB_PI_BIN` (or `kb install`
    /// should inject it) because shell-PATH lookup will not work under
    /// launchd.
    pub fn is_shim(&self) -> bool {
        matches!(self, PiLocation::PerShellShim { .. })
    }
}

/// Patterns that identify per-shell-session shims that **will not** be
/// resolvable from inside a launchd-managed process.
const SHIM_PATTERNS: &[(&str, &str)] = &[
    // (substring,   manager-name)
    ("/.local/state/fnm_multishells/", "fnm"),
    ("/.nvm/versions/node/",           "nvm"),
];

/// Locate `pi` on the operator's `PATH`.  Follows symlinks to find the
/// most-stable path and reports whether the operator-visible path is a
/// per-session shim.
pub fn locate_pi() -> PiLocation {
    let resolved = match which_pi_via_env_path() {
        Some(p) => p,
        None    => return PiLocation::NotFound,
    };

    // Follow symlinks.  `canonicalize` would also work but it requires
    // the target to exist (it does, here, but be permissive); we walk
    // manually so we can present a useful error if the chain is broken.
    let stable = canonicalize_lossy(&resolved);

    let resolved_str = resolved.to_string_lossy();
    for (pat, manager) in SHIM_PATTERNS {
        if resolved_str.contains(pat) {
            return PiLocation::PerShellShim {
                shim:    resolved,
                stable:  stable.clone(),
                manager,
            };
        }
    }

    // No shim pattern matched.  Sometimes `which` returns a stable
    // path but its real target lives elsewhere (rare for pi).  Prefer
    // the canonical form.
    PiLocation::Stable(stable)
}

/// Walk every directory in `$PATH` and return the first `pi` we find.
///
/// Distinct from `which::which()` because we explicitly do **not** want
/// to use any caching and we want to be transparent about which
/// directory matched (useful for diagnostics).
fn which_pi_via_env_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("pi");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Locate `node` on the operator's `PATH` and return the directory it
/// lives in (after symlink-following).
///
/// Why we need this separately from `pi`: `pi` is a Node script with
/// a `#!/usr/bin/env node` shebang.  When the daemon spawns it under
/// launchd's restricted PATH (`/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin`),
/// `env` cannot find `node` if Node was installed via fnm/nvm into the
/// user's home, and pi exits with `env: node: No such file or directory`
/// before producing any RPC events.  `kb install` prepends the directory
/// returned here to the plist's `PATH` so the daemon — and every
/// subprocess it spawns — can resolve `node`, `npm`, and `npx`.
///
/// Returns the **directory containing the resolved `node` binary**,
/// not the binary itself, because that's what we want to add to PATH.
/// Returns `None` if `node` is not on PATH.
pub fn locate_node_bin_dir() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("node");
        if candidate.is_file() {
            // Follow symlinks so an fnm shim resolves to the real bin
            // dir under `node-versions/<v>/installation/bin/`.
            let resolved = canonicalize_lossy(&candidate);
            return resolved.parent().map(Path::to_path_buf);
        }
    }
    None
}

/// `std::fs::canonicalize` that falls back to the input when
/// canonicalization fails.  Handles broken symlinks gracefully — a
/// broken pi link is a problem the caller should surface, not a panic.
fn canonicalize_lossy(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn make_executable(p: &Path) {
        let mut perms = std::fs::metadata(p).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(p, perms).unwrap();
    }

    /// Run the scoped closure with `$PATH` set to `dirs`, restoring the
    /// previous value when done.  Tests use this to control resolution.
    fn with_path<R>(dirs: &[&Path], f: impl FnOnce() -> R) -> R {
        let saved = std::env::var_os("PATH");
        let joined = std::env::join_paths(dirs.iter().map(|p| p.to_path_buf())).unwrap();
        // SAFETY: tests are single-threaded inside a `cargo test` worker
        // by default; we restore in the trailing block.
        unsafe { std::env::set_var("PATH", &joined); }
        let out = f();
        unsafe {
            match saved {
                Some(v) => std::env::set_var("PATH", v),
                None    => std::env::remove_var("PATH"),
            }
        }
        out
    }

    #[test]
    fn not_found_when_not_on_path() {
        let tmp = TempDir::new().unwrap();
        let result = with_path(&[tmp.path()], locate_pi);
        assert!(matches!(result, PiLocation::NotFound));
        assert!(result.stable_path().is_none());
        assert!(!result.is_shim());
    }

    #[test]
    fn stable_path_returned_when_pi_lives_in_a_normal_dir() {
        let tmp = TempDir::new().unwrap();
        let bin = tmp.path().join("pi");
        std::fs::write(&bin, "#!/bin/sh\necho stub\n").unwrap();
        make_executable(&bin);

        let result = with_path(&[tmp.path()], locate_pi);
        match result {
            PiLocation::Stable(p) => {
                // Canonical form might add `/private` on macOS for /tmp.
                assert!(p.ends_with("pi"));
            }
            other => panic!("expected Stable, got {other:?}"),
        }
    }

    #[test]
    fn fnm_style_shim_detected_and_followed() {
        // Simulate fnm's layout:
        //   <tmp>/.local/state/fnm_multishells/12345_67890/bin/pi  →  shim
        //   <tmp>/.local/share/fnm/node-versions/v24/installation/bin/pi  →  stable
        let tmp = TempDir::new().unwrap();
        let stable_dir =
            tmp.path()
               .join(".local/share/fnm/node-versions/v24/installation/bin");
        let shim_dir =
            tmp.path()
               .join(".local/state/fnm_multishells/12345_67890/bin");
        std::fs::create_dir_all(&stable_dir).unwrap();
        std::fs::create_dir_all(&shim_dir).unwrap();

        let stable = stable_dir.join("pi");
        std::fs::write(&stable, "#!/bin/sh\necho real-pi\n").unwrap();
        make_executable(&stable);

        let shim = shim_dir.join("pi");
        std::os::unix::fs::symlink(&stable, &shim).unwrap();

        let result = with_path(&[&shim_dir], locate_pi);
        match result {
            PiLocation::PerShellShim { manager, stable: ref s, .. } => {
                assert_eq!(manager, "fnm");
                // canonicalize on macOS prefixes /tmp with /private —
                // accept either form by checking the *suffix*.
                assert!(
                    s.to_string_lossy().contains("node-versions/v24/installation/bin/pi"),
                    "stable should resolve into the version dir: {s:?}",
                );
            }
            other => panic!("expected PerShellShim, got {other:?}"),
        }
        assert!(result.is_shim());
        assert!(result.stable_path().is_some());
    }

    #[test]
    fn nvm_style_shim_detected() {
        let tmp = TempDir::new().unwrap();
        let nvm_dir = tmp.path().join(".nvm/versions/node/v20.0.0/bin");
        std::fs::create_dir_all(&nvm_dir).unwrap();
        let pi = nvm_dir.join("pi");
        std::fs::write(&pi, "#!/bin/sh\necho stub\n").unwrap();
        make_executable(&pi);

        let result = with_path(&[&nvm_dir], locate_pi);
        match result {
            PiLocation::PerShellShim { manager, .. } => assert_eq!(manager, "nvm"),
            other => panic!("expected PerShellShim, got {other:?}"),
        }
    }

    #[test]
    fn node_bin_dir_resolves_through_a_symlink() {
        let tmp = TempDir::new().unwrap();
        let real_dir = tmp.path().join("installation/bin");
        let shim_dir = tmp.path().join("shims/bin");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::create_dir_all(&shim_dir).unwrap();

        let real_node = real_dir.join("node");
        std::fs::write(&real_node, "#!/bin/sh\necho fake-node\n").unwrap();
        make_executable(&real_node);
        let shim_node = shim_dir.join("node");
        std::os::unix::fs::symlink(&real_node, &shim_node).unwrap();

        let result = with_path(&[&shim_dir], locate_node_bin_dir).unwrap();
        assert!(
            result.to_string_lossy().contains("installation/bin"),
            "expected resolution into the real install dir, got {result:?}",
        );
    }

    #[test]
    fn node_bin_dir_returns_none_when_node_missing() {
        let tmp = TempDir::new().unwrap();
        let result = with_path(&[tmp.path()], locate_node_bin_dir);
        assert!(result.is_none());
    }
}
