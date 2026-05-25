//! Integration tests for the `kb-obsidian` wrapper binary.
//!
//! These tests spawn the compiled binary as a subprocess (via
//! `assert_cmd`) so the same code path the agent uses is exercised
//! end-to-end: env var parsing, argv classification, plan-file
//! append, JSON status output, and exit codes.
//!
//! A "stub obsidian" — a tiny shell script that echoes its argv and
//! exits with a configurable code — is staged on `KB_OBSIDIAN_BIN` so
//! we don't need the real Obsidian installation present.
//!
//! Test count parity vs the legacy Python wrapper test suite (24
//! cases): every behavioural test that materially affects safety or
//! the wire format is ported.  Cases exercising Python-specific
//! syntax (the `_DASH_ARG_RE` quirk on bytes, etc.) are dropped.

use std::path::{Path, PathBuf};

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;

// ── Fixture helpers ─────────────────────────────────────────────────────────

struct Env {
    _tmp:        TempDir,
    vault:       PathBuf,
    sources:     PathBuf,
    kb:          PathBuf,
    plan_file:   PathBuf,
    stub_dir:    PathBuf,
    /// Path of the stub `obsidian` binary the wrapper will exec.  The
    /// stub echoes `STUB-ECHO: <argv>` to stdout and exits with the
    /// code in env var `STUB_RC` (default 0).
    stub_obsidian: PathBuf,
}

fn setup() -> Env {
    let tmp = TempDir::new().expect("tempdir");
    let vault   = tmp.path().join("Vault");
    let sources = vault.join("Sources");
    let kb      = vault.join("KnowledgeBase");
    let stub_dir= tmp.path().join("bin");
    std::fs::create_dir_all(&sources).unwrap();
    std::fs::create_dir_all(&kb).unwrap();
    std::fs::create_dir_all(&stub_dir).unwrap();

    // Stub obsidian.
    let stub = stub_dir.join("obsidian");
    std::fs::write(
        &stub,
        r#"#!/bin/sh
echo "STUB-ECHO: $@"
exit ${STUB_RC:-0}
"#,
    ).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&stub).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&stub, perms).unwrap();

    let plan_file = tmp.path().join("plan.jsonl");

    Env {
        _tmp: tmp, vault, sources, kb,
        plan_file, stub_dir, stub_obsidian: stub,
    }
}

fn cmd(env: &Env) -> assert_cmd::Command {
    let mut c = assert_cmd::Command::cargo_bin("kb-obsidian")
        .expect("kb-obsidian binary built");
    c.env_clear()
        .env("HOME", env._tmp.path())
        .env("PATH", format!(
            "{}:/usr/bin:/bin",
            env.stub_dir.display(),
        ))
        .env("KB_OBSIDIAN_BIN", &env.stub_obsidian)
        .env("KB_PLAN_FILE",    &env.plan_file)
        .env("KB_VAULT_ROOT",   &env.vault)
        .env("KB_SOURCES_DIR",  &env.sources)
        .env("KB_AGENT_ROOT",   &env.kb);
    c
}

fn read_plan(p: &Path) -> Vec<serde_json::Value> {
    if !p.exists() { return Vec::new(); }
    std::fs::read_to_string(p).unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid JSONL"))
        .collect()
}

// ── Read-path passthrough ────────────────────────────────────────────────────

#[test]
fn read_command_passes_through_apply_mode() {
    let env = setup();
    cmd(&env)
        .env("KB_AGENT_MODE", "apply")
        .args(["search", "query=foo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("STUB-ECHO: search query=foo"));
    assert!(read_plan(&env.plan_file).is_empty(),
        "read commands must NOT write to the plan");
}

#[test]
fn read_command_passes_through_shadow_mode() {
    let env = setup();
    cmd(&env)
        .env("KB_AGENT_MODE", "shadow")
        .args(["search", "query=foo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("STUB-ECHO: search query=foo"));
    assert!(read_plan(&env.plan_file).is_empty());
}

#[test]
fn read_command_propagates_nonzero_exit() {
    let env = setup();
    cmd(&env)
        .env("STUB_RC", "7")
        .args(["search", "query=foo"])
        .assert()
        .code(7);
}

// ── Write-path: apply mode ───────────────────────────────────────────────────

#[test]
fn write_in_apply_mode_passes_through_and_records_plan() {
    let env = setup();
    cmd(&env)
        .env("KB_AGENT_MODE", "apply")
        .args(["create", "path=KnowledgeBase/Foo.md", "content=hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("STUB-ECHO: create path=KnowledgeBase/Foo.md content=hello"));

    let entries = read_plan(&env.plan_file);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["mode"],    "apply");
    assert_eq!(entries[0]["cmd"],     "create");
    assert_eq!(entries[0]["applied"], true);
    assert_eq!(entries[0]["exit_code"], 0);
    assert_eq!(entries[0]["args"][0], "path=KnowledgeBase/Foo.md");
}

#[test]
fn apply_records_failure_when_obsidian_returns_nonzero() {
    let env = setup();
    cmd(&env)
        .env("KB_AGENT_MODE", "apply")
        .env("STUB_RC", "3")
        .args(["create", "path=KnowledgeBase/Foo.md", "content=x"])
        .assert()
        .code(3);
    let e = read_plan(&env.plan_file);
    assert_eq!(e.len(), 1);
    assert_eq!(e[0]["applied"], false);
    assert_eq!(e[0]["exit_code"], 3);
}

// ── Write-path: shadow mode ──────────────────────────────────────────────────

#[test]
fn write_in_shadow_logs_plan_and_skips_obsidian() {
    let env = setup();
    cmd(&env)
        .env("KB_AGENT_MODE", "shadow")
        .args(["create", "path=KnowledgeBase/Foo.md", "content=x"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"mode\":\"shadow\""))
        .stdout(predicate::str::contains("\"status\":\"ok\""))
        .stdout(predicate::str::contains("STUB-ECHO").not());
    let e = read_plan(&env.plan_file);
    assert_eq!(e.len(), 1);
    assert_eq!(e[0]["applied"], false);
    assert!(e[0].get("exit_code").is_none() || e[0]["exit_code"].is_null());
}

// ── Blocked / unknown commands ───────────────────────────────────────────────

#[test]
fn blocked_command_rejected() {
    let env = setup();
    cmd(&env)
        .args(["eval", "code=alert(1)"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("blocked by kb-obsidian policy"));
    assert!(read_plan(&env.plan_file).is_empty());
}

#[test]
fn unknown_command_rejected() {
    let env = setup();
    cmd(&env)
        .args(["totally-not-a-command"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("not in the kb-obsidian allowlist"));
}

// ── Env preconditions ────────────────────────────────────────────────────────

#[test]
fn missing_plan_file_env_rejected() {
    let env = setup();
    let mut c = cmd(&env);
    c.env_remove("KB_PLAN_FILE")
        .args(["search", "query=foo"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("KB_PLAN_FILE not set"));
}

#[test]
fn invalid_mode_rejected() {
    let env = setup();
    cmd(&env)
        .env("KB_AGENT_MODE", "weird")
        .args(["search", "query=foo"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("KB_AGENT_MODE must be"));
}

// ── POSIX-style argument rejection ───────────────────────────────────────────

#[test]
fn dash_prefixed_argument_rejected() {
    let env = setup();
    cmd(&env)
        .args(["create", "--path=KnowledgeBase/Foo.md"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("POSIX-style"));
}

// ── Path-invariant enforcement ───────────────────────────────────────────────

#[test]
fn path_inside_sources_dir_rejected() {
    let env = setup();
    cmd(&env)
        .args(["create", "path=Sources/secret.md", "content=x"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("inside sources_dir"));
}

#[test]
fn path_outside_vault_root_rejected() {
    let env = setup();
    cmd(&env)
        .args(["create", "path=/etc/passwd", "content=x"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("outside vault_root"));
}

#[test]
fn path_outside_agent_root_rejected() {
    let env = setup();
    // Inside vault, but NOT inside KnowledgeBase.
    let outside_kb = "Personal/Diary.md";
    cmd(&env)
        .args(["create", &format!("path={outside_kb}"), "content=x"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("outside agent_root"));
}

#[test]
fn path_inside_agent_root_accepted() {
    let env = setup();
    cmd(&env)
        .args(["create", "path=KnowledgeBase/Topics/Foo.md", "content=x"])
        .assert()
        .success();
}

#[test]
fn relative_path_under_vault_root_accepted() {
    let env = setup();
    cmd(&env)
        .args(["create", "path=KnowledgeBase/Foo.md", "content=x"])
        .assert()
        .success();
}

#[test]
fn agent_root_enforcement_skipped_when_unset() {
    let env = setup();
    let mut c = cmd(&env);
    c.env_remove("KB_AGENT_ROOT")
        .args(["create", "path=Personal/Diary.md", "content=x"])
        .assert()
        .success();
}

#[test]
fn file_arg_with_slash_validated_against_agent_root() {
    let env = setup();
    cmd(&env)
        .args(["append", "file=Personal/Diary.md", "content=x"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("outside agent_root"));
}

#[test]
fn bare_file_wikilink_skips_path_validation() {
    let env = setup();
    // No `/`, no leading `/` — treated as a wikilink target by Obsidian.
    cmd(&env)
        .args(["append", "file=SomeNote", "content=x"])
        .assert()
        .success();
}

#[test]
fn name_arg_with_slash_validated() {
    let env = setup();
    cmd(&env)
        .args(["create", "name=../escape", "content=x"])
        .assert()
        .code(1)
        // ``../escape`` traverses out of the vault, so the vault-root
        // guard fires first.  We accept either rejection — the
        // semantic invariant is that the wrapper refuses path-like
        // values that escape the agent's sandbox.
        .stdout(
            predicate::str::contains("outside agent_root")
                .or(predicate::str::contains("outside vault_root"))
        );
}

// ── Plan file shape ─────────────────────────────────────────────────────────

#[test]
fn plan_entries_have_sorted_keys_for_byte_compat_with_python_wrapper() {
    let env = setup();
    cmd(&env)
        .args(["create", "path=KnowledgeBase/A.md", "content=x"])
        .assert()
        .success();

    let raw = std::fs::read_to_string(&env.plan_file).unwrap();
    // Keys must appear in alphabetical order (sort_keys=True parity).
    let mut prev_idx = 0usize;
    for k in ["applied", "args", "cmd", "exit_code", "mode", "ts"] {
        let needle = format!("\"{k}\"");
        let idx = raw.find(&needle).unwrap_or_else(|| panic!(
            "key {k} not found in plan line: {raw}",
        ));
        assert!(idx >= prev_idx, "key {k} appeared out of order in {raw}");
        prev_idx = idx;
    }
}
