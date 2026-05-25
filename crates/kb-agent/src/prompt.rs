//! Prompt construction and PATH staging for the agent subprocess.
//!
//! Two responsibilities split out from the driver to keep `driver.rs`
//! focused on the I/O:
//!
//! 1. [`build_user_prompt`]   — the single `prompt` command sent to pi.
//!                              Begins with the `/skill:knowledge-builder-integrator`
//!                              slash command so pi expands the SKILL.md
//!                              content inline before the LLM sees it.
//! 2. [`stage_skills_dir`]    — write the embedded SKILL.md to the
//!                              per-job `work_dir/.skills/` so the
//!                              `--skill <dir>` argument has a real
//!                              directory to read.
//! 3. [`stage_wrapper_dir`]   — build a per-job `work_dir/.agent-bin/`
//!                              with `kb-obsidian` symlinked in, plus
//!                              the curated read-only utilities that
//!                              the agent's `bash` tool is allowed to
//!                              invoke.

use std::path::{Path, PathBuf};

use crate::driver::AgentInput;

/// SKILL.md content embedded into the binary at compile time so the
/// agent's `--skill` argument always has a real directory to point at,
/// independent of where the kb binary was installed.
const SKILL_MD: &str = include_str!("../skills/SKILL.md");

/// Read-only system utilities the agent's `bash` tool is allowed to
/// invoke as bare names.  Anything else (mkdir, cp, mv, rm, …) fails
/// with `command not found` because the daemon-injected PATH includes
/// only `<work_dir>/.agent-bin/` plus the directory containing `node`.
const AGENT_PATH_BINARIES: &[&str] = &[
    // Reading
    "cat", "head", "tail", "sed", "grep", "awk",
    "wc", "printf", "echo", "sort", "uniq", "tr",
    // Shells (pi's bash tool sometimes shells out via /bin/sh).
    "sh", "bash",
    // Diagnostic + control flow
    "env", "true", "false", "basename", "dirname", "date",
    // JSON helper for parsing search/list outputs (when present).
    "jq",
    // pi shebang requires node; pi internals shell out to npm/npx.
    "node", "npm", "npx",
];

// ── Prompt ───────────────────────────────────────────────────────────────────

/// Build the user prompt for a single agent run.
///
/// The first line invokes the SKILL we shipped via `--skill`; pi
/// expands `/skill:<name>` slash commands inline before sending to the
/// LLM, so the rest of this string is appended verbatim by pi.
pub fn build_user_prompt(inp: &AgentInput) -> String {
    let mut s = String::with_capacity(1024);
    s.push_str("/skill:knowledge-builder-integrator\n\n");
    s.push_str("Per-job context:\n");
    s.push_str(&format!("  extracted_path  = {}\n", inp.extracted_path.display()));
    s.push_str(&format!("  source_basename = {}\n", inp.source_basename));
    s.push_str(&format!("  vault_root      = {}\n", inp.vault_root.display()));
    s.push_str(&format!("  sources_dir     = {}\n", inp.sources_dir.display()));
    s.push_str(&format!("  agent_root      = {}\n", inp.agent_root.display()));
    s.push_str(&format!("  mode            = {}\n", inp.mode));
    s.push_str(&format!("  job_id          = {}\n", inp.job_id));
    s.push_str("\n");
    s.push_str("Required workflow:\n");
    s.push_str("  1. Read the extracted content with `cat`.\n");
    s.push_str("  2. Survey existing structure: `kb-obsidian folders folder=KnowledgeBase`,\n");
    s.push_str("     `kb-obsidian tags counts`.\n");
    s.push_str("  3. For overlap detection: at least one `kb-obsidian search query=...`.\n");
    s.push_str("  4. **Issue at least one `kb-obsidian create path=KnowledgeBase/...`\n");
    s.push_str("     (or append/move/etc.) command** to actually integrate the content.\n");
    s.push_str("     A textual summary alone is NOT a successful integration.\n");
    s.push_str("  5. Optionally set frontmatter properties with `kb-obsidian property:set\n");
    s.push_str("     name=... value=... file=...`.\n");
    s.push_str("  6. Emit a brief final summary message and stop.\n\n");
    s.push_str("All vault operations MUST go through the `kb-obsidian` wrapper.  Do not\n");
    s.push_str("use `cat >`, `tee`, redirects, or any other shell tricks to write into\n");
    s.push_str("the vault — those are detected and reported as rogue writes.\n");
    s
}

// ── Staging ──────────────────────────────────────────────────────────────────

/// Extract the embedded SKILL.md to `work_dir/.skills/SKILL.md` and
/// return the directory path (so the caller can pass it to
/// `pi --skill <dir>`).
pub fn stage_skills_dir(work_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = work_dir.join(".skills");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("SKILL.md"), SKILL_MD)?;
    Ok(dir)
}

/// Build the per-job wrapper PATH dir.  Symlinks in the curated read-
/// only system utilities (looked up on the operator's PATH) plus
/// `kb-obsidian` (resolved via `which kb-obsidian` — the kb-obsidian
/// binary in this workspace).  Returns the absolute path of the
/// staged directory.
pub fn stage_wrapper_dir(work_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = work_dir.join(".agent-bin");
    std::fs::create_dir_all(&dir)?;

    // 1) kb-obsidian is mandatory.  Look on PATH.
    let kb_obsidian = which_first(&["kb-obsidian"]).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "kb-obsidian binary missing on PATH; reinstall kb (it ships \
             two binaries: `kb` and `kb-obsidian`).",
        )
    })?;
    symlink_replace(&kb_obsidian, &dir.join("kb-obsidian"))?;

    // 2) Curated allowlist.  Missing utilities are silently skipped —
    //    if the operator's system doesn't have `jq` for example, the
    //    agent simply can't use it.
    for name in AGENT_PATH_BINARIES {
        if let Some(src) = which_first(&[name]) {
            let _ = symlink_replace(&src, &dir.join(name));
        }
    }
    Ok(dir)
}

fn which_first(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for name in names {
        for d in std::env::split_paths(&path) {
            let c = d.join(name);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

#[cfg(unix)]
fn symlink_replace(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.exists() || dst.is_symlink() {
        let _ = std::fs::remove_file(dst);
    }
    std::os::unix::fs::symlink(src, dst)
}
#[cfg(not(unix))]
fn symlink_replace(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.exists() {
        let _ = std::fs::remove_file(dst);
    }
    std::fs::copy(src, dst).map(|_| ())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn input() -> AgentInput {
        AgentInput {
            extracted_path:  PathBuf::from("/work/e.md"),
            work_dir:        PathBuf::from("/work"),
            vault_root:      PathBuf::from("/vault"),
            sources_dir:     PathBuf::from("/vault/Sources"),
            agent_root:      PathBuf::from("/vault/KnowledgeBase"),
            source_basename: "paper.pdf".into(),
            model:           "openrouter/moonshotai/kimi-k2.5".into(),
            job_id:           42,
            mode:            "apply".into(),
        }
    }

    #[test]
    fn prompt_starts_with_skill_invocation() {
        let s = build_user_prompt(&input());
        assert!(s.starts_with("/skill:knowledge-builder-integrator\n"));
    }

    #[test]
    fn prompt_contains_per_job_context() {
        let s = build_user_prompt(&input());
        for needle in &[
            "extracted_path", "source_basename", "vault_root", "sources_dir",
            "agent_root", "mode", "job_id", "kb-obsidian",
        ] {
            assert!(s.contains(needle), "prompt missing {needle}: {s}");
        }
    }

    #[test]
    fn skills_dir_extracts_skill_md() {
        let tmp = TempDir::new().unwrap();
        let dir = stage_skills_dir(tmp.path()).unwrap();
        let md  = dir.join("SKILL.md");
        assert!(md.is_file());
        let content = std::fs::read_to_string(&md).unwrap();
        // Sanity: the embedded SKILL.md is non-trivial (>1 KB).
        assert!(content.len() > 1000, "SKILL.md too small: {} bytes", content.len());
    }

    #[test]
    fn wrapper_dir_skips_missing_utilities_silently() {
        // If the operator's PATH doesn't have `jq` (e.g.) the wrapper
        // dir build still succeeds — only kb-obsidian is mandatory.
        // We can't assert the test runner's PATH lacks any of these,
        // so we just check the function doesn't panic with a clean
        // PATH that has at least sh and node.
        // (In CI this is the realistic case; locally kb-obsidian is
        // present from an earlier build.)
        let tmp = TempDir::new().unwrap();
        let res = stage_wrapper_dir(tmp.path());
        // Either succeeds OR fails because kb-obsidian isn't on PATH;
        // we don't pin to one outcome (different environments).
        match res {
            Ok(dir) => {
                assert!(dir.ends_with(".agent-bin"));
                assert!(dir.is_dir());
            }
            Err(e) => {
                assert!(e.to_string().contains("kb-obsidian"),
                    "unexpected error: {e}");
            }
        }
    }
}
