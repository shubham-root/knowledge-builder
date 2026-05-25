//! `kb-obsidian` — policy wrapper around Obsidian's command-line interface.
//!
//! Replaces the legacy Python wrapper at
//! `processors/default/kb_processor/agent/wrappers/kb-obsidian` with a
//! native Rust binary.  Wire format (stdout JSON, plan-file JSONL,
//! exit codes) is **bit-identical** so the wrapper can be swapped in
//! without coordinating with the daemon side.
//!
//! The Knowledge Builder agent (running inside `pi --mode rpc`) is told
//! via its skills to invoke `kb-obsidian <cmd> [key=value]...` for every
//! vault operation.  Read commands pass straight through to the real
//! `obsidian` binary; write commands are intercepted in shadow mode and
//! recorded to a JSONL plan file without touching the vault, or passed
//! through in apply mode (the default).
//!
//! See [`agent::wrappers::kb-obsidian`] in the legacy Python tree (now
//! deleted by Session B) for the original docstring and policy.
//!
//! # Modes
//!
//! `KB_AGENT_MODE=apply` (default)
//! :   every command — read or write — passes through to the real
//!     `obsidian` binary.  Each accepted write is also logged as a JSON
//!     line to `$KB_PLAN_FILE` with `applied=true` plus the exit code.
//!
//! `KB_AGENT_MODE=shadow`
//! :   write commands return mock-success JSON and append a plan entry
//!     to `$KB_PLAN_FILE` without touching the vault.
//!
//! # Always-blocked
//!
//! `eval` is unconditionally rejected (arbitrary JS in the Obsidian app).
//!
//! # Required env vars
//!
//! `KB_PLAN_FILE`
//! :   JSONL path the wrapper appends to.  Created if absent.
//!
//! `KB_AGENT_MODE`
//! :   `shadow` | `apply`.  Defaults to `apply` when unset.
//!
//! # Optional env vars
//!
//! `KB_OBSIDIAN_BIN`
//! :   override the path to the real Obsidian CLI.  Defaults to
//!     `obsidian` on `$PATH`.
//!
//! `KB_VAULT_ROOT` / `KB_SOURCES_DIR` / `KB_AGENT_ROOT`
//! :   when set, write commands have their `path=` / `file=` / `name=`
//!     arguments validated against:
//!         1. Resolves under `KB_VAULT_ROOT`.
//!         2. Resolves OUTSIDE `KB_SOURCES_DIR`.
//!         3. Resolves under `KB_AGENT_ROOT` (when set).
//!     The legacy Python wrapper had identical semantics; an unset
//!     `KB_VAULT_ROOT` or `KB_SOURCES_DIR` skips the path check (used
//!     by some unit-test harnesses).
//!
//! # Exit codes
//!
//! * `0`   — success.
//! * `1`   — wrapper-internal error (bad args, blocked command,
//!           missing env, path invariant violation).
//! * `≥2`  — passthrough exit code from the real `obsidian` binary.

#![deny(unsafe_code)]
#![warn(missing_docs)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use kb_agent::plan::{append_entry, PlanEntry};

// ── Command classification ───────────────────────────────────────────────────

fn read_commands() -> BTreeSet<&'static str> {
    [
        "file", "files", "folder", "folders",
        "read", "outline",
        "search", "search:context",
        "backlinks", "links", "unresolved", "orphans", "deadends",
        "tags", "aliases", "properties", "property:read",
        "daily:path", "daily:read",
        "bases", "base:views", "base:query",
        "bookmarks",
        "diff", "history", "history:list", "history:read",
        "version", "help",
    ].into_iter().collect()
}

fn write_commands() -> BTreeSet<&'static str> {
    [
        "create", "append", "prepend",
        "move", "rename", "delete",
        "property:set", "property:remove",
        "daily:append", "daily:prepend",
        "bookmark",
        "base:create",
    ].into_iter().collect()
}

fn blocked_commands() -> BTreeSet<&'static str> {
    [
        "eval",
        "history:restore",
        "plugin:enable", "plugin:disable", "plugin:install", "plugin:uninstall",
        "plugin:reload", "plugins:restrict",
        "publish:add", "publish:remove",
        "sync",
        "reload", "restart", "devtools", "dev:screenshot",
    ].into_iter().collect()
}

const DEFAULT_OBSIDIAN_BIN: &str = "obsidian";
const DEFAULT_MODE:         &str = "apply";

/// Argument keys whose values may carry a vault-relative or absolute
/// path that must be validated against `vault_root` / `sources_dir` /
/// `agent_root`.
const PATH_KEYS: &[&str] = &["path", "to", "file", "name"];

#[derive(Debug)]
enum Classification { Read, Write, Blocked, Unknown }

fn classify(cmd: &str) -> Classification {
    if blocked_commands().contains(cmd) { return Classification::Blocked; }
    if read_commands().contains(cmd)    { return Classification::Read; }
    if write_commands().contains(cmd)   { return Classification::Write; }
    Classification::Unknown
}

// ── Stdout JSON helpers ──────────────────────────────────────────────────────

fn emit_error(message: &str) -> ! {
    let payload = serde_json::json!({
        "status":  "error",
        "error":   message,
        "wrapper": "kb-obsidian",
    });
    println!("{payload}");
    std::process::exit(1);
}

fn emit_shadow_ok(cmd: &str, args: &[String]) {
    let payload = serde_json::json!({
        "status": "ok",
        "mode":   "shadow",
        "cmd":    cmd,
        "args":   args,
        "note":   "no mutation applied; recorded to plan",
    });
    println!("{payload}");
}

// ── Env parsing ──────────────────────────────────────────────────────────────

fn parse_mode() -> String {
    let raw = std::env::var("KB_AGENT_MODE").unwrap_or_else(|_| DEFAULT_MODE.into());
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed != "shadow" && trimmed != "apply" {
        emit_error(&format!(
            "KB_AGENT_MODE must be 'shadow' or 'apply', got {trimmed:?}",
        ));
    }
    trimmed
}

fn plan_file_path() -> PathBuf {
    let raw = std::env::var("KB_PLAN_FILE").unwrap_or_default();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        emit_error(
            "KB_PLAN_FILE not set; the daemon must export it before \
             spawning the agent subprocess.",
        );
    }
    let p = PathBuf::from(trimmed);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    p
}

fn real_obsidian_bin() -> PathBuf {
    let raw = std::env::var("KB_OBSIDIAN_BIN").unwrap_or_else(|_| DEFAULT_OBSIDIAN_BIN.into());
    // `which`-equivalent without pulling in the full `which` crate.
    let candidate = PathBuf::from(&raw);
    if candidate.is_absolute() && candidate.is_file() {
        return candidate;
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let p = dir.join(&raw);
            if p.is_file() {
                return p;
            }
        }
    }
    emit_error(&format!(
        "Obsidian CLI '{raw}' not found on PATH.  Enable it in \
         Obsidian: Settings → General → Command line interface.",
    ));
}

// ── Argument validation ──────────────────────────────────────────────────────

fn check_arg_syntax(args: &[String]) {
    for tok in args {
        if tok.starts_with("--") {
            emit_error(&format!(
                "argument {tok:?} uses POSIX-style syntax (--key value).  \
                 Obsidian CLI requires `key=value` (no dashes).  \
                 Example: `kb-obsidian create path=Notes/Foo.md content=hi`.",
            ));
        }
        // -x style: reject only if x is alphabetic so we don't trip on
        // legitimate values like `content=-1` (which is a `key=value`,
        // not a leading-dash token).
        if tok.starts_with('-') && tok.len() >= 2 {
            let second = tok.chars().nth(1).unwrap();
            if second.is_ascii_alphabetic() {
                emit_error(&format!(
                    "argument {tok:?} uses POSIX-style syntax (-x).  \
                     Obsidian CLI requires `key=value` (no dashes).",
                ));
            }
        }
    }
}

fn parse_kv_args(args: &[String]) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for tok in args {
        if let Some(eq) = tok.find('=') {
            if eq == 0 { continue; }
            let key = tok[..eq].trim().to_string();
            let val = tok[eq + 1..].to_string();
            out.insert(key, val);
        }
    }
    out
}

/// Resolve a vault-relative or absolute path argument to a canonical
/// absolute path under `vault_root`.
///
/// We do NOT use `std::fs::canonicalize` here because the path may not
/// exist yet (the agent is asking obsidian to *create* it).  Instead we
/// normalise lexically: strip `.` and `..` components, but keep the
/// underlying absolute prefix intact.
fn resolve_in_vault(value: &str, vault_root: &Path) -> PathBuf {
    let p = Path::new(value);
    let absolute = if p.is_absolute() {
        p.to_path_buf()
    } else {
        vault_root.join(p)
    };
    normalise_lexical(&absolute)
}

fn normalise_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => { out.pop(); }
            std::path::Component::CurDir    => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn check_path_invariant(_cmd: &str, args: &[String]) {
    let vault_root_raw  = std::env::var("KB_VAULT_ROOT" ).unwrap_or_default();
    let sources_dir_raw = std::env::var("KB_SOURCES_DIR").unwrap_or_default();
    let agent_root_raw  = std::env::var("KB_AGENT_ROOT" ).unwrap_or_default();

    let vault_root_raw  = vault_root_raw.trim();
    let sources_dir_raw = sources_dir_raw.trim();
    let agent_root_raw  = agent_root_raw.trim();
    if vault_root_raw.is_empty() || sources_dir_raw.is_empty() {
        return;     // unit-test mode
    }

    let vault_root  = normalise_lexical(Path::new(vault_root_raw));
    let sources_dir = normalise_lexical(Path::new(sources_dir_raw));
    let agent_root  = if agent_root_raw.is_empty() {
        None
    } else {
        Some(normalise_lexical(Path::new(agent_root_raw)))
    };

    let kv = parse_kv_args(args);
    for &key in PATH_KEYS {
        let raw_val = match kv.get(key) {
            Some(v) => v.as_str(),
            None    => continue,
        };
        // Bare wikilink / filename values (no `/`) are not path-like —
        // the agent is targeting an existing note by name.  Only `/`-
        // bearing or absolute values get validated.
        let p_raw = Path::new(raw_val);
        if !p_raw.is_absolute() && !raw_val.contains('/') {
            continue;
        }
        let resolved = resolve_in_vault(raw_val, &vault_root);

        if !resolved.starts_with(&vault_root) {
            emit_error(&format!(
                "path {raw_val:?} (resolved {}) is outside vault_root {}.  \
                 All vault paths must be relative to or inside the vault root.",
                resolved.display(), vault_root.display(),
            ));
        }
        if resolved.starts_with(&sources_dir) {
            emit_error(&format!(
                "path {raw_val:?} (resolved {}) is inside sources_dir {}.  \
                 Writes to sources_dir are forbidden by Knowledge Builder policy.",
                resolved.display(), sources_dir.display(),
            ));
        }
        if let Some(ar) = &agent_root {
            if !resolved.starts_with(ar) {
                let hint = ar.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("KnowledgeBase");
                emit_error(&format!(
                    "path {raw_val:?} (resolved {}) is outside agent_root {}.  \
                     All agent mutations must be inside the agent's sandbox; \
                     prefix the path with '{hint}/'.  Example: \
                     'path={hint}/Topics/Foo.md'.",
                    resolved.display(), ar.display(),
                ));
            }
        }
    }
}

// ── Passthrough ──────────────────────────────────────────────────────────────

fn passthrough(bin: &Path, cmd: &str, args: &[String]) -> i32 {
    let status = std::process::Command::new(bin)
        .arg(cmd)
        .args(args)
        .status();
    match status {
        Ok(s)  => s.code().unwrap_or(1),
        Err(e) => {
            emit_error(&format!(
                "failed to spawn obsidian binary {}: {e}",
                bin.display(),
            ));
        }
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 2 {
        emit_error("usage: kb-obsidian <command> [key=value]... [flag]...");
    }

    let cmd = argv[1].clone();
    let args: Vec<String> = argv[2..].to_vec();
    let mode = parse_mode();
    let plan_file = plan_file_path();

    let class = classify(&cmd);
    match class {
        Classification::Blocked => {
            emit_error(&format!(
                "command {cmd:?} is blocked by kb-obsidian policy.  See \
                 docs/agent.md for the allowlist.",
            ));
        }
        Classification::Unknown => {
            let mut allow: Vec<&'static str> =
                read_commands().union(&write_commands()).copied().collect();
            allow.sort();
            emit_error(&format!(
                "command {cmd:?} is not in the kb-obsidian allowlist.  Use \
                 one of: {}",
                allow.join(", "),
            ));
        }
        _ => {}
    }

    // POSIX-style guard before anything mutating happens.
    check_arg_syntax(&args);

    // Path invariant for write commands.
    if matches!(class, Classification::Write) {
        check_path_invariant(&cmd, &args);
    }

    // Read path: passthrough only, no plan entry.
    if matches!(class, Classification::Read) {
        let bin = real_obsidian_bin();
        let rc = passthrough(&bin, &cmd, &args);
        return ExitCode::from(rc as u8);
    }

    // Write path.
    debug_assert!(matches!(class, Classification::Write));

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut entry = PlanEntry {
        ts,
        mode: mode.clone(),
        cmd: cmd.clone(),
        args: args.clone(),
        applied: false,
        exit_code: None,
    };

    if mode == "apply" {
        let bin = real_obsidian_bin();
        let rc  = passthrough(&bin, &cmd, &args);
        entry.applied   = rc == 0;
        entry.exit_code = Some(rc);
        if let Err(e) = append_entry(&plan_file, &entry) {
            emit_error(&format!(
                "failed to append plan entry to {}: {e}",
                plan_file.display(),
            ));
        }
        return ExitCode::from(rc as u8);
    }

    // Shadow mode.
    if let Err(e) = append_entry(&plan_file, &entry) {
        emit_error(&format!(
            "failed to append plan entry to {}: {e}",
            plan_file.display(),
        ));
    }
    emit_shadow_ok(&cmd, &args);
    ExitCode::SUCCESS
}
