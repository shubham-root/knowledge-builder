//! Pre/post vault snapshot for the rogue-write audit.
//!
//! Before the agent runs we walk the vault and stash `(mtime_ns, size)`
//! for every file under `vault_root` (excluding `sources_dir` and
//! `.obsidian`/`.trash`/`.git`/`node_modules`).  After the agent
//! finishes we walk again and diff: any file that appeared, was
//! modified, or vanished but is NOT covered by the agent's plan is a
//! "rogue write" — the agent bypassed the `kb-obsidian` wrapper via
//! raw bash redirects, `cat >`, etc.
//!
//! In apply mode rogue writes are surfaced as a job failure indicator
//! in the run's metadata; in shadow mode they're a hard tripwire (the
//! plan should be the only side effect).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::plan::{Plan, PlanEntry};

// ── Public types ─────────────────────────────────────────────────────────────

/// One vault snapshot.  Map from absolute canonical path to
/// `(mtime_ns, size)`.
pub type VaultSnapshot = BTreeMap<PathBuf, (i128, u64)>;

const SKIP_DIRS: &[&str] = &[".obsidian", ".trash", ".git", "node_modules"];

// ── Snapshot ─────────────────────────────────────────────────────────────────

/// Walk the vault (excluding `sources_dir` and metadata directories)
/// and return a `(path → (mtime_ns, size))` map.
///
/// Best-effort.  Returns an empty map if the walk fails for any
/// reason — the audit then becomes a no-op rather than blowing up the
/// agent run.
pub fn snapshot_vault(vault_root: &Path, sources_dir: &Path) -> VaultSnapshot {
    let sources_canon = sources_dir
        .canonicalize()
        .unwrap_or_else(|_| sources_dir.to_path_buf());

    let mut out = VaultSnapshot::new();
    if !vault_root.exists() {
        return out;
    }

    for entry in WalkDir::new(vault_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if !e.file_type().is_dir() {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            if SKIP_DIRS.iter().any(|d| *d == name) {
                return false;
            }
            // Skip sources_dir at any depth.
            let real = e.path().canonicalize();
            if let Ok(p) = real {
                if p == sources_canon || p.starts_with(&sources_canon) {
                    return false;
                }
            }
            true
        })
        .filter_map(|r| r.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let real = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => path.to_path_buf(),
        };
        if real.starts_with(&sources_canon) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            #[cfg(unix)]
            let mtime_ns: i128 = {
                use std::os::unix::fs::MetadataExt;
                let secs  = meta.mtime() as i128;
                let nsecs = meta.mtime_nsec() as i128;
                secs.saturating_mul(1_000_000_000).saturating_add(nsecs)
            };
            #[cfg(not(unix))]
            let mtime_ns: i128 = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i128)
                .unwrap_or(0);
            let size = meta.len();
            out.insert(real, (mtime_ns, size));
        }
    }
    out
}

// ── Audit ────────────────────────────────────────────────────────────────────

/// Compute the set of files the agent created, modified, or deleted
/// that are NOT in its declared plan.  Returns the **sorted** list of
/// absolute paths (deterministic for testing).
///
/// In the happy case (agent only used the `kb-obsidian` wrapper) the
/// list is empty.  Anything in the list indicates the agent bypassed
/// the wrapper via raw bash and the daemon should surface it loudly.
pub fn audit_vault_diff(
    before:     &VaultSnapshot,
    after:      &VaultSnapshot,
    plan:       &Plan,
    vault_root: &Path,
) -> Vec<PathBuf> {
    let planned: HashSet<PathBuf> = planned_paths(plan, vault_root);

    let mut rogue: BTreeSet<PathBuf> = BTreeSet::new();

    // (created or modified)
    for (path, after_meta) in after {
        match before.get(path) {
            Some(before_meta) if before_meta == after_meta => continue,
            _ => {
                if planned.contains(path) {
                    continue;
                }
                rogue.insert(path.clone());
            }
        }
    }
    // (deleted)
    for path in before.keys() {
        if !after.contains_key(path) && !planned.contains(path) {
            rogue.insert(path.clone());
        }
    }

    rogue.into_iter().collect()
}

/// Compute newly-created paths (in `after` but not `before`).  Used by
/// the link sweeper to widen its input beyond plan-derived paths,
/// catching cases where Obsidian auto-disambiguated a name.
pub fn newly_created(before: &VaultSnapshot, after: &VaultSnapshot) -> Vec<PathBuf> {
    after.keys().filter(|p| !before.contains_key(*p)).cloned().collect()
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn planned_paths(plan: &Plan, vault_root: &Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    for entry in &plan.entries {
        for arg in entry.args.iter().chain(plan_to_arg_iter(entry)) {
            if let Some((key, val)) = split_kv(arg) {
                if !matches!(key, "path" | "to" | "file" | "name") {
                    continue;
                }
                let p = Path::new(val);
                let abs = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    vault_root.join(p)
                };
                let canonical = abs.canonicalize().unwrap_or(abs);
                out.insert(canonical.clone());
                // Some commands omit the `.md` suffix; record both forms.
                let s = canonical.to_string_lossy().to_string();
                if !s.ends_with(".md") {
                    out.insert(PathBuf::from(format!("{s}.md")));
                }
            }
        }
    }
    out
}

/// Empty iterator — used as a marker so we don't need to clone the
/// args twice.  Reserved for future expansion (e.g. inferring source
/// paths from a `move src= dst=` operation).
fn plan_to_arg_iter(_e: &PlanEntry) -> std::iter::Empty<&'static String> {
    std::iter::empty()
}

fn split_kv(tok: &str) -> Option<(&str, &str)> {
    let eq = tok.find('=')?;
    if eq == 0 {
        return None;
    }
    Some((&tok[..eq], &tok[eq + 1..]))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanEntry;
    use std::fs;
    use tempfile::TempDir;

    fn setup() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let vault   = tmp.path().to_path_buf();
        let sources = vault.join("Sources");
        fs::create_dir_all(&sources).unwrap();
        fs::create_dir_all(vault.join("KnowledgeBase")).unwrap();
        (tmp, vault, sources)
    }

    fn entry(cmd: &str, args: &[&str], applied: bool) -> PlanEntry {
        PlanEntry {
            ts: 0,
            mode: "apply".into(),
            cmd: cmd.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            applied,
            exit_code: if applied { Some(0) } else { None },
        }
    }

    #[test]
    fn snapshot_excludes_sources_dir_and_obsidian_metadata() {
        let (_t, vault, sources) = setup();
        fs::write(vault.join("KnowledgeBase/Real.md"), "x").unwrap();
        fs::write(sources.join("hidden-source.pdf"), b"").unwrap();
        fs::create_dir_all(vault.join(".obsidian")).unwrap();
        fs::write(vault.join(".obsidian/workspace.md"), "x").unwrap();
        let snap = snapshot_vault(&vault, &sources);
        let names: Vec<_> = snap.keys()
            .filter_map(|p| p.file_name().and_then(|s| s.to_str()).map(|s| s.to_string()))
            .collect();
        assert!(names.iter().any(|n| n == "Real.md"));
        assert!(!names.iter().any(|n| n == "hidden-source.pdf"));
        assert!(!names.iter().any(|n| n == "workspace.md"));
    }

    #[test]
    fn audit_finds_unplanned_creation_as_rogue() {
        let (_t, vault, sources) = setup();
        let before = snapshot_vault(&vault, &sources);
        // Agent goes rogue and writes outside the plan.
        let rogue_path = vault.join("KnowledgeBase/Rogue.md");
        fs::write(&rogue_path, "agent wrote this via bash").unwrap();
        let after = snapshot_vault(&vault, &sources);
        let plan = Plan { path: PathBuf::new(), entries: vec![] };
        let rogue = audit_vault_diff(&before, &after, &plan, &vault);
        assert_eq!(rogue.len(), 1, "rogue list: {rogue:?}");
        assert!(rogue[0].ends_with("Rogue.md"));
    }

    #[test]
    fn audit_ignores_planned_creation() {
        let (_t, vault, sources) = setup();
        let before = snapshot_vault(&vault, &sources);
        let planned_path = vault.join("KnowledgeBase/Note.md");
        fs::write(&planned_path, "wrapper-driven").unwrap();
        let after = snapshot_vault(&vault, &sources);
        let plan = Plan {
            path: PathBuf::new(),
            entries: vec![entry("create", &["path=KnowledgeBase/Note.md", "content=x"], true)],
        };
        let rogue = audit_vault_diff(&before, &after, &plan, &vault);
        assert!(rogue.is_empty(), "rogue list: {rogue:?}");
    }

    #[test]
    fn newly_created_lists_all_appearances() {
        let (_t, vault, sources) = setup();
        let before = snapshot_vault(&vault, &sources);
        fs::write(vault.join("KnowledgeBase/A.md"), "a").unwrap();
        fs::write(vault.join("KnowledgeBase/B.md"), "b").unwrap();
        let after = snapshot_vault(&vault, &sources);
        let created = newly_created(&before, &after);
        assert_eq!(created.len(), 2);
    }
}
