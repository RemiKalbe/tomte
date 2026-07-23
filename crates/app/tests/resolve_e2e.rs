//! E2E drift stories (spec §5, §6.3): ResolveEngine against a real scratch
//! chezmoi home, real git with a bare origin, and the real daemon served
//! in-process over a unix socket. Each story asserts BOTH filesystem truth
//! and journal truth.
//!
//! The daemon journals to a FILE here (not in-memory): `undo_last` opens the
//! same journal read-only at `engine.journal_path`.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use czui_app::ipc::IpcClient;
use czui_app::resolve::{ResolveEngine, ResolveOutcome};
use czui_core::cmd::SystemRunner;
use czui_core::drift::{ContentHash, DriftClass};
use czui_core::git::GitClient;
use czui_core::scanner::FileDrift;
use czui_core::testsupport::{Scratch, git, sh};
use czui_daemon::core::DaemonCore;
use czui_daemon::server::{ServeCtx, serve};
use czui_journal::Journal;
use czui_proto::{DriftSummary, Event, Request, Response};

/// Content written to the destination to fake a foreign edit.
const DRIFTED: &[u8] = b"a=local\n";
/// The scratch source's rendered content (`dot_testrc` = "a=1\n").
const RENDERED: &[u8] = b"a=1\n";

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Scratch home + bare origin + file-backed journal + in-process daemon
/// server + connected ResolveEngine: the full resolution stack, hermetically.
struct DriftLab {
    s: Scratch,
    engine: ResolveEngine,
    events: Receiver<Event>,
    journal_path: PathBuf,
    target: PathBuf,
}

impl DriftLab {
    fn new() -> Self {
        let s = Scratch::new();
        // The engine's fallback commit runs `git commit` WITHOUT testsupport's
        // per-invocation `-c` overrides, so the repo itself must carry an
        // identity and disable signing (the machine's global config may have
        // 1Password SSH signing enabled). Scratch::new does not persist these.
        git(&s.source, &["config", "user.email", "t@t"]);
        git(&s.source, &["config", "user.name", "t"]);
        git(&s.source, &["config", "commit.gpgsign", "false"]);

        let journal_path = s.root.path().join("journal.db");
        let journal = Journal::open(&journal_path, "e2e").unwrap();
        let core = Arc::new(Mutex::new(
            DaemonCore::new(
                s.chezmoi(),
                GitClient::new(Arc::new(SystemRunner), s.source.clone()),
                journal,
                "origin/main".into(),
            )
            .unwrap(),
        ));
        let sock = s.root.path().join("d.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || serve(listener, ServeCtx::ready(core, now_ts, Arc::new(|| {}))));

        let ipc = Arc::new(IpcClient::connect(&sock).unwrap());
        let events = ipc.subscribe().unwrap();
        let engine = ResolveEngine {
            // The SCRATCH client — mutations must hit the scratch home.
            chezmoi: s.chezmoi(),
            git: GitClient::new(Arc::new(SystemRunner), s.source.clone()),
            ipc,
            journal_path: journal_path.clone(),
        };
        let target = s.home.join(".testrc");
        Self {
            s,
            engine,
            events,
            journal_path,
            target,
        }
    }

    fn source_git(&self) -> GitClient {
        GitClient::new(Arc::new(SystemRunner), self.s.source.clone())
    }

    fn bare_git(&self) -> GitClient {
        GitClient::new(Arc::new(SystemRunner), self.s.bare.clone())
    }

    /// Fresh read-only handle onto the daemon's journal (single writer = daemon).
    fn journal(&self) -> Journal {
        Journal::open_read_only(&self.journal_path, "assert").unwrap()
    }

    /// Direct scanner probe of the managed target: point-in-time filesystem
    /// truth, independent of the daemon's (possibly still rescanning) status.
    fn probe(&self) -> Option<FileDrift> {
        self.s.scanner().probe_one(&self.target).unwrap()
    }

    fn request_rescan(&self) {
        match self.engine.ipc.request(Request::Rescan).unwrap() {
            Response::Ok => {}
            other => panic!("unexpected rescan reply: {other:?}"),
        }
    }

    /// Rescan is async-acked: wait (bounded) for the daemon's ScanDone push
    /// before asserting on its status.
    fn wait_scan_done(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for ScanDone");
            match self.events.recv_timeout(left) {
                Ok(Event::ScanDone { .. }) => return,
                Ok(_) => {}
                Err(e) => panic!("event stream died waiting for ScanDone: {e}"),
            }
        }
    }

    /// Daemon status once no scan holds the core (bounded polling: the
    /// ScanDone push slightly precedes the rescan thread releasing the lock).
    fn settled_status(&self) -> Vec<DriftSummary> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self.engine.ipc.request(Request::Status).unwrap() {
                Response::Status {
                    drifted,
                    scanning: false,
                    ..
                } => return drifted,
                Response::Status { .. } => {}
                other => panic!("unexpected status reply: {other:?}"),
            }
            assert!(
                Instant::now() < deadline,
                "daemon never settled after rescan"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Drift the destination and make the daemon SEE it, so the post-action
    /// in-sync assertions prove a real transition (also leaves the core lock
    /// free, so the next engine action cannot race a scan).
    fn drift_dest(&self) {
        std::fs::write(&self.target, DRIFTED).unwrap();
        assert_eq!(
            self.probe()
                .expect("seeded drift must probe as drift")
                .class,
            DriftClass::DestinationDrift
        );
        self.request_rescan();
        self.wait_scan_done();
        let drifted = self.settled_status();
        assert_eq!(drifted.len(), 1, "daemon must report the seeded drift");
        assert_eq!(drifted[0].target, self.target);
    }
}

#[test]
fn story_a_keep_disk_readds_commits_pushes_and_settles_in_sync() {
    let lab = DriftLab::new();
    lab.drift_dest();
    let head_before = lab.source_git().head_sha().unwrap();

    let outcome = lab.engine.keep_disk(&lab.target).unwrap();
    let ResolveOutcome::Done {
        session,
        committed,
        pushed,
        note,
    } = outcome
    else {
        panic!("expected Done, got {outcome:?}");
    };
    assert!(
        committed,
        "scratch has no autoCommit — the engine's fallback commit must fire"
    );
    assert!(pushed, "origin exists in the scratch — push must succeed");
    assert_eq!(note, None);

    // Filesystem truth: the source now carries the disk content.
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        DRIFTED
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), DRIFTED);

    // Git truth: exactly one new commit, received by the bare origin.
    let head_after = lab.source_git().head_sha().unwrap();
    assert_ne!(head_after, head_before, "fallback commit must move HEAD");
    assert_eq!(lab.source_git().rev_parse("HEAD~1").unwrap(), head_before);
    assert_eq!(
        lab.bare_git().rev_parse("HEAD").unwrap(),
        head_after,
        "push must advance the bare origin's HEAD"
    );

    // Journal truth: a finished session holding the keep_disk decision with
    // both snapshot blobs.
    let journal = lab.journal();
    let (id, decisions) = journal.last_finished_session().unwrap().unwrap();
    assert_eq!(id, session);
    let arr = decisions.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["action"], "keep_disk");
    assert_eq!(
        arr[0]["target"].as_str().unwrap(),
        lab.target.to_str().unwrap()
    );
    let dest_blob = arr[0]["dest_blob"].as_str().unwrap();
    let source_blob = arr[0]["source_blob"].as_str().unwrap();
    assert_eq!(dest_blob, ContentHash::of(DRIFTED).to_hex());
    assert_eq!(journal.get_blob(dest_blob).unwrap().unwrap(), DRIFTED);
    assert_eq!(
        journal.get_blob(source_blob).unwrap().unwrap(),
        RENDERED,
        "source snapshot must hold the pre-mutation source content"
    );

    // Daemon truth: the requested rescan settles to in-sync.
    assert!(
        lab.probe().is_none(),
        "probe must report in-sync after keep_disk"
    );
    lab.wait_scan_done();
    assert!(
        lab.settled_status().is_empty(),
        "daemon status must be drift-free after keep_disk"
    );
}

#[test]
fn story_b_keep_source_restores_rendered_content() {
    let lab = DriftLab::new();
    lab.drift_dest();

    let outcome = lab.engine.keep_source(&lab.target).unwrap();
    let ResolveOutcome::Done {
        session,
        committed,
        pushed,
        note,
    } = outcome
    else {
        panic!("expected Done, got {outcome:?}");
    };
    assert!(!committed && !pushed, "apply never touches the source repo");
    assert_eq!(note, None);

    // Filesystem truth: the disk is restored to the rendered content and the
    // source repo is untouched.
    assert_eq!(std::fs::read(&lab.target).unwrap(), RENDERED);
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        RENDERED
    );
    assert!(lab.source_git().dirty_files().unwrap().is_empty());

    // Journal truth: session with a keep_source decision snapshotting the
    // destination only.
    let journal = lab.journal();
    let (id, decisions) = journal.last_finished_session().unwrap().unwrap();
    assert_eq!(id, session);
    let arr = decisions.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["action"], "keep_source");
    let dest_blob = arr[0]["dest_blob"].as_str().unwrap();
    assert_eq!(
        journal.get_blob(dest_blob).unwrap().unwrap(),
        DRIFTED,
        "snapshot must hold the drifted content undo would restore"
    );
    assert!(
        arr[0].get("source_blob").is_none(),
        "keep_source snapshots the destination only"
    );

    // Daemon truth: in-sync after the rescan.
    assert!(
        lab.probe().is_none(),
        "probe must report in-sync after keep_source"
    );
    lab.wait_scan_done();
    assert!(lab.settled_status().is_empty());
}

#[test]
fn story_c_sync_all_pulls_remote_changes_into_dest() {
    const REMOTE: &[u8] = b"a=remote\n";
    let lab = DriftLab::new();

    // A second machine pushes to origin…
    let other = lab.s.root.path().join("other");
    sh(
        lab.s.root.path(),
        "git",
        &[
            "clone",
            lab.s.bare.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    std::fs::write(other.join("dot_testrc"), REMOTE).unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "remote change"]);
    git(&other, &["push"]);
    // …and the local fetch reveals it as remote-ahead drift.
    lab.source_git().fetch("origin").unwrap();
    assert_eq!(
        lab.probe()
            .expect("fetched change must probe as drift")
            .class,
        DriftClass::RemoteAhead
    );
    lab.request_rescan();
    lab.wait_scan_done();
    let drifted = lab.settled_status();
    assert_eq!(drifted.len(), 1);
    assert_eq!(drifted[0].class, "remote_ahead");

    let outcome = lab.engine.sync_all().unwrap();
    let ResolveOutcome::Done {
        session,
        committed,
        pushed,
        note,
    } = outcome
    else {
        panic!("expected Done, got {outcome:?}");
    };
    assert!(!committed && !pushed, "update commits nothing of its own");
    assert_eq!(note, None);

    // Filesystem truth: destination and source both carry the remote content.
    assert_eq!(std::fs::read(&lab.target).unwrap(), REMOTE);
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        REMOTE
    );
    // Git truth: update pulled the source level with origin.
    assert_eq!(
        lab.source_git().head_sha().unwrap(),
        lab.bare_git().rev_parse("HEAD").unwrap()
    );

    // Journal truth: session with the sync_all decision.
    let journal = lab.journal();
    let (id, decisions) = journal.last_finished_session().unwrap().unwrap();
    assert_eq!(id, session);
    let arr = decisions.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["action"], "sync_all");

    // Daemon truth: in-sync after the rescan.
    assert!(
        lab.probe().is_none(),
        "probe must report in-sync after sync_all"
    );
    lab.wait_scan_done();
    assert!(lab.settled_status().is_empty());
}

#[test]
fn story_d_undo_restores_drifted_content_and_reports_drift_again() {
    let lab = DriftLab::new();
    lab.drift_dest();

    let outcome = lab.engine.keep_source(&lab.target).unwrap();
    let ResolveOutcome::Done { session, .. } = outcome else {
        panic!("expected Done, got {outcome:?}");
    };
    assert_eq!(std::fs::read(&lab.target).unwrap(), RENDERED);
    // Let the keep_source rescan finish (and release the core) before undoing:
    // the undo's IPC calls must not race a scan holding the daemon core.
    lab.wait_scan_done();
    assert!(lab.settled_status().is_empty(), "in-sync before the undo");

    let undone = lab.engine.undo_last().unwrap();
    assert_eq!(
        undone,
        Some(session),
        "undo must target the keep_source session"
    );

    // Filesystem truth: the disk is back to the DRIFTED version.
    assert_eq!(std::fs::read(&lab.target).unwrap(), DRIFTED);

    // Journal truth: a NEW finished session records the undo.
    let journal = lab.journal();
    let (undo_id, decisions) = journal.last_finished_session().unwrap().unwrap();
    assert_ne!(undo_id, session);
    let arr = decisions.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["action"], "undo");
    assert_eq!(arr[0]["of"].as_i64().unwrap(), session);

    // Daemon truth: the drift is visible again after the undo's rescan.
    assert_eq!(
        lab.probe().expect("undo must re-introduce the drift").class,
        DriftClass::DestinationDrift
    );
    lab.wait_scan_done();
    let drifted = lab.settled_status();
    assert_eq!(drifted.len(), 1, "daemon must report the restored drift");
    assert_eq!(drifted[0].target, lab.target);
    assert_eq!(drifted[0].class, "destination_drift");
}

#[test]
fn story_e_templated_source_needs_merge_editor_and_touches_nothing() {
    const TEMPLATE: &[u8] = b"a={{ \"tpl\" }}\n";
    let lab = DriftLab::new();

    // Convert the managed file into a template and level the state with it.
    git(&lab.s.source, &["mv", "dot_testrc", "dot_testrc.tmpl"]);
    std::fs::write(lab.s.source.join("dot_testrc.tmpl"), TEMPLATE).unwrap();
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "templatize"]);
    lab.s.chezmoi().apply(None).unwrap();
    assert_eq!(std::fs::read(&lab.target).unwrap(), b"a=tpl\n");

    // Drift the destination…
    std::fs::write(&lab.target, DRIFTED).unwrap();

    // …one-click keep-disk must refuse: chezmoi re-add silently ignores
    // templates, and pretending success would be a lie.
    let outcome = lab.engine.keep_disk(&lab.target).unwrap();
    assert_eq!(outcome, ResolveOutcome::NeedsMergeEditor);

    // NOTHING changed: template intact, dest still drifted, repo clean.
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc.tmpl")).unwrap(),
        TEMPLATE
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), DRIFTED);
    assert!(lab.source_git().dirty_files().unwrap().is_empty());

    // Journal truth: the refusal happens before SessionStart — no session
    // exists, not even an unfinished one.
    let journal = lab.journal();
    assert!(journal.last_finished_session().unwrap().is_none());
    let timeline = journal.timeline(50, None).unwrap();
    assert!(
        timeline.iter().all(|e| e.kind != "session_start"),
        "keep_disk on a template must not open a session"
    );
}
