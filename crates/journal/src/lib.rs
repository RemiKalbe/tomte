//! Drift journal (spec §8): SQLite (WAL, single writer = daemon) +
//! content-addressed zstd blob store. Timestamps come from callers.

use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    DestChanged,
    SourceChanged,
    RemoteAdvanced,
    Applied,
    Readded,
    Resolved,
    EvalFailed,
    Fetch,
    LeftManagement,
    SessionStart,
    SessionEnd,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DestChanged => "dest_changed",
            Self::SourceChanged => "source_changed",
            Self::RemoteAdvanced => "remote_advanced",
            Self::Applied => "applied",
            Self::Readded => "readded",
            Self::Resolved => "resolved",
            Self::EvalFailed => "eval_failed",
            Self::Fetch => "fetch",
            Self::LeftManagement => "left_management",
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "dest_changed" => Self::DestChanged,
            "source_changed" => Self::SourceChanged,
            "remote_advanced" => Self::RemoteAdvanced,
            "applied" => Self::Applied,
            "readded" => Self::Readded,
            "resolved" => Self::Resolved,
            "eval_failed" => Self::EvalFailed,
            "fetch" => Self::Fetch,
            "left_management" => Self::LeftManagement,
            "session_start" => Self::SessionStart,
            "session_end" => Self::SessionEnd,
            _ => return None,
        })
    }
}

#[derive(Debug)]
pub struct NewEvent<'a> {
    pub target: Option<&'a Path>,
    pub ts: u64,
    pub kind: EventKind,
    pub from_hash: Option<&'a str>,
    pub to_hash: Option<&'a str>,
    pub meta: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub id: i64,
    pub target: Option<PathBuf>,
    pub ts: u64,
    pub machine: String,
    pub kind: String,
    pub from_hash: Option<String>,
    pub to_hash: Option<String>,
    pub meta: Option<serde_json::Value>,
}

const EVENT_SELECT: &str = "
SELECT e.id, en.target_path, e.ts, e.machine, e.kind, e.from_hash, e.to_hash, e.meta
FROM events e LEFT JOIN entries en ON en.id = e.entry_id
";

fn row_to_event(r: &rusqlite::Row) -> rusqlite::Result<EventRow> {
    let meta_text: Option<String> = r.get(7)?;
    Ok(EventRow {
        id: r.get(0)?,
        target: r.get::<_, Option<String>>(1)?.map(PathBuf::from),
        ts: r.get::<_, i64>(2)? as u64,
        machine: r.get(3)?,
        kind: r.get(4)?,
        from_hash: r.get(5)?,
        to_hash: r.get(6)?,
        meta: meta_text.and_then(|t| serde_json::from_str(&t).ok()),
    })
}

impl Journal {
    pub fn upsert_entry(
        &self,
        target: &Path,
        source: Option<&Path>,
        kind: &str,
    ) -> Result<i64, JournalError> {
        self.conn.execute(
            "INSERT INTO entries(target_path, source_path, kind, managed, unmanaged_at)
             VALUES(?1, ?2, ?3, 1, NULL)
             ON CONFLICT(target_path) DO UPDATE SET
               source_path = excluded.source_path,
               kind = excluded.kind,
               managed = 1,
               unmanaged_at = NULL",
            rusqlite::params![
                target.to_string_lossy(),
                source.map(|p| p.to_string_lossy().into_owned()),
                kind
            ],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM entries WHERE target_path = ?1",
            [target.to_string_lossy()],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn mark_unmanaged(&self, target: &Path, ts: u64) -> Result<(), JournalError> {
        self.conn.execute(
            "UPDATE entries SET managed = 0, unmanaged_at = ?2 WHERE target_path = ?1",
            rusqlite::params![target.to_string_lossy(), ts as i64],
        )?;
        Ok(())
    }

    /// Lookup-or-create that never touches an existing entry's source_path —
    /// record_event must not clobber metadata set by upsert_entry.
    fn entry_id_for(&self, target: &Path) -> Result<i64, JournalError> {
        self.conn.execute(
            "INSERT INTO entries(target_path) VALUES(?1)
             ON CONFLICT(target_path) DO NOTHING",
            [target.to_string_lossy()],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM entries WHERE target_path = ?1",
            [target.to_string_lossy()],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn record_event(&self, ev: NewEvent) -> Result<i64, JournalError> {
        let entry_id = match ev.target {
            Some(t) => Some(self.entry_id_for(t)?),
            None => None,
        };
        let meta_text = ev.meta.as_ref().map(|m| m.to_string());
        self.conn.execute(
            "INSERT INTO events(entry_id, ts, machine, kind, from_hash, to_hash, meta)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                entry_id,
                ev.ts as i64,
                self.machine,
                ev.kind.as_str(),
                ev.from_hash,
                ev.to_hash,
                meta_text
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn timeline(
        &self,
        limit: u32,
        before_id: Option<i64>,
    ) -> Result<Vec<EventRow>, JournalError> {
        let sql =
            format!("{EVENT_SELECT} WHERE (?1 IS NULL OR e.id < ?1) ORDER BY e.id DESC LIMIT ?2");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params![before_id, limit], row_to_event)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn events_for(&self, target: &Path, limit: u32) -> Result<Vec<EventRow>, JournalError> {
        let sql = format!("{EVENT_SELECT} WHERE en.target_path = ?1 ORDER BY e.id DESC LIMIT ?2");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                rusqlite::params![target.to_string_lossy(), limit],
                row_to_event,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn last_event_hash(&self, target: &Path) -> Result<Option<String>, JournalError> {
        let sql = format!(
            "{EVENT_SELECT} WHERE en.target_path = ?1 AND e.to_hash IS NOT NULL
             ORDER BY e.id DESC LIMIT 1"
        );
        let row = self
            .conn
            .query_row(&sql, [target.to_string_lossy()], row_to_event)
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(row.and_then(|r| r.to_hash))
    }

    pub fn begin_session(&self, ts: u64) -> Result<i64, JournalError> {
        self.conn
            .execute("INSERT INTO sessions(started_ts) VALUES(?1)", [ts as i64])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn add_decision(
        &self,
        session: i64,
        decision: &serde_json::Value,
    ) -> Result<(), JournalError> {
        let current: String = self.conn.query_row(
            "SELECT decisions FROM sessions WHERE id = ?1",
            [session],
            |r| r.get(0),
        )?;
        let mut arr: serde_json::Value = serde_json::from_str(&current)
            .map_err(|e| JournalError::Corrupt(format!("session {session} decisions: {e}")))?;
        arr.as_array_mut()
            .ok_or_else(|| {
                JournalError::Corrupt(format!("session {session} decisions not an array"))
            })?
            .push(decision.clone());
        self.conn.execute(
            "UPDATE sessions SET decisions = ?2 WHERE id = ?1",
            rusqlite::params![session, arr.to_string()],
        )?;
        Ok(())
    }

    pub fn end_session(&self, session: i64, ts: u64, summary: &str) -> Result<(), JournalError> {
        self.conn.execute(
            "UPDATE sessions SET finished_ts = ?2, summary = ?3 WHERE id = ?1",
            rusqlite::params![session, ts as i64, summary],
        )?;
        Ok(())
    }

    pub fn gc_blobs(&self) -> Result<u32, JournalError> {
        let n = self.conn.execute(
            "DELETE FROM blobs WHERE hash NOT IN (
               SELECT from_hash FROM events WHERE from_hash IS NOT NULL
               UNION
               SELECT to_hash FROM events WHERE to_hash IS NOT NULL
             )",
            [],
        )?;
        Ok(n as u32)
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

    use std::path::Path;

    #[test]
    fn record_and_query_events() {
        let j = Journal::open_in_memory("mac-a").unwrap();
        let t = Path::new("/home/u/.zshrc");
        let h1 = j.put_blob(b"v1", 10).unwrap();
        let h2 = j.put_blob(b"v2", 20).unwrap();
        j.record_event(NewEvent {
            target: Some(t),
            ts: 10,
            kind: EventKind::Applied,
            from_hash: None,
            to_hash: Some(&h1),
            meta: None,
        })
        .unwrap();
        j.record_event(NewEvent {
            target: Some(t),
            ts: 20,
            kind: EventKind::DestChanged,
            from_hash: Some(&h1),
            to_hash: Some(&h2),
            meta: Some(serde_json::json!({"writer": "starship"})),
        })
        .unwrap();
        j.record_event(NewEvent {
            target: None,
            ts: 30,
            kind: EventKind::Fetch,
            from_hash: None,
            to_hash: None,
            meta: None,
        })
        .unwrap();

        let tl = j.timeline(10, None).unwrap();
        assert_eq!(tl.len(), 3);
        assert_eq!(tl[0].kind, "fetch"); // newest first
        assert_eq!(tl[0].target, None);
        assert_eq!(tl[2].kind, "applied");
        assert_eq!(tl[1].meta.as_ref().unwrap()["writer"], "starship");
        assert_eq!(tl[1].machine, "mac-a");

        // keyset pagination
        let page2 = j.timeline(2, Some(tl[1].id)).unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].kind, "applied");

        let evs = j.events_for(t, 10).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(j.last_event_hash(t).unwrap().as_deref(), Some(h2.as_str()));
    }

    #[test]
    fn unmanage_and_remanage() {
        let j = Journal::open_in_memory("m").unwrap();
        let t = Path::new("/home/u/.config/foo");
        j.upsert_entry(t, Some(Path::new("dot_config/foo")), "file")
            .unwrap();
        j.mark_unmanaged(t, 99).unwrap();
        let (managed, unmanaged_at): (i64, Option<i64>) = j
            .conn
            .query_row(
                "SELECT managed, unmanaged_at FROM entries WHERE target_path = ?1",
                [t.to_string_lossy()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((managed, unmanaged_at), (0, Some(99)));
        j.upsert_entry(t, Some(Path::new("dot_config/foo")), "file")
            .unwrap();
        let managed: i64 = j
            .conn
            .query_row(
                "SELECT managed FROM entries WHERE target_path = ?1",
                [t.to_string_lossy()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(managed, 1);
    }

    #[test]
    fn session_flow() {
        let j = Journal::open_in_memory("m").unwrap();
        let s = j.begin_session(100).unwrap();
        j.add_decision(s, &serde_json::json!({"file": "/x", "choice": "ours"}))
            .unwrap();
        j.add_decision(s, &serde_json::json!({"file": "/y", "choice": "edited"}))
            .unwrap();
        j.end_session(s, 200, "resolved 2 files").unwrap();
        let (finished, summary, decisions): (Option<i64>, Option<String>, String) = j
            .conn
            .query_row(
                "SELECT finished_ts, summary, decisions FROM sessions WHERE id = ?1",
                [s],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(finished, Some(200));
        assert_eq!(summary.as_deref(), Some("resolved 2 files"));
        let arr: serde_json::Value = serde_json::from_str(&decisions).unwrap();
        assert_eq!(arr.as_array().unwrap().len(), 2);
    }

    #[test]
    fn gc_removes_only_unreferenced_blobs() {
        let j = Journal::open_in_memory("m").unwrap();
        let kept = j.put_blob(b"referenced", 1).unwrap();
        let orphan = j.put_blob(b"orphan", 2).unwrap();
        j.record_event(NewEvent {
            target: Some(Path::new("/f")),
            ts: 3,
            kind: EventKind::DestChanged,
            from_hash: None,
            to_hash: Some(&kept),
            meta: None,
        })
        .unwrap();
        assert_eq!(j.gc_blobs().unwrap(), 1);
        assert!(j.has_blob(&kept).unwrap());
        assert!(!j.has_blob(&orphan).unwrap());
    }

    #[test]
    fn record_event_preserves_entry_source_path() {
        let j = Journal::open_in_memory("m").unwrap();
        let t = Path::new("/home/u/.zshrc");
        j.upsert_entry(t, Some(Path::new("dot_zshrc")), "file")
            .unwrap();
        j.record_event(NewEvent {
            target: Some(t),
            ts: 1,
            kind: EventKind::DestChanged,
            from_hash: None,
            to_hash: None,
            meta: None,
        })
        .unwrap();
        let src: Option<String> = j
            .conn
            .query_row(
                "SELECT source_path FROM entries WHERE target_path = ?1",
                [t.to_string_lossy()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(src.as_deref(), Some("dot_zshrc"));
    }
}
