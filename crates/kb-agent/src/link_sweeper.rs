//! Post-run wikilink reporter for the Knowledge Builder agent.
//!
//! After the agent finishes in *apply* mode, walk every file it
//! created or modified, count any `[[wikilink]]` that resolves to no
//! existing note, and surface the count in result metadata.  Until
//! 2026-05-26 this module *also rewrote* unresolved links to plain
//! text (`Target [possible linkout - elaboration needed]`).  That
//! rewrite turned out to be hostile to Obsidian's intended use of
//! wikilinks: a `[[Target]]` whose note doesn't exist is a *feature*
//! — it appears as a stub in the graph view and one click creates
//! the note.  The agent legitimately uses these links to plant the
//! "concepts worth a future note" signal that makes the vault
//! organic.
//!
//! This module now *reports* the count of unresolved wikilinks but
//! does **not** modify the files.  A separate companion function,
//! [`relink_files`], performs the inverse rewrite — turning legacy
//! `Target [possible linkout - elaboration needed]` placeholders back
//! into `[[Target]]` form so vaults that suffered the old behaviour
//! can be repaired.  It is exposed via `kb relink`.
//!
//! # Design constraints
//!
//! * Only walks files inside `agent_root` (the agent's mutation
//!   sandbox).  User-authored notes elsewhere in the vault are out
//!   of scope.
//! * Code-fenced blocks (`` ``` ... ``` ``) and inline code
//!   (`` ` ` ``) are preserved verbatim — they may legitimately
//!   contain example wikilink syntax.
//! * Wikilinks pointing at notes the agent just created are
//!   resolved correctly: the existing-notes index is built from a
//!   fresh vault walk **after** the agent finishes.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::plan::PlanEntry;

// ── Public types ─────────────────────────────────────────────────────────────

/// One wikilink that pointed to a non-existent note and was rewritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedLink {
    /// The link target as written (everything between `[[` and either
    /// `|`, `#`, or `]]`).
    pub target: String,
    /// Display alias (`[[Target|alias]]`), if present.
    pub alias: Option<String>,
    /// Section anchor (`[[Target#Section]]`), if present.
    pub section: Option<String>,
}

impl UnresolvedLink {
    /// Render as the plain-text replacement.
    pub fn as_replacement(&self) -> String {
        let display = self.alias.as_deref().unwrap_or(&self.target);
        match &self.section {
            Some(s) => format!("{display} (§{s}) [possible linkout - elaboration needed]"),
            None    => format!("{display} [possible linkout - elaboration needed]"),
        }
    }
}

/// Aggregate stats returned to the pipeline / metadata.
///
/// Mirrors the structure of the legacy Python `SweepStats` so existing
/// downstream code (`kb show`, daemon log fields) keeps the same shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SweepStats {
    /// Files actually opened and scanned for wikilinks.
    pub files_examined: usize,
    /// Files where at least one link was rewritten.
    pub files_modified: usize,
    /// Total unresolved links replaced across all files.
    pub links_replaced: usize,
    /// Up to ~5 examples per file, used for log lines + `kb show`.
    pub examples: Vec<String>,
    // ── Diagnostic counters ──
    /// Total raw plan-derived paths handed to the sweep.
    pub files_input: usize,
    /// Skipped because outside `agent_root`.
    pub skipped_outside_root: usize,
    /// Skipped because the path didn't exist on disk (plan/disk drift —
    /// e.g. Obsidian auto-disambiguated `Foo.md` → `Foo 1.md`).
    pub skipped_not_a_file: usize,
    /// Skipped because the file isn't markdown.
    pub skipped_non_markdown: usize,
}

impl SweepStats {
    /// Render the same key/value shape the daemon log + `processor_meta`
    /// have always used, for downstream compatibility.
    pub fn as_metadata(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert("link_sweep_examined".into(),               (self.files_examined as u64).into());
        m.insert("link_sweep_modified".into(),               (self.files_modified as u64).into());
        m.insert("link_sweep_replaced".into(),               (self.links_replaced as u64).into());
        let examples: Vec<_> = self.examples.iter()
            .take(20)
            .map(|s| serde_json::Value::String(s.clone()))
            .collect();
        m.insert("link_sweep_examples".into(),               examples.into());
        m.insert("link_sweep_input".into(),                  (self.files_input as u64).into());
        m.insert("link_sweep_skipped_outside_root".into(),   (self.skipped_outside_root as u64).into());
        m.insert("link_sweep_skipped_not_a_file".into(),     (self.skipped_not_a_file as u64).into());
        m.insert("link_sweep_skipped_non_markdown".into(),   (self.skipped_non_markdown as u64).into());
        m
    }
}

// ── Regexes ──────────────────────────────────────────────────────────────────

// Compiled once.  Keep the patterns identical to the Python sweeper so
// behaviour matches byte-for-byte.

const WIKILINK_PATTERN: &str =
    r"\[\[([^\]\|#]+?)(#([^\]\|]+))?(\|([^\]]+))?\]\]";
const FENCED_CODE_PATTERN: &str =
    r"(?s)```.*?```";
const INLINE_CODE_PATTERN: &str =
    r"`[^`\n]+`";

// ── Building the existing-notes index ────────────────────────────────────────

/// Walk `vault_root` (excluding `sources_dir` and `.obsidian`/`.trash`)
/// and return the set of identifiers a wikilink might resolve to.
///
/// Three forms are added per note (matching Obsidian's resolution rules):
///   1. Basename without `.md`           (`Foo.md` → `Foo`)
///   2. Vault-relative path without `.md` (`Topics/Foo.md` → `Topics/Foo`)
///   3. Vault-relative path WITH `.md`    (`Topics/Foo.md` → `Topics/Foo.md`)
pub fn build_existing_index(
    vault_root:  &Path,
    sources_dir: &Path,
) -> HashSet<String> {
    let mut out = HashSet::new();
    let sources_canon = sources_dir.canonicalize().unwrap_or_else(|_| sources_dir.to_path_buf());

    if !vault_root.exists() {
        return out;
    }

    for entry in WalkDir::new(vault_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Skip Obsidian's metadata dirs at any depth.
            let name = e.file_name().to_string_lossy();
            !(e.file_type().is_dir() && (name == ".obsidian" || name == ".trash"))
        })
        .filter_map(|r| r.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase()) != Some("md".into()) {
            continue;
        }
        // Skip files under sources_dir (the agent must never target them).
        let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if real.starts_with(&sources_canon) {
            continue;
        }
        let rel = match path.strip_prefix(vault_root) {
            Ok(r)  => r,
            Err(_) => continue,
        };
        // Form 1: basename without .md
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            out.insert(stem.to_string());
        }
        // Forms 2 & 3: relative paths.
        let rel_str = rel.to_string_lossy();
        out.insert(rel_str.to_string());
        if let Some(stripped) = rel_str.strip_suffix(".md") {
            out.insert(stripped.to_string());
        }
    }
    out
}

/// Whether `target` resolves to an entry in `existing`.
///
/// Tolerates the rarely-emitted `[[Foo.md]]` form too.
pub fn is_resolved(target: &str, existing: &HashSet<String>) -> bool {
    if existing.contains(target) {
        return true;
    }
    if let Some(stripped) = target.strip_suffix(".md") {
        if existing.contains(stripped) {
            return true;
        }
    }
    if existing.contains(&format!("{target}.md")) {
        return true;
    }
    false
}

// ── Core: sweep one chunk of markdown ────────────────────────────────────────

/// Replace every unresolved wikilink in `content` with a placeholder.
///
/// Returns `(new_content, replaced)` where `replaced` is the list of
/// links that were rewritten (in document order).  Code-fenced and
/// inline-code spans are preserved verbatim.
pub fn sweep_links_in_text(
    content:  &str,
    existing: &HashSet<String>,
) -> (String, Vec<UnresolvedLink>) {
    // Stash code blocks behind sentinels so the wikilink regex can't
    // touch them.  Order matters: fenced before inline so that a
    // ```...``` block containing inline back-ticks isn't double-stashed.
    let fenced = Regex::new(FENCED_CODE_PATTERN).expect("FENCED_CODE_PATTERN compiles");
    let inline = Regex::new(INLINE_CODE_PATTERN).expect("INLINE_CODE_PATTERN compiles");
    let wlink  = Regex::new(WIKILINK_PATTERN).expect("WIKILINK_PATTERN compiles");

    let mut stash: Vec<String> = Vec::new();
    let stash_token = |i: usize| format!("\u{0}KBLINK\u{0}BLOCK\u{0}{i}\u{0}");

    let mut work = String::with_capacity(content.len());
    let mut last = 0usize;
    for m in fenced.find_iter(content) {
        work.push_str(&content[last..m.start()]);
        stash.push(m.as_str().to_string());
        work.push_str(&stash_token(stash.len() - 1));
        last = m.end();
    }
    work.push_str(&content[last..]);

    let mut work2 = String::with_capacity(work.len());
    let mut last2 = 0usize;
    for m in inline.find_iter(&work) {
        work2.push_str(&work[last2..m.start()]);
        stash.push(m.as_str().to_string());
        work2.push_str(&stash_token(stash.len() - 1));
        last2 = m.end();
    }
    work2.push_str(&work[last2..]);

    let mut replaced: Vec<UnresolvedLink> = Vec::new();
    let swept_str = wlink.replace_all(&work2, |caps: &regex::Captures<'_>| {
        let target  = caps.get(1).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
        let section = caps.get(3).map(|m| m.as_str().trim().to_string()).filter(|s| !s.is_empty());
        let alias   = caps.get(5).map(|m| m.as_str().trim().to_string()).filter(|s| !s.is_empty());

        if is_resolved(&target, existing) {
            return caps.get(0).map(|m| m.as_str().to_string()).unwrap_or_default();
        }
        let link = UnresolvedLink { target, alias, section };
        let rep = link.as_replacement();
        replaced.push(link);
        rep
    });

    // Restore code blocks.
    let mut restored = swept_str.into_owned();
    for (i, block) in stash.iter().enumerate() {
        restored = restored.replace(&stash_token(i), block);
    }
    (restored, replaced)
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Walk every file in `files`, count any unresolved wikilinks, and
/// return aggregate stats.  **Does not modify the files** — unresolved
/// wikilinks are a legitimate Obsidian convention for "concepts worth
/// a future note" and the previous rewriting behaviour was hostile to
/// the graph-view UX.  See module docs.
///
/// Files outside `agent_root` are silently skipped.
pub fn sweep_files(
    files:        impl IntoIterator<Item = PathBuf>,
    vault_root:   &Path,
    sources_dir:  &Path,
    agent_root:   &Path,
) -> SweepStats {
    let mut stats = SweepStats::default();

    let agent_root_canon = agent_root
        .canonicalize()
        .unwrap_or_else(|_| agent_root.to_path_buf());
    let existing = build_existing_index(vault_root, sources_dir);

    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for raw in files {
        stats.files_input += 1;
        let path = match raw.canonicalize() {
            Ok(p)  => p,
            Err(_) => raw.clone(),
        };
        if seen.contains(&path) {
            continue;
        }
        seen.insert(path.clone());

        if !path.is_file() {
            warn!(
                target: "kb_agent::link_sweeper",
                "plan path {} does not exist on disk \
                 (plan/disk drift; possibly Obsidian auto-disambiguation)",
                path.display(),
            );
            stats.skipped_not_a_file += 1;
            continue;
        }
        if !path.starts_with(&agent_root_canon) {
            debug!(
                target: "kb_agent::link_sweeper",
                "skipping {} (outside agent_root {})",
                path.display(), agent_root_canon.display(),
            );
            stats.skipped_outside_root += 1;
            continue;
        }
        let is_md = path.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("md"))
            .unwrap_or(false);
        if !is_md {
            stats.skipped_non_markdown += 1;
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c)  => c,
            Err(e) => {
                warn!(
                    target: "kb_agent::link_sweeper",
                    "cannot read {}: {e}", path.display(),
                );
                continue;
            }
        };
        stats.files_examined += 1;
        let unresolved = unresolved_links_in_text(&content, &existing);
        if unresolved.is_empty() {
            continue;
        }
        // Note: we DO NOT modify the file.  We only report the count.
        stats.files_modified += 0;
        stats.links_replaced += unresolved.len();
        let fname = path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        for link in unresolved.iter().take(5) {
            stats.examples.push(format!("{}: [[{}]]", fname, link.target));
        }
        info!(
            target: "kb_agent::link_sweeper",
            "{} contains {} unresolved wikilink(s) (kept as-is): {:?}",
            fname,
            unresolved.len(),
            unresolved.iter().map(|l| l.target.as_str()).collect::<Vec<_>>(),
        );
    }
    stats
}

/// Inventory unresolved wikilinks in `content` without modifying it.
///
/// This is the read-only twin of [`sweep_links_in_text`].  Used by
/// [`sweep_files`] for report-only mode.
pub fn unresolved_links_in_text(
    content:  &str,
    existing: &HashSet<String>,
) -> Vec<UnresolvedLink> {
    // Reuse sweep_links_in_text by discarding its rewritten output.
    // Code-fence preservation and alias/section parsing are identical.
    let (_, replaced) = sweep_links_in_text(content, existing);
    replaced
}

// ── Plan helpers ─────────────────────────────────────────────────────────────

/// Extract the absolute paths the plan claims to have created or
/// modified (apply-mode entries with `applied=true`).
///
/// Read-only ops (`search`, `read`, etc.) are excluded.
pub fn files_touched_by_plan(
    plan_entries: &[PlanEntry],
    vault_root:   &Path,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in plan_entries {
        if !entry.applied {
            continue;
        }
        if !entry.is_write() {
            continue;
        }
        let raw = match entry.path_arg() {
            Some(s) => s,
            None    => continue,
        };
        let p = Path::new(raw);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            vault_root.join(p)
        };
        out.push(abs);
    }
    out
}

// ── Recovery: undo the legacy "possible linkout" rewrite ───────────────

/// Aggregate stats from a [`relink_files`] run.
#[derive(Debug, Clone, Default)]
pub struct RelinkStats {
    /// Total files passed in.
    pub files_input: usize,
    /// Files actually opened and scanned.
    pub files_examined: usize,
    /// Files where at least one rewrite was applied.
    pub files_modified: usize,
    /// Total `Target [possible linkout - elaboration needed]` placeholders
    /// converted back to `[[Target]]`.
    pub links_restored: usize,
    /// First few rewrites — `"<filename>: [[Target]]"`.
    pub examples: Vec<String>,
}

/// Pattern emitted by the legacy sweeper:
///
/// * `"Foo [possible linkout - elaboration needed]"`
/// * `"alias [possible linkout - elaboration needed]"`            (`[[Foo|alias]]`)
/// * `"Foo (§Section) [possible linkout - elaboration needed]"`    (`[[Foo#Section]]`)
/// * `"alias (§Section) [possible linkout - elaboration needed]"`  (`[[Foo#Section|alias]]`)
///
/// We can recover the wikilink form for the simple case (no alias, no
/// section) deterministically.  For aliased / sectioned forms we make
/// the most useful guess: `[[alias]]`, `[[target#section]]`,
/// `[[target#section|alias]]`.  Note we can't recover the *original*
/// target when only an alias survived — the legacy sweeper threw it
/// away.  Best we can do is wikilink the alias text directly, which
/// at least restores the click-to-create affordance.
const SUFFIX: &str = " [possible linkout - elaboration needed]";
#[allow(dead_code)] // referenced symbolically in the regex below.
const _: &str = SUFFIX;

/// Walk every file in `files` (markdown only, inside `agent_root` per
/// the same containment rules as [`sweep_files`]) and rewrite legacy
/// `Target [possible linkout - elaboration needed]` placeholders back
/// into wikilink form `[[Target]]`.
///
/// **Idempotent**: running it twice on the same file is a no-op the
/// second time (the placeholder pattern no longer matches).
///
/// Code-fenced and inline-code spans are preserved verbatim — if a
/// user ever wrote the literal placeholder string inside ``` `code` ```
/// for documentation purposes, we leave it alone.
///
/// Returns aggregate [`RelinkStats`].  Files outside `agent_root` or
/// non-markdown files are silently skipped.
pub fn relink_files(
    files:      impl IntoIterator<Item = PathBuf>,
    agent_root: &Path,
    dry_run:    bool,
) -> RelinkStats {
    let mut stats = RelinkStats::default();
    let agent_root_canon = agent_root
        .canonicalize()
        .unwrap_or_else(|_| agent_root.to_path_buf());

    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for raw in files {
        stats.files_input += 1;
        let path = match raw.canonicalize() {
            Ok(p)  => p,
            Err(_) => raw.clone(),
        };
        if seen.contains(&path) { continue; }
        seen.insert(path.clone());

        if !path.is_file() { continue; }
        if !path.starts_with(&agent_root_canon) { continue; }
        let is_md = path.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("md"))
            .unwrap_or(false);
        if !is_md { continue; }

        let content = match std::fs::read_to_string(&path) {
            Ok(c)  => c,
            Err(e) => {
                warn!(
                    target: "kb_agent::link_sweeper",
                    "relink: cannot read {}: {e}", path.display(),
                );
                continue;
            }
        };
        stats.files_examined += 1;

        let (new_content, restored) = relink_text(&content);
        if restored.is_empty() {
            continue;
        }
        let fname = path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        for r in restored.iter().take(5) {
            stats.examples.push(format!("{fname}: [[{r}]]"));
        }
        stats.links_restored += restored.len();
        if dry_run {
            info!(
                target: "kb_agent::link_sweeper",
                "relink (DRY-RUN): would restore {} link(s) in {}",
                restored.len(), fname,
            );
            continue;
        }
        match std::fs::write(&path, &new_content) {
            Ok(()) => {
                stats.files_modified += 1;
                info!(
                    target: "kb_agent::link_sweeper",
                    "relink: restored {} link(s) in {}: {:?}",
                    restored.len(), fname, restored.iter().take(5).collect::<Vec<_>>(),
                );
            }
            Err(e) => {
                warn!(
                    target: "kb_agent::link_sweeper",
                    "relink: cannot write {}: {e}", path.display(),
                );
            }
        }
    }
    stats
}

/// Convert every `<text>[possible linkout - elaboration needed]`
/// placeholder in `content` back to `[[<text>]]` form, preserving
/// fenced code blocks and inline code verbatim.
///
/// Returns `(new_content, restored)` where `restored` is the list of
/// the inferred wikilink target strings (without brackets) in document
/// order.
pub fn relink_text(content: &str) -> (String, Vec<String>) {
    use regex::Regex;

    // Stash code so the rewrite can't touch wikilink-syntax examples.
    let fenced = Regex::new(FENCED_CODE_PATTERN).expect("FENCED_CODE_PATTERN compiles");
    let inline = Regex::new(INLINE_CODE_PATTERN).expect("INLINE_CODE_PATTERN compiles");

    let mut stash: Vec<String> = Vec::new();
    let stash_token = |i: usize| format!("\u{0}KBRELINK\u{0}BLOCK\u{0}{i}\u{0}");

    let mut work = String::with_capacity(content.len());
    let mut last = 0usize;
    for m in fenced.find_iter(content) {
        work.push_str(&content[last..m.start()]);
        stash.push(m.as_str().to_string());
        work.push_str(&stash_token(stash.len() - 1));
        last = m.end();
    }
    work.push_str(&content[last..]);

    let mut work2 = String::with_capacity(work.len());
    let mut last2 = 0usize;
    for m in inline.find_iter(&work) {
        work2.push_str(&work[last2..m.start()]);
        stash.push(m.as_str().to_string());
        work2.push_str(&stash_token(stash.len() - 1));
        last2 = m.end();
    }
    work2.push_str(&work[last2..]);

    // Match any `[possible linkout - elaboration needed]` placeholder
    // that consumes a complete line (with optional leading list /
    // blockquote markers).  This is the *only* shape the legacy
    // Python sweeper actually produced in the wild — the agent always
    // dropped its wikilink stubs as bullet items.
    //
    //  Group 1: leading whitespace + list/blockquote markers (preserved)
    //  Group 2: target text
    //  Group 3 (optional): section name (between `(§` and `)`)
    //
    // Inline mid-prose placeholders like `"Body Foo [possible …]"`
    // are deliberately NOT matched: there is no robust boundary that
    // identifies where "Foo" starts versus where the surrounding
    // prose ends, so any heuristic risks corrupting user text.  In
    // the wild we have not observed the sweeper producing those, so
    // leaving them untouched is correct.
    let pattern = Regex::new(
        // Leading group MUST be non-empty: in the wild the legacy
        // sweeper produced bullet items (`- Target [...]`).  Requiring
        // a non-empty leading whitespace / list marker means we never
        // mistake a prose paragraph (`Sentence Foo [...].`) for a
        // sweep output and accidentally rewrite "Sentence Foo" as a
        // wikilink.
        r"(?m)^([\s\-*>]+)([^\n\[\]]+?)(?: \(§([^)\n]+)\))? \[possible linkout - elaboration needed\]",
    ).expect("relink pattern compiles");

    let mut restored: Vec<String> = Vec::new();
    let swept = pattern.replace_all(&work2, |caps: &regex::Captures<'_>| {
        let leading = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let display = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
        let section = caps.get(3).map(|m| m.as_str().trim().to_string());
        if display.is_empty() {
            // Defensive: don't produce an empty `[[]]`; preserve the
            // original (matched) text by re-emitting it.
            return caps.get(0).map(|m| m.as_str().to_string()).unwrap_or_default();
        }
        let target = match &section {
            Some(s) if !s.is_empty() => format!("{display}#{s}"),
            _                        => display.to_string(),
        };
        restored.push(target.clone());
        format!("{leading}[[{target}]]")
    });

    // Restore code blocks.
    let mut out = swept.into_owned();
    for (i, block) in stash.iter().enumerate() {
        out = out.replace(&stash_token(i), block);
    }
    (out, restored)
}
// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanEntry;
    use std::fs;
    use tempfile::TempDir;

    fn entry(cmd: &str, args: &[&str], applied: bool) -> PlanEntry {
        PlanEntry {
            ts: 0, mode: "apply".into(), cmd: cmd.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            applied, exit_code: if applied { Some(0) } else { None },
        }
    }

    // ── helpers ──
    struct V {
        _tmp:    TempDir,
        vault:   PathBuf,
        sources: PathBuf,
        kb:      PathBuf,
    }
    fn setup() -> V {
        let tmp = TempDir::new().unwrap();
        let vault   = tmp.path().to_path_buf();
        let sources = vault.join("Sources");
        let kb      = vault.join("KnowledgeBase");
        fs::create_dir_all(&sources).unwrap();
        fs::create_dir_all(&kb).unwrap();
        V { _tmp: tmp, vault, sources, kb }
    }
    fn write(path: &Path, s: &str) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, s).unwrap();
    }

    // ── helper tests ──

    #[test]
    fn unresolved_link_replacement_renders() {
        let bare = UnresolvedLink {
            target: "Foo".into(), alias: None, section: None,
        };
        assert_eq!(bare.as_replacement(), "Foo [possible linkout - elaboration needed]");
        let with_alias = UnresolvedLink {
            target: "Foo".into(), alias: Some("bar".into()), section: None,
        };
        assert_eq!(with_alias.as_replacement(), "bar [possible linkout - elaboration needed]");
        let with_section = UnresolvedLink {
            target: "Foo".into(), alias: None, section: Some("Intro".into()),
        };
        assert_eq!(with_section.as_replacement(), "Foo (§Intro) [possible linkout - elaboration needed]");
    }

    #[test]
    fn is_resolved_basename() {
        let mut existing = HashSet::new();
        existing.insert("Foo".into());
        assert!(is_resolved("Foo", &existing));
    }

    #[test]
    fn is_resolved_with_md_suffix() {
        let mut existing = HashSet::new();
        existing.insert("Foo".into());
        assert!(is_resolved("Foo.md", &existing));
    }

    #[test]
    fn is_resolved_implicit_md() {
        let mut existing = HashSet::new();
        existing.insert("Foo.md".into());
        assert!(is_resolved("Foo", &existing));
    }

    #[test]
    fn is_resolved_path_form() {
        let mut existing = HashSet::new();
        existing.insert("Topics/Foo".into());
        assert!(is_resolved("Topics/Foo", &existing));
    }

    // ── sweep_links_in_text ──

    #[test]
    fn replaces_unresolved_writes_back() {
        let content = "See [[Missing]] for details.\nAnd [[Other]] too.";
        let existing = HashSet::new();
        let (out, replaced) = sweep_links_in_text(content, &existing);
        assert_eq!(replaced.len(), 2);
        assert!(out.contains("Missing [possible linkout - elaboration needed]"));
        assert!(out.contains("Other [possible linkout - elaboration needed]"));
    }

    #[test]
    fn keeps_resolved_link_to_vault_note() {
        let mut existing = HashSet::new();
        existing.insert("Real".into());
        let (out, replaced) = sweep_links_in_text("link [[Real]] kept", &existing);
        assert!(out.contains("[[Real]]"));
        assert!(replaced.is_empty());
    }

    #[test]
    fn fenced_code_blocks_preserved() {
        let content = "```\n[[InsideFence]]\n```\nbody [[Out]] here";
        let existing = HashSet::new();
        let (out, replaced) = sweep_links_in_text(content, &existing);
        assert!(out.contains("[[InsideFence]]"), "fenced wikilink must remain: {out}");
        assert_eq!(replaced.len(), 1, "only `[[Out]]` should be rewritten");
        assert_eq!(replaced[0].target, "Out");
    }

    #[test]
    fn inline_code_preserved() {
        let content = "use `[[InsideInline]]` for syntax; [[Out]] elsewhere";
        let existing = HashSet::new();
        let (out, replaced) = sweep_links_in_text(content, &existing);
        assert!(out.contains("`[[InsideInline]]`"));
        assert_eq!(replaced.len(), 1);
        assert_eq!(replaced[0].target, "Out");
    }

    #[test]
    fn alias_and_section_extracted() {
        let content = "[[Target|alias]] and [[Target#Section]] and [[Target#Section|alias2]]";
        let existing = HashSet::new();
        let (_, replaced) = sweep_links_in_text(content, &existing);
        assert_eq!(replaced.len(), 3);
        assert_eq!(replaced[0].alias.as_deref(), Some("alias"));
        assert_eq!(replaced[1].section.as_deref(), Some("Section"));
        assert_eq!(replaced[2].alias.as_deref(), Some("alias2"));
        assert_eq!(replaced[2].section.as_deref(), Some("Section"));
    }

    // ── build_existing_index ──

    #[test]
    fn index_includes_basename_and_paths() {
        let v = setup();
        write(&v.kb.join("Foo.md"),  "x");
        write(&v.kb.join("Topics").join("Bar.md"), "x");
        let idx = build_existing_index(&v.vault, &v.sources);
        assert!(idx.contains("Foo"));
        assert!(idx.contains("KnowledgeBase/Foo"));
        assert!(idx.contains("KnowledgeBase/Foo.md"));
        assert!(idx.contains("Bar"));
        assert!(idx.contains("KnowledgeBase/Topics/Bar"));
    }

    #[test]
    fn index_excludes_files_under_sources_dir() {
        let v = setup();
        write(&v.sources.join("Should-Not-Index.md"), "x");
        write(&v.kb.join("Should-Index.md"), "x");
        let idx = build_existing_index(&v.vault, &v.sources);
        assert!(idx.contains("Should-Index"));
        assert!(!idx.contains("Should-Not-Index"));
    }

    #[test]
    fn index_excludes_obsidian_metadata() {
        let v = setup();
        write(&v.vault.join(".obsidian").join("workspace.md"), "x");
        write(&v.kb.join("Real.md"), "x");
        let idx = build_existing_index(&v.vault, &v.sources);
        assert!(idx.contains("Real"));
        assert!(!idx.contains("workspace"));
    }

    // ── sweep_files ──

    #[test]
    fn skips_files_outside_agent_root() {
        let v = setup();
        let outside = v.vault.join("Outside.md");
        write(&outside, "[[Missing]]");
        let stats = sweep_files(vec![outside.clone()], &v.vault, &v.sources, &v.kb);
        assert_eq!(stats.skipped_outside_root, 1);
        assert_eq!(stats.files_examined, 0);
        assert_eq!(stats.links_replaced, 0);
        // File on disk untouched.
        assert_eq!(fs::read_to_string(&outside).unwrap(), "[[Missing]]");
    }

    #[test]
    fn reports_unresolved_inside_agent_root_without_modifying_file() {
        // After 2026-05-26, sweep_files is report-only.  The file
        // content must remain untouched even when wikilinks are
        // unresolved — unresolved wikilinks are a feature in Obsidian.
        let v = setup();
        let target = v.kb.join("New.md");
        write(&target, "Body [[Missing]] body.");
        let before = fs::read_to_string(&target).unwrap();
        let stats = sweep_files(vec![target.clone()], &v.vault, &v.sources, &v.kb);
        assert_eq!(stats.files_examined,  1);
        assert_eq!(stats.files_modified,  0, "sweeper must NOT modify the file");
        assert_eq!(stats.links_replaced,  1, "counted in stats only");
        let after = fs::read_to_string(&target).unwrap();
        assert_eq!(after, before, "file content must be untouched");
        assert!(after.contains("[[Missing]]"));
        assert!(!after.contains("possible linkout"),
            "sweep_files no longer rewrites unresolved links");
    }

    #[test]
    fn relink_text_restores_simple_target_in_list() {
        let input  = "- Foo [possible linkout - elaboration needed]";
        let (out, restored) = relink_text(input);
        assert_eq!(out,      "- [[Foo]]");
        assert_eq!(restored, vec!["Foo".to_string()]);
    }

    #[test]
    fn relink_text_restores_section_form_in_list() {
        let input  = "- Bar (§Intro) [possible linkout - elaboration needed]";
        let (out, restored) = relink_text(input);
        assert_eq!(out, "- [[Bar#Intro]]");
        assert_eq!(restored, vec!["Bar#Intro".to_string()]);
    }

    #[test]
    fn relink_text_preserves_complex_targets_with_dashes_and_colons() {
        let cases = &[
            ("- MEMO - Memory-Based Knowledge Injection [possible linkout - elaboration needed]",
             "- [[MEMO - Memory-Based Knowledge Injection]]",
             "MEMO - Memory-Based Knowledge Injection"),
            ("- TIES-Merging: Resolving Interference When Merging Models [possible linkout - elaboration needed]",
             "- [[TIES-Merging: Resolving Interference When Merging Models]]",
             "TIES-Merging: Resolving Interference When Merging Models"),
            ("- Retrieval-Augmented Generation [possible linkout - elaboration needed]",
             "- [[Retrieval-Augmented Generation]]",
             "Retrieval-Augmented Generation"),
        ];
        for (input, expected_out, expected_target) in cases {
            let (out, restored) = relink_text(input);
            assert_eq!(&out, expected_out, "input: {input:?}");
            assert_eq!(restored, vec![expected_target.to_string()]);
        }
    }

    #[test]
    fn relink_text_handles_multiple_sequential_list_items() {
        let input = "- A [possible linkout - elaboration needed]\n\
                     - B [possible linkout - elaboration needed]\n\
                     - C (§sec) [possible linkout - elaboration needed]";
        let (out, restored) = relink_text(input);
        assert_eq!(out, "- [[A]]\n- [[B]]\n- [[C#sec]]");
        assert_eq!(restored, vec!["A".to_string(), "B".to_string(), "C#sec".to_string()]);
    }

    #[test]
    fn relink_text_preserves_fenced_code() {
        let input = "```\n- Looks like Foo [possible linkout - elaboration needed]\n```\n- Real Bar [possible linkout - elaboration needed]";
        let (out, restored) = relink_text(input);
        assert!(out.contains("```\n- Looks like Foo [possible linkout - elaboration needed]\n```"),
            "fenced block preserved verbatim, got: {out}");
        assert!(out.contains("- [[Real Bar]]"));
        assert_eq!(restored, vec!["Real Bar".to_string()]);
    }

    #[test]
    fn relink_text_preserves_trailing_prose_after_placeholder() {
        // Real-world shape from the Transformer paper note: the legacy
        // sweeper rewrote `[[Foo]]` even when the line continued with
        // descriptive text after the wikilink.  Recovery must preserve
        // that trailing prose.
        let input = "- Machine Translation [possible linkout - elaboration needed] — primary application";
        let (out, restored) = relink_text(input);
        assert_eq!(out, "- [[Machine Translation]] — primary application");
        assert_eq!(restored, vec!["Machine Translation".to_string()]);
    }

    #[test]
    fn relink_text_idempotent() {
        let input = "- Foo [possible linkout - elaboration needed]";
        let (once, _) = relink_text(input);
        let (twice, restored2) = relink_text(&once);
        assert_eq!(once, twice, "second run must be no-op");
        assert!(restored2.is_empty());
    }

    #[test]
    fn relink_text_does_not_touch_inline_mid_prose_occurrences() {
        // We deliberately don't recover inline mid-prose placeholders
        // because there's no robust boundary for the target text.
        // Documented limitation — in practice the legacy sweeper
        // produced bullet-list items, not inline rewrites.
        let input = "See Foo [possible linkout - elaboration needed].";
        let (out, restored) = relink_text(input);
        assert_eq!(out, input);
        assert!(restored.is_empty());
    }

    #[test]
    fn relink_files_rewrites_in_place_inside_agent_root() {
        let v = setup();
        let f = v.kb.join("Recovery.md");
        write(&f, "Body\n- MEMO - Memory-Embedded [possible linkout - elaboration needed]\nbody.");
        let stats = relink_files(vec![f.clone()], &v.kb, /* dry_run */ false);
        assert_eq!(stats.files_input,    1);
        assert_eq!(stats.files_examined, 1);
        assert_eq!(stats.files_modified, 1);
        assert_eq!(stats.links_restored, 1);
        let after = fs::read_to_string(&f).unwrap();
        assert!(after.contains("- [[MEMO - Memory-Embedded]]"), "got: {after}");
        assert!(!after.contains("possible linkout"));
    }

    #[test]
    fn relink_files_dry_run_does_not_write() {
        let v = setup();
        let f = v.kb.join("Dry.md");
        let before = "- Foo [possible linkout - elaboration needed]";
        write(&f, before);
        let stats = relink_files(vec![f.clone()], &v.kb, /* dry_run */ true);
        assert_eq!(stats.files_modified, 0);
        assert_eq!(stats.links_restored, 1);
        let after = fs::read_to_string(&f).unwrap();
        assert_eq!(after, before, "dry-run must not touch the file");
    }

    #[test]
    fn relink_files_skips_outside_agent_root() {
        let v = setup();
        let outside = v.vault.join("NotInKB.md");
        write(&outside, "- X [possible linkout - elaboration needed]");
        let stats = relink_files(vec![outside.clone()], &v.kb, /* dry_run */ false);
        assert_eq!(stats.files_examined, 0);
        assert_eq!(stats.links_restored, 0);
        assert!(fs::read_to_string(&outside).unwrap().contains("possible linkout"));
    }

    #[test]
    fn resolves_to_files_created_in_same_run() {
        // Existing index includes any md already on disk.  The caller
        // creates files BEFORE invoking sweep_files; we just confirm
        // that's what build_existing_index does.
        let v = setup();
        let a = v.kb.join("A.md");
        let b = v.kb.join("B.md");
        write(&a, "see [[B]]");
        write(&b, "see [[A]]");
        let stats = sweep_files(
            vec![a.clone(), b.clone()],
            &v.vault, &v.sources, &v.kb,
        );
        assert_eq!(stats.files_modified, 0, "both wikilinks resolve");
        assert_eq!(stats.links_replaced, 0);
    }

    #[test]
    fn diagnostics_count_drift_to_nonexistent_path() {
        let v = setup();
        let nonexistent = v.kb.join("Renamed.md");   // never created
        let real        = v.kb.join("Real.md");
        write(&real, "hi");
        let stats = sweep_files(
            vec![nonexistent, real],
            &v.vault, &v.sources, &v.kb,
        );
        assert_eq!(stats.files_input,        2);
        assert_eq!(stats.files_examined,     1);
        assert_eq!(stats.skipped_not_a_file, 1);
        let meta = stats.as_metadata();
        assert_eq!(meta["link_sweep_skipped_not_a_file"].as_u64().unwrap(), 1);
    }

    #[test]
    fn skips_non_markdown_files() {
        let v = setup();
        let json = v.kb.join("data.json");
        write(&json, r#"{"x":"[[Y]]"}"#);
        let stats = sweep_files(vec![json.clone()], &v.vault, &v.sources, &v.kb);
        assert_eq!(stats.files_examined, 0);
        assert_eq!(stats.skipped_non_markdown, 1);
        assert!(fs::read_to_string(&json).unwrap().contains("[[Y]]"));
    }

    // ── files_touched_by_plan ──

    #[test]
    fn extracts_create_path() {
        let plan = vec![entry("create", &["path=KnowledgeBase/Foo.md", "content=hi"], true)];
        let v = setup();
        let touched = files_touched_by_plan(&plan, &v.vault);
        assert_eq!(touched, vec![v.vault.join("KnowledgeBase/Foo.md")]);
    }

    #[test]
    fn extracts_append_with_file() {
        let plan = vec![entry("append", &["file=KnowledgeBase/Foo.md", "content=more"], true)];
        let v = setup();
        let touched = files_touched_by_plan(&plan, &v.vault);
        assert_eq!(touched, vec![v.vault.join("KnowledgeBase/Foo.md")]);
    }

    #[test]
    fn skips_unapplied() {
        let plan = vec![entry("create", &["path=Foo.md"], false)];
        let v = setup();
        assert!(files_touched_by_plan(&plan, &v.vault).is_empty());
    }

    #[test]
    fn skips_property_set() {
        let plan = vec![entry("property:set", &["path=Foo.md", "year=2024"], true)];
        let v = setup();
        assert!(files_touched_by_plan(&plan, &v.vault).is_empty());
    }

    #[test]
    fn skips_read_commands() {
        let plan = vec![entry("search", &["query=foo"], true)];
        let v = setup();
        assert!(files_touched_by_plan(&plan, &v.vault).is_empty());
    }
}
