//! SQLite state store — single-writer actor pattern.
//!
//! A dedicated OS thread owns the [`rusqlite::Connection`].  All other
//! components communicate with it by sending [`StateOp`] messages through a
//! bounded [`tokio::sync::mpsc`] channel and awaiting a reply on a per-call
//! [`tokio::sync::oneshot`] channel.
//!
//! This design:
//! - Eliminates `Mutex` contention and `SQLITE_BUSY` errors entirely.
//! - Keeps all SQLite I/O off the tokio thread pool (no `spawn_blocking`
//!   wrapper needed per call after startup).
//! - Exposes natural backpressure through the bounded channel.
//!
//! # Architecture
//! ```text
//! async caller ──(StateOp + oneshot::Sender)──▶ mpsc channel
//!                                                      │
//!                                           StateActor (OS thread)
//!                                           owns rusqlite::Connection
//!                                                      │
//!                                            ◀── oneshot reply ──
//! ```
//!
//! # Quick start
//! ```no_run
//! # use kb_core::state::StateStore;
//! # use std::path::Path;
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! let store = StateStore::new(Path::new("/tmp/state.db"), &[30, 300, 1800]).await?;
//! let stats = store.stats().await?;
//! println!("queued: {}", stats.queued);
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Row;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::migrations;
use crate::types::{
    event_kind, AuditEvent, EnqueueOutcome, FileRow, OutputRecord, ProcessOutput, Stats, Status,
};

/// Bounded channel capacity — large enough to absorb a burst of concurrent
/// callers while preventing unbounded memory growth if the actor stalls.
const CHANNEL_CAP: usize = 256;

// ─── StateOp ─────────────────────────────────────────────────────────────────

/// A single database operation dispatched to the [`StateActor`].
///
/// Every variant carries a `reply` sender through which the actor returns its
/// result.  The `oneshot` channel guarantees each call site gets exactly its
/// own reply — no cross-talk is possible.
///
/// This enum is `#[non_exhaustive]` within the crate; external code must not
/// match on it directly.
pub enum StateOp {
    RegisterSeen {
        path:  PathBuf,
        size:  Option<i64>,
        mtime: Option<i64>,
        inode: Option<u64>,
        reply: oneshot::Sender<crate::Result<FileRow>>,
    },
    SetHash {
        id:    i64,
        hash:  String,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    Enqueue {
        id:    i64,
        reply: oneshot::Sender<crate::Result<EnqueueOutcome>>,
    },
    ClaimNext {
        reply: oneshot::Sender<crate::Result<Option<FileRow>>>,
    },
    MarkDone {
        id:      i64,
        outputs: Vec<ProcessOutput>,
        meta:    Option<Value>,
        reply:   oneshot::Sender<crate::Result<()>>,
    },
    MarkFailed {
        id:        i64,
        error:     String,
        retryable: bool,
        reply:     oneshot::Sender<crate::Result<()>>,
    },
    RecoverInFlight {
        reply: oneshot::Sender<crate::Result<usize>>,
    },
    FindByPath {
        path:  PathBuf,
        reply: oneshot::Sender<crate::Result<Option<FileRow>>>,
    },
    FindByHash {
        hash:  String,
        reply: oneshot::Sender<crate::Result<Vec<FileRow>>>,
    },
    GetStats {
        reply: oneshot::Sender<crate::Result<Stats>>,
    },
    RecordEvent {
        level:   String,
        kind:    String,
        file_id: Option<i64>,
        message: String,
        detail:  Option<String>,
        reply:   oneshot::Sender<crate::Result<()>>,
    },
    ListFiles {
        status_filter: Option<Status>,
        limit:         i64,
        offset:        i64,
        reply:         oneshot::Sender<crate::Result<Vec<FileRow>>>,
    },
    GetFileById {
        id:    i64,
        reply: oneshot::Sender<crate::Result<Option<FileRow>>>,
    },
    GetOutputsForFile {
        file_id: i64,
        reply:   oneshot::Sender<crate::Result<Vec<OutputRecord>>>,
    },
    GetEvents {
        since: Option<i64>,
        level: Option<String>,
        kind:  Option<String>,
        limit: i64,
        reply: oneshot::Sender<crate::Result<Vec<AuditEvent>>>,
    },

    /// Atomic 5-rule dedup + enqueue for a freshly-stable file (§3.3).
    ///
    /// Combines `register_seen` + `set_hash` + `enqueue` in one transaction,
    /// applying all dedup rules in the exact order specified by the plan.
    ProcessStableFile {
        path:         PathBuf,
        size:         i64,
        mtime_ns:     i64,
        inode:        u64,
        content_hash: String,
        reply:        oneshot::Sender<crate::Result<EnqueueOutcome>>,
    },
}

// ─── StateStore ───────────────────────────────────────────────────────────────

/// An async handle to the single-writer SQLite actor.
///
/// Cheaply cloneable — every clone shares the same underlying channel and
/// therefore the same actor / database connection.
///
/// All methods are `async`; they serialize through the actor's OS thread so
/// only one SQL statement executes at a time, eliminating `SQLITE_BUSY`.
#[derive(Clone)]
pub struct StateStore {
    sender: mpsc::Sender<StateOp>,
}

impl StateStore {
    /// Open (or create) the database at `db_path`, run all pending migrations,
    /// and start the background actor thread.
    ///
    /// `backoff_secs` controls retry timing: when a retryable job fails,
    /// `next_attempt_at = now + backoff_secs[attempts - 1]`.  If
    /// `attempts - 1 >= backoff_secs.len()`, the job is considered permanently
    /// failed (terminal `"failed"` status, no further retry).
    ///
    /// # Errors
    /// Returns an error if the DB cannot be opened, migrations fail, or the
    /// actor thread cannot be spawned.
    pub async fn new(db_path: &Path, backoff_secs: &[u64]) -> crate::Result<Self> {
        let db_path = db_path.to_path_buf();
        let backoff = backoff_secs.to_vec();

        // Open + migrate on a blocking thread so we don't stall the runtime
        // during DDL execution (which compiles SQL and may touch the FS).
        let conn = tokio::task::spawn_blocking(move || migrations::db_open(&db_path))
            .await
            .map_err(|e| anyhow::anyhow!("migration task panicked: {e}"))??;

        let (tx, rx) = mpsc::channel::<StateOp>(CHANNEL_CAP);

        // Spawn a dedicated OS thread so SQLite I/O never blocks the tokio
        // thread pool.  `blocking_recv()` is the synchronous analogue of
        // `.recv().await` and works correctly outside an async context.
        std::thread::Builder::new()
            .name("kb-state-actor".into())
            .spawn(move || {
                let mut actor = StateActor::new(conn, backoff);
                actor.run(rx);
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn state actor thread: {e}"))?;

        Ok(Self { sender: tx })
    }

    // ─── internal send helper ─────────────────────────────────────────────────

    /// Build an op via `build_fn(reply_tx)`, send it to the actor, and await
    /// the reply on the corresponding `oneshot` receiver.
    async fn send<T>(
        &self,
        build_fn: impl FnOnce(oneshot::Sender<crate::Result<T>>) -> StateOp,
    ) -> crate::Result<T> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(build_fn(tx))
            .await
            .map_err(|_| anyhow::anyhow!("state actor channel closed"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("state actor dropped reply sender"))?
    }

    // ─── public async API ─────────────────────────────────────────────────────

    /// Register a newly-discovered file path.
    ///
    /// Uses `INSERT OR IGNORE` so calling this multiple times for the same
    /// path is safe — the existing row is returned unchanged on subsequent
    /// calls.  The returned row always reflects the current DB state.
    pub async fn register_seen(
        &self,
        path:  PathBuf,
        size:  Option<i64>,
        mtime: Option<i64>,
        inode: Option<u64>,
    ) -> crate::Result<FileRow> {
        self.send(|reply| StateOp::RegisterSeen { path, size, mtime, inode, reply })
            .await
    }

    /// Store the content hash for a file after it has been hashed.
    ///
    /// Sets `files.content_hash` and bumps `updated_at`.
    pub async fn set_hash(&self, id: i64, hash: String) -> crate::Result<()> {
        self.send(|reply| StateOp::SetHash { id, hash, reply }).await
    }

    /// Apply §3.3 dedup rules and transition the row to `queued` if
    /// appropriate.
    ///
    /// The hash must have been written via [`set_hash`] before calling this.
    ///
    /// Rule 1 (`AlreadyDone`) is the caller's responsibility: if the computed
    /// hash matches `row.content_hash` and `row.status == Done`, do not call
    /// `enqueue` — return `AlreadyDone` directly in the higher-level dedup
    /// logic (T12).
    pub async fn enqueue(&self, id: i64) -> crate::Result<EnqueueOutcome> {
        self.send(|reply| StateOp::Enqueue { id, reply }).await
    }

    /// Atomically claim the next eligible queued job for processing.
    ///
    /// Increments `attempts` and transitions `status → 'processing'` in a
    /// single `UPDATE … RETURNING` statement — no TOCTOU race, no
    /// double-claim across concurrent workers.
    ///
    /// Returns `None` when the queue is empty or all eligible jobs are within
    /// their backoff window.
    pub async fn claim_next(&self) -> crate::Result<Option<FileRow>> {
        self.send(|reply| StateOp::ClaimNext { reply }).await
    }

    /// Mark a job as successfully completed and record all output artifacts.
    ///
    /// Inserts one row into `outputs` per entry in `outputs`, then sets
    /// `status = 'done'`, `processed_at`, and (optionally) `processor_meta`.
    pub async fn mark_done(
        &self,
        id:      i64,
        outputs: Vec<ProcessOutput>,
        meta:    Option<Value>,
    ) -> crate::Result<()> {
        self.send(|reply| StateOp::MarkDone { id, outputs, meta, reply }).await
    }

    /// Mark a job as failed.
    ///
    /// Behaviour depends on `retryable` and the exhaustion of `backoff_secs`:
    /// - If `retryable == true` and `attempts - 1 < backoff_secs.len()`:
    ///   the row transitions back to `'queued'` with
    ///   `next_attempt_at = now + backoff_secs[attempts - 1]`.
    /// - Otherwise: the row is set to `'failed'` (terminal).
    pub async fn mark_failed(
        &self,
        id:        i64,
        error:     String,
        retryable: bool,
    ) -> crate::Result<()> {
        self.send(|reply| StateOp::MarkFailed { id, error, retryable, reply }).await
    }

    /// Reset all `'processing'` rows to `'queued'` after a crash.
    ///
    /// Intended to be called once on daemon startup.  Returns the count of
    /// recovered rows and emits an audit event if any were found.
    pub async fn recover_in_flight(&self) -> crate::Result<usize> {
        self.send(|reply| StateOp::RecoverInFlight { reply }).await
    }

    /// Look up a file row by its canonical absolute path.
    pub async fn find_by_path(&self, path: PathBuf) -> crate::Result<Option<FileRow>> {
        self.send(|reply| StateOp::FindByPath { path, reply }).await
    }

    /// Find all rows that share the given content hash.
    pub async fn find_by_hash(&self, hash: String) -> crate::Result<Vec<FileRow>> {
        self.send(|reply| StateOp::FindByHash { hash, reply }).await
    }

    /// Return aggregate counts per status plus derived metrics.
    pub async fn stats(&self) -> crate::Result<Stats> {
        self.send(|reply| StateOp::GetStats { reply }).await
    }

    /// Append a row to the `events` audit-log table.
    ///
    /// `detail` should be a JSON string or `None`.
    pub async fn record_event(
        &self,
        level:   String,
        kind:    String,
        file_id: Option<i64>,
        message: String,
        detail:  Option<String>,
    ) -> crate::Result<()> {
        self.send(|reply| StateOp::RecordEvent {
            level,
            kind,
            file_id,
            message,
            detail,
            reply,
        })
        .await
    }

    /// Return a paginated list of file rows, optionally filtered by status.
    ///
    /// Rows are ordered by `updated_at DESC` (most-recently-changed first).
    pub async fn list_files(
        &self,
        status_filter: Option<Status>,
        limit:         i64,
        offset:        i64,
    ) -> crate::Result<Vec<FileRow>> {
        self.send(|reply| StateOp::ListFiles {
            status_filter,
            limit,
            offset,
            reply,
        })
        .await
    }

    /// Fetch a single file row by its primary key.
    pub async fn get_file_by_id(&self, id: i64) -> crate::Result<Option<FileRow>> {
        self.send(|reply| StateOp::GetFileById { id, reply }).await
    }

    /// Fetch all output records associated with a given source file.
    pub async fn get_outputs_for_file(&self, file_id: i64) -> crate::Result<Vec<OutputRecord>> {
        self.send(|reply| StateOp::GetOutputsForFile { file_id, reply }).await
    }

    /// Query the events audit log with optional filters.
    ///
    /// - `since` — only return events with `ts > since` (Unix epoch seconds).
    /// - `level` — filter by level string (`"info"`, `"warn"`, `"error"`).
    /// - `kind`  — filter by kind string (see [`crate::event_kind`]).
    /// - `limit` — maximum rows to return, ordered newest-first.
    pub async fn get_events(
        &self,
        since: Option<i64>,
        level: Option<String>,
        kind:  Option<String>,
        limit: i64,
    ) -> crate::Result<Vec<AuditEvent>> {
        self.send(|reply| StateOp::GetEvents {
            since,
            level,
            kind,
            limit,
            reply,
        })
        .await
    }

    /// Atomically apply the §3.3 five-rule dedup + enqueue logic for a
    /// freshly-stabilised file.
    ///
    /// This is the **primary entry point** for the watcher → state pipeline
    /// (T13).  It replaces the three-step `register_seen` → `set_hash` →
    /// `enqueue` sequence with a single, atomic SQLite transaction that
    /// applies all dedup rules in strict order.
    ///
    /// # Rules (applied IN ORDER)
    ///
    /// 1. `path` is `done` **and** current hash matches stored hash
    ///    → [`EnqueueOutcome::AlreadyDone`] (no DB change)
    /// 2. `path` is `done` **but** hash differs
    ///    → reset to `queued`, update metadata → [`EnqueueOutcome::RequeuedRevision`]
    /// 3. Any **other** row with the same `content_hash` is `done`
    ///    → mark this row `skipped` → [`EnqueueOutcome::SkippedDuplicate`]
    /// 4. `path` is `queued` or `processing`
    ///    → no-op → [`EnqueueOutcome::AlreadyPending`]
    /// 5. Otherwise (new path, `seen`, `failed`, `skipped`)
    ///    → transition to `queued` → [`EnqueueOutcome::Queued`]
    ///
    /// Audit events are recorded for `Queued`, `RequeuedRevision`, and
    /// `SkippedDuplicate` outcomes inside the same transaction.
    ///
    /// # Arguments
    /// - `path`         — canonical absolute path to the source file
    /// - `size`         — file size in bytes (from the final stability stat)
    /// - `mtime_ns`     — modification time in nanoseconds since Unix epoch
    /// - `inode`        — macOS inode number
    /// - `content_hash` — `"sha256:<hex>"` string produced by the hasher
    pub async fn process_stable_file(
        &self,
        path:         PathBuf,
        size:         i64,
        mtime_ns:     i64,
        inode:        u64,
        content_hash: String,
    ) -> crate::Result<EnqueueOutcome> {
        self.send(|reply| StateOp::ProcessStableFile {
            path,
            size,
            mtime_ns,
            inode,
            content_hash,
            reply,
        })
        .await
    }
}

// ─── StateActor ───────────────────────────────────────────────────────────────

/// The single OS thread that owns the [`rusqlite::Connection`].
///
/// All SQL — reads and writes — is executed here, serialised through the
/// `mpsc` receiver.  This eliminates all `SQLITE_BUSY` contention and makes
/// every operation conceptually `O(1)` from the caller's perspective.
struct StateActor {
    conn:         rusqlite::Connection,
    backoff_secs: Vec<u64>,
}

impl StateActor {
    fn new(conn: rusqlite::Connection, backoff_secs: Vec<u64>) -> Self {
        Self { conn, backoff_secs }
    }

    /// Drive the actor loop until the sender side of the channel is dropped.
    fn run(&mut self, mut rx: mpsc::Receiver<StateOp>) {
        while let Some(op) = rx.blocking_recv() {
            self.dispatch(op);
        }
        tracing::info!("state actor: channel closed — shutting down");
    }

    fn dispatch(&mut self, op: StateOp) {
        match op {
            StateOp::RegisterSeen { path, size, mtime, inode, reply } => {
                let _ = reply.send(self.register_seen(path, size, mtime, inode));
            }
            StateOp::SetHash { id, hash, reply } => {
                let _ = reply.send(self.set_hash(id, hash));
            }
            StateOp::Enqueue { id, reply } => {
                let _ = reply.send(self.enqueue(id));
            }
            StateOp::ClaimNext { reply } => {
                let _ = reply.send(self.claim_next());
            }
            StateOp::MarkDone { id, outputs, meta, reply } => {
                let _ = reply.send(self.mark_done(id, outputs, meta));
            }
            StateOp::MarkFailed { id, error, retryable, reply } => {
                let _ = reply.send(self.mark_failed(id, error, retryable));
            }
            StateOp::RecoverInFlight { reply } => {
                let _ = reply.send(self.recover_in_flight());
            }
            StateOp::FindByPath { path, reply } => {
                let _ = reply.send(self.find_by_path(path));
            }
            StateOp::FindByHash { hash, reply } => {
                let _ = reply.send(self.find_by_hash(hash));
            }
            StateOp::GetStats { reply } => {
                let _ = reply.send(self.get_stats());
            }
            StateOp::RecordEvent { level, kind, file_id, message, detail, reply } => {
                let _ = reply.send(self.record_event_op(
                    &level,
                    &kind,
                    file_id,
                    &message,
                    detail.as_deref(),
                ));
            }
            StateOp::ListFiles { status_filter, limit, offset, reply } => {
                let _ = reply.send(self.list_files(status_filter, limit, offset));
            }
            StateOp::GetFileById { id, reply } => {
                let _ = reply.send(self.get_file_by_id(id));
            }
            StateOp::GetOutputsForFile { file_id, reply } => {
                let _ = reply.send(self.get_outputs_for_file(file_id));
            }
            StateOp::GetEvents { since, level, kind, limit, reply } => {
                let _ = reply.send(self.get_events(since, level, kind, limit));
            }
            StateOp::ProcessStableFile { path, size, mtime_ns, inode, content_hash, reply } => {
                let _ = reply.send(
                    self.process_stable_file_op(path, size, mtime_ns, inode, content_hash),
                );
            }
        }
    }

    // ─── utility helpers ──────────────────────────────────────────────────────

    /// Current Unix timestamp in whole seconds.
    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
    }

    /// Ordered column list for every `files` query.
    ///
    /// **Column indices (0-based) must match [`map_file_row`]:**
    /// ```text
    /// 0:id  1:path  2:content_hash  3:size  4:mtime_ns  5:inode
    /// 6:status  7:attempts  8:next_attempt_at  9:last_error
    /// 10:first_seen_at  11:updated_at  12:processed_at  13:processor_meta
    /// ```
    const FILE_COLS: &'static str = concat!(
        "id, path, content_hash, size, mtime_ns, inode, status, attempts,",
        " next_attempt_at, last_error, first_seen_at, updated_at,",
        " processed_at, processor_meta",
    );

    /// Map a rusqlite [`Row`] into a [`FileRow`].
    ///
    /// Column order must exactly match [`FILE_COLS`].
    fn map_file_row(row: &Row<'_>) -> rusqlite::Result<FileRow> {
        let path_str: String       = row.get(1)?;
        let inode_raw: Option<i64> = row.get(5)?;
        Ok(FileRow {
            id:              row.get(0)?,
            path:            PathBuf::from(path_str),
            content_hash:    row.get(2)?,
            size:            row.get(3)?,
            mtime_ns:        row.get(4)?,
            // inode is stored as i64 (signed) in SQLite; reinterpret as u64.
            inode:           inode_raw.map(|i| i as u64),
            status:          row.get(6)?,
            attempts:        row.get(7)?,
            next_attempt_at: row.get(8)?,
            last_error:      row.get(9)?,
            first_seen_at:   row.get(10)?,
            updated_at:      row.get(11)?,
            processed_at:    row.get(12)?,
            processor_meta:  row.get(13)?,
        })
    }

    // ─── operations ───────────────────────────────────────────────────────────

    fn register_seen(
        &self,
        path:  PathBuf,
        size:  Option<i64>,
        mtime: Option<i64>,
        inode: Option<u64>,
    ) -> crate::Result<FileRow> {
        let path_str   = path.to_string_lossy().into_owned();
        let now        = Self::now();
        // Store u64 inode as its i64 bit-pattern; callers reconstruct via `as u64`.
        let inode_sql: Option<i64> = inode.map(|i| i as i64);

        // INSERT OR IGNORE: if the path already exists this is a no-op.
        self.conn.execute(
            "INSERT OR IGNORE INTO files \
             (path, size, mtime_ns, inode, status, first_seen_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, 'seen', ?5, ?6)",
            rusqlite::params![path_str, size, mtime, inode_sql, now, now],
        )?;

        // Always return the current row (newly-inserted or pre-existing).
        let sql = format!("SELECT {} FROM files WHERE path = ?1", Self::FILE_COLS);
        let row = self.conn.query_row(&sql, [&path_str], |r| Self::map_file_row(r))?;
        Ok(row)
    }

    fn set_hash(&self, id: i64, hash: String) -> crate::Result<()> {
        let now = Self::now();
        self.conn.execute(
            "UPDATE files SET content_hash = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![hash, now, id],
        )?;
        Ok(())
    }

    /// Implements §3.3 dedup rules 2–5.
    ///
    /// Rule 1 (same path, same hash, already `done`) is the caller's
    /// responsibility: higher-level dedup logic (T12) must compare the
    /// computed hash against `row.content_hash` before calling `enqueue`.
    /// If the hashes match on a `done` row, do not call this — return
    /// [`EnqueueOutcome::AlreadyDone`] directly.
    fn enqueue(&self, id: i64) -> crate::Result<EnqueueOutcome> {
        let now = Self::now();
        let sql = format!("SELECT {} FROM files WHERE id = ?1", Self::FILE_COLS);
        let row = match self.conn.query_row(&sql, [id], |r| Self::map_file_row(r)) {
            Ok(r)                                         => r,
            Err(rusqlite::Error::QueryReturnedNoRows)     => {
                return Err(anyhow::anyhow!("enqueue: no file row with id {id}"));
            }
            Err(e) => return Err(e.into()),
        };

        // Rule 4 — already in flight; no change needed.
        if matches!(row.status, Status::Queued | Status::Processing) {
            return Ok(EnqueueOutcome::AlreadyPending);
        }

        // Rule 2 — previously `done`; hash changed (caller verified this before
        // calling `set_hash` + `enqueue`).  Re-queue as a new revision.
        if row.status == Status::Done {
            self.conn.execute(
                "UPDATE files \
                 SET status = 'queued', next_attempt_at = NULL, \
                     last_error = NULL, updated_at = ?1 \
                 WHERE id = ?2",
                rusqlite::params![now, id],
            )?;
            return Ok(EnqueueOutcome::RequeuedRevision);
        }

        // Rule 3 — duplicate content: a different row with the same hash is
        // already `done`.  Mark this row `skipped`.
        if let Some(ref hash) = row.content_hash {
            let dup: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM files \
                 WHERE content_hash = ?1 AND status = 'done' AND id != ?2",
                rusqlite::params![hash, id],
                |r| r.get(0),
            )?;
            if dup > 0 {
                self.conn.execute(
                    "UPDATE files SET status = 'skipped', updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![now, id],
                )?;
                return Ok(EnqueueOutcome::SkippedDuplicate);
            }
        }

        // Rule 5 — transition to `queued` (covers `seen`, `failed`, `skipped`).
        self.conn.execute(
            "UPDATE files \
             SET status = 'queued', next_attempt_at = NULL, updated_at = ?1 \
             WHERE id = ?2",
            rusqlite::params![now, id],
        )?;
        Ok(EnqueueOutcome::Queued)
    }

    /// Implements §3.3 dedup rules 1-5 atomically in a single transaction.
    ///
    /// All reads, writes, and the audit-event INSERT happen inside one
    /// `BEGIN … COMMIT` so the caller always sees a fully-consistent state
    /// transition — no partial updates are possible even under concurrent
    /// callers (the actor serialises everything anyway, but the transaction
    /// also provides durability on WAL flush).
    ///
    /// # Rule ordering
    ///
    /// ```text
    /// 1. status=done  ∧  hash matches   → AlreadyDone      (no-op, implicit rollback)
    /// 2. status=done  ∧  hash differs   → RequeuedRevision  (UPDATE + event)
    /// 3. ∃ other row: hash=H ∧ done     → SkippedDuplicate  (UPDATE + event)
    /// 4. status=queued|processing       → AlreadyPending    (no-op, implicit rollback)
    /// 5. otherwise                      → Queued            (INSERT/UPDATE + event)
    /// ```
    ///
    /// Rules 3 is checked **before** rule 4 per the specification.
    fn process_stable_file_op(
        &self,
        path:         PathBuf,
        size:         i64,
        mtime_ns:     i64,
        inode:        u64,
        content_hash: String,
    ) -> crate::Result<EnqueueOutcome> {
        let path_str  = path.to_string_lossy().into_owned();
        let now       = Self::now();
        let inode_sql = inode as i64;

        // All operations execute inside this transaction.  On any `?`-propagated
        // error the transaction is automatically rolled back when `tx` is dropped.
        let tx = self.conn.unchecked_transaction()?;

        // ── 1. look up existing row by canonical path ─────────────────────────
        let select_sql = format!("SELECT {} FROM files WHERE path = ?1", Self::FILE_COLS);
        let existing: Option<FileRow> = match tx.query_row(
            &select_sql,
            rusqlite::params![&path_str],
            |r| Self::map_file_row(r),
        ) {
            Ok(row)                                   => Some(row),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e)                                    => return Err(e.into()),
        };

        // ── 2. apply dedup rules in order ─────────────────────────────────────
        //
        // `(file_id, outcome)` is set by whichever rule fires.
        // Rules that leave the DB unchanged return early (tx dropped = no-op
        // rollback — safe because no writes occurred).
        let (file_id, outcome): (i64, EnqueueOutcome) = if let Some(row) = existing {
            let fid = row.id;

            // ── Rule 1: done + same hash ─────────────────────────────────────
            if row.status == Status::Done
                && row.content_hash.as_deref() == Some(content_hash.as_str())
            {
                // Nothing to do; tx rolls back silently.
                return Ok(EnqueueOutcome::AlreadyDone);
            }

            // ── Rule 2: done + different hash ───────────────────────────────
            if row.status == Status::Done {
                tx.execute(
                    "UPDATE files \
                     SET content_hash = ?1, size = ?2, mtime_ns = ?3, inode = ?4, \
                         status = 'queued', next_attempt_at = NULL, last_error = NULL, \
                         updated_at = ?5 \
                     WHERE id = ?6",
                    rusqlite::params![&content_hash, size, mtime_ns, inode_sql, now, fid],
                )?;
                (fid, EnqueueOutcome::RequeuedRevision)
            } else {
                // ── Rule 3: another done row with same hash? (before rule 4) ─
                let dup_done: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM files \
                     WHERE content_hash = ?1 AND status = 'done' AND id != ?2",
                    rusqlite::params![&content_hash, fid],
                    |r| r.get(0),
                )?;

                if dup_done > 0 {
                    tx.execute(
                        "UPDATE files \
                         SET content_hash = ?1, size = ?2, mtime_ns = ?3, inode = ?4, \
                             status = 'skipped', updated_at = ?5 \
                         WHERE id = ?6",
                        rusqlite::params![&content_hash, size, mtime_ns, inode_sql, now, fid],
                    )?;
                    (fid, EnqueueOutcome::SkippedDuplicate)

                } else if matches!(row.status, Status::Queued | Status::Processing) {
                    // ── Rule 4: already in the pipeline ─────────────────────
                    // Nothing to do; tx rolls back silently.
                    return Ok(EnqueueOutcome::AlreadyPending);

                } else {
                    // ── Rule 5: seen / failed / skipped → queue ──────────────
                    tx.execute(
                        "UPDATE files \
                         SET content_hash = ?1, size = ?2, mtime_ns = ?3, inode = ?4, \
                             status = 'queued', next_attempt_at = NULL, last_error = NULL, \
                             updated_at = ?5 \
                         WHERE id = ?6",
                        rusqlite::params![&content_hash, size, mtime_ns, inode_sql, now, fid],
                    )?;
                    (fid, EnqueueOutcome::Queued)
                }
            }
        } else {
            // ── New path: insert as 'seen', then apply rules 3 & 5 ──────────
            tx.execute(
                "INSERT INTO files \
                 (path, content_hash, size, mtime_ns, inode, status, first_seen_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'seen', ?6, ?7)",
                rusqlite::params![&path_str, &content_hash, size, mtime_ns, inode_sql, now, now],
            )?;
            let fid = tx.last_insert_rowid();

            // ── Rule 3: another done row with same hash? ─────────────────────
            let dup_done: i64 = tx.query_row(
                "SELECT COUNT(*) FROM files \
                 WHERE content_hash = ?1 AND status = 'done' AND id != ?2",
                rusqlite::params![&content_hash, fid],
                |r| r.get(0),
            )?;

            if dup_done > 0 {
                tx.execute(
                    "UPDATE files SET status = 'skipped', updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![now, fid],
                )?;
                (fid, EnqueueOutcome::SkippedDuplicate)
            } else {
                // ── Rule 5: queue it ─────────────────────────────────────────
                tx.execute(
                    "UPDATE files SET status = 'queued', updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![now, fid],
                )?;
                (fid, EnqueueOutcome::Queued)
            }
        };

        // ── 3. record an audit event for state-changing outcomes ───────────────
        //
        // AlreadyDone and AlreadyPending returned early above, so only
        // Queued, RequeuedRevision, and SkippedDuplicate reach this point.
        let (ev_kind, ev_msg): (&str, String) = match &outcome {
            EnqueueOutcome::Queued => (
                event_kind::QUEUED,
                format!("queued: {path_str}"),
            ),
            EnqueueOutcome::RequeuedRevision => (
                event_kind::QUEUED,
                format!("requeued revision: {path_str}"),
            ),
            EnqueueOutcome::SkippedDuplicate => (
                event_kind::SKIPPED_DUPLICATE,
                format!("skipped duplicate: {path_str}"),
            ),
            // The remaining variants are unreachable here because they
            // returned early before this point.  Guard them anyway so
            // the match stays exhaustive without `#[allow(unreachable_patterns)]`.
            EnqueueOutcome::AlreadyDone | EnqueueOutcome::AlreadyPending => {
                unreachable!("AlreadyDone and AlreadyPending return early above")
            }
        };

        tx.execute(
            "INSERT INTO events (ts, level, kind, file_id, message, detail) \
             VALUES (?1, 'info', ?2, ?3, ?4, NULL)",
            rusqlite::params![now, ev_kind, file_id, &ev_msg],
        )?;

        tx.commit()?;
        Ok(outcome)
    }

    fn claim_next(&self) -> crate::Result<Option<FileRow>> {
        let now = Self::now();
        // Single UPDATE … RETURNING is atomic: no TOCTOU window; concurrent
        // workers cannot claim the same row even under heavy load.
        let sql = format!(
            "UPDATE files \
             SET status = 'processing', attempts = attempts + 1, updated_at = ?1 \
             WHERE id = ( \
                 SELECT id FROM files \
                 WHERE status = 'queued' \
                   AND (next_attempt_at IS NULL OR next_attempt_at <= ?2) \
                 ORDER BY first_seen_at \
                 LIMIT 1 \
             ) \
             RETURNING {cols}",
            cols = Self::FILE_COLS,
        );
        match self.conn.query_row(&sql, rusqlite::params![now, now], |r| Self::map_file_row(r)) {
            Ok(row)                                     => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows)   => Ok(None),
            Err(e)                                      => Err(e.into()),
        }
    }

    fn mark_done(
        &self,
        id:      i64,
        outputs: Vec<ProcessOutput>,
        meta:    Option<Value>,
    ) -> crate::Result<()> {
        let now      = Self::now();
        let meta_str = meta.map(|v| v.to_string());

        self.conn.execute(
            "UPDATE files \
             SET status = 'done', processed_at = ?1, updated_at = ?2, \
                 processor_meta = ?3, last_error = NULL \
             WHERE id = ?4",
            rusqlite::params![now, now, meta_str, id],
        )?;

        for out in &outputs {
            self.conn.execute(
                "INSERT INTO outputs (source_id, path, kind, bytes, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    id,
                    out.path.to_string_lossy().as_ref(),
                    &out.kind,
                    out.bytes,
                    now,
                ],
            )?;
        }

        Ok(())
    }

    fn mark_failed(
        &self,
        id:        i64,
        error:     String,
        retryable: bool,
    ) -> crate::Result<()> {
        let now = Self::now();

        // `attempts` was already incremented by `claim_next` before the job
        // started.  The zero-based index into `backoff_secs` is `attempts - 1`.
        let attempts: i32 = self.conn.query_row(
            "SELECT attempts FROM files WHERE id = ?1",
            [id],
            |r| r.get(0),
        )?;

        let attempt_idx = attempts.saturating_sub(1) as usize;
        let exhausted   = !retryable || attempt_idx >= self.backoff_secs.len();

        if exhausted {
            // Terminal failure — status stays `failed`.
            self.conn.execute(
                "UPDATE files \
                 SET status = 'failed', last_error = ?1, \
                     next_attempt_at = NULL, updated_at = ?2 \
                 WHERE id = ?3",
                rusqlite::params![error, now, id],
            )?;
        } else {
            // Retryable — reset to `queued` with a future `next_attempt_at`
            // so `claim_next` will pick it up after the backoff window.
            let backoff      = self.backoff_secs[attempt_idx] as i64;
            let next_attempt = now + backoff;
            self.conn.execute(
                "UPDATE files \
                 SET status = 'queued', last_error = ?1, \
                     next_attempt_at = ?2, updated_at = ?3 \
                 WHERE id = ?4",
                rusqlite::params![error, next_attempt, now, id],
            )?;
        }

        Ok(())
    }

    fn recover_in_flight(&self) -> crate::Result<usize> {
        let now = Self::now();

        // Reset every `processing` row back to `queued`.  `attempts` was
        // already incremented by `claim_next` during the previous run, so
        // it accurately reflects the total number of attempts including the
        // crashed one.
        let count = self.conn.execute(
            "UPDATE files \
             SET status = 'queued', next_attempt_at = NULL, updated_at = ?1 \
             WHERE status = 'processing'",
            [now],
        )?;

        if count > 0 {
            self.record_event_op(
                "warn",
                crate::types::event_kind::RECOVERED,
                None,
                &format!("recovered {count} in-flight job(s) after unclean shutdown"),
                None,
            )?;
        }

        Ok(count)
    }

    fn find_by_path(&self, path: PathBuf) -> crate::Result<Option<FileRow>> {
        let path_str = path.to_string_lossy().into_owned();
        let sql      = format!("SELECT {} FROM files WHERE path = ?1", Self::FILE_COLS);
        match self.conn.query_row(&sql, [&path_str], |r| Self::map_file_row(r)) {
            Ok(row)                                     => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows)   => Ok(None),
            Err(e)                                      => Err(e.into()),
        }
    }

    fn find_by_hash(&self, hash: String) -> crate::Result<Vec<FileRow>> {
        let sql = format!(
            "SELECT {} FROM files WHERE content_hash = ?1 ORDER BY first_seen_at",
            Self::FILE_COLS,
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([&hash], |r| Self::map_file_row(r))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn get_stats(&self) -> crate::Result<Stats> {
        let mut stmt = self
            .conn
            .prepare("SELECT status, COUNT(*) FROM files GROUP BY status")?;
        let mut stats = Stats::default();
        for pair in stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })? {
            let (status, count) = pair?;
            match status.as_str() {
                "seen"       => stats.seen       = count,
                "queued"     => stats.queued      = count,
                "processing" => stats.processing  = count,
                "done"       => stats.done        = count,
                "failed"     => stats.failed      = count,
                "skipped"    => stats.skipped     = count,
                _            => {}
            }
        }

        stats.queue_depth = stats.queued + stats.processing;

        // Oldest pending age (queued + processing).
        let oldest: Option<i64> = self
            .conn
            .query_row(
                "SELECT MIN(first_seen_at) FROM files \
                 WHERE status IN ('queued', 'processing')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(None);
        if let Some(ts) = oldest {
            stats.oldest_pending_age_secs = Some(Self::now().saturating_sub(ts));
        }

        // Last error message from the most-recently-updated failed row.
        let last_err: Option<String> = self
            .conn
            .query_row(
                "SELECT last_error FROM files \
                 WHERE status = 'failed' AND last_error IS NOT NULL \
                 ORDER BY updated_at DESC \
                 LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap_or(None);
        stats.last_error = last_err;

        Ok(stats)
    }

    /// Synchronous `record_event` used internally by the actor itself
    /// (e.g., from within [`recover_in_flight`]).
    fn record_event_op(
        &self,
        level:   &str,
        kind:    &str,
        file_id: Option<i64>,
        message: &str,
        detail:  Option<&str>,
    ) -> crate::Result<()> {
        let now = Self::now();
        self.conn.execute(
            "INSERT INTO events (ts, level, kind, file_id, message, detail) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![now, level, kind, file_id, message, detail],
        )?;
        Ok(())
    }

    fn list_files(
        &self,
        status_filter: Option<Status>,
        limit:         i64,
        offset:        i64,
    ) -> crate::Result<Vec<FileRow>> {
        // The NULL-passthrough trick: `(?1 IS NULL OR status = ?1)` acts as an
        // optional filter — when ?1 is NULL the condition is always true.
        let status_str: Option<String> = status_filter.map(|s| s.as_str().to_owned());
        let sql = format!(
            "SELECT {cols} FROM files \
             WHERE (?1 IS NULL OR status = ?1) \
             ORDER BY updated_at DESC \
             LIMIT ?2 OFFSET ?3",
            cols = Self::FILE_COLS,
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params![status_str, limit, offset], |r| {
                Self::map_file_row(r)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn get_file_by_id(&self, id: i64) -> crate::Result<Option<FileRow>> {
        let sql = format!("SELECT {} FROM files WHERE id = ?1", Self::FILE_COLS);
        match self.conn.query_row(&sql, [id], |r| Self::map_file_row(r)) {
            Ok(row)                                     => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows)   => Ok(None),
            Err(e)                                      => Err(e.into()),
        }
    }

    fn get_outputs_for_file(&self, file_id: i64) -> crate::Result<Vec<OutputRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source_id, path, kind, bytes, created_at \
             FROM outputs \
             WHERE source_id = ?1 \
             ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([file_id], |row| {
                let path_str: String = row.get(2)?;
                Ok(OutputRecord {
                    id:         row.get(0)?,
                    source_id:  row.get(1)?,
                    path:       PathBuf::from(path_str),
                    kind:       row.get(3)?,
                    bytes:      row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn get_events(
        &self,
        since: Option<i64>,
        level: Option<String>,
        kind:  Option<String>,
        limit: i64,
    ) -> crate::Result<Vec<AuditEvent>> {
        // NULL-passthrough for each optional filter; avoids dynamic SQL.
        let sql = "SELECT id, ts, level, kind, file_id, message, detail \
                   FROM events \
                   WHERE (?1 IS NULL OR ts    > ?1) \
                     AND (?2 IS NULL OR level = ?2) \
                     AND (?3 IS NULL OR kind  = ?3) \
                   ORDER BY ts DESC \
                   LIMIT ?4";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(rusqlite::params![since, level, kind, limit], |row| {
                Ok(AuditEvent {
                    id:      row.get(0)?,
                    ts:      row.get(1)?,
                    level:   row.get(2)?,
                    kind:    row.get(3)?,
                    file_id: row.get(4)?,
                    message: row.get(5)?,
                    detail:  row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProcessOutput;

    /// Open an in-memory [`StateStore`] for testing.
    async fn open_test_store() -> StateStore {
        // db_open only works with a file path; for tests we open in-memory
        // directly and run migrations, then bypass StateStore::new.
        // We use a temp file instead to exercise the full path.
        let dir  = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        // Keep `dir` alive for the duration via Box::leak (acceptable in tests).
        Box::leak(Box::new(dir));
        StateStore::new(&path, &[30, 300, 1800])
            .await
            .expect("StateStore::new")
    }

    #[tokio::test]
    async fn register_seen_inserts_and_returns_row() {
        let store = open_test_store().await;
        let path  = PathBuf::from("/tmp/test/a.pdf");
        let row   = store
            .register_seen(path.clone(), Some(1024), Some(1_700_000_000), Some(99))
            .await
            .expect("register_seen");
        assert_eq!(row.path, path);
        assert_eq!(row.status, Status::Seen);
        assert_eq!(row.attempts, 0);
        assert!(row.content_hash.is_none());
    }

    #[tokio::test]
    async fn register_seen_is_idempotent() {
        let store = open_test_store().await;
        let path  = PathBuf::from("/tmp/test/b.pdf");
        let r1    = store
            .register_seen(path.clone(), Some(512), None, None)
            .await
            .unwrap();
        let r2    = store
            .register_seen(path.clone(), Some(1024), None, None)
            .await
            .unwrap();
        // Same row — ID must not change; size is NOT updated (INSERT OR IGNORE).
        assert_eq!(r1.id, r2.id);
        assert_eq!(r2.size, Some(512)); // original value preserved
    }

    #[tokio::test]
    async fn set_hash_updates_content_hash() {
        let store = open_test_store().await;
        let row   = store
            .register_seen(PathBuf::from("/tmp/c.pdf"), None, None, None)
            .await
            .unwrap();
        store
            .set_hash(row.id, "sha256:abcdef".into())
            .await
            .unwrap();
        let updated = store.get_file_by_id(row.id).await.unwrap().unwrap();
        assert_eq!(updated.content_hash.as_deref(), Some("sha256:abcdef"));
    }

    #[tokio::test]
    async fn enqueue_transitions_seen_to_queued() {
        let store = open_test_store().await;
        let row   = store
            .register_seen(PathBuf::from("/tmp/d.pdf"), None, None, None)
            .await
            .unwrap();
        store.set_hash(row.id, "sha256:001".into()).await.unwrap();
        let outcome = store.enqueue(row.id).await.unwrap();
        assert_eq!(outcome, EnqueueOutcome::Queued);
        let updated = store.get_file_by_id(row.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Queued);
    }

    #[tokio::test]
    async fn enqueue_returns_already_pending_when_queued() {
        let store = open_test_store().await;
        let row   = store
            .register_seen(PathBuf::from("/tmp/e.pdf"), None, None, None)
            .await
            .unwrap();
        store.set_hash(row.id, "sha256:002".into()).await.unwrap();
        store.enqueue(row.id).await.unwrap();
        let outcome = store.enqueue(row.id).await.unwrap();
        assert_eq!(outcome, EnqueueOutcome::AlreadyPending);
    }

    #[tokio::test]
    async fn enqueue_skips_duplicate_hash() {
        let store = open_test_store().await;
        let hash  = "sha256:dup";

        // First file: queue + claim + mark done.
        let r1 = store
            .register_seen(PathBuf::from("/tmp/f1.pdf"), None, None, None)
            .await
            .unwrap();
        store.set_hash(r1.id, hash.into()).await.unwrap();
        store.enqueue(r1.id).await.unwrap();
        let claimed = store.claim_next().await.unwrap().expect("should claim");
        assert_eq!(claimed.id, r1.id);
        store.mark_done(r1.id, vec![], None).await.unwrap();

        // Second file with same hash: should be skipped.
        let r2 = store
            .register_seen(PathBuf::from("/tmp/f2.pdf"), None, None, None)
            .await
            .unwrap();
        store.set_hash(r2.id, hash.into()).await.unwrap();
        let outcome = store.enqueue(r2.id).await.unwrap();
        assert_eq!(outcome, EnqueueOutcome::SkippedDuplicate);
        let row2 = store.get_file_by_id(r2.id).await.unwrap().unwrap();
        assert_eq!(row2.status, Status::Skipped);
    }

    #[tokio::test]
    async fn claim_next_returns_none_when_empty() {
        let store  = open_test_store().await;
        let result = store.claim_next().await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn claim_next_increments_attempts() {
        let store = open_test_store().await;
        let row   = store
            .register_seen(PathBuf::from("/tmp/g.pdf"), None, None, None)
            .await
            .unwrap();
        store.set_hash(row.id, "sha256:003".into()).await.unwrap();
        store.enqueue(row.id).await.unwrap();
        let claimed = store.claim_next().await.unwrap().unwrap();
        assert_eq!(claimed.status, Status::Processing);
        assert_eq!(claimed.attempts, 1);
    }

    #[tokio::test]
    async fn mark_done_records_outputs() {
        let store = open_test_store().await;
        let row   = store
            .register_seen(PathBuf::from("/tmp/h.pdf"), None, None, None)
            .await
            .unwrap();
        store.set_hash(row.id, "sha256:004".into()).await.unwrap();
        store.enqueue(row.id).await.unwrap();
        store.claim_next().await.unwrap();

        let outputs = vec![ProcessOutput {
            path:  PathBuf::from("/vault/Notes/h.md"),
            kind:  "markdown".into(),
            bytes: 1024,
        }];
        store.mark_done(row.id, outputs, None).await.unwrap();

        let updated = store.get_file_by_id(row.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Done);
        assert!(updated.processed_at.is_some());

        let outs = store.get_outputs_for_file(row.id).await.unwrap();
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].kind.as_deref(), Some("markdown"));
    }

    #[tokio::test]
    async fn mark_failed_retryable_with_backoff() {
        // backoff_secs = [] means the very first retryable failure is terminal.
        let dir   = tempfile::tempdir().unwrap();
        let path  = dir.path().join("x.db");
        let store = StateStore::new(&path, &[]).await.unwrap();

        let row = store
            .register_seen(PathBuf::from("/tmp/i.pdf"), None, None, None)
            .await
            .unwrap();
        store.set_hash(row.id, "sha256:005".into()).await.unwrap();
        store.enqueue(row.id).await.unwrap();
        store.claim_next().await.unwrap().expect("should claim job");

        // retryable=true but backoff_secs is empty → attempt_idx 0 >= 0 → terminal.
        store
            .mark_failed(row.id, "transient error".into(), true)
            .await
            .unwrap();
        let r = store.get_file_by_id(row.id).await.unwrap().unwrap();
        assert_eq!(r.status, Status::Failed, "empty backoff_secs → terminal");
        assert!(r.next_attempt_at.is_none());
    }

    #[tokio::test]
    async fn mark_failed_retryable_queues_with_next_attempt() {
        // backoff_secs = [60] gives one retry window.
        let dir   = tempfile::tempdir().unwrap();
        let path  = dir.path().join("y.db");
        let store = StateStore::new(&path, &[60]).await.unwrap();

        let row = store
            .register_seen(PathBuf::from("/tmp/ii.pdf"), None, None, None)
            .await
            .unwrap();
        store.set_hash(row.id, "sha256:005b".into()).await.unwrap();
        store.enqueue(row.id).await.unwrap();
        store.claim_next().await.unwrap().expect("should claim job");

        // After first attempt (attempts=1), attempt_idx=0, backoff_secs[0]=60 → re-queue.
        store
            .mark_failed(row.id, "transient".into(), true)
            .await
            .unwrap();
        let r = store.get_file_by_id(row.id).await.unwrap().unwrap();
        assert_eq!(r.status, Status::Queued, "first attempt with backoff → re-queued");
        assert!(r.next_attempt_at.is_some(), "should have a future next_attempt_at");
        let nat = r.next_attempt_at.unwrap();
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        assert!(nat > now_secs, "next_attempt_at must be in the future");
    }

    #[tokio::test]
    async fn mark_failed_non_retryable_is_terminal() {
        let store = open_test_store().await;
        let row   = store
            .register_seen(PathBuf::from("/tmp/j.pdf"), None, None, None)
            .await
            .unwrap();
        store.set_hash(row.id, "sha256:006".into()).await.unwrap();
        store.enqueue(row.id).await.unwrap();
        store.claim_next().await.unwrap();
        store
            .mark_failed(row.id, "non-retryable error".into(), false)
            .await
            .unwrap();
        let updated = store.get_file_by_id(row.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Failed);
        assert_eq!(updated.last_error.as_deref(), Some("non-retryable error"));
        assert!(updated.next_attempt_at.is_none());
    }

    #[tokio::test]
    async fn recover_in_flight_resets_processing_rows() {
        let store = open_test_store().await;
        let row   = store
            .register_seen(PathBuf::from("/tmp/k.pdf"), None, None, None)
            .await
            .unwrap();
        store.set_hash(row.id, "sha256:007".into()).await.unwrap();
        store.enqueue(row.id).await.unwrap();
        store.claim_next().await.unwrap(); // now `processing`

        let count = store.recover_in_flight().await.unwrap();
        assert_eq!(count, 1);

        let recovered = store.get_file_by_id(row.id).await.unwrap().unwrap();
        assert_eq!(recovered.status, Status::Queued);
    }

    #[tokio::test]
    async fn stats_returns_correct_counts() {
        let store = open_test_store().await;

        for i in 0..3u32 {
            let r = store
                .register_seen(PathBuf::from(format!("/tmp/s{i}.pdf")), None, None, None)
                .await
                .unwrap();
            store
                .set_hash(r.id, format!("sha256:{i:03}"))
                .await
                .unwrap();
            store.enqueue(r.id).await.unwrap();
        }

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.queued, 3);
        assert_eq!(stats.queue_depth, 3);
        assert!(stats.oldest_pending_age_secs.is_some());
    }

    #[tokio::test]
    async fn record_event_and_get_events() {
        let store = open_test_store().await;
        store
            .record_event(
                "info".into(),
                "test_kind".into(),
                None,
                "hello world".into(),
                None,
            )
            .await
            .unwrap();
        let events = store.get_events(None, None, None, 10).await.unwrap();
        assert!(!events.is_empty());
        let ev = events.iter().find(|e| e.kind == "test_kind").unwrap();
        assert_eq!(ev.message, "hello world");
    }

    #[tokio::test]
    async fn get_events_filter_by_kind() {
        let store = open_test_store().await;
        store
            .record_event("info".into(), "alpha".into(), None, "msg1".into(), None)
            .await
            .unwrap();
        store
            .record_event("warn".into(), "beta".into(), None, "msg2".into(), None)
            .await
            .unwrap();
        let filtered = store
            .get_events(None, None, Some("alpha".into()), 10)
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].kind, "alpha");
    }

    #[tokio::test]
    async fn list_files_with_status_filter() {
        let store = open_test_store().await;
        for i in 0..4u32 {
            let r = store
                .register_seen(PathBuf::from(format!("/tmp/l{i}.pdf")), None, None, None)
                .await
                .unwrap();
            if i < 2 {
                store
                    .set_hash(r.id, format!("sha256:lf{i}"))
                    .await
                    .unwrap();
                store.enqueue(r.id).await.unwrap();
            }
        }
        let queued = store
            .list_files(Some(Status::Queued), 100, 0)
            .await
            .unwrap();
        assert_eq!(queued.len(), 2);
        let seen = store
            .list_files(Some(Status::Seen), 100, 0)
            .await
            .unwrap();
        assert_eq!(seen.len(), 2);
        let all = store.list_files(None, 100, 0).await.unwrap();
        assert_eq!(all.len(), 4);
    }

    #[tokio::test]
    async fn find_by_path_and_find_by_hash() {
        let store = open_test_store().await;
        let path  = PathBuf::from("/tmp/m.pdf");
        let hash  = "sha256:findme";
        let row   = store
            .register_seen(path.clone(), None, None, None)
            .await
            .unwrap();
        store.set_hash(row.id, hash.into()).await.unwrap();

        let by_path = store.find_by_path(path.clone()).await.unwrap().unwrap();
        assert_eq!(by_path.id, row.id);

        let by_hash = store.find_by_hash(hash.into()).await.unwrap();
        assert_eq!(by_hash.len(), 1);
        assert_eq!(by_hash[0].id, row.id);

        let missing = store
            .find_by_path(PathBuf::from("/no/such.pdf"))
            .await
            .unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn requeue_revision_on_done_row() {
        let store = open_test_store().await;
        let row   = store
            .register_seen(PathBuf::from("/tmp/n.pdf"), None, None, None)
            .await
            .unwrap();
        store.set_hash(row.id, "sha256:v1".into()).await.unwrap();
        store.enqueue(row.id).await.unwrap();
        store.claim_next().await.unwrap();
        store.mark_done(row.id, vec![], None).await.unwrap();

        // Simulate hash change (new file content).
        store.set_hash(row.id, "sha256:v2".into()).await.unwrap();
        let outcome = store.enqueue(row.id).await.unwrap();
        assert_eq!(outcome, EnqueueOutcome::RequeuedRevision);
        let updated = store.get_file_by_id(row.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Queued);
    }
}
