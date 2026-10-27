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
use tomte_app::resolve::{
    BatchDecision, BatchItem, BatchOutcome, MergeConclusion, ResolveEngine, ResolveOutcome,
};
use tomte_core::cmd::SystemRunner;
use tomte_core::drift::{ContentHash, DriftClass};
use tomte_core::git::GitClient;
use tomte_core::merge::{Choice, RegionKind};
use tomte_core::scanner::FileDrift;
use tomte_core::structured::{Pick, StructuredDocument};
use tomte_core::testsupport::{Scratch, git, sh};
use tomte_daemon::core::DaemonCore;
use tomte_daemon::server::{ServeCtx, serve};
use tomte_journal::Journal;
use tomte_proto::{DriftSummary, Event, Request, Response};

/// Content written to the destination to fake a foreign edit.
const DRIFTED: &[u8] = b"a=local\n";
/// The scratch source's rendered content (`dot_testrc` = "a=1\n").
const RENDERED: &[u8] = b"a=1\n";
/// Pristine content of the extra managed files a batch story adds.
const MANAGED_BASE: &[u8] = b"x=1\n";

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
    /// The daemon's core, shared with the serving thread — a test that holds
    /// this lock IS a scan holding the daemon, from every client's point of
    /// view.
    core: Arc<Mutex<DaemonCore>>,
}

impl DriftLab {
    fn new() -> Self {
        Self::with_daemon(
            tomte_daemon::server::DEFAULT_BUSY_WAIT,
            tomte_app::resolve::DAEMON_PATIENCE,
        )
    }

    /// A lab whose daemon bounces a core request after `busy_wait` and whose
    /// engine keeps retrying such bounces for `patience` — both shrunk by
    /// the busy stories so a held lock proves the behaviour in seconds.
    fn with_daemon(busy_wait: Duration, patience: Duration) -> Self {
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
        let mut ctx = ServeCtx::ready(core.clone(), now_ts, Arc::new(|| {}));
        ctx.busy_wait = busy_wait;
        std::thread::spawn(move || serve(listener, ctx));

        let ipc = Arc::new(IpcClient::connect(&sock).unwrap());
        let events = ipc.subscribe().unwrap();
        let engine = ResolveEngine {
            // The SCRATCH client — mutations must hit the scratch home.
            chezmoi: s.chezmoi(),
            git: GitClient::new(Arc::new(SystemRunner), s.source.clone()),
            ipc,
            journal_path: journal_path.clone(),
            daemon_patience: patience,
        };
        let target = s.home.join(".testrc");
        Self {
            s,
            engine,
            events,
            journal_path,
            target,
            core,
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

/// Three one-click decisions must cost ONE re-add, ONE commit and ONE push —
/// not three of each. From the field log: re-add ≈ 2.5s and push ≈ 1.5s per
/// invocation, and both accept a list.
#[test]
fn story_m_a_batch_of_keep_disk_shares_one_commit_and_one_push() {
    let lab = DriftLab::new();
    // Three managed files, all drifted on disk. Every source is created and
    // applied in ONE pass before any drift is seeded: `apply(None)` is
    // repo-wide and `--force`, so applying between the drifts would wash the
    // earlier ones straight back out.
    let files: [(&str, &str, &[u8]); 3] = [
        ("dot_a", ".a", b"a=disk\n"),
        ("dot_b", ".b", b"b=disk\n"),
        ("dot_c", ".c", b"c=disk\n"),
    ];
    for (src, _, _) in files {
        std::fs::write(lab.s.source.join(src), b"x=1\n").unwrap();
    }
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "three more managed files"]);
    git(&lab.s.source, &["push"]);
    lab.s.chezmoi().apply(None).unwrap();
    for (_, dst, body) in files {
        std::fs::write(lab.s.home.join(dst), body).unwrap();
    }
    let commits_before = commit_count(&lab.s.bare);

    let outcome = lab.engine.resolve_batch(vec![
        BatchItem {
            target: lab.s.home.join(".a"),
            decision: BatchDecision::KeepDisk,
        },
        BatchItem {
            target: lab.s.home.join(".b"),
            decision: BatchDecision::KeepDisk,
        },
        BatchItem {
            target: lab.s.home.join(".c"),
            decision: BatchDecision::KeepDisk,
        },
    ]);

    // One entry per item, in input order.
    assert_eq!(
        outcome
            .per_target
            .iter()
            .map(|(t, _)| t.clone())
            .collect::<Vec<_>>(),
        vec![
            lab.s.home.join(".a"),
            lab.s.home.join(".b"),
            lab.s.home.join(".c"),
        ]
    );

    // Every file resolved, each with its OWN session (undo stays per file).
    let mut sessions = std::collections::BTreeSet::new();
    for (target, result) in &outcome.per_target {
        match result {
            Ok(ResolveOutcome::Done {
                session,
                committed,
                pushed,
                ..
            }) => {
                assert!(
                    *committed && *pushed,
                    "{}: the batch tail must land on origin",
                    target.display()
                );
                sessions.insert(*session);
            }
            other => panic!("{}: expected Done, got {other:?}", target.display()),
        }
    }
    assert_eq!(
        sessions.len(),
        3,
        "three files, three sessions — undo is per file"
    );

    // ONE commit reached origin for the whole batch.
    assert_eq!(
        commit_count(&lab.s.bare) - commits_before,
        1,
        "a batch is one commit and one push, not one per file"
    );
    for (src, body) in [
        ("dot_a", "a=disk\n"),
        ("dot_b", "b=disk\n"),
        ("dot_c", "c=disk\n"),
    ] {
        assert_eq!(
            std::fs::read_to_string(lab.s.source.join(src)).unwrap(),
            body,
            "{src} must carry the disk content the batch chose"
        );
    }
}

/// Mid-merge the shared tail is unavailable — it stages the whole index, so
/// it would commit conflicts nobody answered — so a batch must fall back to
/// the single-file paths, and still resolve every item.
///
/// This story alone does not prove the fallback is needed — with only one
/// conflict the batch tail reaches the same end state, so it passes with the
/// fallback deleted. `story_o` is the one that bites. What this pins is the
/// other half: mid-merge, a batch still RESOLVES its item and still drives
/// the merge to a commit when nothing is left conflicted.
///
/// chezmoi's git automation is ON deliberately, the way the real user's is.
#[test]
fn story_n_a_batch_while_mid_merge_falls_back_to_single_file_paths() {
    let lab = DriftLab::new();
    chezmoi_commits_and_pushes_for_itself(&lab);
    diverge_on_the_managed_file(&lab); // existing helper: same-file conflict
    std::fs::write(&lab.target, b"a=disk\n").unwrap();
    let outcome = lab.engine.resolve_batch(vec![BatchItem {
        target: lab.target.clone(),
        decision: BatchDecision::KeepDisk,
    }]);
    let (_, r) = &outcome.per_target[0];
    assert!(matches!(r, Ok(ResolveOutcome::Done { .. })), "got {r:?}");
    assert!(
        !lab.source_git().merge_in_progress(),
        "the only conflict was answered, so the merge concluded"
    );
    // And it answered it with the DISK content, not with what the merge left
    // in the source file (conflict markers included).
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        b"a=disk\n"
    );
}

/// WHY the mid-merge fallback is not optional: the shared tail commits the
/// whole index, so running it inside a merge would commit the conflicts the
/// user never answered — conflict markers and all — and call the merge done.
///
/// Two files conflict with origin; the batch answers ONE. The merge must stay
/// open on the other, with its conflict markers still uncommitted.
///
/// This is the assertion story_n cannot make: with a single conflict, the
/// batch tail happens to reach the same end state, so that story passes with
/// the fallback deleted. This one does not.
#[test]
fn story_o_a_mid_merge_batch_never_commits_a_conflict_it_did_not_answer() {
    let lab = DriftLab::new();
    chezmoi_commits_and_pushes_for_itself(&lab);
    std::fs::write(lab.s.source.join("dot_other"), b"b=1\n").unwrap();
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "a second managed file"]);
    git(&lab.s.source, &["push"]);
    diverge_with_conflicts(
        &lab,
        &[
            ("dot_testrc", ORIGIN_CONTENT, LOCAL_SOURCE),
            ("dot_other", OTHER_ORIGIN, OTHER_LOCAL),
        ],
    );

    std::fs::write(&lab.target, b"a=disk\n").unwrap();
    let outcome = lab.engine.resolve_batch(vec![BatchItem {
        target: lab.target.clone(),
        decision: BatchDecision::KeepDisk,
    }]);
    let (_, r) = &outcome.per_target[0];
    let Ok(ResolveOutcome::Done {
        committed,
        pushed,
        note,
        ..
    }) = r
    else {
        panic!("the answered file must still resolve, got {r:?}");
    };
    assert!(
        !committed && !pushed,
        "one conflict is still open, so there is nothing to commit yet"
    );
    assert!(
        note.as_deref().unwrap_or_default().contains("still differ"),
        "the user must be told the merge is waiting on another file, got {note:?}"
    );

    // The merge is still open, still holding the file nobody answered.
    let git_c = lab.source_git();
    assert!(
        git_c.merge_in_progress(),
        "answering one of two conflicts must not conclude the merge"
    );
    assert_eq!(
        git_c.conflicted_files().unwrap(),
        vec![PathBuf::from("dot_other")]
    );
    assert!(
        std::fs::read_to_string(lab.s.source.join("dot_other"))
            .unwrap()
            .contains("<<<<<<<"),
        "and its conflict markers must still be sitting in the working tree, \
         uncommitted"
    );
}

/// The headline invariant of the three-phase design: a batch shares its
/// TAIL, never its journal session, so Undo still works one file at a time.
///
/// `keep_source` items, because `undo_last` restores DESTINATION files
/// (source-side revert is deferred to Plan 7) — so the destination is where
/// a per-file undo is observable at all.
#[test]
fn story_p_undo_after_a_batch_reverts_exactly_one_file() {
    let lab = DriftLab::new();
    more_managed_files(&lab, &["a", "b", "c"]);
    for (dst, body) in [(".a", "a=disk\n"), (".b", "b=disk\n"), (".c", "c=disk\n")] {
        std::fs::write(lab.s.home.join(dst), body).unwrap();
    }

    let outcome = lab.engine.resolve_batch(vec![
        batch_item(&lab, ".a", BatchDecision::KeepSourceLocal),
        batch_item(&lab, ".b", BatchDecision::KeepSourceLocal),
        batch_item(&lab, ".c", BatchDecision::KeepSourceLocal),
    ]);
    let sessions: Vec<i64> = (0..3).map(|ix| done(&outcome, ix).0).collect();
    assert_eq!(
        sessions
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3,
        "three files, three sessions"
    );
    for dst in [".a", ".b", ".c"] {
        assert_eq!(std::fs::read(lab.s.home.join(dst)).unwrap(), MANAGED_BASE);
    }
    // Let the batch's rescan finish (and release the core) before undoing:
    // the undo's IPC calls must not race a scan holding the daemon core.
    lab.wait_scan_done();
    assert!(lab.settled_status().is_empty(), "in-sync before the undo");

    let undone = lab.engine.undo_last().unwrap();
    assert_eq!(
        undone,
        Some(sessions[2]),
        "undo must target the LAST file's OWN session, not the batch"
    );
    assert_eq!(
        std::fs::read(lab.s.home.join(".c")).unwrap(),
        b"c=disk\n",
        "the undone file must be back to what the user had on disk"
    );
    for dst in [".a", ".b"] {
        assert_eq!(
            std::fs::read(lab.s.home.join(dst)).unwrap(),
            MANAGED_BASE,
            "{dst} rode the same tail but is NOT part of .c's undo"
        );
    }
}

/// A mixed batch, one tail: the keep-disk half rides the commit and push,
/// the keep-source half does not. `chezmoi apply` never touches the source
/// repo, so there is nothing for it to have committed — the same verdict
/// single-file `keep_source` reports.
///
/// Also the only end-to-end exercise of `apply_many`.
#[test]
fn story_q_a_mixed_batch_commits_the_keep_disk_half_only() {
    let lab = DriftLab::new();
    more_managed_files(&lab, &["a", "b"]);
    std::fs::write(lab.s.home.join(".a"), b"a=disk\n").unwrap();
    std::fs::write(lab.s.home.join(".b"), b"b=discard-me\n").unwrap();
    let commits_before = commit_count(&lab.s.bare);

    let outcome = lab.engine.resolve_batch(vec![
        batch_item(&lab, ".a", BatchDecision::KeepDisk),
        batch_item(&lab, ".b", BatchDecision::KeepSourceLocal),
    ]);

    let (_, committed, pushed, note) = done(&outcome, 0);
    assert!(committed && pushed, "the keep-disk half must reach origin");
    assert_eq!(note, None);
    let (_, committed, pushed, note) = done(&outcome, 1);
    assert!(
        !committed && !pushed,
        "apply never touches the source repo, so this half committed nothing"
    );
    assert_eq!(note, None);

    // Filesystem truth for both halves, out of ONE shared tail.
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_a")).unwrap(),
        b"a=disk\n",
        "re-add must have carried the disk content into the source"
    );
    assert_eq!(
        std::fs::read(lab.s.home.join(".b")).unwrap(),
        MANAGED_BASE,
        "apply_many must have restored the rendered content"
    );
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_b")).unwrap(),
        MANAGED_BASE,
        "and must not have touched its source"
    );
    assert_eq!(
        commit_count(&lab.s.bare) - commits_before,
        1,
        "one commit for the batch, carrying the keep-disk half"
    );
}

/// Keep source on files whose whole directory was deleted from disk. chezmoi
/// refuses to apply a file whose parent directory is missing (`stat …: no
/// such file or directory`) and fails the WHOLE invocation with it, so one
/// deleted skill folder failed every file in the batch (2026-10-26, a
/// `~/.agents/skills/<skill>/` removed by hand).
///
/// MUTATION: drop the missing-ancestor targets from `ChezmoiClient::apply_many`
/// and `apply` → both applies fail with chezmoi's stat error.
#[test]
fn keep_source_recreates_a_deleted_directory() {
    let lab = DriftLab::new();
    let dir = lab.s.source.join("dot_d/sub");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(lab.s.source.join("dot_d/top"), MANAGED_BASE).unwrap();
    std::fs::write(dir.join("deep"), MANAGED_BASE).unwrap();
    std::fs::create_dir_all(lab.s.source.join("dot_e")).unwrap();
    std::fs::write(lab.s.source.join("dot_e/one"), MANAGED_BASE).unwrap();
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "directories"]);
    git(&lab.s.source, &["push"]);
    lab.s.chezmoi().apply(None).unwrap();
    std::fs::remove_dir_all(lab.s.home.join(".d")).unwrap();
    std::fs::remove_dir_all(lab.s.home.join(".e")).unwrap();

    let outcome = lab.engine.resolve_batch(vec![
        batch_item(&lab, ".d/top", BatchDecision::KeepSourceLocal),
        batch_item(&lab, ".d/sub/deep", BatchDecision::KeepSourceLocal),
    ]);
    done(&outcome, 0);
    done(&outcome, 1);
    for name in [".d/top", ".d/sub/deep"] {
        assert_eq!(
            std::fs::read(lab.s.home.join(name)).unwrap(),
            MANAGED_BASE,
            "{name} must be back on disk"
        );
    }

    lab.engine
        .keep_source(&lab.s.home.join(".e/one"))
        .expect("single-file keep source must recreate its directory too");
    assert_eq!(
        std::fs::read(lab.s.home.join(".e/one")).unwrap(),
        MANAGED_BASE
    );
}

/// Keep disk on a file deleted from disk removes it from the source. Before,
/// the undo snapshot found no disk file and every such row failed with
/// "snapshot returned 1 blobs for 2 paths" (2026-10-26, a skill's WARP.md
/// the upstream had dropped).
///
/// MUTATION: route a deleted target through `re-add` like any other keep
/// disk → the snapshot error comes back and the source files survive.
#[test]
fn keep_disk_on_a_deleted_file_removes_it_from_the_source() {
    let lab = DriftLab::new();
    more_managed_files(&lab, &["gone1", "gone2", "edited"]);
    std::fs::remove_file(lab.s.home.join(".gone1")).unwrap();
    std::fs::remove_file(lab.s.home.join(".gone2")).unwrap();
    std::fs::write(lab.s.home.join(".edited"), b"edited=disk\n").unwrap();
    let commits_before = commit_count(&lab.s.bare);

    let outcome = lab.engine.resolve_batch(vec![
        batch_item(&lab, ".gone1", BatchDecision::KeepDisk),
        batch_item(&lab, ".edited", BatchDecision::KeepDisk),
    ]);
    let (_, committed, pushed, _) = done(&outcome, 0);
    assert!(committed && pushed, "the removal must reach origin");
    done(&outcome, 1);
    assert!(
        !lab.s.source.join("dot_gone1").exists(),
        "the deleted file must be gone from the source"
    );
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_edited")).unwrap(),
        b"edited=disk\n",
        "and the batch's ordinary keep disk still re-added"
    );
    assert_eq!(
        commit_count(&lab.s.bare) - commits_before,
        1,
        "one commit for the batch"
    );

    match lab.engine.keep_disk(&lab.s.home.join(".gone2")) {
        Ok(ResolveOutcome::Done { committed, .. }) => assert!(committed),
        other => panic!("single keep disk on a deleted file: {other:?}"),
    }
    assert!(!lab.s.source.join("dot_gone2").exists());
    assert!(
        !lab.s.home.join(".gone2").exists(),
        "and nothing was recreated on disk"
    );
}

/// A keep-source-only batch must not commit or push AT ALL.
///
/// `apply` never touches the source repo, and the commit phase counts a clean
/// repo as "committed" (chezmoi's autoCommit may have done it). Running the
/// tail regardless reported "committed & pushed" on a sweep that committed
/// nothing, and paid for a no-op push (~1.5s) to say it.
///
/// The tail is not merely wasted, either: it stages with `git add --all`. So
/// the fixture leaves an unrelated source edit in progress — the user
/// halfway through editing another dotfile — and the sweep must not commit
/// it out from under them.
#[test]
fn story_r_a_keep_source_only_batch_commits_and_pushes_nothing() {
    let lab = DriftLab::new();
    more_managed_files(&lab, &["a", "b", "c"]);
    std::fs::write(lab.s.home.join(".a"), b"a=discard\n").unwrap();
    std::fs::write(lab.s.home.join(".b"), b"b=discard\n").unwrap();
    std::fs::write(
        lab.s.source.join("dot_c"),
        b"c=an edit the user has not finished\n",
    )
    .unwrap();
    let head_before = lab.source_git().head_sha().unwrap();
    let commits_before = commit_count(&lab.s.bare);

    let outcome = lab.engine.resolve_batch(vec![
        batch_item(&lab, ".a", BatchDecision::KeepSourceLocal),
        batch_item(&lab, ".b", BatchDecision::KeepSourceLocal),
    ]);
    for ix in 0..2 {
        let (_, committed, pushed, note) = done(&outcome, ix);
        assert!(
            !committed && !pushed,
            "entry {ix}: apply commits nothing and pushes nothing"
        );
        assert_eq!(note, None);
    }
    assert_eq!(
        lab.source_git().head_sha().unwrap(),
        head_before,
        "no commit may have been made"
    );
    assert_eq!(
        commit_count(&lab.s.bare),
        commits_before,
        "and nothing may have been pushed"
    );
    assert_eq!(
        lab.source_git().dirty_files().unwrap(),
        vec![PathBuf::from("dot_c")],
        "the unrelated source edit must still be the user's to finish"
    );
    for dst in [".a", ".b"] {
        assert_eq!(std::fs::read(lab.s.home.join(dst)).unwrap(), MANAGED_BASE);
    }
}

/// A templated keep-disk rides the batch like any other keep-disk: its
/// write-back lands in the same commit and push as its plain sibling, its
/// slot is kept, and — because the shared tail ends sessions in input order —
/// the last item to land is the last finished session, which is what the
/// banner names and Undo reverts.
#[test]
fn story_s_a_templated_keep_disk_shares_the_batch_commit_with_its_sibling() {
    let lab = DriftLab::new();
    std::fs::write(lab.s.config_path(), "[data]\ntestvalue = \"tpl\"\n").unwrap();
    git(&lab.s.source, &["mv", "dot_testrc", "dot_testrc.tmpl"]);
    std::fs::write(lab.s.source.join("dot_testrc.tmpl"), TPL_SOURCE).unwrap();
    more_managed_files(&lab, &["a"]); // commits and pushes the template too, then applies
    assert_eq!(std::fs::read(&lab.target).unwrap(), TPL_RENDERED);

    std::fs::write(&lab.target, TPL_LITERAL_DRIFT).unwrap();
    std::fs::write(lab.s.home.join(".a"), b"a=disk\n").unwrap();
    let commits_before = commit_count(&lab.s.bare);

    let outcome = lab.engine.resolve_batch(vec![
        BatchItem {
            target: lab.target.clone(),
            decision: BatchDecision::KeepDisk,
        },
        batch_item(&lab, ".a", BatchDecision::KeepDisk),
    ]);

    assert_eq!(outcome.per_target.len(), 2, "one entry per item");
    assert_eq!(
        outcome.per_target[0].0, lab.target,
        "slot 0 is the templated item"
    );
    let (tmpl_session, c0, p0, _) = done(&outcome, 0);
    let (plain_session, c1, p1, _) = done(&outcome, 1);
    assert!(c0 && p0 && c1 && p1, "both landed on the shared tail");
    assert_eq!(
        commit_count(&lab.s.bare) - commits_before,
        1,
        "ONE commit and push for the two of them"
    );

    assert_eq!(
        std::fs::read_to_string(lab.s.source.join("dot_testrc.tmpl")).unwrap(),
        "a=local\nvalue={{ .testvalue }}\n"
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), TPL_LITERAL_DRIFT);
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_a")).unwrap(),
        b"a=disk\n"
    );

    // Per-file sessions (§2.3), ended in input order: the last slot is the
    // last finished session — the one Undo reverts and the banner names.
    assert_ne!(tmpl_session, plain_session);
    let (last, _) = lab.journal().last_finished_session().unwrap().unwrap();
    assert_eq!(last, plain_session);

    // Daemon truth: in sync. The batch applied the templated target after
    // writing its source, so chezmoi's own record of what it last wrote
    // moved with the template; without that apply the daemon would report
    // the file as a conflict right after a successful batch.
    assert!(lab.rescan_and_settle().is_empty());
}

/// An all-templated batch has no `re-add` at all, so its commit must be
/// driven by the write-backs alone. Without that, the batch would report
/// `committed: false` with the template sitting dirty and uncommitted —
/// a silent lie the next catch-up would sweep into an unnamed commit.
#[test]
fn story_s3_an_all_templated_batch_commits_and_pushes_its_write_backs() {
    let lab = DriftLab::new();
    templatize_and_drift(&lab, TPL_LITERAL_DRIFT);
    let head_before = lab.source_git().head_sha().unwrap();

    let outcome = lab.engine.resolve_batch(vec![BatchItem {
        target: lab.target.clone(),
        decision: BatchDecision::KeepDisk,
    }]);

    let (_, committed, pushed, note) = done(&outcome, 0);
    assert!(committed && pushed, "the write-back is a source change");
    assert_eq!(note, None);
    let head_after = lab.source_git().head_sha().unwrap();
    assert_ne!(head_after, head_before);
    assert_eq!(lab.bare_git().head_sha().unwrap(), head_after, "pushed");
    assert!(lab.source_git().dirty_files().unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(lab.s.source.join("dot_testrc.tmpl")).unwrap(),
        "a=local\nvalue={{ .testvalue }}\n"
    );
    assert!(lab.rescan_and_settle().is_empty());
}

/// The rejected item in the LAST slot. Its finished session has the highest
/// id, and the journal orders by id — so "newest finished session" would be
/// the rejection, and Undo would "revert" a no-op while the banner names
/// the plain sibling that actually landed. Undo has to skip rejections.
#[test]
fn story_s4_undo_after_a_batch_skips_the_rejected_last_item_and_reverts_the_landed_one() {
    let lab = DriftLab::new();
    std::fs::write(lab.s.config_path(), "[data]\ntestvalue = \"tpl\"\n").unwrap();
    git(&lab.s.source, &["mv", "dot_testrc", "dot_testrc.tmpl"]);
    std::fs::write(lab.s.source.join("dot_testrc.tmpl"), TPL_SOURCE).unwrap();
    more_managed_files(&lab, &["a"]);
    assert_eq!(std::fs::read(&lab.target).unwrap(), TPL_RENDERED);

    std::fs::write(lab.s.home.join(".a"), b"a=disk\n").unwrap();
    std::fs::write(&lab.target, TPL_VALUE_DRIFT).unwrap();

    let outcome = lab.engine.resolve_batch(vec![
        batch_item(&lab, ".a", BatchDecision::KeepDisk),
        BatchItem {
            target: lab.target.clone(),
            decision: BatchDecision::KeepDisk,
        },
    ]);
    let (plain_session, committed, _, _) = done(&outcome, 0);
    assert!(committed);
    assert!(
        matches!(
            outcome.per_target[1].1,
            Ok(ResolveOutcome::ProtectedSpan { .. })
        ),
        "{:?}",
        outcome.per_target[1].1
    );

    // The rejection's session is the newest finished one by id…
    let journal = lab.journal();
    let (newest, _) = journal.last_finished_session().unwrap().unwrap();
    assert!(
        newest > plain_session,
        "premise: the rejection outranks by id"
    );

    // …and Undo still reverts the plain sibling, the last thing that landed.
    let undone = lab.engine.undo_last().unwrap();
    assert_eq!(undone, Some(plain_session));
    let (_, decisions) = journal.last_finished_session().unwrap().unwrap();
    assert_eq!(decisions[0]["action"], "undo");
    assert_eq!(decisions[0]["of"], plain_session);
}

/// A templated keep-disk the write-back must reject keeps its SLOT, reports
/// the precise refusal, and costs its plain sibling nothing: the sibling
/// still lands on the shared tail, in the batch's one commit.
#[test]
fn story_s2_a_rejected_templated_keep_disk_keeps_its_slot_and_its_sibling_still_lands() {
    let lab = DriftLab::new();
    std::fs::write(lab.s.config_path(), "[data]\ntestvalue = \"tpl\"\n").unwrap();
    git(&lab.s.source, &["mv", "dot_testrc", "dot_testrc.tmpl"]);
    std::fs::write(lab.s.source.join("dot_testrc.tmpl"), TPL_SOURCE).unwrap();
    more_managed_files(&lab, &["a"]);
    assert_eq!(std::fs::read(&lab.target).unwrap(), TPL_RENDERED);

    std::fs::write(&lab.target, TPL_VALUE_DRIFT).unwrap();
    std::fs::write(lab.s.home.join(".a"), b"a=disk\n").unwrap();
    let commits_before = commit_count(&lab.s.bare);

    let outcome = lab.engine.resolve_batch(vec![
        BatchItem {
            target: lab.target.clone(),
            decision: BatchDecision::KeepDisk,
        },
        batch_item(&lab, ".a", BatchDecision::KeepDisk),
    ]);

    assert_eq!(outcome.per_target.len(), 2);
    assert_eq!(
        outcome.per_target[0].0, lab.target,
        "the refused item keeps slot 0"
    );
    let Ok(ResolveOutcome::ProtectedSpan { detail }) = &outcome.per_target[0].1 else {
        panic!("expected ProtectedSpan, got {:?}", outcome.per_target[0].1);
    };
    assert!(detail.contains("protected"), "detail: {detail}");
    let (plain_session, committed, pushed, _) = done(&outcome, 1);
    assert!(committed && pushed);
    assert_eq!(commit_count(&lab.s.bare) - commits_before, 1);

    // The template and its drifted destination are untouched.
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc.tmpl")).unwrap(),
        TPL_SOURCE
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), TPL_VALUE_DRIFT);
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_a")).unwrap(),
        b"a=disk\n"
    );

    // Journal truth: the rejection closed its own session (attempt +
    // rejection). The sibling's session has the higher id here only because
    // it is the later slot — see `story_s4` for the other order, where Undo
    // has to look past the rejection.
    let journal = lab.journal();
    let (last, _) = journal.last_finished_session().unwrap().unwrap();
    assert_eq!(last, plain_session);
    let timeline = journal.timeline(50, None).unwrap();
    assert!(
        timeline.iter().any(|e| e.kind == "session_end"),
        "the rejected item's session was ended"
    );
}

/// A templated keep-disk is a decision like any other, so a batch of nothing
/// but templated files DOES catch up with origin first. When that catch-up
/// conflicts on the template itself, the item cannot be written from a source
/// full of conflict markers: it reports the merge editor as the way in, and
/// the merge it started stays open for that editor to conclude (spec §6.1),
/// exactly as a plain file's keep-disk does in `story_o`.
#[test]
fn story_t_an_all_templated_batch_catches_up_and_reports_a_conflicted_template_honestly() {
    const BASE_TPL: &[u8] = b"a={{ \"base\" }}\n";
    const ORIGIN_TPL: &[u8] = b"a={{ \"origin\" }}\n";
    const LOCAL_TPL: &[u8] = b"a={{ \"local\" }}\n";
    let lab = DriftLab::new();
    git(&lab.s.source, &["mv", "dot_testrc", "dot_testrc.tmpl"]);
    std::fs::write(lab.s.source.join("dot_testrc.tmpl"), BASE_TPL).unwrap();
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "templatize"]);
    git(&lab.s.source, &["push"]);
    diverge_with_conflicts(&lab, &[("dot_testrc.tmpl", ORIGIN_TPL, LOCAL_TPL)]);

    let git_c = lab.source_git();
    assert!(
        git_c.divergence("origin/main").unwrap().behind > 0,
        "premise: a merge is waiting to be started"
    );
    std::fs::write(&lab.target, DRIFTED).unwrap();

    let outcome = lab.engine.resolve_batch(vec![BatchItem {
        target: lab.target.clone(),
        decision: BatchDecision::KeepDisk,
    }]);

    let err = outcome.per_target[0]
        .1
        .as_ref()
        .expect_err("a source full of conflict markers cannot be written back")
        .to_string();
    assert!(
        err.contains("is mid-merge with origin") && err.contains("merge editor"),
        "{err}"
    );
    assert!(
        git_c.merge_in_progress(),
        "the decision started the merge; it is the editor's to conclude"
    );
    assert_eq!(
        std::fs::read(&lab.target).unwrap(),
        DRIFTED,
        "the destination is untouched"
    );
}

/// The two decision groups must be reported SEPARATELY, and the reason is
/// Undo.
///
/// `chezmoi apply` can fail on its own — a template that no longer renders is
/// enough — long after `chezmoi re-add` has already rewritten the keep-disk
/// sources. A single shared verdict called those files "not resolved", skipped
/// their `session_end`, and so put real writes beyond `undo_last`, which reads
/// the last FINISHED session. The next decision's catch-up would then sweep
/// them into a commit named "Update dotfiles (pre-merge)" with no way back.
#[test]
fn story_u_a_failed_apply_does_not_strand_the_keep_disk_files_that_landed() {
    let lab = DriftLab::new();
    // A second managed file, templated and valid, so the fixture can apply.
    std::fs::write(lab.s.source.join("dot_broken.tmpl"), b"b={{ \"ok\" }}\n").unwrap();
    more_managed_files(&lab, &["a"]);
    assert_eq!(
        std::fs::read(lab.s.home.join(".broken")).unwrap(),
        b"b=ok\n"
    );

    // Now break it, so `chezmoi apply` on it fails while `re-add` on the
    // OTHER file still succeeds (verified: re-add does not render siblings).
    std::fs::write(lab.s.source.join("dot_broken.tmpl"), b"b={{ }}\n").unwrap();
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "break the template"]);
    git(&lab.s.source, &["push"]);
    std::fs::write(lab.s.home.join(".a"), b"a=disk\n").unwrap();
    let commits_before = commit_count(&lab.s.bare);

    let outcome = lab.engine.resolve_batch(vec![
        batch_item(&lab, ".a", BatchDecision::KeepDisk),
        batch_item(&lab, ".broken", BatchDecision::KeepSourceLocal),
    ]);

    // The keep-source half failed, and says which call failed.
    let (_, result) = &outcome.per_target[1];
    let Err(e) = result else {
        panic!("the broken template cannot have applied, got {result:?}");
    };
    let msg = e.to_string();
    assert!(
        msg.contains("the batch apply failed"),
        "must name the call that failed, got {msg:?}"
    );

    // The keep-disk half LANDED, and says so.
    let (session_a, committed, pushed, note) = done(&outcome, 0);
    assert!(
        committed && pushed,
        "re-add succeeded, so this file was resolved and committed: note {note:?}"
    );
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_a")).unwrap(),
        b"a=disk\n",
        "its source really was rewritten — reporting failure would be a lie"
    );
    assert_eq!(commit_count(&lab.s.bare) - commits_before, 1);

    // And it is UNDOABLE: its session was finished, so `undo_last` can find
    // it. Before the groups were split this was `None` — the write existed
    // with no finished session pointing at it.
    assert_eq!(
        lab.engine.undo_last().unwrap(),
        Some(session_a),
        "the file that landed must be reachable by undo"
    );
}

/// Extra managed files for a batch story: `dot_<name>` = `MANAGED_BASE`,
/// committed, pushed, and applied — so `~/.<name>` exists and is in sync,
/// ready to be drifted. Applied in ONE pass on purpose: `apply(None)` is
/// repo-wide and `--force`, so applying between drifts would wash the earlier
/// ones straight back out.
fn more_managed_files(lab: &DriftLab, names: &[&str]) {
    for name in names {
        std::fs::write(lab.s.source.join(format!("dot_{name}")), MANAGED_BASE).unwrap();
    }
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "more managed files"]);
    git(&lab.s.source, &["push"]);
    lab.s.chezmoi().apply(None).unwrap();
}

fn batch_item(lab: &DriftLab, name: &str, decision: BatchDecision) -> BatchItem {
    BatchItem {
        target: lab.s.home.join(name),
        decision,
    }
}

/// The `Done` fields of one `per_target` entry: `(session, committed, pushed,
/// note)`, or a panic naming the file and what came back instead.
fn done(outcome: &BatchOutcome, ix: usize) -> (i64, bool, bool, Option<String>) {
    let (target, result) = &outcome.per_target[ix];
    match result {
        Ok(ResolveOutcome::Done {
            session,
            committed,
            pushed,
            note,
        }) => (*session, *committed, *pushed, note.clone()),
        other => panic!("{}: expected Done, got {other:?}", target.display()),
    }
}

/// Commits on the bare origin's `main`. Read with a plain subprocess, the way
/// `behind_count` does in the daemon suite — counting commits is an assertion
/// about the fixture, not a capability the production `GitClient` needs.
fn commit_count(repo: &std::path::Path) -> usize {
    let out = std::process::Command::new("git")
        .args(["rev-list", "--count", "main"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "rev-list in {}: {}",
        repo.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
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

/// A source file this machine has never applied — in the source repo,
/// absent on disk — is the plainest Keep source there is, and it used to be
/// refused: `keep_source` snapshotted the disk copy for undo first, found
/// nothing, and failed with "snapshot returned 0 blobs for 1 path"
/// (2026-10-14: every file of a 427-file skill folder). The decision now
/// journals `dest_blob: null`, and undo removes what `apply` created.
///
/// MUTATION: `snapshot_dest` treating NotFound as the old error → the
/// `keep_source` call fails. MUTATION: undo skipping `UndoRestore::Absent` →
/// the target survives the undo and the last assertion fails.
#[test]
fn story_v_keep_source_creates_a_file_missing_on_disk_and_undo_removes_it() {
    let lab = DriftLab::new();
    std::fs::write(lab.s.source.join("dot_newrc"), MANAGED_BASE).unwrap();
    git(&lab.s.source, &["add", "."]);
    git(
        &lab.s.source,
        &["commit", "-m", "a file never applied here"],
    );
    git(&lab.s.source, &["push"]);
    let target = lab.s.home.join(".newrc");
    assert!(!target.exists(), "test premise: not on disk");

    let outcome = lab.engine.keep_source(&target).unwrap();
    let ResolveOutcome::Done { session, .. } = outcome else {
        panic!("expected Done, got {outcome:?}");
    };
    assert_eq!(std::fs::read(&target).unwrap(), MANAGED_BASE);

    // Journal truth: the decision landed, with no disk content to go back to.
    let journal = lab.journal();
    let (id, decisions) = journal.last_finished_session().unwrap().unwrap();
    assert_eq!(id, session);
    let arr = decisions.as_array().unwrap();
    assert_eq!(arr[0]["action"], "keep_source");
    assert!(
        arr[0]["dest_blob"].is_null(),
        "absent on disk is journaled as null, not omitted: {:?}",
        arr[0]
    );
    lab.wait_scan_done();

    // Undo puts the disk back the way it was: without the file.
    assert_eq!(lab.engine.undo_last().unwrap(), Some(session));
    assert!(!target.exists(), "undo removes what apply created");
}

/// The batch path makes the same call: two never-applied files, one Keep
/// source batch, both created. MUTATION: `open_batch_session` snapshotting
/// `[target]` and demanding one blob → both items fail.
#[test]
fn story_w_a_keep_source_batch_creates_files_missing_on_disk() {
    let lab = DriftLab::new();
    for name in ["newa", "newb"] {
        std::fs::write(lab.s.source.join(format!("dot_{name}")), MANAGED_BASE).unwrap();
    }
    git(&lab.s.source, &["add", "."]);
    git(
        &lab.s.source,
        &["commit", "-m", "two files never applied here"],
    );
    git(&lab.s.source, &["push"]);

    let outcome = lab.engine.resolve_batch(vec![
        batch_item(&lab, ".newa", BatchDecision::KeepSourceLocal),
        batch_item(&lab, ".newb", BatchDecision::KeepSourceLocal),
    ]);
    for ix in 0..2 {
        let (_, committed, pushed, note) = done(&outcome, ix);
        assert!(!committed && !pushed, "apply never touches the source repo");
        assert_eq!(note, None);
    }
    assert_eq!(
        std::fs::read(lab.s.home.join(".newa")).unwrap(),
        MANAGED_BASE
    );
    assert_eq!(
        std::fs::read(lab.s.home.join(".newb")).unwrap(),
        MANAGED_BASE
    );
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

/// A templated keep-disk is not a refusal: the on-disk text is written back
/// into the template's literal segments and verified by re-render — the same
/// write-after-verify path the merge editor's Save and "keep origin" already
/// use — then committed and pushed like any other keep-disk. Before this it
/// answered a blanket "use the merge editor", which sent the user into the
/// editor to click "disk" on every region by hand.
#[test]
fn story_e_templated_keep_disk_writes_the_disk_text_back_and_lands() {
    let lab = DriftLab::new();
    templatize_and_drift(&lab, TPL_LITERAL_DRIFT);
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
    assert!(committed && pushed, "a template write is a source change");
    assert_eq!(note, None);
    let head_after = lab.source_git().head_sha().unwrap();
    assert_ne!(head_after, head_before, "one commit for the write");
    assert_eq!(
        lab.bare_git().head_sha().unwrap(),
        head_after,
        "and it reached origin"
    );

    // Filesystem truth: the literal line took the disk edit, the expression
    // survived verbatim, the destination still says what it said.
    assert_eq!(
        std::fs::read_to_string(lab.s.source.join("dot_testrc.tmpl")).unwrap(),
        "a=local\nvalue={{ .testvalue }}\n"
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), TPL_LITERAL_DRIFT);
    assert!(lab.source_git().dirty_files().unwrap().is_empty());

    // Journal truth: filed as keep_disk — not as a hand-made merge — with
    // both blobs, so undo restores the destination AND the pre-write template.
    let journal = lab.journal();
    let (id, decisions) = journal.last_finished_session().unwrap().unwrap();
    assert_eq!(id, session);
    let arr = decisions.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["action"], "keep_disk");
    assert_eq!(
        journal
            .get_blob(arr[0]["dest_blob"].as_str().unwrap())
            .unwrap()
            .unwrap(),
        TPL_LITERAL_DRIFT
    );
    assert_eq!(
        journal
            .get_blob(arr[0]["source_blob"].as_str().unwrap())
            .unwrap()
            .unwrap(),
        TPL_SOURCE
    );

    // Daemon truth: in sync after the rescan.
    lab.wait_scan_done();
    assert!(lab.settled_status().is_empty());
}

/// The one refusal that remains for a templated keep-disk is the precise one:
/// the disk edit changed the RENDERED VALUE of a template expression, which
/// no literal write-back can express. `ProtectedSpan` names it; nothing is
/// mutated; the attempt and the rejection are journaled.
#[test]
fn story_e2_templated_keep_disk_touching_a_value_is_rejected_and_touches_nothing() {
    let lab = DriftLab::new();
    templatize_and_drift(&lab, TPL_VALUE_DRIFT);
    let head_before = lab.source_git().head_sha().unwrap();

    let outcome = lab.engine.keep_disk(&lab.target).unwrap();
    let ResolveOutcome::ProtectedSpan { detail } = outcome else {
        panic!("expected ProtectedSpan, got {outcome:?}");
    };
    assert!(detail.contains("protected"), "detail: {detail}");

    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc.tmpl")).unwrap(),
        TPL_SOURCE
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), TPL_VALUE_DRIFT);
    assert!(lab.source_git().dirty_files().unwrap().is_empty());
    assert_eq!(lab.source_git().head_sha().unwrap(), head_before);

    let journal = lab.journal();
    let (_, decisions) = journal.last_finished_session().unwrap().unwrap();
    let arr = decisions.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["action"], "keep_disk");
    assert_eq!(arr[1]["action"], "keep_disk_rejected");
    assert_eq!(
        lab.probe().expect("drift must survive the rejection").class,
        DriftClass::DestinationDrift
    );

    // A rejection is not undoable state: nothing landed, so there is nothing
    // for Undo to find.
    assert_eq!(lab.engine.undo_last().unwrap(), None);
}

/// Keep-source never needed the merge editor for a templated file — `chezmoi
/// apply` renders templates — and this pins that it lands.
#[test]
fn story_e3_templated_keep_source_restores_the_render() {
    let lab = DriftLab::new();
    templatize_and_drift(&lab, TPL_LITERAL_DRIFT);

    let outcome = lab.engine.keep_source(&lab.target).unwrap();
    assert!(
        matches!(
            outcome,
            ResolveOutcome::Done {
                committed: false,
                pushed: false,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), TPL_RENDERED);
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc.tmpl")).unwrap(),
        TPL_SOURCE
    );
    lab.wait_scan_done();
    assert!(lab.settled_status().is_empty());
}

/// A queued decision that meets a scan holding the daemon's core waits it
/// out: the daemon bounces "daemon busy" after its wait, the engine retries
/// the bounce, and the decision lands once the core is free. Before this the
/// bounce was the row's failure — "daemon busy" on a file the user had just
/// decided, whenever a scan ran a few seconds long.
/// MUTATION: return the bounce instead of retrying (`request_patiently`
/// → `self.ipc.request`) → fails with "daemon busy".
#[test]
fn story_w_a_decision_waits_out_a_scan_holding_the_core() {
    let lab = DriftLab::with_daemon(
        Duration::from_millis(200),
        tomte_app::resolve::DAEMON_PATIENCE,
    );
    lab.drift_dest();

    // A "scan" that holds the core for two seconds — ten bounces' worth.
    // The holder reports once it OWNS the lock (the daemon's own scans
    // contend for it too), so the decision below always starts against a
    // held core rather than racing the holder for it.
    let (hold, locked) = hold_the_core(&lab, Duration::from_secs(2));
    locked.recv().unwrap();
    let started = Instant::now();
    let outcome = lab.engine.keep_disk(&lab.target).unwrap();
    hold.join().unwrap();

    assert!(
        matches!(outcome, ResolveOutcome::Done { .. }),
        "landed once the core was free, got {outcome:?}"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(1500),
        "it really did wait for the lock: {:?}",
        started.elapsed()
    );
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        DRIFTED
    );
    let journal = lab.journal();
    let (_, decisions) = journal.last_finished_session().unwrap().unwrap();
    assert_eq!(
        decisions.as_array().unwrap().len(),
        1,
        "exactly one session, one decision"
    );
}

/// …and only a daemon that stays busy past the engine's patience becomes the
/// row's failure, in those words, with nothing journaled or touched.
/// MUTATION: never give up (drop the `waited >= patience` return) → hangs
/// past the 3s hold and lands instead of failing.
#[test]
fn story_w2_a_daemon_busy_past_the_patience_budget_is_the_rows_failure() {
    let lab = DriftLab::with_daemon(Duration::from_millis(100), Duration::from_millis(700));
    lab.drift_dest();

    let (hold, locked) = hold_the_core(&lab, Duration::from_secs(3));
    locked.recv().unwrap();
    let started = Instant::now();
    let err = lab
        .engine
        .keep_disk(&lab.target)
        .expect_err("the daemon stayed busy past the budget")
        .to_string();
    let waited = started.elapsed();
    hold.join().unwrap();

    assert!(
        err.contains("stayed busy") && err.contains("daemon busy"),
        "{err}"
    );
    assert!(
        waited >= Duration::from_millis(700) && waited < Duration::from_secs(3),
        "gave up after the budget, before the hold ended: {waited:?}"
    );
    // Nothing happened: the bounce executes nothing, and the engine never
    // got as far as opening a session.
    assert_eq!(std::fs::read(&lab.target).unwrap(), DRIFTED);
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        b"a=1\n"
    );
    let journal = lab.journal();
    assert!(journal.last_finished_session().unwrap().is_none());
    assert!(
        journal
            .timeline(50, None)
            .unwrap()
            .iter()
            .all(|e| e.kind != "session_start"),
        "no session was opened"
    );
}

/// A daemon that stays busy past the budget for one file is not asked again
/// for the next: the batch reports the rest as not attempted, at once, rather
/// than holding the queue for one budget per file.
/// MUTATION: drop the `stuck` short-circuit in Phase 1 → the second slot
/// reports "stayed busy" after its own budget instead of "not attempted".
#[test]
fn story_w3_a_stuck_daemon_costs_a_batch_one_budget_not_one_per_file() {
    let lab = DriftLab::with_daemon(Duration::from_millis(100), Duration::from_millis(700));
    more_managed_files(&lab, &["a"]);
    std::fs::write(&lab.target, DRIFTED).unwrap();
    std::fs::write(lab.s.home.join(".a"), b"a=disk\n").unwrap();

    let (hold, locked) = hold_the_core(&lab, Duration::from_secs(4));
    locked.recv().unwrap();
    let outcome = lab.engine.resolve_batch(vec![
        BatchItem {
            target: lab.target.clone(),
            decision: BatchDecision::KeepDisk,
        },
        batch_item(&lab, ".a", BatchDecision::KeepDisk),
    ]);
    hold.join().unwrap();

    let first = outcome.per_target[0].1.as_ref().unwrap_err().to_string();
    let second = outcome.per_target[1].1.as_ref().unwrap_err().to_string();
    assert!(first.contains("stayed busy"), "{first}");
    assert!(
        second.starts_with("not attempted") && second.contains("stayed busy"),
        "{second}"
    );
    // Nothing happened to either file.
    assert_eq!(std::fs::read(&lab.target).unwrap(), DRIFTED);
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_a")).unwrap(),
        MANAGED_BASE
    );
    assert!(lab.journal().last_finished_session().unwrap().is_none());
}

/// Hold the daemon's core for `hold` on another thread — a scan, from every
/// client's point of view. The returned receiver fires once the lock is
/// actually owned; callers wait on it, because the daemon's own scans
/// contend for the same lock and a sleep is not a handshake.
fn hold_the_core(lab: &DriftLab, hold: Duration) -> (std::thread::JoinHandle<()>, Receiver<()>) {
    let core = lab.core.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let _scan = core.lock().unwrap();
        tx.send(()).unwrap();
        std::thread::sleep(hold);
    });
    (handle, rx)
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
    templatize_with_and_drift(lab, TPL_SOURCE, TPL_RENDERED, drift);
}

/// `templatize_and_drift` for any template text and its rendering with
/// `testvalue = "tpl"`.
fn templatize_with_and_drift(lab: &DriftLab, template: &[u8], rendered: &[u8], drift: &[u8]) {
    // The scratch config exists (empty) since Scratch::new — give the
    // template its variable, template_roundtrip's scratch_chezmoi pattern.
    std::fs::write(lab.s.config_path(), "[data]\ntestvalue = \"tpl\"\n").unwrap();
    git(&lab.s.source, &["mv", "dot_testrc", "dot_testrc.tmpl"]);
    std::fs::write(lab.s.source.join("dot_testrc.tmpl"), template).unwrap();
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "templatize"]);
    lab.s.chezmoi().apply(None).unwrap();
    assert_eq!(std::fs::read(&lab.target).unwrap(), rendered);

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
    // …and it is not undoable state: Undo looks past it and finds nothing.
    assert_eq!(lab.engine.undo_last().unwrap(), None);

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

/// Story X/Y fixtures: a JSON managed file.
const JSON_BASE: &[u8] = b"{\n  \"theme\": \"one\",\n  \"tabs\": 2\n}\n";
const JSON_SOURCE: &[u8] = b"{\n  \"theme\": \"two\",\n  \"tabs\": 2\n}\n";
const JSON_DISK: &[u8] = b"{\n  \"theme\": \"one\",\n  \"tabs\": 2,\n  \"vim\": true\n}\n";
const JSON_MERGED: &[u8] = b"{\n  \"theme\": \"two\",\n  \"tabs\": 2,\n  \"vim\": true\n}\n";
/// Story Y: a JSON template with one expression, its rendering, the drifted
/// disk (tabs AND the rendered name edited) and the expected template/disk.
const JSON_TPL: &[u8] = b"{\n  \"name\": \"{{ .testvalue }}\",\n  \"tabs\": 2\n}\n";
const JSON_TPL_RENDERED: &[u8] = b"{\n  \"name\": \"tpl\",\n  \"tabs\": 2\n}\n";
const JSON_TPL_DRIFT: &[u8] = b"{\n  \"name\": \"X\",\n  \"tabs\": 4\n}\n";
const JSON_TPL_SAVED: &[u8] = b"{\n  \"name\": \"{{ .testvalue }}\",\n  \"tabs\": 4\n}\n";
const JSON_TPL_DISK_SAVED: &[u8] = b"{\n  \"name\": \"tpl\",\n  \"tabs\": 4\n}\n";

/// Replace the managed file's content in the source, commit, apply and let
/// the daemon settle, so source, disk and last-written all equal `content`.
fn set_managed_content(lab: &DriftLab, content: &[u8]) {
    std::fs::write(lab.s.source.join("dot_testrc"), content).unwrap();
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "set managed content"]);
    lab.s.chezmoi().apply(None).unwrap();
    assert_eq!(std::fs::read(&lab.target).unwrap(), content);
    assert!(lab.rescan_and_settle().is_empty());
}

/// Compute the structured document for the lab's current merge inputs.
fn structured_state(
    inputs: &merge_inputs::MergeInputs,
) -> tomte_app::structured_state::StructuredState {
    let doc = StructuredDocument::compute(
        inputs.base.as_deref(),
        &inputs.ours,
        &inputs.theirs,
        inputs.span_map.as_ref(),
    )
    .expect("JSON inputs must be structured");
    tomte_app::structured_state::StructuredState::new(doc)
}

/// Story X: a structured save lands BOTH a disk-added key and a source-side
/// change, through the real engine, and Undo restores the disk.
///
/// Mutation: the assemble step drops the Added splice
/// (`crates/core/src/structured/splice.rs`) → the assembled text lacks "vim"
/// and the first assertion on `resolved` fails.
#[test]
fn story_x_a_structured_save_lands_a_disk_added_key_and_a_source_change() {
    let lab = DriftLab::new();
    set_managed_content(&lab, JSON_BASE);

    // Source changes (committed); the daemon sees it first, snapshotting the
    // last-written disk as the 3-way base.
    std::fs::write(lab.s.source.join("dot_testrc"), JSON_SOURCE).unwrap();
    git(&lab.s.source, &["add", "."]);
    git(&lab.s.source, &["commit", "-m", "source edit"]);
    let drifted = lab.rescan_and_settle();
    assert_eq!(drifted[0].class, "source_ahead");
    // Then the disk adds a key.
    std::fs::write(&lab.target, JSON_DISK).unwrap();
    let drifted = lab.rescan_and_settle();
    assert_eq!(drifted[0].class, "conflict");

    let inputs = lab.load_merge_inputs();
    let state = structured_state(&inputs);
    let decisions: Vec<_> = state
        .doc
        .decisions
        .iter()
        .map(|d| (d.path.as_str(), d.kind))
        .collect();
    assert_eq!(
        decisions,
        vec![
            ("theme", RegionKind::TheirsOnly),
            ("vim", RegionKind::OursOnly)
        ]
    );
    let resolved = state.assembled().unwrap().unwrap();
    assert_eq!(resolved.as_bytes(), JSON_MERGED);

    let outcome = lab.engine.resolve_merged(&inputs, &resolved).unwrap();
    let ResolveOutcome::Done {
        committed, pushed, ..
    } = outcome
    else {
        panic!("expected Done, got {outcome:?}");
    };
    assert!(committed && pushed);
    assert_eq!(std::fs::read(&lab.target).unwrap(), JSON_MERGED);
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc")).unwrap(),
        JSON_MERGED
    );

    lab.engine
        .undo_last()
        .unwrap()
        .expect("the merge is undoable");
    assert_eq!(std::fs::read(&lab.target).unwrap(), JSON_DISK);
}

/// Story Y: a structured save on a templated JSON source locks the key the
/// template pins ("name") and writes the literal key ("tabs") back into the
/// template, leaving the expression intact.
///
/// Mutation: skip the lock pass (`crates/core/src/structured/merge.rs`) →
/// "name" takes the auto Disk pick, so the story fails at the lock assertion
/// and, with that removed, at the assembled-text assertion (`X` where the
/// source has `tpl`); `resolve_merged` is never reached. The engine's own
/// refusal path (`write_back` → `ProtectedSpanTouched` →
/// `ResolveOutcome::ProtectedSpan`) is covered by story H.
#[test]
fn story_y_a_structured_save_on_a_templated_json_source_writes_back_the_literal_key() {
    let lab = DriftLab::new();
    templatize_with_and_drift(&lab, JSON_TPL, JSON_TPL_RENDERED, JSON_TPL_DRIFT);

    let inputs = lab.load_merge_inputs();
    assert!(inputs.templated);
    assert!(inputs.span_map.is_some());
    let mut state = structured_state(&inputs);
    let locked_of = |state: &tomte_app::structured_state::StructuredState, path: &str| {
        state
            .doc
            .decisions
            .iter()
            .find(|d| d.path == path)
            .unwrap_or_else(|| panic!("no decision for {path}"))
            .locked
            .is_some()
    };
    assert!(locked_of(&state, "name"), "the template pins name");
    assert!(!locked_of(&state, "tabs"), "tabs is a literal");
    let tabs = state
        .doc
        .decisions
        .iter()
        .find(|d| d.path == "tabs")
        .unwrap()
        .id;
    // A no-op in this 3-way (OursOnly auto-resolves to Disk); kept to read
    // as the user's action.
    state.pick(tabs, Pick::Disk);
    let resolved = state.assembled().unwrap().unwrap();
    assert_eq!(resolved.as_bytes(), JSON_TPL_DISK_SAVED);

    let outcome = lab.engine.resolve_merged(&inputs, &resolved).unwrap();
    let ResolveOutcome::Done { .. } = outcome else {
        panic!("expected Done, got {outcome:?}");
    };
    assert_eq!(
        std::fs::read(lab.s.source.join("dot_testrc.tmpl")).unwrap(),
        JSON_TPL_SAVED
    );
    assert_eq!(std::fs::read(&lab.target).unwrap(), JSON_TPL_DISK_SAVED);
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

    lab.engine.abort_repo_reconcile().unwrap();

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

    // The dashboard's quick action enqueues a `Decision::KeepDisk`
    // (`dashboard.rs`'s `quick_action_button`); this is the engine call the
    // batch that takes it ends up making here, where the repo is diverged
    // and the shared tail falls back to the single-file paths.
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

/// `abort_repo_reconcile` answers honestly in both directions, and the
/// idempotent half is not a nicety: Cancel is reachable with no merge running
/// (the editor open on a file the merge already concluded, a second Discard
/// landing behind the first), and `git merge --abort` answers that with a
/// hard error. Reporting that as a failed roll-back would raise a red banner
/// about a merge that is simply gone.
///
/// The merging half is the one that used to be unprovable at all: before
/// Task 6 `merge_abort` discarded its own result, so this function returned
/// `()` and could not distinguish "rolled back" from "did not".
///
/// MUTATION: drop the `merge_in_progress()` guard → the first call returns
/// `Err` ("There is no merge to abort") and the assertion fails. MUTATION:
/// `let _ = self.git.merge_abort()` (the pre-Task-6 shape) → the compile
/// fails, which is the point of returning the result.
#[test]
fn aborting_is_idempotent_when_there_is_nothing_to_abort_and_real_when_there_is() {
    let lab = DriftLab::new();
    let git_c = lab.source_git();

    // Nothing to abort: the repo is ALREADY where the caller wants it.
    assert!(!git_c.merge_in_progress(), "no merge to start with");
    assert!(
        lab.engine.abort_repo_reconcile().is_ok(),
        "a repo with no merge is already rolled back; saying otherwise would \
         put a red banner over a merge that does not exist"
    );
    let head_before = git_c.head_sha().unwrap();

    // And with a real merge open, it really rolls it back.
    diverge_on_the_managed_file(&lab);
    assert!(
        lab.engine.ensure_merge_started().unwrap(),
        "the fixture must actually enter a merge"
    );
    assert!(git_c.merge_in_progress());

    lab.engine
        .abort_repo_reconcile()
        .expect("an abort of a real merge succeeds");
    assert!(
        !git_c.merge_in_progress(),
        "and the merge is really gone, not merely reported gone"
    );
    assert_ne!(
        git_c.head_sha().unwrap(),
        head_before,
        "sanity: the fixture moved HEAD, so `merge_in_progress` is not just \
         answering about an untouched repo"
    );
}
