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

use tomte_app::ipc::IpcClient;
use tomte_app::merge_inputs;
use tomte_app::merge_state::MergeState;
use tomte_app::resolve::{MergeConclusion, ResolveEngine, ResolveOutcome};
use tomte_core::cmd::SystemRunner;
use tomte_core::drift::{ContentHash, DriftClass};
use tomte_core::git::GitClient;
use tomte_core::merge::Choice;
use tomte_core::scanner::FileDrift;
use tomte_core::testsupport::{Scratch, git, sh};
use tomte_daemon::core::DaemonCore;
use tomte_daemon::server::{ServeCtx, serve};
use tomte_journal::Journal;
use tomte_proto::{DriftSummary, Event, Request, Response};

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
        // `Scratch::new` writes the repo-LOCAL identity and `commit.gpgsign
        // = false` the production `GitClient` needs: it runs plain `git`,
        // without testsupport's per-invocation `-c` overrides, so without
        // that it would inherit the developer's global signing config.
        let s = Scratch::new();
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

    /// Make the daemon observe the current filesystem state and return its
    /// settled drift list (rescan is async-acked: request → ScanDone → poll).
    fn rescan_and_settle(&self) -> Vec<DriftSummary> {
        self.request_rescan();
        self.wait_scan_done();
        self.settled_status()
    }

    /// `merge_inputs::load` with the lab's own stack, ready for `MergeState`.
    fn load_merge_inputs(&self) -> merge_inputs::MergeInputs {
        merge_inputs::load(
            &self.engine.chezmoi,
            &self.engine.git,
            "origin/main",
            &self.journal_path,
            &self.target,
        )
        .unwrap()
    }
}

/// Pick `Choice::Ours` for every conflict and assemble. Works 3-way (real
/// conflicts to decide) and degraded 2-way (no conflicts: assembly is the
/// default choices) alike — base presence is a daemon-history detail the
/// stories must not depend on.
fn resolve_all_ours(state: &mut MergeState) -> String {
    for region in state.conflicts() {
        state.pick(region, Choice::Ours);
    }
    state
        .assembled()
        .expect("fully resolved document must assemble")
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

/// The source edit committed in story F, distinct from both the last-applied
/// content and the destination edit — the classic both-sides Conflict.
const SOURCE_EDITED: &[u8] = b"a=source\n";

#[test]
fn story_f_merged_save_converges_source_and_dest_on_the_chosen_text() {
    let lab = DriftLab::new();

    // The source side changes (committed)…
    std::fs::write(lab.s.source.join("dot_testrc"), SOURCE_EDITED).unwrap();
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "source edit"]);
    // …and the daemon sees the source-ahead state FIRST: journaling that
    // probe snapshots the still-last-written destination ("a=1\n"), which is
    // exactly the blob merge_inputs later resolves as the 3-way base.
    let drifted = lab.rescan_and_settle();
    assert_eq!(drifted.len(), 1);
    assert_eq!(drifted[0].class, "source_ahead");

    // …then the destination changes DIFFERENTLY → both sides moved.
    std::fs::write(&lab.target, DRIFTED).unwrap();
    assert_eq!(
        lab.probe().expect("both-sides drift must probe").class,
        DriftClass::Conflict
    );
    let drifted = lab.rescan_and_settle();
    assert_eq!(drifted.len(), 1);
    assert_eq!(drifted[0].target, lab.target);
    assert_eq!(drifted[0].class, "conflict");

    let head_before = lab.source_git().head_sha().unwrap();

    // Load → model → decide: the conflict is real and "ours" (disk) wins.
    let inputs = lab.load_merge_inputs();
    assert!(!inputs.templated);
    assert!(inputs.span_map.is_none());
    let mut state = MergeState::new(&inputs);
    assert!(
        !state.conflicts().is_empty(),
        "both sides changed the same line — the editor must demand a decision"
    );
    let resolved = resolve_all_ours(&mut state);
    assert_eq!(resolved.as_bytes(), DRIFTED);

    let outcome = lab.engine.resolve_merged(&inputs, &resolved).unwrap();
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
        "the merged source write leaves the repo dirty — fallback commit must fire"
    );
    assert!(pushed, "origin exists in the scratch — push must succeed");
    assert_eq!(note, None);

    // Filesystem truth: source and destination both carry the resolved text.
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        DRIFTED
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), DRIFTED);

    // Git truth: exactly one new commit on top of the source edit, pushed.
    let head_after = lab.source_git().head_sha().unwrap();
    assert_ne!(head_after, head_before, "fallback commit must move HEAD");
    assert_eq!(lab.source_git().rev_parse("HEAD~1").unwrap(), head_before);
    assert_eq!(lab.bare_git().rev_parse("HEAD").unwrap(), head_after);

    // Journal truth: a finished session holding the merge decision with both
    // pre-mutation snapshot blobs.
    let journal = lab.journal();
    let (id, decisions) = journal.last_finished_session().unwrap().unwrap();
    assert_eq!(id, session);
    let arr = decisions.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["action"], "merge");
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
        SOURCE_EDITED,
        "source snapshot must hold the pre-merge source content"
    );

    // Daemon truth: the requested rescan settles to in-sync.
    assert!(
        lab.probe().is_none(),
        "probe must be in-sync after the merge"
    );
    lab.wait_scan_done();
    assert!(lab.settled_status().is_empty());
}

/// Stories G/H: the managed file as a template — one LITERAL line plus one
/// line rendered from config `[data]` (testvalue = "tpl").
const TPL_SOURCE: &[u8] = b"a=1\nvalue={{ .testvalue }}\n";
const TPL_RENDERED: &[u8] = b"a=1\nvalue=tpl\n";
/// Story G: the destination edited on the LITERAL line only.
const TPL_LITERAL_DRIFT: &[u8] = b"a=local\nvalue=tpl\n";
/// Story H: the destination edited on the RENDERED VALUE of {{ .testvalue }}.
const TPL_VALUE_DRIFT: &[u8] = b"a=1\nvalue=changed\n";

/// Convert the managed file into a `[data]`-driven template, level all
/// states with it, then drift the destination to `drift` and make the daemon
/// see it.
fn templatize_and_drift(lab: &DriftLab, drift: &[u8]) {
    // The scratch config exists (empty) since Scratch::new — give the
    // template its variable, template_roundtrip's scratch_chezmoi pattern.
    std::fs::write(lab.s.config_path(), "[data]\ntestvalue = \"tpl\"\n").unwrap();
    git(&lab.s.source, &["mv", "dot_testrc", "dot_testrc.tmpl"]);
    std::fs::write(lab.s.source.join("dot_testrc.tmpl"), TPL_SOURCE).unwrap();
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "templatize"]);
    lab.s.chezmoi().apply(None).unwrap();
    assert_eq!(std::fs::read(&lab.target).unwrap(), TPL_RENDERED);

    std::fs::write(&lab.target, drift).unwrap();
    assert_eq!(
        lab.probe().expect("seeded drift must probe as drift").class,
        DriftClass::DestinationDrift
    );
    let drifted = lab.rescan_and_settle();
    assert_eq!(drifted.len(), 1, "daemon must report the seeded drift");
    assert_eq!(drifted[0].target, lab.target);
    assert_eq!(drifted[0].class, "destination_drift");
}

#[test]
fn story_g_templated_literal_edit_writes_back_and_keeps_the_expression() {
    let lab = DriftLab::new();
    templatize_and_drift(&lab, TPL_LITERAL_DRIFT);

    let inputs = lab.load_merge_inputs();
    assert!(inputs.templated);
    assert!(
        inputs.span_map.is_some(),
        "templated load must anchor protected spans"
    );
    assert_eq!(inputs.theirs.as_bytes(), TPL_RENDERED);
    let mut state = MergeState::new(&inputs);
    let resolved = resolve_all_ours(&mut state);
    assert_eq!(resolved.as_bytes(), TPL_LITERAL_DRIFT);

    let outcome = lab.engine.resolve_merged(&inputs, &resolved).unwrap();
    let ResolveOutcome::Done {
        session,
        committed,
        pushed,
        note,
    } = outcome
    else {
        panic!("expected Done, got {outcome:?}");
    };
    assert!(committed, "the template write leaves the repo dirty");
    assert!(pushed);
    assert_eq!(note, None);

    // Filesystem truth: the template gained EXACTLY the literal edit and the
    // {{ .testvalue }} expression survives verbatim; the destination carries
    // the resolved (re-rendered) text.
    let new_template = std::fs::read_to_string(lab.s.source.join("dot_testrc.tmpl")).unwrap();
    assert_eq!(new_template, "a=local\nvalue={{ .testvalue }}\n");
    assert!(
        new_template.contains("{{ .testvalue }}"),
        "template expressions must survive write-back"
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), TPL_LITERAL_DRIFT);

    // Journal truth: the merge decision snapshots the drifted destination and
    // the pre-mutation template.
    let journal = lab.journal();
    let (id, decisions) = journal.last_finished_session().unwrap().unwrap();
    assert_eq!(id, session);
    let arr = decisions.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["action"], "merge");
    let dest_blob = arr[0]["dest_blob"].as_str().unwrap();
    let source_blob = arr[0]["source_blob"].as_str().unwrap();
    assert_eq!(
        journal.get_blob(dest_blob).unwrap().unwrap(),
        TPL_LITERAL_DRIFT
    );
    assert_eq!(
        journal.get_blob(source_blob).unwrap().unwrap(),
        TPL_SOURCE,
        "source snapshot must hold the pre-merge template"
    );

    // Daemon truth: in-sync after the merge's rescan.
    assert!(
        lab.probe().is_none(),
        "probe must be in-sync after the merge"
    );
    lab.wait_scan_done();
    assert!(lab.settled_status().is_empty());
}

#[test]
fn story_h_templated_value_edit_is_rejected_and_touches_nothing() {
    let lab = DriftLab::new();
    templatize_and_drift(&lab, TPL_VALUE_DRIFT);
    let head_before = lab.source_git().head_sha().unwrap();

    let inputs = lab.load_merge_inputs();
    assert!(inputs.templated);
    let mut state = MergeState::new(&inputs);
    let resolved = resolve_all_ours(&mut state);
    assert_eq!(resolved.as_bytes(), TPL_VALUE_DRIFT);

    // Keeping "ours" here means keeping an edit to the rendered value of
    // {{ .testvalue }} — a protected span: write-back must refuse.
    let outcome = lab.engine.resolve_merged(&inputs, &resolved).unwrap();
    let ResolveOutcome::ProtectedSpan { detail } = outcome else {
        panic!("expected ProtectedSpan, got {outcome:?}");
    };
    assert!(detail.contains("protected"), "detail: {detail}");

    // NOTHING mutated: template byte-identical, destination still drifted,
    // repo clean, HEAD unmoved.
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc.tmpl")).unwrap(),
        TPL_SOURCE
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), TPL_VALUE_DRIFT);
    assert!(lab.source_git().dirty_files().unwrap().is_empty());
    assert_eq!(lab.source_git().head_sha().unwrap(), head_before);

    // Journal truth: the finished session records the attempt AND the
    // rejection — the merge decision followed by merge_rejected.
    let journal = lab.journal();
    let (_, decisions) = journal.last_finished_session().unwrap().unwrap();
    let arr = decisions.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["action"], "merge");
    assert_eq!(arr[1]["action"], "merge_rejected");
    assert_eq!(
        arr[1]["target"].as_str().unwrap(),
        lab.target.to_str().unwrap()
    );
    assert!(
        arr[1]["detail"].as_str().unwrap().contains("protected"),
        "rejection detail must be journaled"
    );

    // Daemon truth: no rescan was requested (nothing changed) — the drift is
    // still reported, and the filesystem still probes as drifted.
    assert_eq!(
        lab.probe().expect("drift must survive the rejection").class,
        DriftClass::DestinationDrift
    );
    let drifted = lab.settled_status();
    assert_eq!(drifted.len(), 1);
    assert_eq!(drifted[0].target, lab.target);
}

/// Stories I/J: ORIGIN holds its own version of the managed file.
/// What another machine pushed…
const ORIGIN_CONTENT: &[u8] = b"a=origin\n";
/// …and what this machine's source says on top of it.
const LOCAL_SOURCE: &[u8] = b"a=this-machine\n";

/// Seed the one state where "keep source · origin" is a decision of its own:
/// a second machine pushes `ORIGIN_CONTENT`, this machine fetches it,
/// fast-forwards onto it, and then commits `LOCAL_SOURCE` over the top. Local
/// `main` ends up AHEAD of `origin/main` with a different blob for the file,
/// so the two sources genuinely disagree while a resolve's push stays a
/// fast-forward. The destination is levelled with the local source, so the
/// only disagreement in play is source-vs-origin.
/// Turn on the chezmoi git automation the way a real user does.
///
/// Every fixture here writes an EMPTY chezmoi.toml, so `autoCommit`/`autoPush`
/// were off in every test — and the whole suite was therefore blind to what
/// chezmoi does on a machine that has them on: it runs its OWN git from inside
/// `re-add`, committing and pushing without asking Tomte. That blindness is
/// how the 0.1.15 field bug shipped green.
fn chezmoi_commits_and_pushes_for_itself(lab: &DriftLab) {
    std::fs::write(
        lab.s.root.path().join("chezmoi.toml"),
        "[git]\nautoCommit = true\nautoPush = true\n",
    )
    .unwrap();
}

/// The field failure from 0.1.15, reproduced with the user's actual chezmoi
/// configuration: behind origin, autoPush on, one drifted file.
///
/// What happened: the decision skipped the join (this file was not one of
/// origin's conflicts), then ran `chezmoi re-add`. chezmoi's autoCommit made a
/// local commit and its autoPush was REJECTED as non-fast-forward — leaving
/// the repo diverged, so the next decision failed too. The user saw
/// "Keep disk failed: chezmoi exited with code 1 ... [rejected] main -> main".
#[test]
fn keep_disk_succeeds_while_behind_origin_with_chezmoi_autopush_on() {
    let lab = DriftLab::new();
    chezmoi_commits_and_pushes_for_itself(&lab);
    origin_is_ahead_on_an_unrelated_file(&lab);

    // The user edits a managed file on disk.
    std::fs::write(&lab.target, b"a=from-the-disk\n").unwrap();

    let git_c = lab.source_git();
    assert!(
        git_c.divergence("origin/main").unwrap().behind > 0,
        "premise: behind origin, which is what makes autoPush reject"
    );

    // 0.1.15 failed HERE with chezmoi's raw non-fast-forward stderr.
    lab.engine
        .keep_disk(&lab.target)
        .expect("a decision must not fail because the repo was behind origin");

    let after = git_c.divergence("origin/main").unwrap();
    assert_eq!(
        (after.behind, after.ahead),
        (0, 0),
        "the decision must land on origin, not strand a local commit"
    );
    assert!(
        !git_c.merge_in_progress(),
        "and must not leave a merge open"
    );

    // The disk content is what reached the source, and origin's unrelated
    // commit survived the join.
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        b"a=from-the-disk\n"
    );
    assert!(
        lab.s.source.join("dot_unrelated").exists(),
        "origin's own commit must still be there"
    );
}

/// The sibling action, same configuration. `keep_source` survives being
/// behind for a different reason than `keep_disk` does: it runs `chezmoi
/// apply`, which does not touch the SOURCE, so chezmoi's autoCommit never
/// fires and the push is Tomte's own — where `commit_phase`'s
/// rebase-and-retry recovery can see the rejection and replay on top.
///
/// Worth pinning: it means the two actions stay correct through DIFFERENT
/// mechanisms, and a change to either could silently break only one.
#[test]
fn keep_source_also_survives_being_behind_with_chezmoi_autopush_on() {
    let lab = DriftLab::new();
    chezmoi_commits_and_pushes_for_itself(&lab);
    origin_is_ahead_on_an_unrelated_file(&lab);

    std::fs::write(&lab.target, b"a=disk-edit-to-discard\n").unwrap();
    let git_c = lab.source_git();
    assert!(
        git_c.divergence("origin/main").unwrap().behind > 0,
        "premise: behind"
    );

    lab.engine
        .keep_source(&lab.target)
        .expect("keep_source must not fail because the repo was behind origin");

    let after = git_c.divergence("origin/main").unwrap();
    assert_eq!(
        (after.behind, after.ahead),
        (0, 0),
        "the decision must land on origin"
    );
    assert!(
        lab.s.source.join("dot_unrelated").exists(),
        "origin's own commit must still be there"
    );
}

/// Origin moved on a file this machine has never touched, and the fetch was
/// never integrated: the repo is simply BEHIND and the join is clean.
fn origin_is_ahead_on_an_unrelated_file(lab: &DriftLab) {
    let other = lab.s.root.path().join("other-ahead");
    sh(
        lab.s.root.path(),
        "git",
        &[
            "clone",
            lab.s.bare.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    std::fs::write(other.join("dot_unrelated"), b"origin only\n").unwrap();
    git(&other, &["add", "."]);
    git(
        &other,
        &["commit", "-m", "origin adds a file this machine lacks"],
    );
    git(&other, &["push"]);
    // Fetched but deliberately NOT merged — this is the "behind" state.
    lab.source_git().fetch("origin").unwrap();
}

/// The 0.1.15 field bug: a decision on a repo that is merely BEHIND origin
/// deferred the join to the daemon, then ran a chezmoi command anyway. With
/// chezmoi's autoPush that push is rejected — after autoCommit has already
/// made a local commit, turning "behind" into "diverged" and breaking the
/// next decision too. The join has to happen first.
#[test]
fn story_k_a_decision_joins_a_repo_that_is_merely_behind() {
    let lab = DriftLab::new();
    origin_is_ahead_on_an_unrelated_file(&lab);

    let behind = lab.source_git().divergence("origin/main").unwrap().behind;
    assert!(
        behind > 0,
        "precondition: the repo must start behind origin"
    );

    let merging = lab.engine.ensure_merge_started().unwrap();

    assert!(
        !merging,
        "a clean join leaves nothing conflicted for the user to answer"
    );
    assert_eq!(
        lab.source_git().divergence("origin/main").unwrap().behind,
        0,
        "the repo must be caught up BEFORE anything that can push runs"
    );
    assert!(
        !lab.source_git().merge_in_progress(),
        "a clean join must not leave the repo mid-merge"
    );
    assert!(
        lab.s.source.join("dot_unrelated").exists(),
        "the join must actually bring origin's commit in"
    );
}

fn origin_holds_its_own_version(lab: &DriftLab) {
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
    std::fs::write(other.join("dot_testrc"), ORIGIN_CONTENT).unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "origin's version"]);
    git(&other, &["push"]);

    lab.source_git().fetch("origin").unwrap();
    git(&lab.s.source, &["merge", "--ff-only", "origin/main"]);
    std::fs::write(lab.s.source.join("dot_testrc"), LOCAL_SOURCE).unwrap();
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "this machine's version"]);
    lab.s.chezmoi().apply(None).unwrap();
    assert_eq!(std::fs::read(&lab.target).unwrap(), LOCAL_SOURCE);
    // Drain the event stream and leave the core lock free before the action.
    lab.rescan_and_settle();
}

#[test]
fn story_i_keep_source_origin_makes_origins_version_the_source() {
    let lab = DriftLab::new();
    origin_holds_its_own_version(&lab);
    let head_before = lab.source_git().head_sha().unwrap();

    // Premise: the two SOURCES disagree — origin's blob is not what this
    // machine renders. (`keep_source` would apply the local source and change
    // nothing about that; only this action can hand the file to origin.)
    let inputs = lab.load_merge_inputs();
    assert_eq!(inputs.theirs.as_bytes(), LOCAL_SOURCE);
    assert_eq!(
        inputs.origin.as_deref().map(str::as_bytes),
        Some(ORIGIN_CONTENT),
        "origin must carry its own version for this story to mean anything"
    );

    let outcome = lab
        .engine
        .keep_source_origin(&lab.target, inputs.origin.as_deref().unwrap())
        .unwrap();
    let ResolveOutcome::Done {
        session,
        committed,
        pushed,
        note,
    } = outcome
    else {
        panic!("expected Done, got {outcome:?}");
    };
    assert_eq!(
        session, 1,
        "the refusals in story J aside, this is session 1"
    );
    assert!(
        committed,
        "origin's content lands in the source — the fallback commit must fire"
    );
    assert!(pushed, "origin exists in the scratch — push must succeed");
    assert_eq!(note, None);

    // Filesystem truth: BOTH sources now say what origin said, and the
    // destination was applied from it.
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        ORIGIN_CONTENT
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), ORIGIN_CONTENT);

    // Git truth: one new commit on top of this machine's own, pushed.
    let head_after = lab.source_git().head_sha().unwrap();
    assert_ne!(
        head_after, head_before,
        "the source write must be committed"
    );
    assert_eq!(lab.source_git().rev_parse("HEAD~1").unwrap(), head_before);
    assert_eq!(
        lab.bare_git().rev_parse("HEAD").unwrap(),
        head_after,
        "push must advance the bare origin's HEAD"
    );

    // Journal truth: the session records what actually happened — a
    // keep_source_origin, NOT a hand-made merge — with both pre-mutation
    // snapshot blobs so undo can restore.
    let journal = lab.journal();
    let (id, decisions) = journal.last_finished_session().unwrap().unwrap();
    assert_eq!(id, session);
    let arr = decisions.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["action"], "keep_source_origin");
    assert_eq!(
        arr[0]["target"].as_str().unwrap(),
        lab.target.to_str().unwrap()
    );
    let dest_blob = arr[0]["dest_blob"].as_str().unwrap();
    let source_blob = arr[0]["source_blob"].as_str().unwrap();
    assert_eq!(dest_blob, ContentHash::of(LOCAL_SOURCE).to_hex());
    assert_eq!(journal.get_blob(dest_blob).unwrap().unwrap(), LOCAL_SOURCE);
    assert_eq!(
        journal.get_blob(source_blob).unwrap().unwrap(),
        LOCAL_SOURCE,
        "source snapshot must hold this machine's pre-action source"
    );

    // Daemon truth: the requested rescan settles to in-sync.
    assert!(
        lab.probe().is_none(),
        "probe must report in-sync after keep_source_origin"
    );
    lab.wait_scan_done();
    assert!(lab.settled_status().is_empty());
}

#[test]
fn story_j_keep_source_origin_refuses_before_touching_anything() {
    let lab = DriftLab::new();
    origin_holds_its_own_version(&lab);
    let head_before = lab.source_git().head_sha().unwrap();

    // (1) Origin moved since the user looked: the caller's shown text no
    // longer matches the blob at origin. Writing here would put content into
    // the source that nobody reviewed — refuse instead.
    let err = lab
        .engine
        .keep_source_origin(&lab.target, "a=what-the-user-saw\n")
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("changed since it was shown"),
        "stale-origin refusal must say so: {msg}"
    );

    // (2) The remote ref is DERIVED from the current branch, not hardcoded:
    // on a branch origin has never seen there is no blob at all, and the
    // refusal names the ref it actually looked at.
    git(&lab.s.source, &["branch", "-m", "dotfiles"]);
    let err = lab
        .engine
        .keep_source_origin(&lab.target, str::from_utf8(ORIGIN_CONTENT).unwrap())
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("origin/dotfiles"),
        "the remote ref must come from the CURRENT branch: {msg}"
    );
    assert!(
        msg.contains("has no version"),
        "a file origin has never seen must refuse, not resolve to nothing: {msg}"
    );
    git(&lab.s.source, &["branch", "-m", "main"]);

    // Neither refusal touched anything: source byte-identical, repo clean,
    // HEAD unmoved, destination untouched, nothing finished in the journal.
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        LOCAL_SOURCE
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), LOCAL_SOURCE);
    assert!(lab.source_git().dirty_files().unwrap().is_empty());
    assert_eq!(lab.source_git().head_sha().unwrap(), head_before);
    assert!(lab.journal().last_finished_session().unwrap().is_none());

    // …and neither even OPENED a session: session ids are the journal's
    // rowids, so the first real resolution still gets 1. That is what pins
    // both guards ahead of `session_start` — an unfinished session would be
    // invisible to `last_finished_session` above.
    let outcome = lab
        .engine
        .keep_source_origin(&lab.target, str::from_utf8(ORIGIN_CONTENT).unwrap())
        .unwrap();
    let ResolveOutcome::Done { session, .. } = outcome else {
        panic!("expected Done, got {outcome:?}");
    };
    assert_eq!(
        session, 1,
        "a refusal must return before session_start — two refusals ran first"
    );
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        ORIGIN_CONTENT
    );
}

// ---------------------------------------------------------------------------
// Task 10: a per-file decision resolves the real git merge.
//
// These stories need a genuine DIVERGENCE, not just a local commit ahead of
// origin: since the daemon auto-joins any divergence a virtual merge proves
// clean, only a SAME-FILE conflict still describes something a human has to
// answer. `origin_holds_its_own_version` above is deliberately the other
// shape (this machine fast-forwards first, so its push stays a fast-forward)
// and cannot serve here.
// ---------------------------------------------------------------------------

/// A second managed file, so a merge can leave more than one conflict open.
const OTHER_ORIGIN: &[u8] = b"b=origin\n";
const OTHER_LOCAL: &[u8] = b"b=this-machine\n";

/// Diverge this machine from origin with a same-file conflict on every named
/// source file: a second machine commits its version and pushes, while this
/// machine commits a different version of the same lines WITHOUT integrating
/// origin first. Local `main` ends up both ahead of and behind `origin/main`,
/// with the same blobs touched on both sides — the one shape `git merge`
/// cannot settle on its own.
fn diverge_with_conflicts(lab: &DriftLab, files: &[(&str, &[u8], &[u8])]) {
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
    for (name, origin_content, _) in files {
        std::fs::write(other.join(name), origin_content).unwrap();
    }
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "origin's version"]);
    git(&other, &["push"]);

    // No fetch/merge in between: this machine commits straight on top of the
    // shared base, which is what makes the histories diverge.
    for (name, _, local_content) in files {
        std::fs::write(lab.s.source.join(name), local_content).unwrap();
    }
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "this machine's version"]);
    lab.s.chezmoi().apply(None).unwrap();
    lab.source_git().fetch("origin").unwrap();

    let d = lab.source_git().divergence("origin/main").unwrap();
    assert!(
        d.ahead > 0 && d.behind > 0,
        "the fixture must actually diverge, got {d:?}"
    );
    lab.rescan_and_settle();
}

/// The one managed file diverges.
fn diverge_on_the_managed_file(lab: &DriftLab) {
    diverge_with_conflicts(lab, &[("dot_testrc", ORIGIN_CONTENT, LOCAL_SOURCE)]);
}

/// Task 10 Step 1, at the primitives: the merge starts only when a decision
/// asks for it, one resolution answers it, and concluding it clears the
/// divergence for real.
#[test]
fn resolving_a_diverged_file_starts_and_finishes_the_merge() {
    let lab = DriftLab::new();
    diverge_on_the_managed_file(&lab);
    let git = lab.source_git();
    assert!(!git.merge_in_progress(), "no merge before a decision");

    assert!(
        lab.engine.ensure_merge_started().unwrap(),
        "a diverged repo must enter a real merge"
    );
    assert!(git.merge_in_progress());
    assert_eq!(
        git.conflicted_files().unwrap(),
        vec![PathBuf::from("dot_testrc")]
    );

    lab.engine
        .resolve_repo_conflict(&lab.s.source.join("dot_testrc"), "a=resolved\n")
        .unwrap();
    lab.engine.finish_repo_reconcile().unwrap();

    assert!(!git.merge_in_progress(), "merge committed");
    assert_eq!(git.divergence("origin/main").unwrap().behind, 0);
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        b"a=resolved\n"
    );
}

/// Never speculatively: with nothing to integrate, the repo is left alone.
#[test]
fn ensure_merge_started_leaves_an_undiverged_repo_alone() {
    let lab = DriftLab::new();
    let head_before = lab.source_git().head_sha().unwrap();
    assert!(!lab.engine.ensure_merge_started().unwrap());
    assert!(!lab.source_git().merge_in_progress());
    assert_eq!(lab.source_git().head_sha().unwrap(), head_before);
}

/// The whole point of the task: the user answers ONE review row, and that
/// answer — not a separate reconcile button — is what drives the merge to a
/// commit. Origin's side wins here.
#[test]
fn keep_source_origin_on_a_diverged_file_commits_the_merge() {
    let lab = DriftLab::new();
    diverge_on_the_managed_file(&lab);

    let inputs = lab.load_merge_inputs();
    assert_eq!(
        inputs.origin.as_deref().map(str::as_bytes),
        Some(ORIGIN_CONTENT),
        "origin must carry its own version for this story to mean anything"
    );

    let outcome = lab
        .engine
        .keep_source_origin(&lab.target, inputs.origin.as_deref().unwrap())
        .unwrap();
    let ResolveOutcome::Done {
        committed, pushed, ..
    } = outcome
    else {
        panic!("expected Done, got {outcome:?}");
    };
    assert!(committed, "the last open conflict must commit the merge");
    assert!(pushed);

    let git = lab.source_git();
    assert!(!git.merge_in_progress(), "merge committed");
    assert_eq!(
        git.divergence("origin/main").unwrap().behind,
        0,
        "the divergence the decision answered must be gone"
    );
    // Origin's content is now the source content, and the destination agrees.
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        ORIGIN_CONTENT
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), ORIGIN_CONTENT);
    assert!(
        git.dirty_files().unwrap().is_empty(),
        "no conflict markers or stray files left behind"
    );
}

/// The same, from the other side: "keep disk" re-adds the destination and
/// that becomes the merge's answer for the file.
#[test]
fn keep_disk_on_a_diverged_file_commits_the_merge_with_the_disk_content() {
    let lab = DriftLab::new();
    diverge_on_the_managed_file(&lab);
    std::fs::write(&lab.target, DRIFTED).unwrap();

    let outcome = lab.engine.keep_disk(&lab.target).unwrap();
    let ResolveOutcome::Done {
        committed, pushed, ..
    } = outcome
    else {
        panic!("expected Done, got {outcome:?}");
    };
    assert!(committed);
    assert!(pushed);

    let git = lab.source_git();
    assert!(!git.merge_in_progress());
    assert_eq!(git.divergence("origin/main").unwrap().behind, 0);
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        DRIFTED,
        "the disk content, not a merge artefact, is what got committed"
    );
}

/// Two conflicts, no queue: answering one leaves the merge open and says so,
/// the still-conflicted file reads its panes from the merge's index stages
/// (never from its conflict-markered working copy), the apply-only action
/// refuses to touch it, and the second answer commits the merge.
#[test]
fn a_second_conflict_keeps_the_merge_open_until_it_too_is_answered() {
    let lab = DriftLab::new();
    std::fs::write(lab.s.source.join("dot_other"), b"b=1\n").unwrap();
    git(&lab.s.source, &["add", "."]);
    git(
        &lab.s.source,
        &["commit", "-m", "add a second managed file"],
    );
    git(&lab.s.source, &["push"]);
    diverge_with_conflicts(
        &lab,
        &[
            ("dot_testrc", ORIGIN_CONTENT, LOCAL_SOURCE),
            ("dot_other", OTHER_ORIGIN, OTHER_LOCAL),
        ],
    );

    // Answer the first row: the merge starts and takes this answer, but the
    // second file is still unanswered, so nothing is committed yet.
    let inputs = lab.load_merge_inputs();
    let outcome = lab
        .engine
        .keep_source_origin(&lab.target, inputs.origin.as_deref().unwrap())
        .unwrap();
    let ResolveOutcome::Done {
        committed,
        pushed,
        note,
        ..
    } = outcome
    else {
        panic!("expected Done, got {outcome:?}");
    };
    assert!(!committed, "one answer of two cannot commit the merge");
    assert!(!pushed);
    let note = note.expect("an unfinished merge must say so");
    assert!(note.contains("still differ from origin"), "note: {note}");

    let git = lab.source_git();
    assert!(git.merge_in_progress(), "the merge stays open");
    assert_eq!(
        git.conflicted_files().unwrap(),
        vec![PathBuf::from("dot_other")]
    );

    // The second file is now a mid-merge conflict. Its working-tree source
    // holds git's markers, so the editor's panes must come from the index.
    let other_target = lab.s.home.join(".other");
    let conflict = lab
        .engine
        .repo_conflict_for(&other_target)
        .unwrap()
        .expect("a conflicted file must offer repo-conflict inputs");
    assert_eq!(
        conflict.ours.as_bytes(),
        OTHER_LOCAL,
        "stage 2 = this machine"
    );
    assert_eq!(conflict.theirs.as_bytes(), OTHER_ORIGIN, "stage 3 = origin");
    assert!(
        std::fs::read_to_string(lab.s.source.join("dot_other"))
            .unwrap()
            .contains("<<<<<<<"),
        "premise: the working copy really is conflict-markered"
    );

    // …so the apply-only action must refuse rather than write those markers
    // into the user's destination file.
    let err = lab
        .engine
        .keep_source(&other_target)
        .expect_err("keep_source must refuse a mid-merge file");
    assert!(err.to_string().contains("mid-merge"), "err: {err}");
    assert!(
        !std::fs::read_to_string(&other_target)
            .unwrap()
            .contains("<<<<<<<"),
        "the destination must be untouched by the refusal"
    );

    // Answer it (what the merge editor's Save does) and the merge concludes.
    lab.engine
        .resolve_repo_conflict(&conflict.source_path, "b=resolved\n")
        .unwrap();
    match lab.engine.finish_merge_if_resolved().unwrap() {
        MergeConclusion::Committed { pushed, note } => {
            assert!(pushed, "note: {note:?}");
        }
        other => panic!("expected Committed, got {other:?}"),
    }
    assert!(!git.merge_in_progress());
    assert_eq!(git.divergence("origin/main").unwrap().behind, 0);
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        ORIGIN_CONTENT
    );
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_other")).unwrap(),
        b"b=resolved\n"
    );
}

// ---------------------------------------------------------------------------
// Task 10, fix round 1.
// ---------------------------------------------------------------------------

/// Make every later commit in this repo unsignable. This is the locked
/// 1Password signing agent, reproduced without 1Password — the everyday
/// cause of the one merge state nothing else can dig the repo out of.
fn unsignable(repo: &std::path::Path) {
    git(repo, &["config", "commit.gpgsign", "true"]);
    git(repo, &["config", "gpg.format", "ssh"]);
    git(repo, &["config", "gpg.ssh.program", "/bin/false"]);
    git(repo, &["config", "user.signingkey", "ssh-ed25519 AAAAfake"]);
}

fn signable(repo: &std::path::Path) {
    git(repo, &["config", "commit.gpgsign", "false"]);
}

/// The stranding case, and the way out of it. When the merge commit fails,
/// everything is resolved and staged — so NO conflicted file is left to be a
/// review row, and nothing in the ordinary flow will ever ask again. The repo
/// must therefore report itself as pending, and be concludable without a
/// decision to carry.
#[test]
fn an_unsignable_merge_commit_strands_the_merge_and_stays_recoverable() {
    let lab = DriftLab::new();
    diverge_on_the_managed_file(&lab);
    let git_c = lab.source_git();
    let source = lab.s.source.join("dot_testrc");

    assert!(lab.engine.ensure_merge_started().unwrap());
    lab.engine
        .resolve_repo_conflict(&source, "a=resolved\n")
        .unwrap();

    unsignable(&lab.s.source);
    let err = lab
        .engine
        .finish_merge_if_resolved()
        .expect_err("an unsignable merge commit must fail loudly, not silently succeed");

    // The exact shape that used to strand: open merge, nothing conflicted.
    assert!(git_c.merge_in_progress(), "the merge is still open");
    assert!(
        git_c.conflicted_files().unwrap().is_empty(),
        "…and nothing is left to answer, so no review row speaks for it"
    );
    assert!(
        lab.engine.pending_merge_commit().unwrap(),
        "so it MUST report itself pending (the failure was: {err})"
    );

    // The way out, from inside the app: exactly the call the dashboard
    // banner's "Finish merge" button makes, with no decision in hand.
    signable(&lab.s.source);
    match lab.engine.finish_merge_if_resolved().unwrap() {
        MergeConclusion::Committed { pushed, note } => assert!(pushed, "note: {note:?}"),
        other => panic!("expected Committed, got {other:?}"),
    }
    assert!(!git_c.merge_in_progress(), "merge committed");
    assert!(!lab.engine.pending_merge_commit().unwrap());
    assert_eq!(git_c.divergence("origin/main").unwrap().behind, 0);
    assert_eq!(
        std::fs::read(&source).unwrap(),
        b"a=resolved\n",
        "the resolution the user made before the failure must survive it"
    );
}

/// A merge with conflicts LEFT is not stranded: those files are still review
/// rows. Only the all-answered case needs the banner, and saying so here
/// keeps the banner from crying wolf over ordinary mid-merge progress.
#[test]
fn a_merge_with_conflicts_left_is_not_reported_as_pending() {
    let lab = DriftLab::new();
    diverge_on_the_managed_file(&lab);
    assert!(lab.engine.ensure_merge_started().unwrap());
    assert!(
        !lab.engine.pending_merge_commit().unwrap(),
        "an unanswered conflict recovers through its own review row"
    );
}

/// The narrowed trigger. Origin moved a file this decision is not about, so
/// answering this one must NOT put the repo into a merge — a merge writes
/// conflict markers into every disagreeing source, including ones the user
/// never selected.
#[test]
fn a_decision_about_an_unconflicted_file_still_catches_the_repo_up() {
    let lab = DriftLab::new();
    // A second managed file, shared with origin.
    std::fs::write(lab.s.source.join("dot_other"), b"b=1\n").unwrap();
    git(&lab.s.source, &["add", "."]);
    git(
        &lab.s.source,
        &["commit", "-m", "add a second managed file"],
    );
    git(&lab.s.source, &["push"]);
    // Origin conflicts on dot_other ONLY; this machine also edits dot_testrc,
    // which origin never touched.
    diverge_with_conflicts(&lab, &[("dot_other", OTHER_ORIGIN, OTHER_LOCAL)]);
    std::fs::write(lab.s.source.join("dot_testrc"), LOCAL_SOURCE).unwrap();
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "local-only edit"]);

    let git_c = lab.source_git();
    assert!(
        git_c.divergence("origin/main").unwrap().behind > 0,
        "premise: the repo starts behind origin"
    );

    // Answering dot_testrc — a file origin never touched — must NOT be
    // refused and must NOT be silently skipped. 0.1.15 skipped the join here
    // because this file was not one of the conflicts, then ran a chezmoi
    // command anyway: autoCommit made a local commit, autoPush was rejected
    // for being behind, and "behind" became "diverged", breaking the next
    // decision too. Whose file conflicts has no bearing on whether the repo
    // needs origin's commits.
    let merging = lab
        .engine
        .ensure_merge_started()
        .expect("catching up must never fail a decision the user is making");

    assert!(
        merging,
        "origin conflicts on dot_other, so a merge is open for it to be answered in"
    );
    assert!(git_c.merge_in_progress());
    // `behind` is still non-zero here and that is correct: HEAD does not move
    // until the merge commits. What matters is that MERGE_HEAD is origin's
    // tip, so the commit this merge eventually makes CONTAINS origin — which
    // is exactly what makes the later push fast-forward.
    assert_eq!(
        git_c.rev_parse("MERGE_HEAD").unwrap(),
        git_c.rev_parse("origin/main").unwrap(),
        "the open merge must be against origin's tip"
    );
    assert_eq!(
        git_c.conflicted_files().unwrap(),
        vec![PathBuf::from("dot_other")],
        "only the genuinely conflicting file is left to answer"
    );

    // dot_testrc is NOT conflicted, so it is answerable right now — resolving
    // it does not wait on dot_other. That is the isolation that matters:
    // decisions are independent even though the eventual push is one commit.
    assert!(
        !git_c
            .conflicted_files()
            .unwrap()
            .contains(&PathBuf::from("dot_testrc")),
        "a file origin never touched must not become a conflict to answer"
    );
}

/// Cancel's blast radius, measured. An abort rolls back every source file the
/// merge staged — including one the user already answered and was told was
/// recorded — while its applied destination stays. That asymmetry is exactly
/// what the editor now has to warn about, so pin both halves of it.
#[test]
fn aborting_rolls_back_recorded_resolutions_but_not_their_destinations() {
    let lab = DriftLab::new();
    std::fs::write(lab.s.source.join("dot_other"), b"b=1\n").unwrap();
    git(&lab.s.source, &["add", "."]);
    git(
        &lab.s.source,
        &["commit", "-m", "add a second managed file"],
    );
    git(&lab.s.source, &["push"]);
    diverge_with_conflicts(
        &lab,
        &[
            ("dot_testrc", ORIGIN_CONTENT, LOCAL_SOURCE),
            ("dot_other", OTHER_ORIGIN, OTHER_LOCAL),
        ],
    );

    // Answer file A through an ordinary review row; it reports Done.
    let inputs = lab.load_merge_inputs();
    let outcome = lab
        .engine
        .keep_source_origin(&lab.target, inputs.origin.as_deref().unwrap())
        .unwrap();
    assert!(matches!(outcome, ResolveOutcome::Done { .. }));
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        ORIGIN_CONTENT
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), ORIGIN_CONTENT);

    // What Cancel would discard, as the editor now asks before acting.
    let rollback = lab.engine.merge_rollback_files().unwrap();
    assert!(
        rollback.contains(&PathBuf::from("dot_testrc")),
        "the answered file must be named as at risk, got {rollback:?}"
    );

    lab.engine.abort_repo_reconcile();

    // A's SOURCE went back; A's DESTINATION did not. That divergence between
    // what the user was told and what is true is the whole reason Cancel has
    // to say this out loud first.
    assert!(!lab.source_git().merge_in_progress());
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        LOCAL_SOURCE,
        "the recorded resolution really is rolled back"
    );
    assert_eq!(
        std::fs::read(&lab.target).unwrap(),
        ORIGIN_CONTENT,
        "…while its applied destination is not"
    );
    assert!(
        lab.engine.merge_rollback_files().unwrap().is_empty(),
        "no merge, nothing left to roll back"
    );
}

/// The dashboard's quick action is `keep_disk`, and `keep_disk` reaches
/// `ensure_merge_started` — so the dashboard can strand a merge exactly like
/// a review row can. That matters more there than anywhere else: the failure
/// note tells the user to "finish it from the dashboard", so if that surface
/// does not probe, the sentence points at a screen showing nothing.
#[test]
fn the_dashboards_own_action_can_strand_a_merge() {
    let lab = DriftLab::new();
    diverge_on_the_managed_file(&lab);
    std::fs::write(&lab.target, DRIFTED).unwrap();
    unsignable(&lab.s.source);

    // Exactly what `run_quick_action` runs on that surface.
    let outcome = lab.engine.keep_disk(&lab.target).unwrap();
    let ResolveOutcome::Done {
        committed, note, ..
    } = outcome
    else {
        panic!("expected Done, got {outcome:?}");
    };
    assert!(!committed);
    let note = note.expect("a merge that could not be committed must say so");
    assert!(
        note.contains("finish it from the dashboard"),
        "the note points the user at the dashboard: {note}"
    );

    // …and the state that note promises a way out of is exactly the one the
    // dashboard banner is driven by.
    assert!(
        lab.engine.pending_merge_commit().unwrap(),
        "the dashboard's own action must be able to raise its own banner"
    );

    signable(&lab.s.source);
    match lab.engine.finish_merge_if_resolved().unwrap() {
        MergeConclusion::Committed { pushed, note } => assert!(pushed, "note: {note:?}"),
        other => panic!("expected Committed, got {other:?}"),
    }
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        DRIFTED,
        "the disk content the quick action chose is what got committed"
    );
}
