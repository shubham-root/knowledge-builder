//! Plan reader/writer for the Knowledge Builder agent.
//!
//! The `kb-obsidian` wrapper appends one JSON object per intercepted
//! mutation to a JSONL file at `$KB_PLAN_FILE`.  This module is the
//! reader/writer for that protocol.  The wire format is **bit-identical
//! to the legacy Python implementation** so plan files written by an
//! older Python wrapper remain readable, and vice versa during the
//! migration window.
//!
//! # JSONL schema (one object per line)
//!
//! ```json
//! {
//!     "ts":         <int>,           // unix epoch seconds
//!     "mode":       "shadow" | "apply",
//!     "cmd":        "<obsidian subcommand, e.g. \"create\">",
//!     "args":       ["<key=value tokens as the agent passed them>"],
//!     "applied":    true,            // true only in apply mode after passthrough
//!     "exit_code":  0                // present only when applied=true (or false
//!                                     // after passthrough failed)
//! }
//! ```

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ── Public types ─────────────────────────────────────────────────────────────

/// One staged mutation parsed from a plan file.
///
/// `args` is kept as `Vec<String>` (rather than a parsed key-value map)
/// so the original argv is preserved for downstream tooling that may
/// want to replay it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    /// Unix epoch seconds when the wrapper accepted the command.
    pub ts: i64,
    /// `"shadow"` or `"apply"`.  Validated on parse.
    pub mode: String,
    /// Obsidian subcommand, e.g. `"create"`, `"property:set"`.
    pub cmd: String,
    /// `key=value` tokens passed to the wrapper.
    pub args: Vec<String>,
    /// `true` only in apply mode after the real obsidian binary
    /// returned `rc == 0`.
    pub applied: bool,
    /// Obsidian's exit code.  Absent (`None`) for shadow-mode entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

impl PlanEntry {
    /// Coarse category used by `kb show` and the link sweeper to group
    /// mutations.
    pub fn kind(&self) -> &'static str {
        match self.cmd.as_str() {
            "create" => "create",
            "append" | "prepend" | "daily:append" | "daily:prepend" => "append",
            "property:set" => "property_set",
            "property:remove" => "property_remove",
            "move" | "rename" => "rename",
            "delete" => "delete",
            "bookmark" => "bookmark",
            c if c.starts_with("base:") => "base",
            _ => "other",
        }
    }

    /// `true` for commands that wrote new content to disk and therefore
    /// produce a file the link sweeper should examine.
    pub fn is_write(&self) -> bool {
        matches!(self.cmd.as_str(), "create" | "append" | "prepend")
    }

    /// Extract the `path=` or `file=` argument value, if any.  Used by
    /// the link sweeper's plan-derived path collector.  Returns `None`
    /// for commands that don't carry a path-like argument.
    pub fn path_arg(&self) -> Option<&str> {
        for tok in &self.args {
            let bytes = tok.as_bytes();
            if let Some(eq) = bytes.iter().position(|&b| b == b'=') {
                if eq == 0 {
                    continue;
                }
                let key = &tok[..eq];
                let val = &tok[eq + 1..];
                if matches!(key, "path" | "file") {
                    return Some(val);
                }
            }
        }
        None
    }
}

/// The complete set of mutations from one agent run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    /// File the entries were read from (or will be written to).  Used
    /// for diagnostics; not part of the wire format.
    pub path: PathBuf,
    /// All plan entries, in document order.
    pub entries: Vec<PlanEntry>,
}

impl Plan {
    /// Number of entries.  `Plan::is_empty` checks if zero.
    pub fn len(&self) -> usize { self.entries.len() }

    /// `true` if the plan has no entries.
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    /// Group entries by [`PlanEntry::kind`].  Keys are the static
    /// strings returned by `kind()`.
    pub fn by_kind(&self) -> std::collections::BTreeMap<&'static str, Vec<&PlanEntry>> {
        let mut out = std::collections::BTreeMap::<&'static str, Vec<&PlanEntry>>::new();
        for e in &self.entries {
            out.entry(e.kind()).or_default().push(e);
        }
        out
    }

    /// Human-readable one-line summary used by the daemon log + `kb show`.
    ///
    /// ```text
    /// plan(7 entries; applied=6): create=4, property_set=2, append=1
    /// ```
    pub fn summary(&self) -> String {
        if self.entries.is_empty() {
            return "(empty plan — agent proposed no mutations)".into();
        }
        let bk = self.by_kind();
        let parts: Vec<String> = bk
            .iter()
            .map(|(k, items)| format!("{k}={}", items.len()))
            .collect();
        let applied = self.entries.iter().filter(|e| e.applied).count();
        format!(
            "plan({} entries; applied={}): {}",
            self.entries.len(),
            applied,
            parts.join(", "),
        )
    }
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// Error raised by [`read_plan`] or [`iter_plan`].
#[derive(Debug, thiserror::Error)]
pub enum PlanParseError {
    /// The plan file or one of its lines could not be read.
    #[error("io error reading plan {path:?}: {source}")]
    Io {
        /// Path that produced the error.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A line was not valid UTF-8 JSON.
    #[error("{path:?}:{line_no}: invalid JSON: {source}")]
    InvalidJson {
        /// Path that produced the error.
        path: PathBuf,
        /// 1-indexed line number.
        line_no: usize,
        /// Underlying parse error.
        #[source]
        source: serde_json::Error,
    },

    /// A line parsed but didn't satisfy the schema.
    #[error("{path:?}:{line_no}: {detail}")]
    Schema {
        /// Path that produced the error.
        path: PathBuf,
        /// 1-indexed line number.
        line_no: usize,
        /// What's wrong with the entry.
        detail: String,
    },
}

// ── Parsing ──────────────────────────────────────────────────────────────────

/// Read every entry from `path`.
///
/// Returns an empty [`Plan`] if the file does not exist (the common case
/// when the agent finishes without proposing any mutations).
///
/// Blank lines are silently skipped.  Any malformed line aborts the
/// whole parse with a [`PlanParseError`] — we don't return partial plans
/// on purpose, because a corrupt JSONL line typically indicates a
/// wrapper bug or filesystem truncation that the operator must
/// investigate.
pub fn read_plan(path: &Path) -> Result<Plan, PlanParseError> {
    let mut plan = Plan { path: path.to_path_buf(), entries: Vec::new() };
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(plan),
        Err(e) => return Err(PlanParseError::Io { path: path.into(), source: e }),
    };
    for (line_no, line_res) in BufReader::new(f).lines().enumerate() {
        let line = line_res.map_err(|e| PlanParseError::Io {
            path: path.into(), source: e,
        })?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry = parse_line(trimmed, path, line_no + 1)?;
        plan.entries.push(entry);
    }
    Ok(plan)
}

/// Streaming variant of [`read_plan`].
///
/// Yields entries one at a time without holding the whole file in memory.
pub fn iter_plan(
    path: &Path,
) -> Result<impl Iterator<Item = Result<PlanEntry, PlanParseError>>, PlanParseError> {
    let owned_path = path.to_path_buf();
    let f = match File::open(path) {
        Ok(f) => Some(f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(PlanParseError::Io { path: path.into(), source: e }),
    };
    Ok(PlanIter {
        inner:    f.map(|f| BufReader::new(f).lines().enumerate()),
        path:     owned_path,
    })
}

struct PlanIter<I> {
    inner: Option<I>,
    path:  PathBuf,
}

impl<I> Iterator for PlanIter<I>
where
    I: Iterator<Item = (usize, std::io::Result<String>)>,
{
    type Item = Result<PlanEntry, PlanParseError>;
    fn next(&mut self) -> Option<Self::Item> {
        let inner = self.inner.as_mut()?;
        loop {
            let (idx, line_res) = inner.next()?;
            match line_res {
                Err(e) => {
                    return Some(Err(PlanParseError::Io {
                        path: self.path.clone(),
                        source: e,
                    }));
                }
                Ok(line) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    return Some(parse_line(trimmed, &self.path, idx + 1));
                }
            }
        }
    }
}

fn parse_line(line: &str, path: &Path, line_no: usize) -> Result<PlanEntry, PlanParseError> {
    let v: serde_json::Value = serde_json::from_str(line).map_err(|e| {
        PlanParseError::InvalidJson { path: path.into(), line_no, source: e }
    })?;
    let map = v.as_object().ok_or_else(|| PlanParseError::Schema {
        path: path.into(),
        line_no,
        detail: format!(
            "top-level value must be an object, got {}",
            value_type_name(&v),
        ),
    })?;
    for required in ["ts", "mode", "cmd", "args", "applied"] {
        if !map.contains_key(required) {
            return Err(PlanParseError::Schema {
                path: path.into(),
                line_no,
                detail: format!("plan entry missing required key {required:?}"),
            });
        }
    }
    let mode_str = map["mode"].as_str().unwrap_or_default();
    if !matches!(mode_str, "shadow" | "apply") {
        return Err(PlanParseError::Schema {
            path: path.into(),
            line_no,
            detail: format!(
                "plan entry 'mode' must be 'shadow' or 'apply': got {mode_str:?}",
            ),
        });
    }
    let args_arr = map["args"].as_array().ok_or_else(|| PlanParseError::Schema {
        path: path.into(),
        line_no,
        detail: "plan entry 'args' must be an array of strings".into(),
    })?;
    let mut args: Vec<String> = Vec::with_capacity(args_arr.len());
    for a in args_arr {
        match a.as_str() {
            Some(s) => args.push(s.to_string()),
            None => return Err(PlanParseError::Schema {
                path: path.into(),
                line_no,
                detail: "plan entry 'args' must contain only strings".into(),
            }),
        }
    }
    let exit_code = match map.get("exit_code") {
        Some(serde_json::Value::Null) | None => None,
        Some(v) => v.as_i64().map(|n| n as i32),
    };
    Ok(PlanEntry {
        ts:        map["ts"].as_i64().unwrap_or_default(),
        mode:      mode_str.to_string(),
        cmd:       map["cmd"].as_str().unwrap_or_default().to_string(),
        args,
        applied:   map["applied"].as_bool().unwrap_or_default(),
        exit_code,
    })
}

fn value_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null    => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

// ── Writing ──────────────────────────────────────────────────────────────────

/// Append one entry to a plan file in JSONL format.
///
/// The wrapper uses this on every accepted command.  The line is
/// flushed before this function returns so concurrent readers always
/// see a complete record.
///
/// **Concurrency safety.** The agent's `bash` tool can issue several
/// `kb-obsidian` invocations in parallel within a single turn (and on
/// Unix the underlying `obsidian` CLI may queue them).  Two writers
/// landing in `append_entry` at the same time previously interleaved
/// their `<json>` and `\n` syscalls, producing a corrupt JSONL file
/// (`{...}{...}\n\n` instead of two well-formed lines).  We now:
///
/// 1. build the full `<json>\n` payload in memory before opening the
///    file, so we issue exactly one `write_all`, and
/// 2. take an exclusive advisory `flock(2)` on the file descriptor
///    while writing.  POSIX guarantees a single `write(2)` to a file
///    opened with `O_APPEND` is atomic relative to other appenders;
///    the lock layered on top makes this hold even if `write_all`
///    chunks the syscall.
///
/// Keys are sorted alphabetically (`applied`, `args`, `cmd`, …) so the
/// wire format matches the Python wrapper's `json.dumps(..., sort_keys=True)`
/// byte-for-byte.
pub fn append_entry(plan_file: &Path, entry: &PlanEntry) -> std::io::Result<()> {
    if let Some(parent) = plan_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Serialise via a BTreeMap to guarantee sorted keys (serde's
    // derived Serialize for a struct preserves declaration order, which
    // would not match Python's sort_keys=True).
    let mut map = std::collections::BTreeMap::<&'static str, serde_json::Value>::new();
    map.insert("applied", serde_json::Value::Bool(entry.applied));
    map.insert("args",    serde_json::Value::Array(
        entry.args.iter().map(|s| serde_json::Value::String(s.clone())).collect(),
    ));
    map.insert("cmd",     serde_json::Value::String(entry.cmd.clone()));
    if let Some(rc) = entry.exit_code {
        map.insert("exit_code", serde_json::Value::Number(rc.into()));
    }
    map.insert("mode",    serde_json::Value::String(entry.mode.clone()));
    map.insert("ts",      serde_json::Value::Number(entry.ts.into()));
    let line = serde_json::to_string(&map)
        .expect("BTreeMap<&str, Value> always serialises");
    // Build full payload (`<json>\n`) up front so a single write_all
    // covers the record (1) regardless of buffer size.
    let mut payload = Vec::with_capacity(line.len() + 1);
    payload.extend_from_slice(line.as_bytes());
    payload.push(b'\n');

    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(plan_file)?;

    // Exclusive advisory lock; auto-released on `f`'s drop.
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = f.as_raw_fd();
        // SAFETY: `fd` is valid for the lifetime of `f`; libc::flock is
        // a thin syscall wrapper.
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    f.write_all(&payload)?;
    f.flush()?;
    // Lock released when `f` falls out of scope (close releases flock).
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(cmd: &str, args: &[&str], applied: bool) -> PlanEntry {
        PlanEntry {
            ts: 1_700_000_000,
            mode: "apply".into(),
            cmd: cmd.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            applied,
            exit_code: if applied { Some(0) } else { None },
        }
    }

    #[test]
    fn missing_file_returns_empty_plan() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("missing.jsonl");
        let plan = read_plan(&p).unwrap();
        assert!(plan.is_empty());
        assert_eq!(plan.summary(), "(empty plan — agent proposed no mutations)");
    }

    #[test]
    fn parses_well_formed_entries() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("plan.jsonl");
        std::fs::write(
            &p,
            r#"{"ts":1,"mode":"apply","cmd":"create","args":["path=KnowledgeBase/Foo.md","content=hi"],"applied":true,"exit_code":0}
{"ts":2,"mode":"apply","cmd":"property:set","args":["path=KnowledgeBase/Foo.md","year=2024"],"applied":true,"exit_code":0}
"#,
        ).unwrap();
        let plan = read_plan(&p).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan.entries[0].cmd, "create");
        assert!(plan.entries[0].applied);
        assert_eq!(plan.entries[0].exit_code, Some(0));
        assert_eq!(plan.entries[1].kind(), "property_set");
    }

    #[test]
    fn blank_lines_ignored() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("blank.jsonl");
        std::fs::write(
            &p,
            "\n\n{\"ts\":1,\"mode\":\"shadow\",\"cmd\":\"create\",\"args\":[],\"applied\":false}\n\n",
        ).unwrap();
        let plan = read_plan(&p).unwrap();
        assert_eq!(plan.len(), 1);
    }

    #[test]
    fn missing_required_key_raises() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bad.jsonl");
        std::fs::write(&p, r#"{"ts":1,"cmd":"create","args":[],"applied":false}"#).unwrap();
        let err = read_plan(&p).unwrap_err();
        match err {
            PlanParseError::Schema { detail, .. } => {
                assert!(detail.contains("missing required key"));
                assert!(detail.contains("\"mode\""));
            }
            other => panic!("expected Schema, got {other:?}"),
        }
    }

    #[test]
    fn invalid_json_raises() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bad.jsonl");
        std::fs::write(&p, "{not json\n").unwrap();
        let err = read_plan(&p).unwrap_err();
        assert!(matches!(err, PlanParseError::InvalidJson { .. }));
    }

    #[test]
    fn top_level_array_rejected() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bad.jsonl");
        std::fs::write(&p, "[1,2,3]\n").unwrap();
        let err = read_plan(&p).unwrap_err();
        match err {
            PlanParseError::Schema { detail, .. } => {
                assert!(detail.contains("top-level value must be an object"));
            }
            other => panic!("expected Schema, got {other:?}"),
        }
    }

    #[test]
    fn invalid_mode_rejected() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("bad.jsonl");
        std::fs::write(
            &p,
            r#"{"ts":1,"mode":"weird","cmd":"create","args":[],"applied":false}"#,
        ).unwrap();
        let err = read_plan(&p).unwrap_err();
        match err {
            PlanParseError::Schema { detail, .. } => {
                assert!(detail.contains("'shadow' or 'apply'"));
            }
            other => panic!("expected Schema, got {other:?}"),
        }
    }

    #[test]
    fn kind_categorisation() {
        let cases: &[(&str, &str)] = &[
            ("create",          "create"),
            ("append",          "append"),
            ("prepend",         "append"),
            ("daily:append",    "append"),
            ("daily:prepend",   "append"),
            ("property:set",    "property_set"),
            ("property:remove", "property_remove"),
            ("move",            "rename"),
            ("rename",          "rename"),
            ("delete",          "delete"),
            ("bookmark",        "bookmark"),
            ("base:create",     "base"),
            ("search",          "other"),
        ];
        for (cmd, expected) in cases {
            let e = entry(cmd, &[], true);
            assert_eq!(e.kind(), *expected, "cmd={cmd}");
        }
    }

    #[test]
    fn append_then_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("rt.jsonl");
        let e1 = entry("create",       &["path=KnowledgeBase/A.md", "content=hello"], true);
        let e2 = entry("property:set", &["path=KnowledgeBase/A.md", "year=2024"],    true);
        append_entry(&p, &e1).unwrap();
        append_entry(&p, &e2).unwrap();
        let plan = read_plan(&p).unwrap();
        assert_eq!(plan.entries, vec![e1, e2]);
    }

    /// Regression test: when several `kb-obsidian` invocations land in
    /// `append_entry` concurrently (the agent's bash tool can issue
    /// commands in parallel within a turn), the JSONL file used to end
    /// up with two records concatenated on the same line, e.g.
    /// `{...}{...}\n` instead of `{...}\n{...}\n`.  We now combine the
    /// payload + `\n` into a single `write_all` and take an `flock` on
    /// the file descriptor; this test fires N threads that each append
    /// a sizeable record (8 KiB content field) and asserts every line
    /// of the resulting JSONL parses cleanly.
    #[test]
    fn concurrent_appends_produce_well_formed_jsonl() {
        use std::sync::Arc;
        use std::thread;

        let tmp = TempDir::new().unwrap();
        let p = Arc::new(tmp.path().join("concurrent.jsonl"));

        // Each writer hammers append_entry 50 times — with 8 threads
        // that's 400 records, plenty to surface interleaving.
        const THREADS: usize = 8;
        const PER_THREAD: usize = 50;
        let mut handles = Vec::new();
        for tid in 0..THREADS {
            let path = Arc::clone(&p);
            handles.push(thread::spawn(move || {
                // Big content payload makes a torn write more likely.
                let big = "x".repeat(8 * 1024);
                for i in 0..PER_THREAD {
                    let e = PlanEntry {
                        ts: (tid * PER_THREAD + i) as i64,
                        mode: "apply".into(),
                        cmd:  "create".into(),
                        args: vec![
                            format!("path=KnowledgeBase/T{tid}-{i}.md"),
                            format!("content={big}"),
                        ],
                        applied: true,
                        exit_code: Some(0),
                    };
                    append_entry(&path, &e).unwrap();
                }
            }));
        }
        for h in handles { h.join().unwrap(); }

        // Every line must parse.  Total count must equal threads * per_thread.
        let plan = read_plan(&p).expect("plan must parse cleanly after concurrent writes");
        assert_eq!(plan.entries.len(), THREADS * PER_THREAD,
            "expected {} entries, got {}",
            THREADS * PER_THREAD, plan.entries.len(),
        );
        // Sanity: no record was truncated mid-content.
        for entry in &plan.entries {
            let content = entry.args.iter()
                .find(|a| a.starts_with("content="))
                .expect("every entry has a content arg");
            let body = &content["content=".len()..];
            assert_eq!(body.len(), 8 * 1024, "truncated content: {} bytes", body.len());
        }
    }

    #[test]
    fn append_writes_sorted_keys_matching_python_format() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("sorted.jsonl");
        let e = entry("create", &["path=KB/A.md"], true);
        append_entry(&p, &e).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        // Keys must appear in alphabetical order (matches sort_keys=True).
        let order = ["applied", "args", "cmd", "exit_code", "mode", "ts"];
        let mut last_idx = 0usize;
        for k in order {
            let needle = format!("\"{k}\"");
            let idx = raw.find(&needle).unwrap_or_else(|| panic!(
                "key {k} not found in: {raw}",
            ));
            assert!(idx >= last_idx, "key {k} appeared out of sorted order: {raw}");
            last_idx = idx;
        }
    }

    #[test]
    fn path_arg_extraction() {
        assert_eq!(
            entry("create", &["path=KB/A.md", "content=x"], true).path_arg(),
            Some("KB/A.md"),
        );
        assert_eq!(
            entry("append", &["file=KB/B.md"], true).path_arg(),
            Some("KB/B.md"),
        );
        assert_eq!(
            entry("property:set", &["year=2024"], true).path_arg(),
            None,
        );
        assert_eq!(
            entry("create", &["==broken"], true).path_arg(),
            None,
            "tokens that start with '=' must not crash extraction",
        );
    }

    #[test]
    fn iter_plan_streams() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("iter.jsonl");
        for i in 0..50i64 {
            append_entry(&p, &PlanEntry {
                ts: i, mode: "apply".into(), cmd: "create".into(),
                args: vec![format!("path=KB/{i}.md")],
                applied: true, exit_code: Some(0),
            }).unwrap();
        }
        let count = iter_plan(&p).unwrap()
            .map(|r| r.unwrap())
            .count();
        assert_eq!(count, 50);
    }

    #[test]
    fn summary_renders_counts_and_kinds() {
        let plan = Plan {
            path: PathBuf::from("x"),
            entries: vec![
                entry("create",       &["path=A.md"], true),
                entry("create",       &["path=B.md"], true),
                entry("property:set", &["path=A.md", "year=2024"], false),
            ],
        };
        let s = plan.summary();
        assert!(s.contains("plan(3 entries; applied=2)"), "got: {s}");
        assert!(s.contains("create=2"));
        assert!(s.contains("property_set=1"));
    }
}
