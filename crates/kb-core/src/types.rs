//! Shared domain types for Knowledge Builder.
//!
//! These are the core data structures passed between crates.
//! All types derive `Serialize`/`Deserialize` so they can be read from
//! the database (via `rusqlite`) and serialized to JSON for the HTTP API
//! and processor contract.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── Status state machine ──────────────────────────────────────────────────────

/// Lifecycle status of a source file tracked in the `files` table.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// File detected but not yet stable+hashed.
    Seen,
    /// Stable, hashed, and waiting for a worker to claim it.
    Queued,
    /// Claimed by a worker; subprocess is running.
    Processing,
    /// Processor finished successfully; outputs recorded.
    Done,
    /// Processor failed; may be retried depending on `attempts` vs `max_attempts`.
    Failed,
    /// Content-hash duplicate of an already-done file; intentionally skipped.
    Skipped,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Status::Seen       => "seen",
            Status::Queued     => "queued",
            Status::Processing => "processing",
            Status::Done       => "done",
            Status::Failed     => "failed",
            Status::Skipped    => "skipped",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for Status {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "seen"       => Ok(Status::Seen),
            "queued"     => Ok(Status::Queued),
            "processing" => Ok(Status::Processing),
            "done"       => Ok(Status::Done),
            "failed"     => Ok(Status::Failed),
            "skipped"    => Ok(Status::Skipped),
            other        => Err(anyhow::anyhow!("unknown status: {other:?}")),
        }
    }
}

// ── FileRow ───────────────────────────────────────────────────────────────────

/// A row from the `files` table; mirrors the schema exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRow {
    pub id:             i64,
    /// Canonical absolute path to the source file.
    pub path:           String,
    /// `"sha256:<hex>"` once the file has been hashed; `None` while `seen`.
    pub content_hash:   Option<String>,
    pub size:           Option<i64>,
    pub mtime_ns:       Option<i64>,
    pub inode:          Option<i64>,
    pub status:         Status,
    pub attempts:       i64,
    /// Unix epoch seconds; `None` means the job is immediately ready.
    pub next_attempt_at: Option<i64>,
    pub last_error:     Option<String>,
    pub first_seen_at:  DateTime<Utc>,
    pub updated_at:     DateTime<Utc>,
    pub processed_at:   Option<DateTime<Utc>>,
    /// Arbitrary JSON metadata returned by the processor.
    pub processor_meta: Option<serde_json::Value>,
}

// ── OutputRecord ─────────────────────────────────────────────────────────────

/// A row from the `outputs` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputRecord {
    pub id:         i64,
    pub source_id:  i64,
    /// Canonical absolute path to the produced file.
    pub path:       String,
    /// `"markdown"` | `"asset"` | any processor-defined kind.
    pub kind:       Option<String>,
    pub bytes:      Option<i64>,
    pub created_at: DateTime<Utc>,
}

// ── Processor contract types ──────────────────────────────────────────────────

/// JSON object written to the processor's `stdin`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorInput {
    pub input_path:   String,
    pub content_hash: String,
    pub vault_root:   String,
    pub sources_dir:  String,
    pub work_dir:     String,
    pub job_id:       i64,
    pub attempt:      i64,
}

/// One output path entry inside [`ProcessResult`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputEntry {
    pub path:  String,
    pub kind:  Option<String>,
    pub bytes: Option<i64>,
}

/// The last JSON line emitted on `stdout` by the processor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessResult {
    /// `"ok"` or `"error"`.
    pub status:    String,
    /// Present when `status == "ok"`.
    #[serde(default)]
    pub outputs:   Vec<OutputEntry>,
    /// Present when `status == "error"`.
    pub error:     Option<String>,
    /// `true` = retry after backoff; `false` = permanent failure.
    #[serde(default = "default_retryable")]
    pub retryable: bool,
    /// Arbitrary key/value metadata (tokens used, model name, etc.).
    pub metadata:  Option<serde_json::Value>,
}

fn default_retryable() -> bool { true }

impl ProcessResult {
    pub fn is_ok(&self) -> bool { self.status == "ok" }
}

// ── Audit event ───────────────────────────────────────────────────────────────

/// A row from the `events` audit-log table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRow {
    pub id:      i64,
    pub ts:      DateTime<Utc>,
    pub level:   String,
    pub kind:    String,
    pub file_id: Option<i64>,
    pub message: String,
    pub detail:  Option<serde_json::Value>,
}

/// Well-known event kind constants (open set; processor may emit others).
pub mod event_kind {
    pub const DISCOVERED:        &str = "discovered";
    pub const STABLE:            &str = "stable";
    pub const HASHED:            &str = "hashed";
    pub const QUEUED:            &str = "queued";
    pub const SKIPPED_DUPLICATE: &str = "skipped_duplicate";
    pub const CLAIMED:           &str = "claimed";
    pub const PROCESSOR_STARTED: &str = "processor_started";
    pub const PROCESSOR_EXIT:    &str = "processor_exit";
    pub const OUTPUT_RECORDED:   &str = "output_recorded";
    pub const DONE:              &str = "done";
    pub const FAILED:            &str = "failed";
    pub const RECOVERED:         &str = "recovered";
    pub const SCAN_STARTED:      &str = "scan_started";
    pub const SCAN_FINISHED:     &str = "scan_finished";
    pub const CONFIG_LOADED:     &str = "config_loaded";
    pub const DAEMON_STARTED:    &str = "daemon_started";
    pub const DAEMON_STOPPING:   &str = "daemon_stopping";
}

// ── Aggregate stats ───────────────────────────────────────────────────────────

/// Counts returned by the `/stats` HTTP endpoint and `kb status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stats {
    pub seen:       i64,
    pub queued:     i64,
    pub processing: i64,
    pub done:       i64,
    pub failed:     i64,
    pub skipped:    i64,
    /// Seconds since epoch of the oldest `queued` or `processing` entry.
    pub oldest_pending_secs: Option<i64>,
}
