//! SQLite schema migrations for Knowledge Builder.
//!
//! Each migration is a `(version, sql)` pair.  The [`run`] function applies
//! any unapplied migrations in version order, inside a single transaction.
//! This is intentionally simple: migrations are append-only and
//! non-destructive in v1.

use rusqlite::Connection;

/// Apply all pending migrations to `conn`.
///
/// Creates the `schema_version` table on the first run, then applies
/// each migration whose version number is greater than the currently
/// recorded version.
pub fn run(conn: &Connection) -> crate::Result<()> {
    // WAL mode + synchronous=NORMAL for performance with crash safety.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous  = NORMAL;
         PRAGMA foreign_keys = ON;",
    )?;

    // Bootstrap the version table if it doesn't exist yet.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version    INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );",
    )?;

    let current_version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    for (version, sql) in MIGRATIONS {
        if *version <= current_version {
            continue;
        }

        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![version, chrono::Utc::now().timestamp()],
        )?;
        tx.commit()?;

        tracing::info!(migration_version = version, "applied migration");
    }

    Ok(())
}

/// All schema migrations in order.  Never remove or reorder entries.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, MIGRATION_001),
];

const MIGRATION_001: &str = r#"
-- Sources discovered in the watched folder.
CREATE TABLE IF NOT EXISTS files (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  path            TEXT    NOT NULL UNIQUE,
  content_hash    TEXT,
  size            INTEGER,
  mtime_ns        INTEGER,
  inode           INTEGER,
  status          TEXT    NOT NULL
                    CHECK (status IN
                      ('seen','queued','processing','done','failed','skipped')),
  attempts        INTEGER NOT NULL DEFAULT 0,
  next_attempt_at INTEGER,
  last_error      TEXT,
  first_seen_at   INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  processed_at    INTEGER,
  processor_meta  TEXT
);

CREATE INDEX IF NOT EXISTS idx_files_status
  ON files(status);
CREATE INDEX IF NOT EXISTS idx_files_hash
  ON files(content_hash);
CREATE INDEX IF NOT EXISTS idx_files_next_attempt
  ON files(status, next_attempt_at);

-- Outputs produced by processing a source.
CREATE TABLE IF NOT EXISTS outputs (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id  INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  path       TEXT    NOT NULL,
  kind       TEXT,
  bytes      INTEGER,
  created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_outputs_source ON outputs(source_id);
CREATE INDEX IF NOT EXISTS idx_outputs_path   ON outputs(path);

-- Audit log for ops visibility.
CREATE TABLE IF NOT EXISTS events (
  id      INTEGER PRIMARY KEY AUTOINCREMENT,
  ts      INTEGER NOT NULL,
  level   TEXT    NOT NULL,
  kind    TEXT    NOT NULL,
  file_id INTEGER REFERENCES files(id) ON DELETE SET NULL,
  message TEXT    NOT NULL,
  detail  TEXT
);

CREATE INDEX IF NOT EXISTS idx_events_ts      ON events(ts);
CREATE INDEX IF NOT EXISTS idx_events_file_id ON events(file_id);
"#;
