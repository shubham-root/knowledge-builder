//! SQLite schema migrations for Knowledge Builder.
//!
//! # Design
//! - Migrations are numbered, append-only SQL strings stored in [`MIGRATIONS`].
//! - [`run_migrations`] is idempotent and safe to call on every startup.
//! - [`db_open`] is the preferred entry-point: opens the file, sets pragmas,
//!   runs all pending migrations, and returns a ready-to-use [`Connection`].
//!
//! # Adding a new migration
//! 1. Write `const MIGRATION_00N: &str = r#"…"#;`
//! 2. Append `(N, MIGRATION_00N)` to [`MIGRATIONS`].
//! 3. Never remove, reorder, or edit existing entries.

use std::path::Path;

use rusqlite::Connection;

// ─── Public API ──────────────────────────────────────────────────────────────

/// Open (or create) the SQLite database at `path`, run all pending migrations,
/// and return a ready-to-use [`Connection`].
///
/// The parent directory is created if it does not exist.
///
/// This is the preferred way to obtain a database connection.  The returned
/// connection has WAL journal mode, `synchronous = NORMAL`, and
/// `foreign_keys = ON` already applied.
///
/// # Errors
/// Returns an error if the directory cannot be created, the database cannot be
/// opened, or any migration fails.
pub fn db_open(path: &Path) -> crate::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    run_migrations(&conn)?;
    Ok(conn)
}

/// Apply all pending migrations to an already-open [`Connection`].
///
/// This function:
/// 1. Sets pragmas: `journal_mode = WAL`, `synchronous = NORMAL`,
///    `foreign_keys = ON`.
/// 2. Creates the `schema_version` table if it does not already exist.
/// 3. Queries the highest applied version.
/// 4. For each entry in [`MIGRATIONS`] whose version is higher than the
///    current, executes the SQL inside a transaction and records the new
///    version in `schema_version`.
///
/// Safe to call multiple times on the same connection — already-applied
/// migrations are skipped.
///
/// # Errors
/// Returns an error if any pragma, DDL statement, or version insert fails.
pub fn run_migrations(conn: &Connection) -> crate::Result<()> {
    // ── 1. Pragmas ────────────────────────────────────────────────────────
    // Must be set before any table access; WAL mode persists in the DB file.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;\
         PRAGMA synchronous  = NORMAL;\
         PRAGMA foreign_keys = ON;",
    )?;

    // ── 2. Bootstrap schema_version ───────────────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version    INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );",
    )?;

    // ── 3. Check current schema version ──────────────────────────────────
    let current_version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // ── 4. Apply pending migrations in order ─────────────────────────────
    for &(version, sql) in MIGRATIONS {
        if version <= current_version {
            continue;
        }

        // Each migration runs in its own transaction so that a partial failure
        // leaves the schema at the last successfully-applied version.
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![version, chrono::Utc::now().timestamp()],
        )?;
        tx.commit()?;

        tracing::info!(migration_version = version, "applied db migration");
    }

    Ok(())
}

/// Backward-compatible alias — existing callers in `state.rs` use `migrations::run()`.
#[inline]
pub fn run(conn: &Connection) -> crate::Result<()> {
    run_migrations(conn)
}

// ─── Migration registry ───────────────────────────────────────────────────────

/// All schema migrations in ascending version order.
///
/// **Never** remove, reorder, or modify an existing entry.
/// To evolve the schema, append a new `(version, sql)` pair.
const MIGRATIONS: &[(i64, &str)] = &[(1, MIGRATION_001)];

// ─── Migration SQL ────────────────────────────────────────────────────────────

/// v1: Create all tables and indexes described in §7 of PLAN.md.
const MIGRATION_001: &str = r#"
-- ── files ────────────────────────────────────────────────────────────────────
-- Each row represents one source file discovered in the watched folder.
CREATE TABLE IF NOT EXISTS files (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    path            TEXT    NOT NULL UNIQUE,
    -- "sha256:<hex>"; NULL until the file is stable and hashed.
    content_hash    TEXT,
    size            INTEGER,
    mtime_ns        INTEGER,
    inode           INTEGER,
    status          TEXT    NOT NULL
                        CHECK (status IN (
                            'seen',
                            'queued',
                            'processing',
                            'done',
                            'failed',
                            'skipped'
                        )),
    attempts        INTEGER NOT NULL DEFAULT 0,
    -- Unix epoch seconds; NULL means "ready now".
    next_attempt_at INTEGER,
    last_error      TEXT,
    first_seen_at   INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    processed_at    INTEGER,
    -- Arbitrary JSON from the processor (model names, token counts, …).
    processor_meta  TEXT
);

CREATE INDEX IF NOT EXISTS idx_files_status
    ON files(status);
CREATE INDEX IF NOT EXISTS idx_files_hash
    ON files(content_hash);
-- Composite: worker scans WHERE status='queued' AND next_attempt_at <= now.
CREATE INDEX IF NOT EXISTS idx_files_next_attempt
    ON files(status, next_attempt_at);

-- ── outputs ───────────────────────────────────────────────────────────────────
-- Each row represents one output file produced by processing a source.
CREATE TABLE IF NOT EXISTS outputs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id       INTEGER NOT NULL
                        REFERENCES files(id) ON DELETE CASCADE,
    path            TEXT    NOT NULL,
    -- "markdown" | "asset" | …
    kind            TEXT,
    bytes           INTEGER,
    created_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_outputs_source
    ON outputs(source_id);
CREATE INDEX IF NOT EXISTS idx_outputs_path
    ON outputs(path);

-- ── events ────────────────────────────────────────────────────────────────────
-- Append-only audit log for operational visibility.
CREATE TABLE IF NOT EXISTS events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    ts              INTEGER NOT NULL,
    -- "info" | "warn" | "error"
    level           TEXT    NOT NULL,
    -- "discovered" | "queued" | "claimed" | "done" | "failed" | "recovered" | …
    kind            TEXT    NOT NULL,
    file_id         INTEGER
                        REFERENCES files(id) ON DELETE SET NULL,
    message         TEXT    NOT NULL,
    -- Arbitrary JSON payload (optional).
    detail          TEXT
);

CREATE INDEX IF NOT EXISTS idx_events_ts
    ON events(ts);
CREATE INDEX IF NOT EXISTS idx_events_file_id
    ON events(file_id);
"#;

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn open_mem() -> Connection {
        Connection::open_in_memory().expect("in-memory db")
    }

    /// Migration v1 applies without error on a fresh database.
    #[test]
    fn migration_v1_runs_cleanly() {
        let conn = open_mem();
        run_migrations(&conn).expect("run_migrations failed");

        // schema_version must contain exactly 1 row.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "schema_version should have 1 row after v1");

        // The recorded version must be 1.
        let ver: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 1);
    }

    /// All three domain tables must exist after migration.
    #[test]
    fn all_tables_created() {
        let conn = open_mem();
        run_migrations(&conn).unwrap();

        for table in &["files", "outputs", "events"] {
            let n: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table}"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "table '{table}' should be empty but accessible");
        }
    }

    /// Exactly 7 named indexes (those matching `idx_%`) must be present.
    ///
    /// Expected:
    ///   idx_files_status, idx_files_hash, idx_files_next_attempt,
    ///   idx_outputs_source, idx_outputs_path,
    ///   idx_events_ts, idx_events_file_id
    #[test]
    fn index_count_is_seven() {
        let conn = open_mem();
        run_migrations(&conn).unwrap();

        let idx_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='index' AND name LIKE 'idx_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx_count, 7, "expected 7 named indexes, got {idx_count}");
    }

    /// Running migrations twice must be idempotent — no extra rows, no error.
    #[test]
    fn run_migrations_is_idempotent() {
        let conn = open_mem();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap(); // second call must succeed

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "schema_version must still have exactly 1 row");
    }

    /// `db_open` on a temp file must create the DB and run migrations.
    #[test]
    fn db_open_creates_and_migrates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("subdir").join("state.db");

        // The parent subdirectory must not exist yet — db_open must create it.
        assert!(!db_path.parent().unwrap().exists());

        let conn = db_open(&db_path).expect("db_open failed");

        let ver: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 1);
    }

    /// The `files` table must enforce the status CHECK constraint.
    #[test]
    fn files_status_check_constraint() {
        let conn = open_mem();
        run_migrations(&conn).unwrap();

        let now = chrono::Utc::now().timestamp();

        // Valid status values must be accepted.
        for status in &["seen", "queued", "processing", "done", "failed", "skipped"] {
            conn.execute(
                "INSERT INTO files \
                 (path, status, first_seen_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![format!("/tmp/{status}.pdf"), status, now, now],
            )
            .unwrap_or_else(|e| panic!("valid status '{status}' rejected: {e}"));
        }

        // An invalid status must be rejected.
        let result = conn.execute(
            "INSERT INTO files \
             (path, status, first_seen_at, updated_at) \
             VALUES ('/tmp/bad.pdf', 'invalid_status', ?1, ?2)",
            rusqlite::params![now, now],
        );
        assert!(
            result.is_err(),
            "INSERT with invalid status should have been rejected"
        );
    }

    /// The `outputs.source_id` FK must cascade-delete when the parent row is removed.
    #[test]
    fn outputs_cascade_delete_on_source_removed() {
        let conn = open_mem();
        run_migrations(&conn).unwrap();

        let now = chrono::Utc::now().timestamp();

        conn.execute(
            "INSERT INTO files (path, status, first_seen_at, updated_at) \
             VALUES ('/tmp/src.pdf', 'done', ?1, ?2)",
            rusqlite::params![now, now],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO outputs (source_id, path, created_at) \
             VALUES (?1, '/tmp/note.md', ?2)",
            rusqlite::params![file_id, now],
        )
        .unwrap();

        // Deleting the parent must cascade.
        conn.execute("DELETE FROM files WHERE id = ?1", [file_id])
            .unwrap();

        let orphan_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM outputs WHERE source_id = ?1",
                [file_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphan_count, 0, "cascade delete should remove output rows");
    }

    /// The `events.file_id` FK must SET NULL when the parent row is removed.
    #[test]
    fn events_set_null_on_source_removed() {
        let conn = open_mem();
        run_migrations(&conn).unwrap();

        let now = chrono::Utc::now().timestamp();

        conn.execute(
            "INSERT INTO files (path, status, first_seen_at, updated_at) \
             VALUES ('/tmp/ev.pdf', 'done', ?1, ?2)",
            rusqlite::params![now, now],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO events (ts, level, kind, file_id, message) \
             VALUES (?1, 'info', 'done', ?2, 'processed')",
            rusqlite::params![now, file_id],
        )
        .unwrap();
        let event_id = conn.last_insert_rowid();

        // Deleting the parent should NULL-out the FK.
        conn.execute("DELETE FROM files WHERE id = ?1", [file_id])
            .unwrap();

        let fk_val: Option<i64> = conn
            .query_row(
                "SELECT file_id FROM events WHERE id = ?1",
                [event_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(fk_val.is_none(), "file_id should be NULL after source deleted");
    }

    /// The `run` alias must call `run_migrations` correctly.
    #[test]
    fn run_alias_works() {
        let conn = open_mem();
        super::run(&conn).expect("`run` alias failed");
        let ver: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 1);
    }
}
