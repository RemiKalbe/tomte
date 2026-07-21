//! DaemonCore: single-threaded drift state machine (spec §3.1).
//! No clocks, no I/O loops — callers supply `now_ts` and drive it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

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
    /// Shared with the socket server so Subscribe never needs the core
    /// lock (the initial scan can hold it for minutes on secret-manager
    /// timeouts — head-of-line blocking the handshake was a real bug).
    subscribers: std::sync::Arc<Mutex<Vec<Sender<Event>>>>,
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
            subscribers,
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
            .filter(|(c, _)| *c == cs)
            .map(|(_, s)| *s)
            .unwrap_or(now_ts);
        self.drift_state
            .insert(target.clone(), (cs.to_string(), since));
        self.emit(Event::Drift {
            target,
            class: cs.to_string(),
            ts: now_ts,
        });
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
            if p.starts_with(&self.source_dir) {
                let rel = p.strip_prefix(&self.source_dir).unwrap_or(p);
                let mut comps = rel.components();
                if comps
                    .next()
                    .map(|c| c.as_os_str() == ".git")
                    .unwrap_or(false)
                {
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
        self.emit(Event::ScanDone {
            ts: now_ts,
            drifted,
        });
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
                self.emit(Event::FetchDone {
                    ts: now_ts,
                    behind: 0,
                });
                return Ok(());
            }
        }
        let behind = self
            .git
            .divergence(&self.remote_ref)
            .map(|d| d.behind)
            .unwrap_or(0);
        self.journal.record_event(NewEvent {
            target: None,
            ts: now_ts,
            kind: EventKind::Fetch,
            from_hash: None,
            to_hash: None,
            meta: Some(serde_json::json!({"behind": behind})),
        })?;
        let changed = self
            .git
            .changed_files("HEAD", &self.remote_ref)
            .unwrap_or_default();
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
            .map(|(t, (class, since))| DriftSummary {
                target: t.clone(),
                class: class.clone(),
                since_ts: Some(*since),
            })
            .collect();
        (drifted, self.in_sync_count, self.degraded.clone())
    }
}
