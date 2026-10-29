//! DaemonCore behavior against a real scratch chezmoi home.

use tomte_core::chezmoi::ChezmoiClient;
use tomte_core::cmd::SystemRunner;
use tomte_core::git::GitClient;
use tomte_core::scanner::ScanError;
use tomte_core::testsupport::{Scratch, git, sh};
use tomte_daemon::core::DaemonCore;
use tomte_journal::Journal;
use tomte_proto::Event;

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
    core.handle_paths_changed(std::slice::from_ref(&target), 100)
        .unwrap();

    let ks = kinds(&core, &target);
    assert!(ks.contains(&"dest_changed".to_string()), "{ks:?}");
    match rx.try_recv().unwrap() {
        Event::Drift {
            target: t,
            class,
            ts,
            ..
        } => {
            assert_eq!(t, target);
            assert_eq!(class, "destination_drift");
            assert_eq!(ts, 100);
        }
        other => panic!("expected Drift, got {other:?}"),
    }
    // dedup: same state again → no new event
    core.handle_paths_changed(std::slice::from_ref(&target), 101)
        .unwrap();
    assert_eq!(kinds(&core, &target).len(), ks.len());
    // blob snapshot exists for the new content
    let h = tomte_core::drift::ContentHash::of(b"a=2\n").to_hex();
    assert!(core.journal().has_blob(&h).unwrap());
}

#[test]
fn expected_change_journals_applied_without_push() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let rx = core.subscribe();
    let target = s.home.join(".testrc");
    core.expect_changes(std::slice::from_ref(&target), 60, 100);
    std::fs::write(&target, "a=applied\n").unwrap();
    core.handle_paths_changed(std::slice::from_ref(&target), 110)
        .unwrap();

    let ks = kinds(&core, &target);
    assert_eq!(ks, vec!["applied".to_string()]);
    assert!(rx.try_recv().is_err(), "no Drift push for expected changes");

    // TTL expiry: same path changed after deadline is foreign again
    std::fs::write(&target, "a=foreign\n").unwrap();
    core.handle_paths_changed(std::slice::from_ref(&target), 200)
        .unwrap();
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
    assert!(matches!(
        rx.try_recv().unwrap(),
        Event::LeftManagement { .. }
    ));
}

#[test]
fn fetch_integrates_when_fast_forward_is_lossless() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let rx = core.subscribe();
    // second machine pushes
    let other = s.root.path().join("other");
    sh(
        s.root.path(),
        "git",
        &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    std::fs::write(other.join("dot_testrc"), "a=remote\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "remote"]);
    git(&other, &["push"]);

    core.handle_fetch(400).unwrap();
    // New contract (2026-08-10): a clean, non-diverged repo INTEGRATES what
    // fetch brought — fast-forward, then the file reads as source-ahead
    // (apply-able) instead of remote_ahead purgatory.
    let target = s.home.join(".testrc");
    assert!(kinds(&core, &target).contains(&"source_changed".to_string()));
    let behind_after = std::process::Command::new("git")
        .args(["rev-list", "--count", "HEAD..origin/main"])
        .current_dir(&s.source)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&behind_after.stdout).trim(), "0");
    let events: Vec<Event> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Drift { class, .. } if class == "source_ahead")),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::FetchDone { behind: 1, .. }))
    );
}

#[test]
fn fetch_leaves_diverged_repos_untouched() {
    // Both sides touch the SAME file with different content — a genuine
    // conflict. (A divergence on DIFFERENT files is now auto-joined by a
    // clean virtual merge; see `fetch_auto_joins_a_clean_divergence` below.
    // This test guards the case that must still be left for review.)
    let s = Scratch::new();
    let mut core = core_for(&s);
    // second machine pushes…
    let other = s.root.path().join("other");
    sh(
        s.root.path(),
        "git",
        &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    std::fs::write(other.join("dot_testrc"), "a=remote\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "remote"]);
    git(&other, &["push"]);
    // …while the local source has its own unpushed, conflicting commit.
    std::fs::write(s.source.join("dot_testrc"), "a=local\n").unwrap();
    git(&s.source, &["add", "."]);
    git(&s.source, &["commit", "-m", "local"]);

    core.handle_fetch(400).unwrap();
    let target = s.home.join(".testrc");
    assert!(kinds(&core, &target).contains(&"source_changed".to_string()));
    let behind_after = std::process::Command::new("git")
        .args(["rev-list", "--count", "HEAD..origin/main"])
        .current_dir(&s.source)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&behind_after.stdout).trim(),
        "1",
        "conflicting diverged repo must NOT be integrated"
    );
    assert!(
        !GitClient::new(std::sync::Arc::new(SystemRunner), s.source.clone()).merge_in_progress(),
        "a probed-but-not-attempted merge must not leave the repo mid-merge"
    );
}

#[test]
fn fetch_auto_joins_a_clean_divergence() {
    // Both sides moved, on different files: the virtual merge proves it
    // clean, so fetch joins the histories itself instead of leaving a
    // divergence for the user to reconcile by hand.
    let s = Scratch::new();
    let mut core = core_for(&s);
    let rx = core.subscribe();
    let other = s.root.path().join("other");
    sh(
        s.root.path(),
        "git",
        &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    std::fs::write(other.join("remote-only"), "x\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "remote"]);
    git(&other, &["push"]);
    // …while we commit a DIFFERENT file locally.
    std::fs::write(s.source.join("local-only"), "y\n").unwrap();
    git(&s.source, &["add", "."]);
    git(&s.source, &["commit", "-m", "local"]);

    core.handle_fetch(400).unwrap();
    let behind_after = std::process::Command::new("git")
        .args(["rev-list", "--count", "HEAD..origin/main"])
        .current_dir(&s.source)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&behind_after.stdout).trim(),
        "0",
        "clean divergence must be joined"
    );
    assert!(
        !GitClient::new(std::sync::Arc::new(SystemRunner), s.source.clone()).merge_in_progress()
    );
    let events: Vec<Event> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::FetchDone { behind: 1, .. }))
    );
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
    core.handle_paths_changed(std::slice::from_ref(&target), 600)
        .unwrap();
    assert!(kinds(&core, &target).is_empty());
}

/// The 2026-10-11 loop: the source repo's `git status` wrote `index.lock`
/// inside a vendored skill's own `.git`, the watcher reported it, and the
/// daemon heard a source change — forever. The nested path must not start a
/// source-side rescan; the real source path still does.
///
/// MUTATION: classify inline with the 0.1.20 rule (only
/// `rel.components().next() == ".git"` is looked at) → the nested
/// `index.lock` reads as a source change, `rescan_source_side` finds the
/// dirty `dot_testrc`, and the first assertion sees a Drift push.
#[test]
fn a_nested_repos_index_lock_is_not_a_source_change() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let rx = core.subscribe();
    // Dirty the source behind the core's back: a source-side rescan WOULD
    // find this (source_edit_is_source_ahead in the scanner tests).
    std::fs::write(s.source.join("dot_testrc"), "a=edited\n").unwrap();

    let nested = s.source.join("dot_claude/skills/humanizer/.git/index.lock");
    core.handle_paths_changed(std::slice::from_ref(&nested), 100)
        .unwrap();
    assert!(
        rx.try_recv().is_err(),
        "a nested .git path must not trigger a source-side rescan"
    );

    core.handle_paths_changed(&[s.source.join("dot_testrc")], 101)
        .unwrap();
    assert!(
        matches!(rx.try_recv().unwrap(), Event::Drift { .. }),
        "the real source path does"
    );
}

// ---------------------------------------------------------------------------
// The unattended auto-join and the commit signer (final review, Critical).
// ---------------------------------------------------------------------------

/// Stage a clean divergence: origin and this machine each moved, on DIFFERENT
/// files, so `merge-tree` proves the join clean and the auto-join is offered.
fn stage_a_clean_divergence(s: &Scratch) {
    let other = s.root.path().join("other");
    sh(
        s.root.path(),
        "git",
        &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    std::fs::write(other.join("remote-only"), "x\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "remote"]);
    git(&other, &["push"]);
    std::fs::write(s.source.join("local-only"), "y\n").unwrap();
    git(&s.source, &["add", "."]);
    git(&s.source, &["commit", "-m", "local"]);
}

fn behind_count(repo: &std::path::Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-list", "--count", "HEAD..origin/main"])
        .current_dir(repo)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Signing ON with a signer that cannot sign — a locked 1Password agent,
/// reproduced without 1Password. `Scratch::new` writes repo-local
/// `commit.gpgsign=false`, so this path only exists if a test turns signing
/// back ON deliberately; that is the whole point of this fixture.
fn unsignable(repo: &std::path::Path) {
    git(repo, &["config", "commit.gpgsign", "true"]);
    git(repo, &["config", "gpg.format", "ssh"]);
    git(repo, &["config", "gpg.ssh.program", "/bin/false"]);
    git(repo, &["config", "user.signingkey", "ssh-ed25519 AAAAfake"]);
}

/// Signing ON with a real key generated for the test: the `gpg.format = ssh`
/// shape a 1Password setup uses, minus the agent that can be locked.
fn signable_for_real(repo: &std::path::Path) {
    let key = repo.join("../signing-key");
    sh(
        repo,
        "ssh-keygen",
        &["-q", "-t", "ed25519", "-N", "", "-f", key.to_str().unwrap()],
    );
    git(repo, &["config", "commit.gpgsign", "true"]);
    git(repo, &["config", "gpg.format", "ssh"]);
    git(repo, &["config", "user.signingkey", key.to_str().unwrap()]);
    // This machine's GLOBAL config points the signer at 1Password's helper,
    // which rejects a key it does not hold — pin it so the test does not
    // depend on whose laptop runs it.
    git(repo, &["config", "gpg.ssh.program", "ssh-keygen"]);

    // Git cannot VERIFY an ssh signature without an allowed-signers file, and
    // reports `%G?` as 'N' when it cannot verify — the same letter it uses for
    // "not signed at all". Without this the test read a perfectly signed
    // commit as unsigned on any machine whose global config lacks the file,
    // which is every CI runner (2026-09-01: this reached a release tag).
    // Naming the key we just generated makes the check hermetic AND stronger:
    // 'G' proves the commit is signed by exactly this key, not merely signed.
    let allowed = repo.join("../allowed-signers");
    let pubkey = std::fs::read_to_string(format!("{}.pub", key.display()))
        .expect("ssh-keygen wrote a public key");
    std::fs::write(&allowed, format!("t@t {pubkey}")).expect("write allowed-signers");
    git(
        repo,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            allowed.to_str().unwrap(),
        ],
    );
}

/// The daemon's auto-join is the one commit-creating operation Tomte performs
/// with nobody watching, on the fetch timer, holding the core lock. When the
/// commit signer cannot sign, `git merge` gets as far as staging everything
/// and then dies on the commit — leaving MERGE_HEAD set with NOTHING
/// conflicted (verified against git 2.50.1). That is the stranded-merge shape,
/// created on Tomte's own initiative, and on a locked 1Password it arrives
/// only after the agent's GUI prompt times out — during which every non-Status
/// IPC bounces as "daemon busy".
///
/// So the signer is probed BEFORE the join: no signer, no join, and the repo
/// is left exactly where it was for the next fetch to try again.
#[test]
fn an_unsignable_signer_skips_the_auto_join_instead_of_stranding_the_repo() {
    let s = Scratch::new();
    stage_a_clean_divergence(&s);
    unsignable(&s.source);
    let head_before = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&s.source)
        .output()
        .unwrap()
        .stdout;

    let mut core = core_for(&s);
    let rx = core.subscribe();
    let t0 = std::time::Instant::now();
    core.handle_fetch(400).unwrap();
    let elapsed = t0.elapsed();

    assert!(
        !GitClient::new(std::sync::Arc::new(SystemRunner), s.source.clone()).merge_in_progress(),
        "an unsignable join must never be STARTED — Tomte does not leave the \
         repo mid-merge on its own initiative"
    );
    assert_eq!(
        behind_count(&s.source),
        "1",
        "the divergence is left intact for the next fetch"
    );
    assert_eq!(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&s.source)
            .output()
            .unwrap()
            .stdout,
        head_before,
        "HEAD must not have moved"
    );
    // The fetch itself still succeeded and still reports what it found — a
    // skipped join is not a failed fetch.
    let events: Vec<Event> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::FetchDone { behind: 1, .. })),
        "{events:?}"
    );
    // The whole point is that this does not sit on the core lock: the IPC
    // server bounces non-Status calls as "daemon busy" after 20s.
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "the fetch must not stall the daemon: took {elapsed:?}"
    );
}

/// The pre-flight is a gate, not a ban: with signing on and a signer that
/// actually answers, the auto-join goes through exactly as before — and the
/// commit it makes is signed.
#[test]
fn a_live_signer_lets_the_auto_join_through_and_signs_it() {
    let s = Scratch::new();
    stage_a_clean_divergence(&s);
    signable_for_real(&s.source);

    let mut core = core_for(&s);
    core.handle_fetch(400).unwrap();

    assert_eq!(
        behind_count(&s.source),
        "0",
        "a clean divergence with a live signer must still be joined"
    );
    assert!(
        !GitClient::new(std::sync::Arc::new(SystemRunner), s.source.clone()).merge_in_progress()
    );
    let sig = std::process::Command::new("git")
        .args(["log", "-1", "--format=%G?"])
        .current_dir(&s.source)
        .output()
        .unwrap();
    // 'G' — a good signature from the key the fixture generated. Anything
    // else fails: 'N' is unsigned (or unverifiable), 'B' is a bad signature,
    // 'U'/'E' mean git could not tie it to the expected key. The fixture
    // configures allowed-signers precisely so this can be an equality check
    // rather than "not the worst letter".
    assert_eq!(
        String::from_utf8_lossy(&sig.stdout).trim(),
        "G",
        "the auto-join's merge commit must carry a signature from the \
         fixture's own key"
    );
}

/// A repo that does not sign its commits is never probed — there is nothing
/// that could block, and an unnecessary `commit-tree` per fetch would be pure
/// cost. (`Scratch::new` leaves signing off, so this is the default shape.)
#[test]
fn an_unsigned_repo_auto_joins_without_a_signer_probe() {
    let s = Scratch::new();
    stage_a_clean_divergence(&s);
    let mut core = core_for(&s);
    core.handle_fetch(400).unwrap();
    assert_eq!(behind_count(&s.source), "0");
}

/// The pre-flight narrows the window; it cannot close it. A signer that
/// answers the probe and then dies on the real commit — or any other reason
/// the merge commit fails after `git merge` has already staged everything —
/// leaves MERGE_HEAD set with NOTHING conflicted. Reproduced here with a
/// failing `commit-msg` hook, which is that exact shape and needs no signer
/// at all (verified against git 2.50.1).
///
/// No user decision is inside that merge, so the daemon backs it out: Tomte
/// does not leave the repo mid-merge on its own initiative, and "we probed
/// first" is not a licence to leave one behind when the probe was wrong.
#[test]
fn a_join_that_fails_after_starting_is_backed_out_not_left_mid_merge() {
    let s = Scratch::new();
    stage_a_clean_divergence(&s);
    let hooks = s.source.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("commit-msg");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    sh(&s.source, "chmod", &["+x", hook.to_str().unwrap()]);

    let mut core = core_for(&s);
    core.handle_fetch(400).unwrap();

    let git_c = GitClient::new(std::sync::Arc::new(SystemRunner), s.source.clone());
    assert!(
        !git_c.merge_in_progress(),
        "a join that died mid-flight must be aborted, not left for the user to find"
    );
    assert!(
        git_c.staged_files().unwrap().is_empty(),
        "the abort must also unstage what the failed merge staged"
    );
    assert_eq!(
        behind_count(&s.source),
        "1",
        "and the divergence is back to where it was"
    );
}

/// A scan now runs OFF the lock (spec 2026-10-11 §2.2), so the app can
/// resolve a file while the scan is still looking at it. The report then
/// says "drifted" about a file the daemon already saw resolved; the
/// daemon's own, fresher knowledge wins.
///
/// MUTATION: drop the touched-set note from `record_expected_apply` → the
/// stale report re-adds `.testrc` to `drift_state`, and the status lists
/// a file the user just resolved.
#[test]
fn a_target_the_app_resolved_during_a_scan_stays_resolved() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let target = s.home.join(".testrc");
    let source_text = std::fs::read(s.source.join("dot_testrc")).unwrap();
    std::fs::write(&target, "a=drift\n").unwrap();

    let (scanner, ticket) = core.begin_scan(100).expect("no scan in progress");
    let report = scanner.scan();
    assert_eq!(
        report.as_ref().unwrap().drifted.len(),
        1,
        "the scan saw the drift"
    );

    // While it ran: the app announced and applied the resolution.
    core.expect_changes(std::slice::from_ref(&target), 60, 101);
    std::fs::write(&target, &source_text).unwrap();
    core.handle_paths_changed(std::slice::from_ref(&target), 102)
        .unwrap();

    let outcome = core.apply_scan(ticket, report, 103).unwrap();
    assert_eq!(
        outcome.drifted, 0,
        "the stale verdict lost to the fresher apply"
    );
    assert!(core.status_snapshot().0.is_empty());
    assert!(!core.scan_in_progress());
}

/// The mirror image: the file drifted AFTER the scan looked at it. The
/// report says clean; the daemon already journaled the drift.
///
/// MUTATION: apply the stale removals to touched targets too (drop the
/// `touched.contains` guard on the stale loop) → `.testrc` vanishes from
/// the status although it is drifted right now.
#[test]
fn a_target_that_drifted_during_a_scan_stays_drifted() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let target = s.home.join(".testrc");

    let (scanner, ticket) = core.begin_scan(100).unwrap();
    let report = scanner.scan();
    assert!(
        report.as_ref().unwrap().drifted.is_empty(),
        "clean when the scan looked"
    );

    std::fs::write(&target, "a=later\n").unwrap();
    core.handle_paths_changed(std::slice::from_ref(&target), 101)
        .unwrap();

    let outcome = core.apply_scan(ticket, report, 102).unwrap();
    assert_eq!(outcome.drifted, 1);
    assert_eq!(core.status_snapshot().0[0].class, "destination_drift");
}

/// One scan at a time, and a request made during one is not lost: the
/// caller is told to go again.
///
/// MUTATION: never set `rescan_wanted` in `begin_scan` → `None` still
/// comes back, but `apply_scan` reports `rescan_wanted: false` and a
/// Rescan pressed during a scan silently vanishes.
#[test]
fn a_scan_requested_during_a_scan_runs_after_it() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let (scanner, ticket) = core.begin_scan(100).unwrap();
    assert!(core.scan_in_progress());
    assert!(core.begin_scan(101).is_none(), "one scan at a time");
    let outcome = core.apply_scan(ticket, scanner.scan(), 102).unwrap();
    assert!(outcome.rescan_wanted);
    assert!(!core.scan_in_progress());
    assert!(core.begin_scan(103).is_some(), "and the next one may start");
}

/// MUTATION: clear `scan_in_progress` only on the `Ok` path → a failed scan
/// leaves `begin_scan` returning `None` forever: no scan ever runs again.
///
/// MUTATION: take `rescan_wanted` before `report?` (the original shape) → the
/// request asked during the failing scan is lost and this reads false.
#[test]
fn a_failed_scan_releases_the_in_progress_flag() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let (_scanner, ticket) = core.begin_scan(100).unwrap();
    assert!(core.begin_scan(100).is_none(), "asked during the scan");
    let err = core.apply_scan(
        ticket,
        Err(ScanError::Io(std::io::Error::other("boom"))),
        101,
    );
    assert!(err.is_err());
    assert!(!core.scan_in_progress());
    let (scanner, ticket) = core.begin_scan(102).unwrap();
    let outcome = core.apply_scan(ticket, scanner.scan(), 103).unwrap();
    assert!(
        outcome.rescan_wanted,
        "the request survived the failed scan"
    );
}

/// A scan whose thread never came back (30 min and counting) is taken over
/// rather than blocking every later scan; if its report does turn up, it
/// is the OLDER one and must not clobber the newer scan's flag or state.
///
/// MUTATION: drop the generation check in `apply_scan` → the old report
/// applies and clears the NEW scan's `scan_in_progress`.
#[test]
fn a_stale_scan_is_taken_over_and_its_late_report_is_ignored() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let (old_scanner, old_ticket) = core.begin_scan(100).unwrap();
    let (_new_scanner, _new_ticket) = core
        .begin_scan(100 + 31 * 60)
        .expect("a 31-minute-old scan is taken over");
    let outcome = core
        .apply_scan(old_ticket, old_scanner.scan(), 100 + 31 * 60 + 1)
        .unwrap();
    assert!(!outcome.rescan_wanted);
    assert!(
        core.scan_in_progress(),
        "the newer scan is still the one in progress"
    );
}

/// A file taken out of management while the scan ran (its source deleted,
/// like `chezmoi forget`): the report still lists it, because it was
/// managed when the scan looked. `apply_scan` reconciles first and must
/// not journal a drift for a file that is no longer managed.
///
/// MUTATION: drop the `self.managed.contains(&fd.target)` filter in
/// `apply_scan` → `.testrc` is listed as drifted after it left management.
#[test]
fn a_target_forgotten_during_a_scan_is_not_resurrected_by_its_report() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let rx = core.subscribe();
    let target = s.home.join(".testrc");
    std::fs::write(&target, "a=drift\n").unwrap();
    let (scanner, ticket) = core.begin_scan(100).unwrap();
    let report = scanner.scan();
    assert_eq!(report.as_ref().unwrap().drifted.len(), 1);

    std::fs::remove_file(s.source.join("dot_testrc")).unwrap();

    let outcome = core.apply_scan(ticket, report, 101).unwrap();
    assert_eq!(outcome.drifted, 0);
    assert!(core.status_snapshot().0.is_empty());
    assert!(
        kinds(&core, &target).contains(&"left_management".to_string()),
        "{:?}",
        kinds(&core, &target)
    );
    let mut saw_left = false;
    while let Ok(ev) = rx.try_recv() {
        saw_left |= matches!(ev, Event::LeftManagement { .. });
    }
    assert!(saw_left);
}

/// A file fixed OUTSIDE Tomte while a scan runs (`chezmoi apply` in a
/// terminal: no `expect_changes`, the watcher sees a foreign change that
/// probes as in sync): the per-event path clears it through `clear_drift`,
/// and the stale report must not put it back.
///
/// MUTATION: replace `self.clear_drift(target)` in `probe_and_journal`'s
/// in-sync arm with `self.drift_state.remove(target)` → the report
/// re-adds `.testrc` and `drifted` reads 1.
#[test]
fn a_target_fixed_by_hand_during_a_scan_stays_resolved() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let target = s.home.join(".testrc");
    let source_text = std::fs::read(s.source.join("dot_testrc")).unwrap();
    std::fs::write(&target, "a=drift\n").unwrap();
    core.handle_paths_changed(std::slice::from_ref(&target), 99)
        .unwrap();
    let (scanner, ticket) = core.begin_scan(100).unwrap();
    let report = scanner.scan();
    assert_eq!(report.as_ref().unwrap().drifted.len(), 1);
    std::fs::write(&target, &source_text).unwrap();
    core.handle_paths_changed(std::slice::from_ref(&target), 101)
        .unwrap();
    assert!(
        core.status_snapshot().0.is_empty(),
        "the event path cleared it"
    );
    let outcome = core.apply_scan(ticket, report, 102).unwrap();
    assert_eq!(outcome.drifted, 0, "the stale report did not resurrect it");
}

/// A scan that FAILS must still reach `apply_scan`: that is what clears
/// `scan_in_progress`, and without it Status says "scanning" forever and
/// every Rescan is swallowed as `rescan_wanted` until the 30-minute
/// takeover.
///
/// MUTATION: in `scan::run_scan`, return the scan's `Err` before calling
/// `apply_scan` → the flag stays set and the second assertion fails.
#[test]
fn run_scan_releases_the_flag_when_the_scan_fails() {
    use std::os::unix::fs::PermissionsExt as _;
    let s = Scratch::new();
    let core = std::sync::Arc::new(std::sync::Mutex::new(core_for(&s)));
    // Make the scan fail deterministically: an unreadable source tree makes
    // `chezmoi managed` (and every git call) fail. Permissions are restored
    // before the scratch is dropped, whatever the outcome.
    let original = std::fs::metadata(&s.source).unwrap().permissions();
    std::fs::set_permissions(&s.source, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = tomte_daemon::scan::run_scan(&core, || 100);
    std::fs::set_permissions(&s.source, original).unwrap();
    assert!(
        result.is_err(),
        "the failure is reported, not swallowed: {result:?}"
    );
    let mut c = core.lock().unwrap();
    assert!(!c.scan_in_progress(), "a failed scan releases the flag");
    assert!(c.begin_scan(101).is_some(), "and the next scan may start");
}

/// A managed folder replaced by a link journals ONE `type_changed` event over
/// two scans, publishes it, and the snapshot carries the change.
///
/// MUTATION: delete the `type_change_recorded` guard in `journal_probe`
/// (record on every scan) → two events, the count assertion fails.
#[test]
fn type_change_is_journaled_once_and_published() {
    let s = Scratch::new();
    std::fs::create_dir(s.source.join("dot_d")).unwrap();
    std::fs::write(s.source.join("dot_d/one"), "1\n").unwrap();
    git(&s.source, &["add", "-A"]);
    git(&s.source, &["commit", "-m", "folder"]);
    git(&s.source, &["push"]);
    s.chezmoi().apply(None).unwrap();
    std::fs::rename(s.home.join(".d"), s.home.join(".real")).unwrap();
    std::os::unix::fs::symlink(".real", s.home.join(".d")).unwrap();

    let mut core = core_for(&s);
    let rx = core.subscribe();
    core.full_rescan(100).unwrap();
    core.full_rescan(200).unwrap();

    let target = s.home.join(".d");
    let events: Vec<_> = core
        .journal()
        .events_for(&target, 20)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "type_changed")
        .collect();
    assert_eq!(events.len(), 1, "{events:?}");
    let meta = events[0].meta.as_ref().unwrap();
    assert_eq!(meta["label"], "replaced by a link");
    assert_eq!(meta["class"], "type_changed");

    let (list, _, _) = core.status_snapshot();
    let d = list.iter().find(|d| d.target == target).expect("listed");
    assert_eq!(d.class, "type_changed");
    let tc = d.type_change.as_ref().expect("type change on the summary");
    assert!(tc.disk_moved);
    assert_eq!(
        tc.disk,
        tomte_proto::WireEntryKind::Link {
            text: ".real".into()
        }
    );

    let pushed = std::iter::from_fn(|| rx.try_recv().ok()).any(|e| {
        matches!(e, Event::Drift { class, type_change: Some(_), target: t, .. }
            if class == "type_changed" && t == target)
    });
    assert!(pushed, "a Drift event carried the type change");
}
