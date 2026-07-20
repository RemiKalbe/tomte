# chezmoi-ui v0 — Plan 3: Drift Journal & IPC Protocol

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Two new library crates: `czui-journal` (SQLite drift journal + content-addressed zstd blob store + sessions + GC, spec §8) and `czui-proto` (newline-delimited JSON IPC types with version handshake, spec §3.3).

**Architecture:** `czui-journal` depends on `czui-core` (reuses `ContentHash`) and wraps a single `rusqlite::Connection` — the daemon is the sole writer (WAL mode; the app will read the DB file read-only and journal through IPC). Timestamps and machine names are passed in by callers, never read from clocks inside the library (testability). `czui-proto` is dependency-light (serde/serde_json only) and shared verbatim by daemon (Plan 4) and app (Plan 5).

**Tech Stack:** rusqlite 0.40 (bundled SQLite; API verified against local crate source: `Connection::{open, open_in_memory, execute_batch, pragma_update, last_insert_rowid}` all present), zstd 0.13, serde/serde_json.

**Prerequisites:** Plans 1–2 complete (54 tests green).

**Plan series (spec: `docs/superpowers/specs/2026-07-19-chezmoi-ui-v0-design.md`):**
1. ✅ Foundation & drift model — 2. ✅ Merge engine & template mapping — **3. This plan** — 4. `chezmoid` daemon (watcher/fetch/socket) — 5. GPUI app shell — 6. Merge editor UI, sync pipeline, packaging.

## Global Constraints

Identical to Plan 1's, plus:

- New workspace dependencies: `rusqlite = { version = "0.40", features = ["bundled"] }`, `zstd = "0.13"`. Nothing else.
- `czui-journal` may depend on `czui-core` and rusqlite/zstd/serde. `czui-proto` may depend ONLY on serde, serde_json, thiserror (wire types must stay portable).
- No clock reads (`SystemTime::now`) inside library code — timestamps are `u64` unix-seconds parameters supplied by callers.
- The journal is single-writer by design (spec §3.3); no connection pooling, no `Send + Sync` gymnastics — the daemon will own it behind a `Mutex` in Plan 4.

## File Structure

```
Cargo.toml                      # + members, + rusqlite/zstd workspace deps
crates/journal/
  Cargo.toml                    # package czui-journal, lib czui_journal
  src/lib.rs                    # Journal: open/init/schema, blobs, events, sessions, GC
crates/proto/
  Cargo.toml                    # package czui-proto, lib czui_proto
  src/lib.rs                    # Request/Response/Event frames, codec, handshake
```

Single-file crates are deliberate: `journal` is one cohesive responsibility over one connection; split only if it outgrows ~600 lines in later plans.

---

### Task 1: `czui-journal` scaffold + schema

**Files:**
- Modify: `Cargo.toml` (workspace: add member `crates/journal`, add `rusqlite = { version = "0.40", features = ["bundled"] }` and `zstd = "0.13"` to `[workspace.dependencies]`)
- Create: `crates/journal/Cargo.toml`, `crates/journal/src/lib.rs`

**Interfaces:**
- Produces:
  - `Journal::open(path: &Path, machine: &str) -> Result<Journal, JournalError>` (creates file + schema, WAL mode)
  - `Journal::open_in_memory(machine: &str) -> Result<Journal, JournalError>` (tests)
  - `Journal::schema_version(&self) -> Result<u32, JournalError>` (returns 1)
  - `Journal::machine(&self) -> &str`
  - `JournalError::{Sqlite(rusqlite::Error), Corrupt(String)}` (thiserror, `#[from]` for Sqlite)

- [ ] **Step 1: Create the crate and write the failing tests**

`crates/journal/Cargo.toml`:
```toml
[package]
name = "czui-journal"
version = "0.0.1"
edition.workspace = true
license.workspace = true

[lib]
name = "czui_journal"

[dependencies]
czui-core = { path = "../core" }
rusqlite.workspace = true
zstd.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

Tests (in `crates/journal/src/lib.rs` `#[cfg(test)]` module):
```rust
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p czui-journal`
Expected: compile errors (crate empty / types undefined). Add the member to the workspace first so cargo finds it.

- [ ] **Step 3: Implement**

`crates/journal/src/lib.rs`:
```rust
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
        Ok(Self { conn, machine: machine.to_string() })
    }

    pub fn schema_version(&self) -> Result<u32, JournalError> {
        let v: String = self.conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )?;
        v.parse().map_err(|_| JournalError::Corrupt(format!("bad schema_version: {v}")))
    }

    pub fn machine(&self) -> &str {
        &self.machine
    }
}
```

Also add `"crates/journal"` to the workspace `members` and the two new `[workspace.dependencies]` entries.

- [ ] **Step 4: Run tests**

Run: `cargo test -p czui-journal`
Expected: 3 passed. (First build compiles bundled SQLite — takes a minute.)

- [ ] **Step 5: Full gate + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add Cargo.toml Cargo.lock crates/journal
git commit -m "feat(journal): czui-journal crate with SQLite schema and WAL"
```

---

### Task 2: Blob store

**Files:**
- Modify: `crates/journal/src/lib.rs`

**Interfaces:**
- Consumes: `czui_core::drift::ContentHash` (hashing), zstd.
- Produces:
  - `Journal::put_blob(&self, content: &[u8], now_ts: u64) -> Result<String, JournalError>` — returns hex sha256 of the *raw* content; deduplicates (same content stored once)
  - `Journal::get_blob(&self, hash: &str) -> Result<Option<Vec<u8>>, JournalError>` — decompressed bytes
  - `Journal::has_blob(&self, hash: &str) -> Result<bool, JournalError>`
  - `Journal::blob_store_size(&self) -> Result<u64, JournalError>` — total compressed bytes

- [ ] **Step 1: Write the failing tests** (append to the test module)

```rust
    #[test]
    fn blob_roundtrip_and_dedup() {
        let j = Journal::open_in_memory("m").unwrap();
        let content = b"export EDITOR=hx\nexport VISUAL=hx\n".repeat(50);
        let h1 = j.put_blob(&content, 100).unwrap();
        let h2 = j.put_blob(&content, 200).unwrap();
        assert_eq!(h1, h2, "same content must dedup to the same hash");
        let n: u32 = j.conn.query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0)).unwrap();
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p czui-journal blob`
Expected: compile errors (`put_blob` undefined).

- [ ] **Step 3: Implement** (append to `impl Journal`)

```rust
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
            .query_row("SELECT content_zstd FROM blobs WHERE hash = ?1", [hash], |r| r.get(0))
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
        let n: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM blobs WHERE hash = ?1",
            [hash],
            |r| r.get(0),
        )?;
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
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p czui-journal`
Expected: 5 passed.

- [ ] **Step 5: Full gate + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add crates/journal/src/lib.rs
git commit -m "feat(journal): content-addressed zstd blob store with dedup"
```

---

### Task 3: Entries, events, sessions, timeline, GC

**Files:**
- Modify: `crates/journal/src/lib.rs`

**Interfaces:**
- Produces (Plan 4 daemon and Plan 5 app depend on these exact names):
  - `EventKind::{DestChanged, SourceChanged, RemoteAdvanced, Applied, Readded, Resolved, EvalFailed, Fetch, LeftManagement, SessionStart, SessionEnd}` with `as_str() -> &'static str` (snake_case: `dest_changed`, …) and `parse(&str) -> Option<EventKind>`
  - `NewEvent<'a> { target: Option<&'a Path>, ts: u64, kind: EventKind, from_hash: Option<&'a str>, to_hash: Option<&'a str>, meta: Option<serde_json::Value> }`
  - `EventRow { id: i64, target: Option<PathBuf>, ts: u64, machine: String, kind: String, from_hash: Option<String>, to_hash: Option<String>, meta: Option<serde_json::Value> }`
  - `Journal::upsert_entry(&self, target: &Path, source: Option<&Path>, kind: &str) -> Result<i64, JournalError>` (re-upserting marks it managed again and clears `unmanaged_at`)
  - `Journal::mark_unmanaged(&self, target: &Path, ts: u64) -> Result<(), JournalError>`
  - `Journal::record_event(&self, ev: NewEvent) -> Result<i64, JournalError>` (auto-upserts the entry when `target` is given; uses `self.machine`)
  - `Journal::timeline(&self, limit: u32, before_id: Option<i64>) -> Result<Vec<EventRow>, JournalError>` (id-descending, keyset pagination)
  - `Journal::events_for(&self, target: &Path, limit: u32) -> Result<Vec<EventRow>, JournalError>`
  - `Journal::last_event_hash(&self, target: &Path) -> Result<Option<String>, JournalError>` (latest event's `to_hash`)
  - `Journal::begin_session(&self, ts: u64) -> Result<i64, JournalError>`, `add_decision(&self, session: i64, decision: &serde_json::Value)`, `end_session(&self, session: i64, ts: u64, summary: &str)`
  - `Journal::gc_blobs(&self) -> Result<u32, JournalError>` — deletes blobs referenced by no event's `from_hash`/`to_hash`; returns count deleted

- [ ] **Step 1: Write the failing tests** (append)

```rust
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
        j.upsert_entry(t, Some(Path::new("dot_config/foo")), "file").unwrap();
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
        j.upsert_entry(t, Some(Path::new("dot_config/foo")), "file").unwrap();
        let managed: i64 = j
            .conn
            .query_row("SELECT managed FROM entries WHERE target_path = ?1", [t.to_string_lossy()], |r| r.get(0))
            .unwrap();
        assert_eq!(managed, 1);
    }

    #[test]
    fn session_flow() {
        let j = Journal::open_in_memory("m").unwrap();
        let s = j.begin_session(100).unwrap();
        j.add_decision(s, &serde_json::json!({"file": "/x", "choice": "ours"})).unwrap();
        j.add_decision(s, &serde_json::json!({"file": "/y", "choice": "edited"})).unwrap();
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
```


- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p czui-journal`
Expected: compile errors (`NewEvent`, `EventKind` undefined).

- [ ] **Step 3: Implement** (append; `use std::path::{Path, PathBuf};` at top)

```rust
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

    pub fn timeline(&self, limit: u32, before_id: Option<i64>) -> Result<Vec<EventRow>, JournalError> {
        let sql = format!(
            "{EVENT_SELECT} WHERE (?1 IS NULL OR e.id < ?1) ORDER BY e.id DESC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params![before_id, limit], row_to_event)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn events_for(&self, target: &Path, limit: u32) -> Result<Vec<EventRow>, JournalError> {
        let sql = format!(
            "{EVENT_SELECT} WHERE en.target_path = ?1 ORDER BY e.id DESC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params![target.to_string_lossy(), limit], row_to_event)?
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
        self.conn.execute(
            "INSERT INTO sessions(started_ts) VALUES(?1)",
            [ts as i64],
        )?;
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
            .ok_or_else(|| JournalError::Corrupt(format!("session {session} decisions not an array")))?
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
```

Add this regression test alongside the others (it guards `entry_id_for` never clobbering `source_path`):

```rust
    #[test]
    fn record_event_preserves_entry_source_path() {
        let j = Journal::open_in_memory("m").unwrap();
        let t = Path::new("/home/u/.zshrc");
        j.upsert_entry(t, Some(Path::new("dot_zshrc")), "file").unwrap();
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
            .query_row("SELECT source_path FROM entries WHERE target_path = ?1", [t.to_string_lossy()], |r| r.get(0))
            .unwrap();
        assert_eq!(src.as_deref(), Some("dot_zshrc"));
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p czui-journal`
Expected: 10 passed (3 + 2 + 5 new).

- [ ] **Step 5: Full gate + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add crates/journal/src/lib.rs
git commit -m "feat(journal): entries, events, sessions, timeline queries, and blob GC"
```

---

### Task 4: `czui-proto` — IPC wire types & codec

**Files:**
- Modify: `Cargo.toml` (workspace member `crates/proto`)
- Create: `crates/proto/Cargo.toml`, `crates/proto/src/lib.rs`

**Interfaces:**
- Produces (daemon Plan 4 / app Plan 5 share these exactly):
  - `PROTOCOL_VERSION: u32 = 1`
  - `Request::{Hello { version: u32 }, Subscribe, Status, Timeline { limit: u32, before_id: Option<i64> }, EventsFor { target: PathBuf, limit: u32 }, ExpectChanges { paths: Vec<PathBuf>, ttl_secs: u32 }, Rescan, Pause, Resume, SnapshotBlobs { paths: Vec<PathBuf> }, SessionStart { ts: u64 }, SessionDecision { session: i64, decision: serde_json::Value }, SessionEnd { session: i64, ts: u64, summary: String }}`
  - `Response::{HelloOk { version: u32, machine: String }, Ok, Error { message: String }, Status { drifted: Vec<DriftSummary>, in_sync: u64, degraded: Option<String> }, Timeline { events: Vec<EventSummary> }, SessionStarted { session: i64 }, Blobs { hashes: Vec<String> }}`
  - `Event::{Drift { target: PathBuf, class: String, ts: u64 }, RemoteAdvanced { target: PathBuf, ts: u64 }, EvalFailed { target: Option<PathBuf>, hint: String, ts: u64 }, LeftManagement { target: PathBuf, ts: u64 }, FetchDone { ts: u64, behind: u32 }, ScanDone { ts: u64, drifted: u32 }}`
  - `DriftSummary { target: PathBuf, class: String, since_ts: Option<u64> }`, `EventSummary { id: i64, target: Option<PathBuf>, kind: String, ts: u64 }`
  - `ClientFrame { id: u64, request: Request }`
  - `ServerFrame::{Reply { id: u64, response: Response }, Push { event: Event }}`
  - `write_frame<W: io::Write, T: Serialize>(w: &mut W, frame: &T) -> io::Result<()>` (one JSON per line)
  - `read_frame<T: DeserializeOwned>(line: &str) -> Result<T, serde_json::Error>`
  - `check_hello(client_version: u32) -> Result<(), String>` (message names both versions)
  - All enums use `#[serde(tag = "type", rename_all = "snake_case")]`; `ServerFrame` uses `#[serde(tag = "frame", rename_all = "snake_case")]`.

- [ ] **Step 1: Create the crate and write the failing tests**

`crates/proto/Cargo.toml`:
```toml
[package]
name = "czui-proto"
version = "0.0.1"
edition.workspace = true
license.workspace = true

[lib]
name = "czui_proto"

[dependencies]
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
```

Tests:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn request_roundtrips_and_wire_shape_is_stable() {
        let req = ClientFrame {
            id: 7,
            request: Request::ExpectChanges {
                paths: vec![PathBuf::from("/a"), PathBuf::from("/b")],
                ttl_secs: 30,
            },
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let line = String::from_utf8(buf).unwrap();
        assert!(line.ends_with('\n'));
        assert!(line.contains("\"type\":\"expect_changes\""), "wire tag must be snake_case: {line}");
        let back: ClientFrame = read_frame(line.trim_end()).unwrap();
        assert_eq!(back.id, 7);
        assert!(matches!(back.request, Request::ExpectChanges { ttl_secs: 30, .. }));
    }

    #[test]
    fn server_frames_distinguish_reply_and_push() {
        let reply = ServerFrame::Reply { id: 3, response: Response::Ok };
        let push = ServerFrame::Push {
            event: Event::Drift { target: PathBuf::from("/x"), class: "conflict".into(), ts: 42 },
        };
        let r = serde_json::to_string(&reply).unwrap();
        let p = serde_json::to_string(&push).unwrap();
        assert!(r.contains("\"frame\":\"reply\""));
        assert!(p.contains("\"frame\":\"push\""));
        let back: ServerFrame = read_frame(&p).unwrap();
        match back {
            ServerFrame::Push { event: Event::Drift { ts: 42, .. } } => {}
            other => panic!("bad roundtrip: {other:?}"),
        }
    }

    #[test]
    fn hello_check() {
        assert!(check_hello(PROTOCOL_VERSION).is_ok());
        let err = check_hello(PROTOCOL_VERSION + 1).unwrap_err();
        assert!(err.contains(&PROTOCOL_VERSION.to_string()));
        assert!(err.contains(&(PROTOCOL_VERSION + 1).to_string()));
    }

    #[test]
    fn every_request_variant_roundtrips() {
        let variants = vec![
            Request::Hello { version: 1 },
            Request::Subscribe,
            Request::Status,
            Request::Timeline { limit: 50, before_id: Some(9) },
            Request::EventsFor { target: PathBuf::from("/t"), limit: 5 },
            Request::ExpectChanges { paths: vec![], ttl_secs: 1 },
            Request::Rescan,
            Request::Pause,
            Request::Resume,
            Request::SnapshotBlobs { paths: vec![PathBuf::from("/s")] },
            Request::SessionStart { ts: 1 },
            Request::SessionDecision { session: 2, decision: serde_json::json!({"c": "ours"}) },
            Request::SessionEnd { session: 2, ts: 3, summary: "done".into() },
        ];
        for v in variants {
            let s = serde_json::to_string(&v).unwrap();
            let back: Request = read_frame(&s).unwrap();
            assert_eq!(
                std::mem::discriminant(&v),
                std::mem::discriminant(&back),
                "variant changed through roundtrip: {s}"
            );
        }
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p czui-proto`
Expected: compile errors.

- [ ] **Step 3: Implement**

`crates/proto/src/lib.rs`:
```rust
//! IPC wire types (spec §3.3): newline-delimited JSON with request ids.
//! Shared verbatim by chezmoid (server) and the app (client).

use std::io;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello { version: u32 },
    Subscribe,
    Status,
    Timeline { limit: u32, before_id: Option<i64> },
    EventsFor { target: PathBuf, limit: u32 },
    ExpectChanges { paths: Vec<PathBuf>, ttl_secs: u32 },
    Rescan,
    Pause,
    Resume,
    SnapshotBlobs { paths: Vec<PathBuf> },
    SessionStart { ts: u64 },
    SessionDecision { session: i64, decision: serde_json::Value },
    SessionEnd { session: i64, ts: u64, summary: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftSummary {
    pub target: PathBuf,
    pub class: String,
    pub since_ts: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventSummary {
    pub id: i64,
    pub target: Option<PathBuf>,
    pub kind: String,
    pub ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    HelloOk { version: u32, machine: String },
    Ok,
    Error { message: String },
    Status { drifted: Vec<DriftSummary>, in_sync: u64, degraded: Option<String> },
    Timeline { events: Vec<EventSummary> },
    SessionStarted { session: i64 },
    Blobs { hashes: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Drift { target: PathBuf, class: String, ts: u64 },
    RemoteAdvanced { target: PathBuf, ts: u64 },
    EvalFailed { target: Option<PathBuf>, hint: String, ts: u64 },
    LeftManagement { target: PathBuf, ts: u64 },
    FetchDone { ts: u64, behind: u32 },
    ScanDone { ts: u64, drifted: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientFrame {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    Reply {
        id: u64,
        #[serde(flatten)]
        response: Response,
    },
    Push {
        #[serde(flatten)]
        event: Event,
    },
}

pub fn write_frame<W: io::Write, T: Serialize>(w: &mut W, frame: &T) -> io::Result<()> {
    let json = serde_json::to_string(frame)?;
    w.write_all(json.as_bytes())?;
    w.write_all(b"\n")
}

pub fn read_frame<T: DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line)
}

pub fn check_hello(client_version: u32) -> Result<(), String> {
    if client_version == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(format!(
            "protocol version mismatch: daemon speaks {PROTOCOL_VERSION}, client speaks {client_version}"
        ))
    }
}
```

Implementer note: `#[serde(flatten)]` on an internally-tagged enum inside a struct is a known-working serde pattern, but verify the wire shape the tests assert (`"type":"expect_changes"` alongside `"id":7`). If serde rejects the flatten+tag combination at runtime, fall back to non-flattened fields (`{ id, request: {...} }` / `{ frame, response: {...} }`) AND update the two wire-shape assertions to match — the framing contract (one JSON object per line, ids echoed on replies, pushes id-less) is what matters, not the exact nesting.

- [ ] **Step 4: Run tests**

Run: `cargo test -p czui-proto`
Expected: 4 passed.

- [ ] **Step 5: Full gate + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add Cargo.toml Cargo.lock crates/proto
git commit -m "feat(proto): IPC wire types, ndjson codec, and version handshake"
```

---

## Self-Review Notes (completed during plan writing)

- **Spec coverage for this plan's slice:** §8 journal schema (entries/events/blobs/sessions, kinds list incl. `left_management`) ✓ Tasks 1–3; §8 retention "GC of unreferenced blobs" ✓ (size-cap trigger policy belongs to the daemon loop, Plan 4); §3.3 ndjson framing, ids on replies, id-less pushes, version handshake, journaling commands (`SnapshotBlobs`, `SessionStart/Decision/End` wire types) ✓ Task 4; §6.3 session decisions recording ✓ Task 3.
- **Type consistency:** `EventKind::as_str` snake_case strings match `EventRow.kind` assertions and proto `EventSummary.kind` usage; `put_blob` hex output matches `czui_core::drift::ContentHash::to_hex` (asserted by a test); `Journal.conn` is `pub(crate)` so tests can inspect tables.
- **Bug caught and fixed during review:** `record_event` originally called `upsert_entry(target, None, …)`, which would null an existing `source_path` via `excluded.source_path` — replaced directly in the plan code with an `entry_id_for` lookup-or-create that never touches metadata, guarded by the `record_event_preserves_entry_source_path` regression test.
- **Known simplifications (accepted):** `timeline` keyset pagination by id only (ts-equal ordering follows insert order); `gc_blobs` ignores the size cap (daemon decides when to run GC); session decisions stored as a JSON array column (single-writer, low volume) rather than a table.
