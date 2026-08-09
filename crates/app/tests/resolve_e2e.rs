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
use tomte_app::resolve::{ResolveEngine, ResolveOutcome};
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

    /// Make the daemon observe the current filesystem state and return its
    /// settled drift list (rescan is async-acked: request → ScanDone → poll).
    fn rescan_and_settle(&self) -> Vec<DriftSummary> {
        self.request_rescan();
        self.wait_scan_done();
        self.settled_status()
    }

    /// `merge_inputs::load` with the lab's own stack, ready for `MergeState`.
    fn load_merge_inputs(&self) -> merge_inputs::MergeInputs {
        merge_inputs::load(&self.engine.chezmoi, &self.journal_path, &self.target).unwrap()
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
