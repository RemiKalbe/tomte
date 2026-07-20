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

    pub fn put_blob(&self, content: &[u8], now_ts: u64) -> Result<String, JournalError> {
        let hash = czui_core::drift::ContentHash::of(content).to_hex();
        let compressed = zstd::encode_all(content, 3)
            .map_err(|e| JournalError::Corrupt(format!("zstd encode: {e}")))?;
        self.conn.execute(
            "INSERT INTO blobs(hash, content_zstd, size_raw, created_ts)
             VALUES(?1, ?2, ?3, ?4) ON CONFLICT(hash) DO NOTHING",
            rusqlite::params![hash, compressed, content.len() as i64, now_ts as i64],
        )?;
        Ok(hash)
    }

    pub fn get_blob(&self, hash: &str) -> Result<Option<Vec<u8>>, JournalError> {
        let row: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT content_zstd FROM blobs WHERE hash = ?1",
                [hash],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        match row {
            Some(compressed) => {
                let raw = zstd::decode_all(compressed.as_slice())
                    .map_err(|e| JournalError::Corrupt(format!("zstd decode for {hash}: {e}")))?;
                Ok(Some(raw))
            }
            None => Ok(None),
        }
    }

    pub fn has_blob(&self, hash: &str) -> Result<bool, JournalError> {
        let n: u32 =
            self.conn
                .query_row("SELECT COUNT(*) FROM blobs WHERE hash = ?1", [hash], |r| {
                    r.get(0)
                })?;
        Ok(n > 0)
    }

    pub fn blob_store_size(&self) -> Result<u64, JournalError> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(LENGTH(content_zstd)), 0) FROM blobs",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u64)
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

    #[test]
    fn blob_roundtrip_and_dedup() {
        let j = Journal::open_in_memory("m").unwrap();
        let content = b"export EDITOR=hx\nexport VISUAL=hx\n".repeat(50);
        let h1 = j.put_blob(&content, 100).unwrap();
        let h2 = j.put_blob(&content, 200).unwrap();
        assert_eq!(h1, h2, "same content must dedup to the same hash");
        let n: u32 = j
            .conn
            .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(j.get_blob(&h1).unwrap().unwrap(), content);
        assert!(j.has_blob(&h1).unwrap());
        assert!(!j.has_blob("deadbeef").unwrap());
        assert!(j.get_blob("deadbeef").unwrap().is_none());
        // repetitive content must actually compress
        assert!(j.blob_store_size().unwrap() < content.len() as u64);
    }

    #[test]
    fn blob_hash_matches_core_content_hash() {
        let j = Journal::open_in_memory("m").unwrap();
        let h = j.put_blob(b"abc", 1).unwrap();
        assert_eq!(h, czui_core::drift::ContentHash::of(b"abc").to_hex());
    }
}
