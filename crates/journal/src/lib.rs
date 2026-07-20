//! Drift journal (spec §8): SQLite (WAL, single writer = daemon) +
//! content-addressed zstd blob store. Timestamps come from callers.

use std::path::Path;

use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("journal corrupt: {0}")]
    Corrupt(String),
}

pub struct Journal {
    pub(crate) conn: Connection,
    machine: String,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS entries (
  id INTEGER PRIMARY KEY,
  target_path TEXT NOT NULL UNIQUE,
  source_path TEXT,
  kind TEXT NOT NULL DEFAULT 'file',
  managed INTEGER NOT NULL DEFAULT 1,
  unmanaged_at INTEGER
);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY,
  entry_id INTEGER REFERENCES entries(id),
  ts INTEGER NOT NULL,
  machine TEXT NOT NULL,
  kind TEXT NOT NULL,
  from_hash TEXT,
  to_hash TEXT,
  meta TEXT
);
CREATE INDEX IF NOT EXISTS events_entry_ts ON events(entry_id, ts);
CREATE INDEX IF NOT EXISTS events_ts ON events(ts);
CREATE TABLE IF NOT EXISTS blobs (
  hash TEXT PRIMARY KEY,
  content_zstd BLOB NOT NULL,
  size_raw INTEGER NOT NULL,
  created_ts INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
  id INTEGER PRIMARY KEY,
  started_ts INTEGER NOT NULL,
  finished_ts INTEGER,
  summary TEXT,
  decisions TEXT NOT NULL DEFAULT '[]'
);
";

impl Journal {
    pub fn open(path: &Path, machine: &str) -> Result<Self, JournalError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn, machine)
    }

    pub fn open_in_memory(machine: &str) -> Result<Self, JournalError> {
        Self::init(Connection::open_in_memory()?, machine)
    }

    fn init(conn: Connection, machine: &str) -> Result<Self, JournalError> {
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', '1')
             ON CONFLICT(key) DO NOTHING",
            [],
        )?;
        Ok(Self {
            conn,
            machine: machine.to_string(),
        })
    }

    pub fn schema_version(&self) -> Result<u32, JournalError> {
        let v: String = self.conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )?;
        v.parse()
            .map_err(|_| JournalError::Corrupt(format!("bad schema_version: {v}")))
    }

    pub fn machine(&self) -> &str {
        &self.machine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_schema_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        {
            let j = Journal::open(&path, "test-machine").unwrap();
            assert_eq!(j.schema_version().unwrap(), 1);
            assert_eq!(j.machine(), "test-machine");
        }
        // reopen: no error, same version
        let j = Journal::open(&path, "test-machine").unwrap();
        assert_eq!(j.schema_version().unwrap(), 1);
    }

    #[test]
    fn file_db_uses_wal() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(&dir.path().join("j.db"), "m").unwrap();
        let mode: String = j
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn in_memory_open_works() {
        let j = Journal::open_in_memory("m").unwrap();
        assert_eq!(j.schema_version().unwrap(), 1);
    }
}
