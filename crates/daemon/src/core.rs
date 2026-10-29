//! DaemonCore: single-threaded drift state machine (spec §3.1).
//! No clocks, no I/O loops — callers supply `now_ts` and drive it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use tomte_core::chezmoi::{ChezmoiClient, ChezmoiError};
use tomte_core::drift::{ContentHash, DriftClass};
use tomte_core::git::{GitClient, GitError, MergeStart, MergeTreeOutcome};
use tomte_core::scanner::{DriftScanner, FileDrift, ScanError, ScanReport};
use tomte_core::type_change::{EntryKind, TypeChange};
use tomte_journal::{BLOB_CAP, EventKind, Journal, JournalError, NewEvent};
use tomte_proto::{DriftSummary, Event, WireEntryKind, WireTypeChange};

/// How long the unattended auto-join may wait for the commit signer to prove
/// it is awake. Long enough for a signer that is merely slow, far short of
/// the IPC "daemon busy" cutoff (20s) that a stalled fetch would trip.
const SIGNER_PROBE: std::time::Duration = std::time::Duration::from_secs(5);

/// Ceiling on the auto-join's own `git merge`. The signer pre-flight should
/// already have kept us out of any wait, but a signer that answers the probe
/// and then stalls on the real commit must still not hold the core lock long
/// enough for the app to read the daemon as stuck (2026-08-02). User-driven
/// merges keep [`GitClient::merge`]'s full 60s — there, waiting is correct.
const AUTO_JOIN_MERGE: std::time::Duration = std::time::Duration::from_secs(10);

/// A scan "in progress" for longer than this is presumed dead (its thread
/// never came back) and taken over by the next `begin_scan`.
const SCAN_STALE_SECS: u64 = 30 * 60;

/// Proof that `begin_scan` ran, consumed by `apply_scan`. Carries the
/// generation so a report from a scan that was taken over is recognised.
pub struct ScanTicket {
    generation: u64,
    started_ts: u64,
}

/// What `apply_scan` hands back: the live drifted count, and whether a
/// rescan was asked for while this one ran (the caller goes again).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanOutcome {
    pub drifted: u32,
    pub rescan_wanted: bool,
}

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
        DriftClass::TypeChanged => "type_changed",
    }
}

fn wire_kind(kind: &EntryKind) -> WireEntryKind {
    match kind {
        EntryKind::Absent => WireEntryKind::Absent,
        EntryKind::File => WireEntryKind::File,
        EntryKind::Folder => WireEntryKind::Folder,
        EntryKind::Link(text) => WireEntryKind::Link { text: text.clone() },
    }
}

pub fn wire(tc: &TypeChange) -> WireTypeChange {
    WireTypeChange {
        source: wire_kind(&tc.source),
        disk: wire_kind(&tc.disk),
        disk_moved: tc.disk_moved,
    }
}

/// What one watcher path means to the daemon (spec 2026-10-11 §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathChange {
    /// A file of the chezmoi source tree changed.
    SourceContent,
    /// The source repo's own HEAD or refs moved (commit, fetch, checkout).
    SourceGitRefs,
    /// A managed destination file changed.
    Target,
    /// Nothing the daemon acts on: the top-level `.git`'s index, objects and
    /// locks; anything inside a NESTED repository's `.git` (a vendored skill
    /// with its own history — `git status` in the source repo refreshes its
    /// index and writes `index.lock` there, and hearing that as a source
    /// change was a self-sustaining rescan loop, 2026-10-11); an unmanaged
    /// path outside the source tree.
    Ignored,
}

/// Pure, so the rule is a value a test can read. `managed` is the current
/// managed-target set.
pub fn classify_path(source_dir: &Path, managed: &BTreeSet<PathBuf>, path: &Path) -> PathChange {
    let Ok(rel) = path.strip_prefix(source_dir) else {
        return if managed.contains(path) {
            PathChange::Target
        } else {
            PathChange::Ignored
        };
    };
    let mut comps = rel.components();
    let first_is_git = comps.next().is_some_and(|c| c.as_os_str() == ".git");
    // `comps` now starts at the second component: a `.git` anywhere below
    // the first one is a nested repository.
    if comps.any(|c| c.as_os_str() == ".git") {
        return PathChange::Ignored;
    }
    if first_is_git {
        let s = rel.to_string_lossy();
        if s.ends_with("HEAD") || s.contains("refs") {
            PathChange::SourceGitRefs
        } else {
            PathChange::Ignored
        }
    } else {
        PathChange::SourceContent
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
    drift_state: BTreeMap<PathBuf, (String, u64, Option<WireTypeChange>)>,
    in_sync_count: u64,
    /// Managed TARGET paths whose origin source differs from local's, from
    /// the most recent full scan. Retained between scans exactly like
    /// `in_sync_count`, so `status_snapshot` can read it between rescans.
    origin_differs: BTreeSet<PathBuf>,
    degraded: Option<String>,
    paused: bool,
    /// Outcome of the previous full scan — journal scans only on change.
    last_scan_drifted: Option<u32>,
    /// Last SUCCESSFUL origin fetch (seeded from the journal at startup so
    /// freshness survives daemon restarts).
    last_fetch_ts: Option<u64>,
    /// Shared with the socket server so Subscribe never needs the core
    /// lock (the initial scan can hold it for minutes on secret-manager
    /// timeouts — head-of-line blocking the handshake was a real bug).
    subscribers: std::sync::Arc<Mutex<Vec<Sender<Event>>>>,
    /// A scan is running OFF the lock (spec 2026-10-11 §2.2): `begin_scan`
    /// set it, `apply_scan` clears it. `Status` reports it as `scanning`.
    scan_in_progress: bool,
    scan_started_ts: u64,
    /// Bumped by every `begin_scan`; a `ScanTicket` carries the value it
    /// was issued with, so a taken-over scan's late report is recognised.
    scan_generation: u64,
    /// A rescan asked for while one ran. `apply_scan` hands it back.
    rescan_wanted: bool,
    /// Targets the per-event path wrote `drift_state` for since
    /// `begin_scan`. `apply_scan` leaves these alone: the report's view of
    /// them is older than the daemon's own.
    touched_during_scan: BTreeSet<PathBuf>,
}

impl DaemonCore {
    pub fn new(
        chezmoi: ChezmoiClient,
        git: GitClient,
        journal: Journal,
        remote_ref: String,
    ) -> Result<Self, DaemonError> {
        Self::new_with_subscribers(chezmoi, git, journal, remote_ref, std::sync::Arc::default())
    }

    /// Like [`DaemonCore::new`], but sharing an externally-owned subscriber
    /// list — the server hands out subscriptions before the core exists.
    pub fn new_with_subscribers(
        chezmoi: ChezmoiClient,
        git: GitClient,
        journal: Journal,
        remote_ref: String,
        subscribers: std::sync::Arc<Mutex<Vec<Sender<Event>>>>,
    ) -> Result<Self, DaemonError> {
        let source_dir = chezmoi.source_dir()?;
        let managed: BTreeSet<PathBuf> = chezmoi.managed()?.into_iter().collect();
        let scanner = DriftScanner::new(chezmoi.clone(), git.clone(), remote_ref.clone());
        let last_fetch_ts = journal.timeline(200, None).ok().and_then(|rows| {
            rows.into_iter()
                .filter(|r| r.kind == "fetch")
                .find(|r| r.meta.as_ref().is_none_or(|m| m.get("error").is_none()))
                .map(|r| r.ts)
        });
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
            origin_differs: BTreeSet::new(),
            degraded: None,
            paused: false,
            last_scan_drifted: None,
            last_fetch_ts,
            subscribers,
            scan_in_progress: false,
            scan_started_ts: 0,
            scan_generation: 0,
            rescan_wanted: false,
            touched_during_scan: BTreeSet::new(),
        })
    }

    /// The resolved chezmoi source directory (watcher needs it for the
    /// recursive-watch decision).
    pub fn source_dir(&self) -> &Path {
        &self.source_dir
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

    /// Clone of the shared subscriber list for lock-free-of-core Subscribe.
    pub fn subscriber_handle(&self) -> std::sync::Arc<Mutex<Vec<Sender<Event>>>> {
        self.subscribers.clone()
    }

    pub fn subscribe(&mut self) -> Receiver<Event> {
        let (tx, rx) = channel();
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.push(tx);
        }
        rx
    }

    fn emit(&mut self, ev: Event) {
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.retain(|s| s.send(ev.clone()).is_ok());
        }
    }

    pub fn expect_changes(&mut self, paths: &[PathBuf], ttl_secs: u32, now_ts: u64) {
        let deadline = now_ts + ttl_secs as u64;
        for p in paths {
            self.expected.push((p.clone(), deadline));
        }
    }

    /// The ONLY two writers of `drift_state` outside `apply_scan`. Both note
    /// the target while a scan is in progress, which is what lets the scan
    /// run with the lock released and still never overwrite something the
    /// daemon learned after the scan looked.
    fn set_drift(
        &mut self,
        target: PathBuf,
        class: String,
        since: u64,
        type_change: Option<WireTypeChange>,
    ) {
        if self.scan_in_progress {
            self.touched_during_scan.insert(target.clone());
        }
        self.drift_state.insert(target, (class, since, type_change));
    }

    fn clear_drift(&mut self, target: &Path) {
        if self.scan_in_progress {
            self.touched_during_scan.insert(target.to_path_buf());
        }
        self.drift_state.remove(target);
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

    /// Whether the newest `type_changed` event of `target` already names
    /// this `(source, disk)` pair, so rescans of the same change stay quiet.
    fn type_change_recorded(
        &self,
        target: &Path,
        source: &serde_json::Value,
        disk: &serde_json::Value,
    ) -> bool {
        match self.journal.events_for(target, 10) {
            Ok(events) => events
                .iter()
                .find(|e| e.kind == "type_changed")
                .and_then(|e| e.meta.as_ref())
                .is_some_and(|m| m.get("source") == Some(source) && m.get("disk") == Some(disk)),
            Err(_) => false,
        }
    }

    fn snapshot_file(
        &self,
        path: &Path,
        now_ts: u64,
    ) -> Result<(Option<String>, Option<serde_json::Value>), DaemonError> {
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

    /// Writes `drift_state` through `set_drift`, which is what makes an
    /// off-lock scan safe.
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
                self.emit(Event::EvalFailed {
                    target: Some(target.clone()),
                    hint,
                    ts: now_ts,
                });
            }
        } else if class == DriftClass::TypeChanged {
            if let Some(tc) = fd.type_change.as_ref().map(wire) {
                let (source, disk) = (
                    serde_json::to_value(&tc.source).unwrap_or_default(),
                    serde_json::to_value(&tc.disk).unwrap_or_default(),
                );
                if !self.type_change_recorded(&target, &source, &disk) {
                    self.journal.record_event(NewEvent {
                        target: Some(&target),
                        ts: now_ts,
                        kind: EventKind::TypeChanged,
                        from_hash: None,
                        to_hash: None,
                        meta: Some(serde_json::json!({
                            "class": cs,
                            "label": tc.label(),
                            "source": source,
                            "disk": disk,
                        })),
                    })?;
                }
            }
        } else {
            // destination side
            let dest_hex = fd.probe.destination.map(|h| h.to_hex());
            let last = self.journal.last_event_hash(&target)?;
            if dest_hex != last
                && !self.already_recorded(&target, "dest_changed", dest_hex.as_deref())
            {
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
            ) && let Ok(Some(rendered)) = &fd.probe.rendered
            {
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
            .filter(|(c, _, _)| *c == cs)
            .map(|(_, s, _)| *s)
            .unwrap_or(now_ts);
        let type_change = fd.type_change.as_ref().map(wire);
        self.set_drift(target.clone(), cs.to_string(), since, type_change.clone());
        self.emit(Event::Drift {
            target,
            class: cs.to_string(),
            ts: now_ts,
            type_change,
        });
        Ok(())
    }

    fn probe_and_journal(&mut self, target: &Path, now_ts: u64) -> Result<(), DaemonError> {
        match self.scanner.probe_one(target)? {
            Some(fd) => self.journal_probe(&fd, now_ts)?,
            None => {
                self.clear_drift(target);
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
        self.clear_drift(target);
        Ok(())
    }

    pub fn handle_paths_changed(
        &mut self,
        paths: &[PathBuf],
        now_ts: u64,
    ) -> Result<(), DaemonError> {
        if self.paused {
            return Ok(());
        }
        let mut source_changed = false;
        let mut git_refs_changed = false;
        let mut targets: Vec<PathBuf> = Vec::new();

        for p in paths {
            match classify_path(&self.source_dir, &self.managed, p) {
                PathChange::SourceContent => source_changed = true,
                PathChange::SourceGitRefs => git_refs_changed = true,
                PathChange::Target => targets.push(p.clone()),
                PathChange::Ignored => {}
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
        rels.extend(
            self.git
                .changed_files(&self.remote_ref, "HEAD")
                .unwrap_or_default(),
        );
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
        let departed_paths: Vec<PathBuf> = self.managed.difference(&fresh).cloned().collect();
        for departed in &departed_paths {
            self.journal.mark_unmanaged(departed, now_ts)?;
            self.journal.record_event(NewEvent {
                target: Some(departed),
                ts: now_ts,
                kind: EventKind::LeftManagement,
                from_hash: None,
                to_hash: None,
                meta: None,
            })?;
            self.clear_drift(departed);
            delta.removed.push(departed.clone());
        }
        for arrived in fresh.difference(&self.managed) {
            delta.added.push(arrived.clone());
        }
        for ev in delta
            .removed
            .iter()
            .map(|t| Event::LeftManagement {
                target: t.clone(),
                ts: now_ts,
            })
            .collect::<Vec<_>>()
        {
            self.emit(ev);
        }
        self.managed = fresh;
        Ok(delta)
    }

    /// Phase 1 of a scan (spec 2026-10-11 §2.2), under the lock for
    /// microseconds: a scanner the caller runs WITHOUT the lock, and the
    /// ticket `apply_scan` wants back. `None` while another scan is in
    /// progress — the request is remembered as `rescan_wanted`, never lost.
    /// A scan in progress for more than `SCAN_STALE_SECS` is presumed dead
    /// and taken over.
    pub fn begin_scan(&mut self, now_ts: u64) -> Option<(DriftScanner, ScanTicket)> {
        if self.scan_in_progress {
            let age = now_ts.saturating_sub(self.scan_started_ts);
            if age < SCAN_STALE_SECS {
                self.rescan_wanted = true;
                return None;
            }
            tomte_core::log_warn!(
                "scan",
                "a scan has been in progress for {age}s — taking over"
            );
        }
        Some(self.start_scan(now_ts))
    }

    fn start_scan(&mut self, now_ts: u64) -> (DriftScanner, ScanTicket) {
        self.scan_in_progress = true;
        self.scan_started_ts = now_ts;
        self.scan_generation += 1;
        self.touched_during_scan.clear();
        (
            self.scanner.clone(),
            ScanTicket {
                generation: self.scan_generation,
                started_ts: now_ts,
            },
        )
    }

    pub fn scan_in_progress(&self) -> bool {
        self.scan_in_progress
    }

    /// Phase 3: land a report. Always releases `scan_in_progress` for the
    /// scan the ticket belongs to, `Err` included. A target the per-event
    /// path touched meanwhile keeps its state (see `set_drift`); a target
    /// that left management meanwhile is dropped, not journaled. A rescan
    /// asked for meanwhile is handed back only on success: every `Err` path
    /// leaves the flag set, so the next successful apply carries it.
    pub fn apply_scan(
        &mut self,
        ticket: ScanTicket,
        report: Result<ScanReport, ScanError>,
        now_ts: u64,
    ) -> Result<ScanOutcome, DaemonError> {
        if ticket.generation != self.scan_generation {
            tomte_core::log_warn!(
                "scan",
                "ignoring the report of a scan that was taken over (started t={})",
                ticket.started_ts
            );
            return Ok(ScanOutcome {
                drifted: self.drift_state.len() as u32,
                rescan_wanted: false,
            });
        }
        let touched = std::mem::take(&mut self.touched_during_scan);
        self.scan_in_progress = false;
        let report = report?;
        self.reconcile_managed(now_ts)?;
        self.degraded = report.degraded.as_ref().map(|f| f.hint.clone());
        // The report's number as is: off by the touched count until the next
        // scan, which is the honest option — recounting here would mean
        // guessing which touched files the scan had tallied as in sync.
        self.in_sync_count = report.in_sync_count as u64;
        self.origin_differs = report.origin_differs;
        // Targets the scan skipped. Never silent: one of these used to abort
        // the whole scan, and the status then sat at "0 in sync" forever
        // (2026-08-27). They are excluded from `in_sync_count`, so the tally
        // never claims an unchecked file is fine.
        if !report.unprobed.is_empty() {
            tomte_core::log_warn!(
                "scan",
                "{} target(s) could not be checked",
                report.unprobed.len()
            );
            for u in &report.unprobed {
                tomte_core::log_warn!("scan", "  {}: {}", u.target.display(), u.reason);
            }
        }
        let fresh_targets: BTreeSet<PathBuf> =
            report.drifted.iter().map(|d| d.target.clone()).collect();
        let stale: Vec<PathBuf> = self
            .drift_state
            .keys()
            .filter(|t| !fresh_targets.contains(*t) && !touched.contains(*t))
            .cloned()
            .collect();
        for t in &stale {
            self.clear_drift(t);
        }
        let mut skipped = 0usize;
        for fd in &report.drifted {
            if touched.contains(&fd.target) || !self.managed.contains(&fd.target) {
                skipped += 1;
                continue;
            }
            self.journal_probe(fd, now_ts)?;
        }
        let drifted = self.drift_state.len() as u32;
        tomte_core::log_info!(
            "scan",
            "{} managed, {} drifted ({} report entries skipped: changed or unmanaged meanwhile), {} unprobed, {}s",
            self.managed.len(),
            drifted,
            skipped,
            report.unprobed.len(),
            now_ts.saturating_sub(ticket.started_ts)
        );
        // Journal the scan only when its outcome CHANGED — routine no-change
        // scans (hourly safety net, app-requested) were flooding the
        // timeline with thousands of identical "scan" rows.
        if self.last_scan_drifted != Some(drifted) {
            self.journal.record_event(NewEvent {
                target: None,
                ts: now_ts,
                kind: EventKind::Fetch,
                from_hash: None,
                to_hash: None,
                meta: Some(serde_json::json!({"scan": true, "drifted": drifted})),
            })?;
        }
        self.last_scan_drifted = Some(drifted);
        self.emit(Event::ScanDone {
            ts: now_ts,
            drifted,
        });
        let rescan_wanted = std::mem::take(&mut self.rescan_wanted);
        Ok(ScanOutcome {
            drifted,
            rescan_wanted,
        })
    }

    /// The one-call shape, lock held throughout: `tomted --once` and tests
    /// that hold the core anyway. The daemon binary never calls this — it
    /// goes through `crate::scan::run_scan`, which releases the lock for
    /// phase 2. Takes over any scan in progress: the caller holds the core,
    /// so nothing else can be applying.
    pub fn full_rescan(&mut self, now_ts: u64) -> Result<u32, DaemonError> {
        let (scanner, ticket) = self.start_scan(now_ts);
        let report = scanner.scan();
        Ok(self.apply_scan(ticket, report, now_ts)?.drifted)
    }

    /// May the UNATTENDED auto-join create a commit right now? `Err` carries
    /// the reason it may not, for the log.
    ///
    /// `git merge` commits, and committing runs the user's configured signer.
    /// With 1Password SSH signing a locked vault leaves that signer sitting
    /// on a GUI prompt — and this path runs on the fetch timer while HOLDING
    /// THE DAEMON CORE LOCK, so the wait is not one user's merge going slow:
    /// every non-Status IPC bounces as "daemon busy" for its duration, and
    /// the join then fails anyway. Same class of outage as the 2026-08-02
    /// sleep/wake incident that [`GitClient::fetch`]'s env guard exists to
    /// prevent, reached through a different command.
    ///
    /// So ask the REAL signer first, bounded, with an operation that writes
    /// only a dangling object and never touches HEAD, the index or the
    /// working tree ([`GitClient::signer_probe`]) — nothing here may leave
    /// the repo in a state the user did not ask for. A repo that does not
    /// sign its commits is never asked: there is nothing that could block.
    ///
    /// This gate is for the daemon's unattended join ONLY. User-initiated
    /// merges go through the app and SHOULD wait on the agent — the user is
    /// right there and can unlock it.
    fn signer_ready(&self) -> Result<(), String> {
        if !self.git.commit_signing_configured() {
            return Ok(());
        }
        self.git
            .signer_probe(SIGNER_PROBE)
            .map_err(|e| format!("commit signer did not sign within {SIGNER_PROBE:?}: {e}"))
    }

    /// Back out a merge NOBODY ASKED FOR (an auto-join that went wrong), and
    /// report what the REPO says afterwards — never what the abort's exit
    /// status said on its own.
    ///
    /// The two are different facts, and they come apart in both directions.
    /// `git merge --abort` can exit non-zero having already cleared
    /// `MERGE_HEAD` (it unwound the merge, then tripped on a checkout), and
    /// it can exit ZERO with the merge still sitting there. So the
    /// `merge_in_progress` probe decides which sentence is written and the
    /// abort's own error only ever supplies the REASON. Reporting either one
    /// without the other is how a log ends up asserting a repo state that
    /// is not the case.
    ///
    /// Nothing here has a caller to propagate to — this runs inside the
    /// fetch loop — so this log line IS the whole report, which is exactly
    /// why it has to be true. The app's Finish/Cancel banner is the user's
    /// way out of whatever is left.
    fn abort_unwanted_merge(&self, what: &str) {
        let aborted = self.git.merge_abort();
        if self.git.merge_in_progress() {
            tomte_core::log_warn!(
                "fetch",
                "…and {what} could not be aborted ({}); the app's Finish/Cancel banner now \
                 owns it",
                match &aborted {
                    Err(why) => why.to_string(),
                    Ok(()) => "git exited 0 but MERGE_HEAD is still there".into(),
                }
            );
        } else if let Err(why) = &aborted {
            tomte_core::log_info!(
                "fetch",
                "…and {what} was aborted despite git exiting non-zero ({why}); the repo is \
                 back where it started"
            );
        } else {
            tomte_core::log_info!(
                "fetch",
                "…and {what} was aborted; the repo is back where it started"
            );
        }
    }

    pub fn handle_fetch(&mut self, now_ts: u64) -> Result<(), DaemonError> {
        match self.git.fetch("origin") {
            Ok(()) => {}
            Err(e) => {
                // Raw git output makes users think the app is broken; the
                // two field failure modes get named (2026-08-17): a locked
                // 1Password holding the SSH key (timeout, empty stderr) and
                // plain network loss.
                let raw = e.to_string();
                let e = if raw.contains("timed out") && raw.contains("stderr tail: \"\"") {
                    format!(
                        "{raw} — usually 1Password locked (the SSH key lives in its agent); retries on schedule"
                    )
                } else if raw.contains("Could not resolve host")
                    || raw.contains("Couldn't connect")
                    || raw.contains("unable to access")
                {
                    format!("{raw} — network unreachable; retries on schedule")
                } else {
                    raw
                };
                // A FAILED fetch is journaled but emits no FetchDone — the
                // app's freshness tile must never claim "fetched Xs ago" for
                // a fetch that died (2026-08-08).
                self.journal.record_event(NewEvent {
                    target: None,
                    ts: now_ts,
                    kind: EventKind::Fetch,
                    from_hash: None,
                    to_hash: None,
                    meta: Some(serde_json::json!({"error": e.to_string()})),
                })?;
                self.emit(Event::FetchFailed {
                    ts: now_ts,
                    error: e.to_string(),
                });
                return Ok(());
            }
        }
        self.last_fetch_ts = Some(now_ts);
        // "How far apart are we" gates every integration below, so a failure
        // to ANSWER it must not read as "level" in the logs or the journal.
        // The numbers still degrade to 0 — that is the direction that leaves
        // the repo alone — but nothing downstream gets to call it a measured
        // zero.
        let div = match self.git.divergence(&self.remote_ref) {
            Ok(d) => Some(d),
            Err(e) => {
                tomte_core::log_warn!(
                    "fetch",
                    "could not measure divergence from {} ({e}) — no integration this round",
                    self.remote_ref
                );
                None
            }
        };
        let behind = div.map(|d| d.behind).unwrap_or(0);
        let ahead = div.map(|d| d.ahead).unwrap_or(0);
        self.journal.record_event(NewEvent {
            target: None,
            ts: now_ts,
            kind: EventKind::Fetch,
            from_hash: None,
            to_hash: None,
            meta: Some(match div {
                Some(d) => serde_json::json!({"behind": d.behind}),
                None => serde_json::json!({"behind_unknown": true}),
            }),
        })?;
        // The paths fetch brought in — computed BEFORE integration so the
        // probe below still knows what moved once HEAD catches up. An empty
        // list here silently skips the whole per-target loop, so a failure
        // says so rather than passing for "fetch brought nothing".
        let changed = match self.git.changed_files("HEAD", &self.remote_ref) {
            Ok(v) => v,
            Err(e) => {
                tomte_core::log_warn!(
                    "fetch",
                    "could not list what {} brought in ({e}) — no targets re-probed this round",
                    self.remote_ref
                );
                Vec::new()
            }
        };
        // Integrate, don't just observe: with no local commits and a clean
        // tree, fast-forwarding is lossless — and NOT doing it left every
        // later push non-fast-forward ("I literally clicked fetch",
        // 2026-08-10). A dirty tree is left untouched either way — Tomte
        // never integrates over uncommitted local changes.
        let clean_tree = self
            .git
            .dirty_files()
            .map(|d| d.is_empty())
            .unwrap_or(false);
        if behind > 0 && clean_tree {
            if ahead == 0 {
                // Lossless: nothing local to preserve.
                match self.git.ff_merge(&self.remote_ref) {
                    Ok(()) => {
                        tomte_core::log_info!(
                            "fetch",
                            "fast-forwarded {behind} commit(s) to {}",
                            self.remote_ref
                        );
                    }
                    Err(e) => {
                        tomte_core::log_warn!("fetch", "fast-forward skipped: {e}");
                    }
                }
            } else {
                // Diverged. Join ONLY if a virtual merge proves it clean —
                // Tomte never leaves the repo mid-merge on its own initiative.
                match self.git.merge_tree_conflicts(&self.remote_ref) {
                    Ok(MergeTreeOutcome::Clean) => {
                        if let Err(reason) = self.signer_ready() {
                            // Nothing is wrong with the repo — the signer is
                            // simply not available right now (a locked
                            // 1Password is the everyday case). The join is a
                            // convenience; the next fetch will offer it again.
                            tomte_core::log_info!(
                                "fetch",
                                "clean join with {} available but not taken: {reason}; \
                                 retrying next fetch",
                                self.remote_ref
                            );
                        } else {
                            // `merge` returns `MergeStart`. A `Conflicts` here
                            // would mean the repo is NOW mid-merge — that must
                            // never be logged as a success, and must never be
                            // left for the user to discover.
                            match self.git.merge_within(&self.remote_ref, AUTO_JOIN_MERGE) {
                                Ok(MergeStart::Clean) => tomte_core::log_info!(
                                    "fetch",
                                    "joined {behind} commit(s) from {}",
                                    self.remote_ref
                                ),
                                Ok(MergeStart::Conflicts(paths)) => {
                                    tomte_core::log_warn!(
                                        "fetch",
                                        "merge-tree said clean but {} path(s) conflicted; aborting",
                                        paths.len()
                                    );
                                    // `Conflicts` means the repo IS mid-merge,
                                    // so this needs no guard of its own.
                                    self.abort_unwanted_merge("that merge");
                                }
                                // A non-conflict failure — the signer passed
                                // the pre-flight and then died on the real
                                // commit, or the 10s ceiling hit. `git merge`
                                // leaves MERGE_HEAD set with everything
                                // STAGED in that case (verified against git
                                // 2.50.1): the stranded-merge shape, and
                                // nobody asked for it. No user decision is in
                                // it, so back it out.
                                Err(e) => {
                                    tomte_core::log_warn!("fetch", "join failed: {e}");
                                    if self.git.merge_in_progress() {
                                        self.abort_unwanted_merge("the merge it left open");
                                    }
                                }
                            }
                        }
                    }
                    // NOTE the empty case: `Conflicts(vec![])` means
                    // conflicted with no individually-listed file (a
                    // directory rename conflict, say). It must NOT be
                    // treated as clean — never match on `.is_empty()`.
                    Ok(MergeTreeOutcome::Conflicts(paths)) => tomte_core::log_info!(
                        "fetch",
                        "{} conflict(s) with {} — left for review",
                        paths.len(),
                        self.remote_ref
                    ),
                    Ok(MergeTreeOutcome::Unknown) => tomte_core::log_warn!(
                        "fetch",
                        "merge-tree gave no answer; not auto-joining"
                    ),
                    Err(e) => tomte_core::log_warn!("fetch", "merge probe failed: {e}"),
                }
            }
        }
        // origin_differs must reflect state AFTER any integration above: a
        // clean auto-join folds origin's commits into HEAD, so those paths
        // no longer differ from origin. Reusing the pre-join `changed` set
        // here would mark files as differing at exactly the moment
        // auto-join made them agree — so this is a second, independent
        // `git diff`, taken post-integration, on every fetch (~every 15
        // min: cheap, and the only way to get this right regardless of
        // which integration path (ff-merge, merge, conflict, none) ran
        // above). A failure here must not fail the fetch or guess at the
        // answer — it just leaves origin_differs at its last-known value
        // for the targets this fetch touched, logged so it's never silent.
        let origin_differs_now = match self.git.changed_files("HEAD", &self.remote_ref) {
            Ok(v) => Some(v.into_iter().collect::<BTreeSet<PathBuf>>()),
            Err(e) => {
                tomte_core::log_warn!(
                    "fetch",
                    "could not refresh origin_differs after integration: {e}"
                );
                None
            }
        };
        for rel in changed {
            let abs = self.source_dir.join(&rel);
            let Ok(targets) = self.chezmoi.target_paths(std::slice::from_ref(&abs)) else {
                continue;
            };
            for t in targets {
                if self.managed.contains(&t) {
                    self.probe_and_journal(&t, now_ts)?;
                    if let Some(now) = &origin_differs_now {
                        if now.contains(&rel) {
                            self.origin_differs.insert(t);
                        } else {
                            self.origin_differs.remove(&t);
                        }
                    }
                }
            }
        }
        self.emit(Event::FetchDone { ts: now_ts, behind });
        Ok(())
    }

    pub fn snapshot_blobs(
        &mut self,
        paths: &[PathBuf],
        now_ts: u64,
    ) -> Result<Vec<String>, DaemonError> {
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
            .map(|(t, (class, since, type_change))| DriftSummary {
                target: t.clone(),
                class: class.clone(),
                since_ts: Some(*since),
                origin_differs: self.origin_differs.contains(t),
                type_change: type_change.clone(),
            })
            .collect();
        (drifted, self.in_sync_count, self.degraded.clone())
    }

    pub fn last_fetch_ts(&self) -> Option<u64> {
        self.last_fetch_ts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    /// MUTATION: drop the "first component" distinction (treat ANY `.git`
    /// component with the HEAD/refs rule) → the nested `HEAD` case reads
    /// `SourceGitRefs`. MUTATION: drop the any-component check (only the
    /// first component is ever looked at, the 0.1.20 shape) → the nested
    /// `index.lock` case reads `SourceContent`, which is the self-sustaining
    /// rescan loop of 2026-10-11.
    #[test]
    fn classify_path_ignores_nested_repositories_and_keeps_the_top_level_refs_rule() {
        let src = Path::new("/src");
        let m = managed(&["/home/.zshrc"]);
        let at = |rel: &str| classify_path(src, &m, &src.join(rel));
        assert_eq!(
            at("dot_claude/skills/humanizer/.git/index.lock"),
            PathChange::Ignored
        );
        assert_eq!(
            at("dot_claude/skills/humanizer/.git/HEAD"),
            PathChange::Ignored
        );
        assert_eq!(
            at("dot_claude/skills/humanizer/.git/refs/heads/main"),
            PathChange::Ignored
        );
        assert_eq!(at(".git/index.lock"), PathChange::Ignored);
        assert_eq!(at(".git/objects/ab/cdef"), PathChange::Ignored);
        assert_eq!(at(".git/HEAD"), PathChange::SourceGitRefs);
        assert_eq!(at(".git/ORIG_HEAD"), PathChange::SourceGitRefs);
        assert_eq!(
            at(".git/refs/remotes/origin/main"),
            PathChange::SourceGitRefs
        );
        assert_eq!(at("dot_zshrc"), PathChange::SourceContent);
        assert_eq!(
            at("dot_claude/skills/humanizer/SKILL.md"),
            PathChange::SourceContent
        );
        assert_eq!(
            classify_path(src, &m, Path::new("/home/.zshrc")),
            PathChange::Target
        );
        assert_eq!(
            classify_path(src, &m, Path::new("/tmp/x")),
            PathChange::Ignored
        );
    }
}
