//! SQLite state store — single-writer actor pattern.
//!
//! The `StateStore` struct owns a `rusqlite::Connection` and is the only
//! entity that writes to the database.  All other components communicate
//! with it via an async `mpsc` channel (see the actor task in `spawn`).
//!
//! For the scaffold phase this module exposes the public API surface so that
//! downstream crates compile.  Full implementations will be added in T5.

use std::path::Path;
use rusqlite::Connection;
use crate::migrations;

/// Outcome of attempting to enqueue a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// Row is now `queued`.
    Queued,
    /// Another row with the same content hash is `done`; this row is `skipped`.
    SkippedDuplicate,
    /// Row was already `queued` or `processing`; no change made.
    AlreadyPending,
    /// Row was already `done` with the same hash; no change needed.
    AlreadyDone,
}

/// Synchronous handle to the SQLite state store.
///
/// In the full implementation this will be wrapped in a tokio actor task.
/// For now it exposes a direct, synchronous API so scaffold code compiles.
pub struct StateStore {
    conn: Connection,
}

impl StateStore {
    /// Open (or create) the database at `db_path`, run migrations, and return
    /// a new `StateStore`.
    pub fn open(db_path: impl AsRef<Path>) -> crate::Result<Self> {
        let path = db_path.as_ref();

        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        migrations::run(&conn)?;

        Ok(Self { conn })
    }

    /// Open an in-memory database — used in unit tests.
    pub fn open_in_memory() -> crate::Result<Self> {
        let conn = Connection::open_in_memory()?;
        migrations::run(&conn)?;
        Ok(Self { conn })
    }

    /// Return aggregate counts per status.
    pub fn stats(&self) -> crate::Result<crate::types::Stats> {
        use crate::types::Stats;
        let mut stmt = self.conn.prepare(
            "SELECT status, COUNT(*) FROM files GROUP BY status",
        )?;
        let mut stats = Stats::default();
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (status, count) = row?;
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
        Ok(stats)
    }

    /// Record an audit event into the `events` table.
    pub fn record_event(
        &self,
        level:   &str,
        kind:    &str,
        file_id: Option<i64>,
        message: &str,
        detail:  Option<&serde_json::Value>,
    ) -> crate::Result<()> {
        self.conn.execute(
            "INSERT INTO events (ts, level, kind, file_id, message, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                chrono::Utc::now().timestamp(),
                level,
                kind,
                file_id,
                message,
                detail.map(|d| d.to_string()),
            ],
        )?;
        Ok(())
    }

    /// Expose the raw connection for use in migrations / tests.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}
