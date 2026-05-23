//! Shared domain types for Knowledge Builder.
//!
//! These are the canonical data structures passed between every crate.  They
//! mirror the SQLite schema exactly so that DB rows can be mapped without any
//! intermediate conversion layer.
//!
//! All path fields use [`PathBuf`] rather than [`String`] so callers always
//! work with the type-system-enforced path representation.  Timestamps are
//! stored as Unix epoch seconds (`i64`) matching SQLite `INTEGER` columns.
//!
//! # SQL text mapping
//!
//! [`Status`] implements [`rusqlite::types::ToSql`] and
//! [`rusqlite::types::FromSql`] so it can be written and read from the
//! `files.status` `TEXT` column without manual conversion at each call-site.

use std::path::PathBuf;
use std::str::FromStr;
use std::fmt;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};

// ── Status ────────────────────────────────────────────────────────────────────

/// Lifecycle status of a source file in the `files` table.
///
/// The state machine is:
/// ```text
///   seen ──► queued ──► processing ──► done
///                            │
///                            └──► failed ──► (re-queued after backoff)
///   seen ──► skipped   (duplicate content hash already done)
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// File detected but not yet stable + hashed.
    Seen,
    /// Stable, hashed, and waiting for a worker to claim it.
    Queued,
    /// Claimed by a worker; processor subprocess is running.
    Processing,
    /// Processor finished successfully; outputs recorded in `outputs` table.
    Done,
    /// Processor failed; may be retried depending on `attempts` vs `max_attempts`.
    Failed,
    /// Content-hash duplicate of an already-`done` file; intentionally skipped.
    Skipped,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Status {
    /// Return the canonical lowercase string representation stored in SQLite.
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Seen       => "seen",
            Status::Queued     => "queued",
            Status::Processing => "processing",
            Status::Done       => "done",
            Status::Failed     => "failed",
            Status::Skipped    => "skipped",
        }
    }
}

impl FromStr for Status {
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

// ── rusqlite SQL text mapping for Status ──────────────────────────────────────

impl ToSql for Status {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for Status {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        Status::from_str(s).map_err(|e| FromSqlError::Other(Box::new(
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
        )))
    }
}

// ── FileRow ───────────────────────────────────────────────────────────────────

/// A row from the `files` table; field names and types mirror the schema.
///
/// Timestamps (`first_seen_at`, `updated_at`, `processed_at`, `next_attempt_at`)
/// are Unix epoch **seconds** stored as `INTEGER` in SQLite.
///
/// `mtime_ns` is nanoseconds since the Unix epoch (higher precision for
/// stability tracking); `inode` is the macOS inode number (unsigned 64-bit,
/// stored as the raw bit pattern in SQLite's `INTEGER`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRow {
    /// Auto-incremented primary key.
    pub id: i64,

    /// Canonical absolute path to the source file.
    pub path: PathBuf,

    /// `"sha256:<hex>"` once the file has been hashed; `None` while `seen`.
    pub content_hash: Option<String>,

    /// File size in bytes at the time of last stat.
    pub size: Option<i64>,

    /// File modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: Option<i64>,

    /// macOS inode number.  Stored as the bitwise reinterpretation of `u64`
    /// because SQLite integers are signed 64-bit.
    pub inode: Option<u64>,

    /// Current lifecycle status.
    pub status: Status,

    /// Number of processing attempts so far (0 = not yet attempted).
    pub attempts: i32,

    /// Earliest Unix epoch second at which the job should be re-tried.
    /// `None` means the job is immediately eligible.
    pub next_attempt_at: Option<i64>,

    /// Human-readable description of the last processing error, if any.
    pub last_error: Option<String>,

    /// Unix epoch second when this path was first observed.
    pub first_seen_at: i64,

    /// Unix epoch second of the last status change.
    pub updated_at: i64,

    /// Unix epoch second when the file was successfully processed; `None` if not yet done.
    pub processed_at: Option<i64>,

    /// Raw JSON string returned by the processor's `metadata` field.
    /// Kept as `Option<String>` to preserve the exact bytes the processor wrote.
    pub processor_meta: Option<String>,
}

// ── OutputRecord ─────────────────────────────────────────────────────────────

/// A row from the `outputs` table — one artifact produced by processing a source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputRecord {
    /// Auto-incremented primary key.
    pub id: i64,

    /// Foreign key into `files.id` (the source that produced this output).
    pub source_id: i64,

    /// Canonical absolute path to the produced file inside the vault.
    pub path: PathBuf,

    /// Processor-defined kind string: `"markdown"`, `"asset"`, etc.
    pub kind: Option<String>,

    /// File size in bytes at the time the output was recorded.
    pub bytes: Option<i64>,

    /// Unix epoch second when this output was recorded.
    pub created_at: i64,
}

// ── AuditEvent ────────────────────────────────────────────────────────────────

/// A row from the `events` audit-log table.
///
/// Events are append-only and exposed via the HTTP `/events` endpoint and
/// `kb tail`.  They complement structured tracing logs with a persistent,
/// queryable operational history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Auto-incremented primary key.
    pub id: i64,

    /// Unix epoch second when the event was recorded.
    pub ts: i64,

    /// Severity: `"info"`, `"warn"`, or `"error"`.
    pub level: String,

    /// Event kind string (see [`event_kind`] constants).
    pub kind: String,

    /// Optional reference to the `files.id` row this event concerns.
    pub file_id: Option<i64>,

    /// Human-readable summary message.
    pub message: String,

    /// Optional JSON-encoded supplementary detail (token counts, error
    /// messages, etc.) stored as a raw string.
    pub detail: Option<String>,
}

/// Well-known event kind constants.
///
/// The set is open: processors may emit additional kinds.  These constants
/// cover all events the daemon itself records.
pub mod event_kind {
    pub const DISCOVERED:        &str = "discovered";
    pub const STABLE:            &str = "stable";
    pub const HASHED:            &str = "hashed";
    pub const QUEUED:            &str = "queued";
    pub const SKIPPED_DUPLICATE: &str = "skipped_duplicate";
    pub const CLAIMED:           &str = "claimed";
    pub const PROCESSOR_STARTED: &str = "processor_started";
    pub const PROCESSOR_STDOUT:  &str = "processor_stdout";
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

// ── Processor contract types ──────────────────────────────────────────────────

/// JSON object written to the processor's `stdin` before sending EOF.
///
/// All path fields are serialized as UTF-8 strings (Serde's default for
/// [`PathBuf`] on all platforms).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorInput {
    /// Canonical absolute path to the source file being processed.
    pub input_path: PathBuf,

    /// Content hash in `"sha256:<hex>"` format.
    pub content_hash: String,

    /// Canonical absolute path to the vault root directory.
    pub vault_root: PathBuf,

    /// Canonical absolute path to the sources directory (strict sub-directory
    /// of `vault_root`).
    pub sources_dir: PathBuf,

    /// Per-job working directory under `processor.work_dir_root`.
    /// The processor should write all transient artifacts here.
    pub work_dir: PathBuf,

    /// Row ID of the corresponding `files` entry.
    pub job_id: i64,

    /// 1-based attempt number (1 = first attempt, 2 = first retry, …).
    pub attempt: i32,
}

/// One output artifact described in a successful [`ProcessResult`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessOutput {
    /// Absolute path to the produced file.  Must satisfy the vault invariant
    /// (inside `vault_root`, outside `sources_dir`).
    pub path: PathBuf,

    /// Processor-defined kind: `"markdown"`, `"asset"`, etc.
    pub kind: String,

    /// File size in bytes as reported by the processor.
    pub bytes: i64,
}

/// The last line of `stdout` emitted by the processor subprocess.
///
/// Uses an internally-tagged serde representation so the wire format is:
/// ```json
/// {"status": "ok",    "outputs": [...], "metadata": {...}}
/// {"status": "error", "error": "...",   "retryable": true, "metadata": {...}}
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ProcessResult {
    /// The processor completed successfully.
    Ok {
        /// All output artifacts produced during this run.
        outputs: Vec<ProcessOutput>,

        /// Optional arbitrary metadata (model name, token counts, etc.).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },

    /// The processor reported a failure.
    Error {
        /// Human-readable error description.
        error: String,

        /// `true` if the failure is transient and the job should be retried
        /// after backoff; `false` for permanent failures (processor bugs,
        /// invalid input format, etc.).
        #[serde(default = "default_retryable")]
        retryable: bool,

        /// Optional metadata from the step that failed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
}

fn default_retryable() -> bool {
    true
}

impl ProcessResult {
    /// Returns `true` when the variant is [`ProcessResult::Ok`].
    pub fn is_ok(&self) -> bool {
        matches!(self, ProcessResult::Ok { .. })
    }

    /// Returns `true` when the variant is [`ProcessResult::Error`].
    pub fn is_err(&self) -> bool {
        !self.is_ok()
    }

    /// Convenience accessor — returns the outputs slice for an `Ok` result,
    /// or an empty slice for an `Error` result.
    pub fn outputs(&self) -> &[ProcessOutput] {
        match self {
            ProcessResult::Ok { outputs, .. } => outputs.as_slice(),
            ProcessResult::Error { .. } => &[],
        }
    }

    /// Convenience accessor — returns `Some(&error)` for an `Error` result,
    /// or `None` for an `Ok` result.
    pub fn error_message(&self) -> Option<&str> {
        match self {
            ProcessResult::Error { error, .. } => Some(error.as_str()),
            ProcessResult::Ok { .. } => None,
        }
    }

    /// Whether this result should be retried.  `Ok` results always return
    /// `false`; `Error` results return the inner `retryable` flag.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProcessResult::Ok { .. } => false,
            ProcessResult::Error { retryable, .. } => *retryable,
        }
    }
}

// ── EnqueueOutcome ────────────────────────────────────────────────────────────

/// Outcome returned by the state store when a file is offered for enqueuing.
///
/// The caller uses this to decide whether to proceed with processing or log
/// a skip event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnqueueOutcome {
    /// The file is new (or a new revision) and is now `queued`.
    Queued,

    /// A different row with the same `content_hash` is already `done`;
    /// this row has been marked `skipped`.
    SkippedDuplicate,

    /// This row is already `queued` or `processing`; no state change.
    AlreadyPending,

    /// This row is already `done` with the same content hash; nothing to do.
    AlreadyDone,

    /// This row was previously `done` but the content hash has changed;
    /// it has been re-`queued` as a new revision.
    RequeuedRevision,
}

// ── Stats ─────────────────────────────────────────────────────────────────────

/// Aggregate counts returned by the `/stats` HTTP endpoint and `kb status`.
///
/// All count fields default to `0` so the struct can be built incrementally
/// from a `GROUP BY status` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stats {
    /// Number of files in the `seen` state (detected but not yet hashed).
    pub seen: i64,

    /// Number of files waiting to be claimed by a worker.
    pub queued: i64,

    /// Number of files currently being processed.
    pub processing: i64,

    /// Number of files that have been successfully processed.
    pub done: i64,

    /// Number of files that have failed processing (terminal or pending retry).
    pub failed: i64,

    /// Number of files skipped due to duplicate content hash.
    pub skipped: i64,

    /// Total number of files currently eligible for processing
    /// (`queued` + `processing`).  Derived from the counts above for
    /// convenience; populated by the state store's `stats()` method.
    pub queue_depth: i64,

    /// Age in seconds of the oldest `queued` or `processing` entry, relative
    /// to `first_seen_at`.  `None` when the queue is empty.
    pub oldest_pending_age_secs: Option<i64>,

    /// The `last_error` field of the most recently updated `failed` row,
    /// if any.  Surfaced here so `kb status` can show a quick hint without
    /// a separate query.
    pub last_error: Option<String>,
}
