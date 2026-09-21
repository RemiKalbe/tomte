# chezmoi-ui v0 — Plan 4: `chezmoid` Watcher Daemon

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The `czui-daemon` crate and `chezmoid` binary (spec §3.1): FSEvents watching with debounce, expected-changes suppression, managed-set reconciliation, source-side and fetch-driven drift detection, journal writing with blob snapshots, and the unix-socket IPC server speaking `czui-proto`.

**Architecture:** `DaemonCore` is a single-threaded state machine (no I/O loops inside) that takes `now_ts` parameters — the binary wires it to real threads (notify watcher → debouncer → core; fetch timer; hourly rescan; socket server holding `Arc<Mutex<DaemonCore>>`). The `Journal` lives inside the core (single writer preserved). Event fanout is a `Vec<mpsc::Sender<czui_proto::Event>>`. All chezmoi/git semantics reuse Plan 1's clients; per-target probing reuses the scanner.

**Tech Stack:** notify 8.2 (API verified from local crate source: `recommended_watcher(handler)`, `Watcher::watch(&mut, path, RecursiveMode)`, `unwatch`), toml 1, gethostname 1, plus existing czui-core/journal/proto.

**Prerequisites:** Plans 1–3 complete (68 tests green).

## Global Constraints

Identical to Plan 1's, plus:

- New workspace dependencies: `notify = "8"`, `toml = "1"`, `gethostname = "1"`. Nothing else.
- `DaemonCore` (library part of czui-daemon) reads no clocks — `now_ts: u64` is always a parameter. Only `src/main.rs` may call `SystemTime::now()`.
- The daemon NEVER mutates managed files, the source worktree, or chezmoi state; its only external writes are the journal DB and `git fetch` (spec §3.1). Tests enforce hermeticity (scratch dirs only).
- Blob snapshots are capped at 4 MiB; larger content records the event with `meta.blob = "skipped_too_large"` instead of a blob.
- Suppressed (pre-announced) changes journal an `applied` event with `meta.expected = true` and emit no `Drift` push.

## File Structure

```
Cargo.toml                        # + member crates/daemon, + notify/toml/gethostname deps
crates/core/src/scanner.rs        # + pub probe_one
crates/core/src/git.rs            # + rev_parse
crates/core/src/chezmoi.rs        # + #[derive(Clone)] pattern (Clone impl)
crates/core/src/testsupport.rs    # feature-gated Scratch (shared with daemon tests)
crates/core/Cargo.toml            # + [features] test-support, optional tempfile
crates/daemon/
  Cargo.toml                      # package czui-daemon; bin chezmoid
  src/lib.rs                      # pub mod core; pub mod debounce; pub mod server; pub mod settings;
  src/core.rs                     # DaemonCore
  src/debounce.rs                 # Debouncer thread
  src/server.rs                   # unix socket server
  src/settings.rs                 # Settings (toml)
  src/main.rs                     # chezmoid binary wiring
  tests/daemon_core.rs            # scratch-based integration tests
  tests/server_ipc.rs             # socket round-trip tests
```

---

### Task 1: Core prep — `probe_one`, `rev_parse`, `Clone`, shared `testsupport`

**Files:**
- Modify: `crates/core/src/scanner.rs`, `crates/core/src/git.rs`, `crates/core/src/chezmoi.rs`, `crates/core/src/lib.rs`, `crates/core/Cargo.toml`, `crates/core/tests/scanner_integration.rs` (one new test)
- Create: `crates/core/src/testsupport.rs`

**Interfaces:**
- Produces:
  - `DriftScanner::probe_one(&self, target: &Path) -> Result<Option<FileDrift>, ScanError>` — probes a single target (resolves source dir + last-written itself; `None` = in sync)
  - `GitClient::rev_parse(&self, rev: &str) -> Result<String, GitError>` (trimmed sha)
  - `impl Clone for ChezmoiClient` and `impl Clone for GitClient` (both are Arc + owned data)
  - `czui_core::testsupport::{Scratch, sh, git}` behind feature `test-support` — the exact helper from `crates/core/tests/support/mod.rs`, made `pub`, with `tempfile` as an optional dependency enabled by the feature. (The existing `tests/support/mod.rs` stays as-is; daemon tests use the feature-gated module.)

- [x] **Step 1: Write the failing tests**

Append to `crates/core/src/git.rs` tests:
```rust
    #[test]
    fn rev_parse_returns_sha() {
        let (_g, work) = scratch();
        let sha = client(&work).rev_parse("HEAD").unwrap();
        assert_eq!(sha.len(), 40);
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
    }
```

Append to `crates/core/tests/scanner_integration.rs`:
```rust
#[test]
fn probe_one_detects_single_file_drift() {
    let s = Scratch::new();
    std::fs::write(s.home.join(".testrc"), "a=changed\n").unwrap();
    let scanner = s.scanner();
    let fd = scanner.probe_one(&s.home.join(".testrc")).unwrap().unwrap();
    assert_eq!(fd.class, DriftClass::DestinationDrift);
    // untouched target probes clean after re-write back
    std::fs::write(s.home.join(".testrc"), "a=1\n").unwrap();
    assert!(scanner.probe_one(&s.home.join(".testrc")).unwrap().is_none());
}
```

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-core rev_parse` and `cargo test -p czui-core --test scanner_integration probe_one`
Expected: compile errors (`rev_parse`, `probe_one` undefined).

- [x] **Step 3: Implement**

`git.rs` — append to `impl GitClient`:
```rust
    pub fn rev_parse(&self, rev: &str) -> Result<String, GitError> {
        Ok(self.run_utf8(&["rev-parse", rev])?.trim().to_string())
    }
```

`scanner.rs` — append to `impl DriftScanner` (it reuses the existing private `probe_file`):
```rust
    /// Probe a single target on demand (daemon per-event path). `None` = in sync.
    pub fn probe_one(&self, target: &Path) -> Result<Option<FileDrift>, ScanError> {
        let source_dir = self.chezmoi.source_dir()?;
        let dump = self.chezmoi.state_dump()?;
        let last = dump
            .entry_state
            .get(target)
            .and_then(|e| e.contents_sha256.as_deref())
            .and_then(ContentHash::from_hex);
        self.probe_file(&source_dir, target, last)
    }
```

`chezmoi.rs`: add `#[derive(Clone)]` to `ChezmoiClient` (field `runner: Arc<dyn CommandRunner>` requires a manual bound-free derive — `Arc<dyn T>` is `Clone`, and `ChezmoiOptions` already is, so plain `#[derive(Clone)]` works). Same for `GitClient` in `git.rs`.

`crates/core/Cargo.toml` — feature + optional dep:
```toml
[features]
test-support = ["dep:tempfile"]

[dependencies]
# … existing …
tempfile = { workspace = true, optional = true }
```
(keep `tempfile` in `[dev-dependencies]` too — dev and optional-regular can coexist).

`crates/core/src/lib.rs`:
```rust
#[cfg(feature = "test-support")]
pub mod testsupport;
```

`crates/core/src/testsupport.rs`: copy `crates/core/tests/support/mod.rs` verbatim, then: make `sh`, `git`, `Scratch`, and all `Scratch` fields/methods `pub`; change internal imports from `czui_core::…` to `crate::…`; add `//! Scratch chezmoi home for integration tests (feature = "test-support").` at top; add `impl Default for Scratch { fn default() -> Self { Self::new() } }` if clippy demands it. This module is test tooling compiled only under the feature — the "no unwrap/expect in lib code" constraint does NOT apply to it; keep the helper's original unwraps.

- [x] **Step 4: Run tests**

Run: `cargo test -p czui-core` then `cargo test -p czui-core --features test-support` (proves the module compiles)
Expected: all green (55 core tests: 54 prior + rev_parse; +1 integration probe test = 61 with integrations).

- [x] **Step 5: Full gate + commit**

Run (separately): `cargo fmt --all`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace`

```bash
git add crates/core
git commit -m "feat(core): probe_one, rev_parse, Clone clients, feature-gated testsupport"
```

---

### Task 2: `DaemonCore`

**Files:**
- Modify: `Cargo.toml` (member `crates/daemon`; add `notify = "8"`, `toml = "1"`, `gethostname = "1"` to workspace deps)
- Create: `crates/daemon/Cargo.toml`, `crates/daemon/src/lib.rs`, `crates/daemon/src/core.rs`, `crates/daemon/tests/daemon_core.rs`

**Interfaces:**
- Produces (server/binary/tests depend on these):
  - `DaemonCore::new(chezmoi: ChezmoiClient, git: GitClient, journal: Journal, remote_ref: String) -> Result<DaemonCore, DaemonError>` (resolves source dir, loads initial managed set)
  - `subscribe(&mut self) -> std::sync::mpsc::Receiver<czui_proto::Event>`
  - `expect_changes(&mut self, paths: &[PathBuf], ttl_secs: u32, now_ts: u64)`
  - `handle_paths_changed(&mut self, paths: &[PathBuf], now_ts: u64) -> Result<(), DaemonError>`
  - `reconcile_managed(&mut self, now_ts: u64) -> Result<WatchDelta, DaemonError>` — `WatchDelta { added: Vec<PathBuf>, removed: Vec<PathBuf> }`
  - `full_rescan(&mut self, now_ts: u64) -> Result<u32, DaemonError>` (returns drifted count; emits `ScanDone`)
  - `handle_fetch(&mut self, now_ts: u64) -> Result<(), DaemonError>` (emits `FetchDone`)
  - `snapshot_blobs(&mut self, paths: &[PathBuf], now_ts: u64) -> Result<Vec<String>, DaemonError>`
  - `status_snapshot(&self) -> (Vec<czui_proto::DriftSummary>, u64, Option<String>)`
  - `watch_paths(&self) -> Vec<PathBuf>` (source dir + managed targets)
  - `set_paused(&mut self, paused: bool)`, `journal(&self) -> &Journal`, `machine(&self) -> &str`
  - `DaemonError::{Scan(ScanError), Chezmoi(ChezmoiError), Git(GitError), Journal(JournalError), Io(std::io::Error)}` (thiserror, all `#[from]`)

**Behavior contract (spec §3.1; the tests encode it):**
1. A changed managed target that was pre-announced via `expect_changes` (within TTL) journals `applied` (`meta.expected=true`, `to_hash` = new dest hash, with blob) and pushes NO `Drift` event.
2. A foreign change to a managed target probes it; classification journals `dest_changed` (with blob snapshot) and/or `source_changed`/`remote_advanced` per class, deduplicated: an event is skipped when the target's recent events already contain the same `(kind, to_hash)`.
3. Source-dir changes trigger `reconcile_managed` (departed entries → `mark_unmanaged` + `left_management` event + push; new entries → watch additions) and probing of the targets mapped from the changed source files.
4. `.git` paths are ignored except `HEAD`/`refs/*`, which trigger source-side reprobing (`changed_files(remote..HEAD)` + dirty files).
5. `handle_fetch` fetches, computes divergence, probes targets of files changed between HEAD and the remote ref; fetch failures journal a `fetch` event with `meta.error` and still emit `FetchDone { behind: 0 }` (stale-but-honest, spec §10).
6. `full_rescan` = reconcile + scan; rebuilds the drift-state map (used by `status_snapshot`), records `degraded` from the scan report, emits `ScanDone`.
7. While paused, `handle_paths_changed` is a no-op.

- [x] **Step 1: Create crate scaffolding**

`crates/daemon/Cargo.toml`:
```toml
[package]
name = "czui-daemon"
version = "0.0.1"
edition.workspace = true
license.workspace = true

[[bin]]
name = "chezmoid"
path = "src/main.rs"

[lib]
name = "czui_daemon"

[dependencies]
czui-core = { path = "../core" }
czui-journal = { path = "../journal" }
czui-proto = { path = "../proto" }
notify.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
toml.workspace = true
gethostname.workspace = true

[dev-dependencies]
czui-core = { path = "../core", features = ["test-support"] }
tempfile.workspace = true
```

`crates/daemon/src/lib.rs`:
```rust
pub mod core;
pub mod debounce;
pub mod server;
pub mod settings;
```
(create `debounce.rs`, `server.rs`, `settings.rs` as `//! see plan task N` placeholders; `src/main.rs` as `fn main() {} // see plan task 5`.)

- [x] **Step 2: Write the failing integration tests**

`crates/daemon/tests/daemon_core.rs`:
```rust
//! DaemonCore behavior against a real scratch chezmoi home.

use std::path::PathBuf;

use czui_core::chezmoi::ChezmoiClient;
use czui_core::cmd::SystemRunner;
use czui_core::git::GitClient;
use czui_core::testsupport::{git, sh, Scratch};
use czui_daemon::core::DaemonCore;
use czui_journal::Journal;
use czui_proto::Event;

fn core_for(s: &Scratch) -> DaemonCore {
    let chezmoi: ChezmoiClient = s.chezmoi();
    let git = GitClient::new(std::sync::Arc::new(SystemRunner), s.source.clone());
    let journal = Journal::open_in_memory("test-mac").unwrap();
    DaemonCore::new(chezmoi, git, journal, "origin/main".to_string()).unwrap()
}

fn kinds(core: &DaemonCore, target: &std::path::Path) -> Vec<String> {
    core.journal()
        .events_for(target, 20)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

#[test]
fn foreign_dest_change_journals_and_pushes() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let rx = core.subscribe();
    let target = s.home.join(".testrc");
    std::fs::write(&target, "a=2\n").unwrap();
    core.handle_paths_changed(&[target.clone()], 100).unwrap();

    let ks = kinds(&core, &target);
    assert!(ks.contains(&"dest_changed".to_string()), "{ks:?}");
    match rx.try_recv().unwrap() {
        Event::Drift { target: t, class, ts } => {
            assert_eq!(t, target);
            assert_eq!(class, "destination_drift");
            assert_eq!(ts, 100);
        }
        other => panic!("expected Drift, got {other:?}"),
    }
    // dedup: same state again → no new event
    core.handle_paths_changed(&[target.clone()], 101).unwrap();
    assert_eq!(kinds(&core, &target).len(), ks.len());
    // blob snapshot exists for the new content
    let h = czui_core::drift::ContentHash::of(b"a=2\n").to_hex();
    assert!(core.journal().has_blob(&h).unwrap());
}

#[test]
fn expected_change_journals_applied_without_push() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let rx = core.subscribe();
    let target = s.home.join(".testrc");
    core.expect_changes(&[target.clone()], 60, 100);
    std::fs::write(&target, "a=applied\n").unwrap();
    core.handle_paths_changed(&[target.clone()], 110).unwrap();

    let ks = kinds(&core, &target);
    assert_eq!(ks, vec!["applied".to_string()]);
    assert!(rx.try_recv().is_err(), "no Drift push for expected changes");

    // TTL expiry: same path changed after deadline is foreign again
    std::fs::write(&target, "a=foreign\n").unwrap();
    core.handle_paths_changed(&[target.clone()], 200).unwrap();
    assert!(kinds(&core, &target).contains(&"dest_changed".to_string()));
}

#[test]
fn forget_triggers_left_management() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let rx = core.subscribe();
    let target = s.home.join(".testrc");
    assert!(core.watch_paths().contains(&target));

    // remove from source (like `chezmoi forget`) and commit
    std::fs::remove_file(s.source.join("dot_testrc")).unwrap();
    let delta = core.reconcile_managed(300).unwrap();
    assert!(delta.removed.contains(&target));
    assert!(!core.watch_paths().contains(&target));
    assert!(kinds(&core, &target).contains(&"left_management".to_string()));
    assert!(matches!(rx.try_recv().unwrap(), Event::LeftManagement { .. }));
}

#[test]
fn fetch_detects_remote_changes() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let rx = core.subscribe();
    // second machine pushes
    let other = s.root.path().join("other");
    sh(s.root.path(), "git", &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()]);
    std::fs::write(other.join("dot_testrc"), "a=remote\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "remote"]);
    git(&other, &["push"]);

    core.handle_fetch(400).unwrap();
    let target = s.home.join(".testrc");
    assert!(kinds(&core, &target).contains(&"remote_advanced".to_string()));
    let events: Vec<Event> = rx.try_iter().collect();
    assert!(events.iter().any(|e| matches!(e, Event::Drift { class, .. } if class == "remote_ahead")));
    assert!(events.iter().any(|e| matches!(e, Event::FetchDone { behind: 1, .. })));
}

#[test]
fn full_rescan_populates_status() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    std::fs::write(s.home.join(".testrc"), "a=drift\n").unwrap();
    let drifted = core.full_rescan(500).unwrap();
    assert_eq!(drifted, 1);
    let (list, in_sync, degraded) = core.status_snapshot();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].class, "destination_drift");
    assert_eq!(in_sync, 0); // the only managed file is drifted
    assert!(degraded.is_none());
}

#[test]
fn paused_core_ignores_changes() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    core.set_paused(true);
    let target = s.home.join(".testrc");
    std::fs::write(&target, "a=paused\n").unwrap();
    core.handle_paths_changed(&[target.clone()], 600).unwrap();
    assert!(kinds(&core, &target).is_empty());
}
```

Note: `DriftClass` → wire string mapping is snake_case: `in_sync`, `destination_drift`, `source_ahead`, `remote_ahead`, `local_source_diverged`, `conflict`, `eval_failed`.

- [x] **Step 3: Run to verify failure**

Run: `cargo test -p czui-daemon --test daemon_core`
Expected: compile errors (`DaemonCore` undefined).

- [x] **Step 4: Implement**

`crates/daemon/src/core.rs`:
```rust
//! DaemonCore: single-threaded drift state machine (spec §3.1).
//! No clocks, no I/O loops — callers supply `now_ts` and drive it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

use czui_core::chezmoi::{ChezmoiClient, ChezmoiError};
use czui_core::drift::{ContentHash, DriftClass};
use czui_core::git::{GitClient, GitError};
use czui_core::scanner::{DriftScanner, FileDrift, ScanError};
use czui_journal::{EventKind, Journal, JournalError, NewEvent};
use czui_proto::{DriftSummary, Event};

const BLOB_CAP: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error(transparent)]
    Scan(#[from] ScanError),
    #[error(transparent)]
    Chezmoi(#[from] ChezmoiError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Default)]
pub struct WatchDelta {
    pub added: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
}

pub fn class_str(class: DriftClass) -> &'static str {
    match class {
        DriftClass::InSync => "in_sync",
        DriftClass::DestinationDrift => "destination_drift",
        DriftClass::SourceAhead => "source_ahead",
        DriftClass::RemoteAhead => "remote_ahead",
        DriftClass::LocalSourceDiverged => "local_source_diverged",
        DriftClass::Conflict => "conflict",
        DriftClass::EvalFailed => "eval_failed",
    }
}

pub struct DaemonCore {
    chezmoi: ChezmoiClient,
    git: GitClient,
    scanner: DriftScanner,
    journal: Journal,
    source_dir: PathBuf,
    remote_ref: String,
    managed: BTreeSet<PathBuf>,
    expected: Vec<(PathBuf, u64)>,
    drift_state: BTreeMap<PathBuf, (String, u64)>,
    in_sync_count: u64,
    degraded: Option<String>,
    paused: bool,
    subscribers: Vec<Sender<Event>>,
}

impl DaemonCore {
    pub fn new(
        chezmoi: ChezmoiClient,
        git: GitClient,
        journal: Journal,
        remote_ref: String,
    ) -> Result<Self, DaemonError> {
        let source_dir = chezmoi.source_dir()?;
        let managed: BTreeSet<PathBuf> = chezmoi.managed()?.into_iter().collect();
        let scanner = DriftScanner::new(chezmoi.clone(), git.clone(), remote_ref.clone());
        Ok(Self {
            chezmoi,
            git,
            scanner,
            journal,
            source_dir,
            remote_ref,
            managed,
            expected: Vec::new(),
            drift_state: BTreeMap::new(),
            in_sync_count: 0,
            degraded: None,
            paused: false,
            subscribers: Vec::new(),
        })
    }

    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    pub fn machine(&self) -> &str {
        self.journal.machine()
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn subscribe(&mut self) -> Receiver<Event> {
        let (tx, rx) = channel();
        self.subscribers.push(tx);
        rx
    }

    fn emit(&mut self, ev: Event) {
        self.subscribers.retain(|s| s.send(ev.clone()).is_ok());
    }

    pub fn expect_changes(&mut self, paths: &[PathBuf], ttl_secs: u32, now_ts: u64) {
        let deadline = now_ts + ttl_secs as u64;
        for p in paths {
            self.expected.push((p.clone(), deadline));
        }
    }

    fn is_expected(&mut self, path: &Path, now_ts: u64) -> bool {
        self.expected.retain(|(_, deadline)| *deadline >= now_ts);
        self.expected.iter().any(|(p, _)| p == path)
    }

    pub fn watch_paths(&self) -> Vec<PathBuf> {
        let mut v = vec![self.source_dir.clone()];
        v.extend(self.managed.iter().cloned());
        v
    }

    /// Skip an event if the target's recent history already ends in the same state.
    fn already_recorded(&self, target: &Path, kind: &str, to_hash: Option<&str>) -> bool {
        match self.journal.events_for(target, 10) {
            Ok(events) => events
                .iter()
                .find(|e| e.kind == kind)
                .is_some_and(|e| e.to_hash.as_deref() == to_hash),
            Err(_) => false,
        }
    }

    fn snapshot_file(&self, path: &Path, now_ts: u64) -> Result<(Option<String>, Option<serde_json::Value>), DaemonError> {
        match std::fs::read(path) {
            Ok(bytes) if bytes.len() <= BLOB_CAP => {
                Ok((Some(self.journal.put_blob(&bytes, now_ts)?), None))
            }
            Ok(bytes) => Ok((
                Some(ContentHash::of(&bytes).to_hex()),
                Some(serde_json::json!({"blob": "skipped_too_large"})),
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((None, None)),
            Err(e) => Err(e.into()),
        }
    }

    fn journal_probe(&mut self, fd: &FileDrift, now_ts: u64) -> Result<(), DaemonError> {
        let target = fd.target.clone();
        let class = fd.class;
        let cs = class_str(class);

        if class == DriftClass::EvalFailed {
            if !self.already_recorded(&target, "eval_failed", None) {
                let hint = match &fd.probe.rendered {
                    Err(f) => f.hint.clone(),
                    Ok(_) => String::new(),
                };
                self.journal.record_event(NewEvent {
                    target: Some(&target),
                    ts: now_ts,
                    kind: EventKind::EvalFailed,
                    from_hash: None,
                    to_hash: None,
                    meta: Some(serde_json::json!({"hint": hint.clone()})),
                })?;
                self.emit(Event::EvalFailed { target: Some(target.clone()), hint, ts: now_ts });
            }
        } else {
            // destination side
            let dest_hex = fd.probe.destination.map(|h| h.to_hex());
            let last = self.journal.last_event_hash(&target)?;
            if dest_hex != last && !self.already_recorded(&target, "dest_changed", dest_hex.as_deref()) {
                let (blob_hash, extra_meta) = self.snapshot_file(&target, now_ts)?;
                let mut meta = serde_json::json!({"class": cs});
                if let Some(serde_json::Value::Object(extra)) = extra_meta {
                    for (k, v) in extra {
                        meta[k] = v;
                    }
                }
                self.journal.record_event(NewEvent {
                    target: Some(&target),
                    ts: now_ts,
                    kind: EventKind::DestChanged,
                    from_hash: last.as_deref(),
                    to_hash: blob_hash.as_deref(),
                    meta: Some(meta),
                })?;
            }
            // source side
            if matches!(
                class,
                DriftClass::SourceAhead | DriftClass::LocalSourceDiverged | DriftClass::Conflict
            ) {
                if let Ok(Some(rendered)) = &fd.probe.rendered {
                    let hex = rendered.to_hex();
                    if !self.already_recorded(&target, "source_changed", Some(&hex)) {
                        self.journal.record_event(NewEvent {
                            target: Some(&target),
                            ts: now_ts,
                            kind: EventKind::SourceChanged,
                            from_hash: None,
                            to_hash: Some(&hex),
                            meta: Some(serde_json::json!({"class": cs})),
                        })?;
                    }
                }
            }
            // remote side
            if matches!(class, DriftClass::RemoteAhead | DriftClass::Conflict)
                && fd.probe.git.remote_ahead
            {
                let remote_hex = fd
                    .source_rel
                    .as_deref()
                    .and_then(|rel| self.git.blob_at(&self.remote_ref, rel).ok().flatten())
                    .map(|bytes| ContentHash::of(&bytes).to_hex());
                if !self.already_recorded(&target, "remote_advanced", remote_hex.as_deref()) {
                    self.journal.record_event(NewEvent {
                        target: Some(&target),
                        ts: now_ts,
                        kind: EventKind::RemoteAdvanced,
                        from_hash: None,
                        to_hash: remote_hex.as_deref(),
                        meta: Some(serde_json::json!({"class": cs})),
                    })?;
                }
            }
        }

        let since = self
            .drift_state
            .get(&target)
            .filter(|(c, _)| *c == cs)
            .map(|(_, s)| *s)
            .unwrap_or(now_ts);
        self.drift_state.insert(target.clone(), (cs.to_string(), since));
        self.emit(Event::Drift { target, class: cs.to_string(), ts: now_ts });
        Ok(())
    }

    fn probe_and_journal(&mut self, target: &Path, now_ts: u64) -> Result<(), DaemonError> {
        match self.scanner.probe_one(target)? {
            Some(fd) => self.journal_probe(&fd, now_ts)?,
            None => {
                self.drift_state.remove(target);
            }
        }
        Ok(())
    }

    fn record_expected_apply(&mut self, target: &Path, now_ts: u64) -> Result<(), DaemonError> {
        let last = self.journal.last_event_hash(target)?;
        let (blob_hash, extra) = self.snapshot_file(target, now_ts)?;
        let mut meta = serde_json::json!({"expected": true});
        if let Some(serde_json::Value::Object(e)) = extra {
            for (k, v) in e {
                meta[k] = v;
            }
        }
        self.journal.record_event(NewEvent {
            target: Some(target),
            ts: now_ts,
            kind: EventKind::Applied,
            from_hash: last.as_deref(),
            to_hash: blob_hash.as_deref(),
            meta: Some(meta),
        })?;
        self.drift_state.remove(target);
        Ok(())
    }

    pub fn handle_paths_changed(&mut self, paths: &[PathBuf], now_ts: u64) -> Result<(), DaemonError> {
        if self.paused {
            return Ok(());
        }
        let mut source_changed = false;
        let mut git_refs_changed = false;
        let mut targets: Vec<PathBuf> = Vec::new();

        for p in paths {
            if p.starts_with(&self.source_dir) {
                let rel = p.strip_prefix(&self.source_dir).unwrap_or(p);
                let mut comps = rel.components();
                if comps.next().map(|c| c.as_os_str() == ".git").unwrap_or(false) {
                    let s = rel.to_string_lossy();
                    if s.ends_with("HEAD") || s.contains("refs") {
                        git_refs_changed = true;
                    }
                } else {
                    source_changed = true;
                }
            } else if self.managed.contains(p) {
                targets.push(p.clone());
            }
        }

        if source_changed {
            let _delta = self.reconcile_managed(now_ts)?;
            self.rescan_source_side(now_ts)?;
        } else if git_refs_changed {
            self.rescan_source_side(now_ts)?;
        }

        for t in targets {
            if self.is_expected(&t, now_ts) {
                self.record_expected_apply(&t, now_ts)?;
            } else {
                self.probe_and_journal(&t, now_ts)?;
            }
        }
        Ok(())
    }

    fn rescan_source_side(&mut self, now_ts: u64) -> Result<(), DaemonError> {
        let mut rels: BTreeSet<PathBuf> = BTreeSet::new();
        rels.extend(self.git.changed_files(&self.remote_ref, "HEAD").unwrap_or_default());
        rels.extend(self.git.dirty_files().unwrap_or_default());
        for rel in rels {
            let abs = self.source_dir.join(&rel);
            let Ok(targets) = self.chezmoi.target_paths(std::slice::from_ref(&abs)) else {
                continue;
            };
            for t in targets {
                if self.managed.contains(&t) {
                    self.probe_and_journal(&t, now_ts)?;
                }
            }
        }
        Ok(())
    }

    pub fn reconcile_managed(&mut self, now_ts: u64) -> Result<WatchDelta, DaemonError> {
        let fresh: BTreeSet<PathBuf> = self.chezmoi.managed()?.into_iter().collect();
        let mut delta = WatchDelta::default();
        for departed in self.managed.difference(&fresh) {
            self.journal.mark_unmanaged(departed, now_ts)?;
            self.journal.record_event(NewEvent {
                target: Some(departed),
                ts: now_ts,
                kind: EventKind::LeftManagement,
                from_hash: None,
                to_hash: None,
                meta: None,
            })?;
            self.drift_state.remove(departed);
            delta.removed.push(departed.clone());
        }
        for arrived in fresh.difference(&self.managed) {
            delta.added.push(arrived.clone());
        }
        for ev in delta
            .removed
            .iter()
            .map(|t| Event::LeftManagement { target: t.clone(), ts: now_ts })
            .collect::<Vec<_>>()
        {
            self.emit(ev);
        }
        self.managed = fresh;
        Ok(delta)
    }

    pub fn full_rescan(&mut self, now_ts: u64) -> Result<u32, DaemonError> {
        self.reconcile_managed(now_ts)?;
        let report = self.scanner.scan()?;
        self.degraded = report.degraded.as_ref().map(|f| f.hint.clone());
        self.in_sync_count = report.in_sync_count as u64;
        let fresh_targets: BTreeSet<PathBuf> =
            report.drifted.iter().map(|d| d.target.clone()).collect();
        let stale: Vec<PathBuf> = self
            .drift_state
            .keys()
            .filter(|t| !fresh_targets.contains(*t))
            .cloned()
            .collect();
        for t in stale {
            self.drift_state.remove(&t);
        }
        let drifted = report.drifted.len() as u32;
        for fd in &report.drifted {
            self.journal_probe(fd, now_ts)?;
        }
        self.journal.record_event(NewEvent {
            target: None,
            ts: now_ts,
            kind: EventKind::Fetch,
            from_hash: None,
            to_hash: None,
            meta: Some(serde_json::json!({"scan": true, "drifted": drifted})),
        })?;
        self.emit(Event::ScanDone { ts: now_ts, drifted });
        Ok(drifted)
    }

    pub fn handle_fetch(&mut self, now_ts: u64) -> Result<(), DaemonError> {
        match self.git.fetch("origin") {
            Ok(()) => {}
            Err(e) => {
                self.journal.record_event(NewEvent {
                    target: None,
                    ts: now_ts,
                    kind: EventKind::Fetch,
                    from_hash: None,
                    to_hash: None,
                    meta: Some(serde_json::json!({"error": e.to_string()})),
                })?;
                self.emit(Event::FetchDone { ts: now_ts, behind: 0 });
                return Ok(());
            }
        }
        let behind = self.git.divergence(&self.remote_ref).map(|d| d.behind).unwrap_or(0);
        self.journal.record_event(NewEvent {
            target: None,
            ts: now_ts,
            kind: EventKind::Fetch,
            from_hash: None,
            to_hash: None,
            meta: Some(serde_json::json!({"behind": behind})),
        })?;
        let changed = self.git.changed_files("HEAD", &self.remote_ref).unwrap_or_default();
        for rel in changed {
            let abs = self.source_dir.join(&rel);
            let Ok(targets) = self.chezmoi.target_paths(std::slice::from_ref(&abs)) else {
                continue;
            };
            for t in targets {
                if self.managed.contains(&t) {
                    self.probe_and_journal(&t, now_ts)?;
                }
            }
        }
        self.emit(Event::FetchDone { ts: now_ts, behind });
        Ok(())
    }

    pub fn snapshot_blobs(&mut self, paths: &[PathBuf], now_ts: u64) -> Result<Vec<String>, DaemonError> {
        let mut hashes = Vec::new();
        for p in paths {
            if let (Some(h), _) = self.snapshot_file(p, now_ts)? {
                hashes.push(h);
            }
        }
        Ok(hashes)
    }

    pub fn status_snapshot(&self) -> (Vec<DriftSummary>, u64, Option<String>) {
        let drifted = self
            .drift_state
            .iter()
            .map(|(t, (class, since))| DriftSummary {
                target: t.clone(),
                class: class.clone(),
                since_ts: Some(*since),
            })
            .collect();
        (drifted, self.in_sync_count, self.degraded.clone())
    }
}
```

- [x] **Step 5: Run tests**

Run: `cargo test -p czui-daemon --test daemon_core`
Expected: 6 passed. (These drive real chezmoi/git against scratch dirs — allow ~30s.)

- [x] **Step 6: Full gate + commit**

Run (separately): `cargo fmt --all`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace`

```bash
git add Cargo.toml Cargo.lock crates/daemon
git commit -m "feat(daemon): DaemonCore drift state machine with suppression and reconciliation"
```

---

### Task 3: Debouncer

**Files:**
- Modify: `crates/daemon/src/debounce.rs`

**Interfaces:**
- Produces:
  - `Debouncer::new(window: Duration) -> (Debouncer, std::sync::mpsc::Sender<PathBuf>)` — feed raw paths in
  - `Debouncer::recv_batch(&self) -> Option<Vec<PathBuf>>` — blocks until a quiet-window elapses after the first path of a burst, then returns the deduplicated batch; `None` when all senders dropped
  - Internally: first path starts a window; the batch flushes once `window` has passed since the *last* received path (rolling debounce, capped at 10× window so floods still flush).

- [x] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn coalesces_bursts_and_dedups() {
        let (deb, tx) = Debouncer::new(Duration::from_millis(50));
        for _ in 0..3 {
            tx.send(std::path::PathBuf::from("/a")).unwrap();
        }
        tx.send(std::path::PathBuf::from("/b")).unwrap();
        let batch = deb.recv_batch().unwrap();
        assert_eq!(batch.len(), 2, "{batch:?}");
        assert!(batch.contains(&std::path::PathBuf::from("/a")));
        assert!(batch.contains(&std::path::PathBuf::from("/b")));
    }

    #[test]
    fn separate_bursts_are_separate_batches() {
        let (deb, tx) = Debouncer::new(Duration::from_millis(30));
        tx.send(std::path::PathBuf::from("/one")).unwrap();
        let b1 = deb.recv_batch().unwrap();
        tx.send(std::path::PathBuf::from("/two")).unwrap();
        let b2 = deb.recv_batch().unwrap();
        assert_eq!((b1.len(), b2.len()), (1, 1));
    }

    #[test]
    fn returns_none_when_senders_gone() {
        let (deb, tx) = Debouncer::new(Duration::from_millis(10));
        drop(tx);
        assert!(deb.recv_batch().is_none());
    }
}
```

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-daemon debounce`
Expected: compile errors.

- [x] **Step 3: Implement**

```rust
//! Rolling debounce for filesystem event paths (spec §3.1, ~500ms window).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

pub struct Debouncer {
    rx: Receiver<PathBuf>,
    window: Duration,
}

impl Debouncer {
    pub fn new(window: Duration) -> (Self, Sender<PathBuf>) {
        let (tx, rx) = channel();
        (Self { rx, window }, tx)
    }

    pub fn recv_batch(&self) -> Option<Vec<PathBuf>> {
        // block for the first path of a burst
        let first = self.rx.recv().ok()?;
        let mut batch = BTreeSet::new();
        batch.insert(first);
        let start = Instant::now();
        let cap = self.window * 10;
        loop {
            match self.rx.recv_timeout(self.window) {
                Ok(p) => {
                    batch.insert(p);
                    if start.elapsed() >= cap {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        Some(batch.into_iter().collect())
    }
}
```

(`Instant` here is fine — the debouncer is runtime plumbing, not journaled state; the no-clock rule applies to `DaemonCore`.)

- [x] **Step 4: Run tests**

Run: `cargo test -p czui-daemon debounce`
Expected: 3 passed.

- [x] **Step 5: Full gate + commit**

```bash
git add crates/daemon/src/debounce.rs
git commit -m "feat(daemon): rolling debouncer for filesystem event bursts"
```

---

### Task 4: Socket server

**Files:**
- Modify: `crates/daemon/src/server.rs`
- Create: `crates/daemon/tests/server_ipc.rs`

**Interfaces:**
- Produces:
  - `serve(listener: std::os::unix::net::UnixListener, core: Arc<Mutex<DaemonCore>>, now_fn: fn() -> u64) -> std::io::Result<()>` — accept loop; one thread per connection; blocks forever (callers spawn it)
  - Protocol per spec §3.3 and czui-proto: first frame must be `Hello`; version mismatch → `Error` reply then close. `Subscribe` registers the connection for `Push` frames. All requests get a `Reply` with the echoed id.
  - Request dispatch: `Status` → `status_snapshot`; `Timeline`/`EventsFor` → journal queries mapped to `EventSummary`; `ExpectChanges`/`Rescan`/`Pause`/`Resume`/`SnapshotBlobs` → core calls; `SessionStart/Decision/End` → journal sessions plus `session_start`/`session_end` events.

- [x] **Step 1: Write the failing integration test**

`crates/daemon/tests/server_ipc.rs`:
```rust
//! Socket round-trip: handshake, request/reply, subscribe/push.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use czui_core::chezmoi::ChezmoiClient;
use czui_core::cmd::SystemRunner;
use czui_core::git::GitClient;
use czui_core::testsupport::Scratch;
use czui_daemon::core::DaemonCore;
use czui_daemon::server::serve;
use czui_journal::Journal;
use czui_proto::{
    read_frame, write_frame, ClientFrame, Request, Response, ServerFrame, PROTOCOL_VERSION,
};

fn setup() -> (Scratch, Arc<Mutex<DaemonCore>>, UnixStream) {
    let s = Scratch::new();
    let chezmoi: ChezmoiClient = s.chezmoi();
    let git = GitClient::new(Arc::new(SystemRunner), s.source.clone());
    let journal = Journal::open_in_memory("ipc-test").unwrap();
    let core = Arc::new(Mutex::new(
        DaemonCore::new(chezmoi, git, journal, "origin/main".into()).unwrap(),
    ));
    let sock = s.root.path().join("d.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let served = core.clone();
    std::thread::spawn(move || serve(listener, served, || 1000));
    let stream = UnixStream::connect(&sock).unwrap();
    (s, core, stream)
}

fn send(stream: &mut UnixStream, id: u64, request: Request) {
    write_frame(stream, &ClientFrame { id, request }).unwrap();
}

fn recv(reader: &mut BufReader<UnixStream>) -> ServerFrame {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    read_frame(line.trim_end()).unwrap()
}

#[test]
fn handshake_then_status_and_push() {
    let (s, core, mut stream) = setup();
    let mut reader = BufReader::new(stream.try_clone().unwrap());

    send(&mut stream, 1, Request::Hello { version: PROTOCOL_VERSION });
    match recv(&mut reader) {
        ServerFrame::Reply { id: 1, response: Response::HelloOk { version, machine } } => {
            assert_eq!(version, PROTOCOL_VERSION);
            assert_eq!(machine, "ipc-test");
        }
        other => panic!("bad hello reply: {other:?}"),
    }

    send(&mut stream, 2, Request::Subscribe);
    assert!(matches!(recv(&mut reader), ServerFrame::Reply { id: 2, response: Response::Ok }));

    send(&mut stream, 3, Request::Status);
    match recv(&mut reader) {
        ServerFrame::Reply { id: 3, response: Response::Status { drifted, .. } } => {
            assert!(drifted.is_empty());
        }
        other => panic!("bad status: {other:?}"),
    }

    // trigger a drift; the push must arrive on this connection
    let target = s.home.join(".testrc");
    std::fs::write(&target, "a=pushed\n").unwrap();
    core.lock().unwrap().handle_paths_changed(&[target.clone()], 1234).unwrap();
    match recv(&mut reader) {
        ServerFrame::Push { event: czui_proto::Event::Drift { target: t, ts: 1234, .. } } => {
            assert_eq!(t, target);
        }
        other => panic!("expected push, got {other:?}"),
    }
}

#[test]
fn version_mismatch_is_rejected() {
    let (_s, _core, mut stream) = setup();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    send(&mut stream, 1, Request::Hello { version: PROTOCOL_VERSION + 9 });
    match recv(&mut reader) {
        ServerFrame::Reply { id: 1, response: Response::Error { message } } => {
            assert!(message.contains("mismatch"));
        }
        other => panic!("expected error, got {other:?}"),
    }
}

#[test]
fn session_flow_over_ipc() {
    let (_s, core, mut stream) = setup();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    send(&mut stream, 1, Request::Hello { version: PROTOCOL_VERSION });
    recv(&mut reader);
    send(&mut stream, 2, Request::SessionStart { ts: 50 });
    let session = match recv(&mut reader) {
        ServerFrame::Reply { response: Response::SessionStarted { session }, .. } => session,
        other => panic!("{other:?}"),
    };
    send(&mut stream, 3, Request::SessionDecision { session, decision: serde_json::json!({"c": "ours"}) });
    recv(&mut reader);
    send(&mut stream, 4, Request::SessionEnd { session, ts: 60, summary: "done".into() });
    recv(&mut reader);
    let tl = core.lock().unwrap().journal().timeline(10, None).unwrap();
    let kinds: Vec<_> = tl.iter().map(|e| e.kind.as_str().to_string()).collect();
    assert!(kinds.contains(&"session_start".to_string()));
    assert!(kinds.contains(&"session_end".to_string()));
}
```

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-daemon --test server_ipc`
Expected: compile errors (`serve` undefined).

- [x] **Step 3: Implement**

`crates/daemon/src/server.rs`:
```rust
//! Unix-socket IPC server (spec §3.3): ndjson frames, hello handshake,
//! request/reply with ids, id-less pushes to subscribers.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use czui_proto::{
    check_hello, write_frame, ClientFrame, Event, EventSummary, Request, Response, ServerFrame,
    PROTOCOL_VERSION,
};

use crate::core::DaemonCore;

pub fn serve(
    listener: UnixListener,
    core: Arc<Mutex<DaemonCore>>,
    now_fn: fn() -> u64,
) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let core = core.clone();
        std::thread::spawn(move || {
            let _ = handle_connection(stream, core, now_fn);
        });
    }
    Ok(())
}

fn reply(out: &Arc<Mutex<UnixStream>>, id: u64, response: Response) -> std::io::Result<()> {
    let mut w = out.lock().expect("socket writer poisoned");
    write_frame(&mut *w, &ServerFrame::Reply { id, response })
}

fn handle_connection(
    stream: UnixStream,
    core: Arc<Mutex<DaemonCore>>,
    now_fn: fn() -> u64,
) -> std::io::Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let out = Arc::new(Mutex::new(stream));
    let mut hello_done = false;

    for line in reader.lines() {
        let line = line?;
        let frame: ClientFrame = match czui_proto::read_frame(&line) {
            Ok(f) => f,
            Err(e) => {
                // no id to echo; use 0 per protocol convention for parse errors
                reply(&out, 0, Response::Error { message: format!("bad frame: {e}") })?;
                continue;
            }
        };
        let id = frame.id;

        if !hello_done {
            match frame.request {
                Request::Hello { version } => match check_hello(version) {
                    Ok(()) => {
                        hello_done = true;
                        let machine = core.lock().expect("core poisoned").machine().to_string();
                        reply(&out, id, Response::HelloOk { version: PROTOCOL_VERSION, machine })?;
                    }
                    Err(message) => {
                        reply(&out, id, Response::Error { message })?;
                        return Ok(()); // close on mismatch
                    }
                },
                _ => {
                    reply(&out, id, Response::Error { message: "hello required first".into() })?;
                }
            }
            continue;
        }

        let response = dispatch(&core, frame.request, &out, now_fn);
        reply(&out, id, response)?;
    }
    Ok(())
}

fn event_summaries(rows: Vec<czui_journal::EventRow>) -> Vec<EventSummary> {
    rows.into_iter()
        .map(|e| EventSummary { id: e.id, target: e.target, kind: e.kind, ts: e.ts })
        .collect()
}

fn dispatch(
    core: &Arc<Mutex<DaemonCore>>,
    request: Request,
    out: &Arc<Mutex<UnixStream>>,
    now_fn: fn() -> u64,
) -> Response {
    let now = now_fn();
    let mut c = match core.lock() {
        Ok(c) => c,
        Err(_) => return Response::Error { message: "daemon state poisoned".into() },
    };
    match request {
        Request::Hello { .. } => Response::HelloOk {
            version: PROTOCOL_VERSION,
            machine: c.machine().to_string(),
        },
        Request::Subscribe => {
            let rx = c.subscribe();
            let out = out.clone();
            std::thread::spawn(move || {
                for ev in rx {
                    let Ok(mut w) = out.lock() else { break };
                    let frame = ServerFrame::Push { event: ev };
                    if write_frame(&mut *w, &frame).is_err() {
                        break;
                    }
                }
            });
            Response::Ok
        }
        Request::Status => {
            let (drifted, in_sync, degraded) = c.status_snapshot();
            Response::Status { drifted, in_sync, degraded }
        }
        Request::Timeline { limit, before_id } => match c.journal().timeline(limit, before_id) {
            Ok(rows) => Response::Timeline { events: event_summaries(rows) },
            Err(e) => Response::Error { message: e.to_string() },
        },
        Request::EventsFor { target, limit } => match c.journal().events_for(&target, limit) {
            Ok(rows) => Response::Timeline { events: event_summaries(rows) },
            Err(e) => Response::Error { message: e.to_string() },
        },
        Request::ExpectChanges { paths, ttl_secs } => {
            c.expect_changes(&paths, ttl_secs, now);
            Response::Ok
        }
        Request::Rescan => match c.full_rescan(now) {
            Ok(_) => Response::Ok,
            Err(e) => Response::Error { message: e.to_string() },
        },
        Request::Pause => {
            c.set_paused(true);
            Response::Ok
        }
        Request::Resume => {
            c.set_paused(false);
            Response::Ok
        }
        Request::SnapshotBlobs { paths } => match c.snapshot_blobs(&paths, now) {
            Ok(hashes) => Response::Blobs { hashes },
            Err(e) => Response::Error { message: e.to_string() },
        },
        Request::SessionStart { ts } => {
            let j = c.journal();
            match j.begin_session(ts) {
                Ok(session) => {
                    let _ = j.record_event(czui_journal::NewEvent {
                        target: None,
                        ts,
                        kind: czui_journal::EventKind::SessionStart,
                        from_hash: None,
                        to_hash: None,
                        meta: Some(serde_json::json!({"session": session})),
                    });
                    Response::SessionStarted { session }
                }
                Err(e) => Response::Error { message: e.to_string() },
            }
        }
        Request::SessionDecision { session, decision } => {
            match c.journal().add_decision(session, &decision) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error { message: e.to_string() },
            }
        }
        Request::SessionEnd { session, ts, summary } => {
            let j = c.journal();
            match j.end_session(session, ts, &summary) {
                Ok(()) => {
                    let _ = j.record_event(czui_journal::NewEvent {
                        target: None,
                        ts,
                        kind: czui_journal::EventKind::SessionEnd,
                        from_hash: None,
                        to_hash: None,
                        meta: Some(serde_json::json!({"session": session, "summary": summary})),
                    });
                    Response::Ok
                }
                Err(e) => Response::Error { message: e.to_string() },
            }
        }
    }
}
```

Implementer note: the two `expect("… poisoned")` calls are on `Mutex::lock` results inside server plumbing where a poisoned mutex means the daemon is already broken; if clippy or the constraints flag them, convert to graceful `let Ok(..) else { return … }` like the subscriber loop does. Note `dispatch` holds the core lock while journaling — correct for the single-writer invariant.

- [x] **Step 4: Run tests**

Run: `cargo test -p czui-daemon --test server_ipc`
Expected: 3 passed.

- [x] **Step 5: Full gate + commit**

```bash
git add crates/daemon/src/server.rs crates/daemon/tests/server_ipc.rs
git commit -m "feat(daemon): unix socket IPC server with handshake, dispatch, and push fanout"
```

---

### Task 5: Settings + `chezmoid` binary + real-machine smoke

**Files:**
- Modify: `crates/daemon/src/settings.rs`, `crates/daemon/src/main.rs`

**Interfaces:**
- Produces:
  - `Settings { fetch_interval_minutes: u64 (default 15), onepassword_account: Option<String> }` with `Settings::load(path: &Path) -> Settings` (missing/invalid file → defaults; invalid logs to stderr)
  - `Settings::chezmoi_env(&self) -> Vec<(String, String)>` (`OP_ACCOUNT` when set — spec §9)
  - `app_support_dir() -> PathBuf` (`$HOME/Library/Application Support/ChezmoiUI`, overridable for every consumer through the env vars below)
  - `chezmoid` binary: env overrides `CZUI_SETTINGS`, `CZUI_JOURNAL`, `CZUI_SOCKET`; flag `--once` = single full rescan printed to stdout, no watcher/fetch/socket (read-only smoke mode)

- [x] **Step 1: Write the failing settings tests** (in `settings.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_missing() {
        let s = Settings::load(std::path::Path::new("/nonexistent/settings.toml"));
        assert_eq!(s.fetch_interval_minutes, 15);
        assert!(s.onepassword_account.is_none());
        assert!(s.chezmoi_env().is_empty());
    }

    #[test]
    fn parses_and_injects_op_account() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.toml");
        std::fs::write(&p, "fetch_interval_minutes = 5\nonepassword_account = \"my.acct\"\n").unwrap();
        let s = Settings::load(&p);
        assert_eq!(s.fetch_interval_minutes, 5);
        assert_eq!(
            s.chezmoi_env(),
            vec![("OP_ACCOUNT".to_string(), "my.acct".to_string())]
        );
    }
}
```

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-daemon settings`
Expected: compile errors.

- [x] **Step 3: Implement settings**

`crates/daemon/src/settings.rs`:
```rust
//! Shared app settings (spec §9): read by daemon and app.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub fetch_interval_minutes: u64,
    pub onepassword_account: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { fetch_interval_minutes: 15, onepassword_account: None }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("chezmoid: invalid settings at {}: {e}; using defaults", path.display());
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        }
    }

    /// Environment injected into every chezmoi/op subprocess (spec §9):
    /// never interactive, account selection comes from settings.
    pub fn chezmoi_env(&self) -> Vec<(String, String)> {
        match &self.onepassword_account {
            Some(acct) => vec![("OP_ACCOUNT".to_string(), acct.clone())],
            None => Vec::new(),
        }
    }
}

pub fn app_support_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join("Library/Application Support/ChezmoiUI")
}
```

- [x] **Step 4: Implement the binary**

`crates/daemon/src/main.rs`:
```rust
//! chezmoid — chezmoi-ui watcher daemon (spec §3.1).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use notify::{RecursiveMode, Watcher};

use czui_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use czui_core::cmd::SystemRunner;
use czui_core::git::GitClient;
use czui_daemon::core::DaemonCore;
use czui_daemon::debounce::Debouncer;
use czui_daemon::server::serve;
use czui_daemon::settings::{app_support_dir, Settings};
use czui_journal::Journal;

fn now_ts() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn env_path(var: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or(default)
}

fn main() -> ExitCode {
    let once = std::env::args().any(|a| a == "--once");
    let support = app_support_dir();
    let settings = Settings::load(&env_path("CZUI_SETTINGS", support.join("settings.toml")));
    let journal_path = env_path("CZUI_JOURNAL", support.join("journal.db"));
    let socket_path = env_path("CZUI_SOCKET", support.join("daemon.sock"));

    if let Some(parent) = journal_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("chezmoid: cannot create {}: {e}", parent.display());
            return ExitCode::FAILURE;
        }
    }

    let runner = Arc::new(SystemRunner);
    let chezmoi = ChezmoiClient::new(
        runner.clone(),
        ChezmoiOptions { env: settings.chezmoi_env(), ..ChezmoiOptions::default() },
    );
    let source_dir = match chezmoi.source_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("chezmoid: cannot locate chezmoi source dir: {e}");
            return ExitCode::FAILURE;
        }
    };
    let git = GitClient::new(runner, source_dir.clone());
    let branch = git.head_branch().unwrap_or_else(|_| "main".into());
    let remote_ref = format!("origin/{branch}");
    let machine = gethostname::gethostname().to_string_lossy().into_owned();
    let journal = match Journal::open(&journal_path, &machine) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("chezmoid: cannot open journal {}: {e}", journal_path.display());
            return ExitCode::FAILURE;
        }
    };
    let mut core = match DaemonCore::new(chezmoi, git, journal, remote_ref) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("chezmoid: init failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    match core.full_rescan(now_ts()) {
        Ok(drifted) => {
            let (list, in_sync, degraded) = core.status_snapshot();
            if let Some(hint) = &degraded {
                eprintln!("degraded scan: {hint}");
            }
            for d in &list {
                println!("{:<22} {}", d.class, d.target.display());
            }
            println!("-- {drifted} drifted, {in_sync} in sync");
        }
        Err(e) => {
            eprintln!("chezmoid: initial scan failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    if once {
        return ExitCode::SUCCESS;
    }

    let core = Arc::new(Mutex::new(core));

    // watcher → debouncer
    let (debouncer, tx) = Debouncer::new(Duration::from_millis(500));
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            for p in event.paths {
                let _ = tx.send(p);
            }
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("chezmoid: watcher init failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    {
        let c = core.lock().expect("core lock");
        for p in c.watch_paths() {
            let mode = if p == source_dir { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
            if let Err(e) = watcher.watch(&p, mode) {
                eprintln!("chezmoid: watch {} failed: {e}", p.display());
            }
        }
    }

    // debounce loop (owns the watcher so watch-set deltas can be applied)
    {
        let core = core.clone();
        std::thread::spawn(move || {
            let mut watcher = watcher; // owned mutably inside the thread
            while let Some(batch) = debouncer.recv_batch() {
                let mut c = match core.lock() {
                    Ok(c) => c,
                    Err(_) => break,
                };
                let before: std::collections::BTreeSet<_> = c.watch_paths().into_iter().collect();
                if let Err(e) = c.handle_paths_changed(&batch, now_ts()) {
                    eprintln!("chezmoid: change handling failed: {e}");
                }
                let after: std::collections::BTreeSet<_> = c.watch_paths().into_iter().collect();
                drop(c);
                for removed in before.difference(&after) {
                    let _ = watcher.unwatch(removed);
                }
                for added in after.difference(&before) {
                    let _ = watcher.watch(added, RecursiveMode::NonRecursive);
                }
            }
        });
    }

    // fetch timer
    {
        let core = core.clone();
        let interval = Duration::from_secs(settings.fetch_interval_minutes.max(1) * 60);
        std::thread::spawn(move || loop {
            std::thread::sleep(interval);
            if let Ok(mut c) = core.lock() {
                if let Err(e) = c.handle_fetch(now_ts()) {
                    eprintln!("chezmoid: fetch failed: {e}");
                }
            }
        });
    }

    // hourly rescan safety net (spec §3.1: FSEvents can drop events)
    {
        let core = core.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(3600));
            if let Ok(mut c) = core.lock() {
                if let Err(e) = c.full_rescan(now_ts()) {
                    eprintln!("chezmoid: rescan failed: {e}");
                }
            }
        });
    }

    // socket server (foreground)
    let _ = std::fs::remove_file(&socket_path);
    let listener = match std::os::unix::net::UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("chezmoid: cannot bind {}: {e}", socket_path.display());
            return ExitCode::FAILURE;
        }
    };
    println!("chezmoid: listening on {}", socket_path.display());
    if let Err(e) = serve(listener, core, now_ts) {
        eprintln!("chezmoid: server failed: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
```

- [x] **Step 5: Real-machine smoke (read-only)**

Run: `CZUI_JOURNAL=$(mktemp -d)/journal.db cargo run -p czui-daemon --bin chezmoid -- --once` (in nushell: `with-env {CZUI_JOURNAL: ...} { cargo run ... }`; a plain `mkdir`ed temp path is fine)
Expected: same shape as Plan 1's drift-scan — possibly a `degraded scan:` hint about OP_ACCOUNT, drifted lines with snake_case classes, summary line, exit 0. The journal lands in the temp path, NOT in `~/Library/Application Support`. No fetch, no socket, no mutations.

- [x] **Step 6: Full gate + commit**

Run (separately): `cargo fmt --all`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace`

```bash
git add crates/daemon/src/settings.rs crates/daemon/src/main.rs
git commit -m "feat(daemon): chezmoid binary with settings, watcher wiring, timers, and --once smoke mode"
```

---

## Self-Review Notes (completed during plan writing)

- **Spec coverage for this plan's slice:** §3.1 watch set incl. source dir + git refs filter, debounce/coalesce, expected-changes suppression w/ TTL, managed-set reconciliation → `left_management` + watch delta, targeted per-event probing, fetch cadence + honest fetch-failure handling, hourly rescan safety net (binary), rescan-on-start (binary) ✓. §3.3 server: handshake/reject, ids echoed, pushes id-less, journaling commands keep daemon as sole writer ✓. §8 blob snapshots on dest changes + 4 MiB cap ✓. §9 OP_ACCOUNT injection from shared settings.toml, non-interactive always ✓ (Task 5). §10 daemon crash-safety = WAL + launchd KeepAlive (packaging is Plan 6; not lost, listed there).
- **Type consistency:** `class_str` snake_case strings match daemon_core test assertions and proto `DriftSummary.class` consumers; `Scratch` fields (`home`, `source`, `root`, `bare`, `chezmoi()`, `scanner()`) match Plan 1's helper; `EventKind` variants match Plan 3.
- **Known simplifications (accepted for v0):** `already_recorded` inspects only the last 10 events per target; wake-from-sleep rescan is folded into the hourly timer (no IOKit sleep notifications yet); no SIGTERM handler (WAL is crash-safe, launchd restarts); `Subscribe`'s push-forwarding thread holds the write mutex per frame (fine at dotfile event rates); `snapshot_blobs` skips >4 MiB files silently in the returned hash list (they still hash for events).
- **Race note encoded in the design:** the debounce loop applies watch-set deltas by diffing `watch_paths()` before/after handling — this keeps `notify` registration in sync with reconciliation without the core knowing about the watcher.
