mod support;

use support::{Scratch, git};
use tomte_core::drift::DriftClass;

#[test]
fn clean_state_is_in_sync() {
    let s = Scratch::new();
    let report = s.scanner().scan().unwrap();
    assert!(report.drifted.is_empty(), "{:?}", report.drifted);
    assert_eq!(report.in_sync_count, 1);
    assert!(report.degraded.is_none());
}

#[test]
fn tool_rewrite_is_destination_drift() {
    let s = Scratch::new();
    std::fs::write(s.home.join(".testrc"), "a=2\n").unwrap();
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    let d = &report.drifted[0];
    assert!(d.target.ends_with(".testrc"));
    assert_eq!(d.class, DriftClass::DestinationDrift);
}

#[test]
fn source_edit_is_source_ahead() {
    let s = Scratch::new();
    std::fs::write(s.source.join("dot_testrc"), "a=3\n").unwrap();
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    assert_eq!(report.drifted[0].class, DriftClass::SourceAhead);
}

#[test]
fn remote_push_is_remote_ahead() {
    let s = Scratch::new();
    let other = s.root.path().join("other");
    support::sh(
        s.root.path(),
        "git",
        &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    std::fs::write(other.join("dot_testrc"), "a=4\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "remote change"]);
    git(&other, &["push"]);
    // scanner does not fetch; the caller owns fetch cadence (spec §3.1)
    support::git(&s.source, &["fetch", "origin"]);
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    assert_eq!(report.drifted[0].class, DriftClass::RemoteAhead);
}

#[test]
fn probe_one_detects_single_file_drift() {
    let s = Scratch::new();
    std::fs::write(s.home.join(".testrc"), "a=changed\n").unwrap();
    let scanner = s.scanner();
    let fd = scanner.probe_one(&s.home.join(".testrc")).unwrap().unwrap();
    assert_eq!(fd.class, DriftClass::DestinationDrift);
    // untouched target probes clean after re-write back
    std::fs::write(s.home.join(".testrc"), "a=1\n").unwrap();
    assert!(
        scanner
            .probe_one(&s.home.join(".testrc"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn disk_and_remote_change_is_conflict() {
    let s = Scratch::new();
    std::fs::write(s.home.join(".testrc"), "a=local\n").unwrap();
    let other = s.root.path().join("other");
    support::sh(
        s.root.path(),
        "git",
        &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    std::fs::write(other.join("dot_testrc"), "a=remote\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "remote change"]);
    git(&other, &["push"]);
    support::git(&s.source, &["fetch", "origin"]);
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    assert_eq!(report.drifted[0].class, DriftClass::Conflict);
}

/// The 2026-08-27 field bug: chezmoi managed a DIRECTORY that had never been
/// applied, so it had no entry state. The scanner probed it as a file,
/// `chezmoi cat` refused ("not a file, script, or symlink"), and the `?` took
/// the entire scan down — leaving the daemon reporting "0 drifted, 0 in sync"
/// indefinitely while the file watcher kept ticking.
#[test]
fn a_managed_directory_never_applied_does_not_abort_the_scan() {
    let s = Scratch::new();
    // A new source directory, never applied: `git status --porcelain` reports
    // the untracked DIRECTORY, which maps to a target directory with no entry
    // state — exactly the shape that broke.
    let dir = s.source.join("dot_newdir");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("inner"), "x\n").unwrap();

    let report = s
        .scanner()
        .scan()
        .expect("a managed directory must not abort the scan");

    // The rest of the scan still did its job: the pre-existing managed file
    // is still accounted for, and the new file is reported as source-ahead.
    assert_eq!(
        report.in_sync_count, 1,
        "the clean managed file still counts"
    );
    assert!(
        report
            .drifted
            .iter()
            .any(|d| d.target.ends_with("inner") && d.class == DriftClass::SourceAhead),
        "the new file inside the directory is still detected: {:?}",
        report.drifted
    );
    // The directory is out of scope, not a failure — it must not show up as
    // something that "could not be checked".
    assert!(
        report.unprobed.is_empty(),
        "a directory is skipped, not recorded as a failure: {:?}",
        report.unprobed
    );
}

/// The general form of the same bug: ANY single unprobeable target used to
/// abort everything. It must be skipped, recorded, and kept out of the
/// in-sync tally — reported, never silently counted as fine.
#[test]
fn an_unreadable_target_is_recorded_not_fatal() {
    let s = Scratch::new();
    // Replace the managed file with a directory of the same name: chezmoi
    // still has file entry state for it, so it IS counted as managed, but
    // reading it fails with something other than NotFound.
    let target = s.home.join(".testrc");
    std::fs::remove_file(&target).unwrap();
    std::fs::create_dir(&target).unwrap();

    let report = s.scanner().scan().expect("one bad target is not fatal");
    assert_eq!(
        report.unprobed.len(),
        1,
        "the unreadable target is recorded: {:?}",
        report.unprobed
    );
    assert!(report.unprobed[0].target.ends_with(".testrc"));
    assert_eq!(
        report.in_sync_count, 0,
        "an unchecked file is unknown, not in sync"
    );
}

#[test]
fn origin_differences_are_detected_per_target() {
    let s = Scratch::new();
    // Push a change to origin that the local repo does not have.
    let other = s.root.path().join("other");
    support::sh(
        s.root.path(),
        "git",
        &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    std::fs::write(other.join("dot_testrc"), "a=origin\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "origin edit"]);
    git(&other, &["push"]);
    support::git(&s.source, &["fetch", "origin"]);

    // A second managed file, edited only in the local WORKING TREE
    // (uncommitted, never pushed). It lands in the scanner's candidate set
    // via `dirty_files` — exactly like `.testrc` lands via `changed_files`
    // — but origin's committed version of it is untouched: HEAD already
    // matches origin for this file, the dirty edit hasn't been committed at
    // all. An implementation that inserted every mapped candidate into
    // `origin_differs` (i.e. deleted the `differs_from_origin.contains(rel)`
    // gate) would wrongly flag this one too; a selective one must not.
    std::fs::write(s.source.join("dot_otherrc"), "b=uncommitted\n").unwrap();

    let report = s.scanner().scan().unwrap();
    assert!(
        !report
            .origin_differs
            .iter()
            .any(|t| t.ends_with(".otherrc")),
        "a dirty but uncommitted local file is not an origin difference: {:?}",
        report.origin_differs
    );
    assert!(
        report.origin_differs.iter().any(|t| t.ends_with(".testrc")),
        "origin's version of .testrc differs: {:?}",
        report.origin_differs
    );
}

/// `source-path` is batched (spec 2026-10-11 §2.3): one call, one line per
/// target in argument order. A target chezmoi does not know makes the whole
/// call fail, and the chunk is then answered one target at a time — never
/// by zipping a short reply against the full chunk.
///
/// MUTATION: on a failed chunk, extend the result with `None` for EVERY
/// target of the chunk instead of retrying per target → the third entry
/// is `None`. (The line-count check is pinned by the scripted-runner test
/// `source_paths_retries_a_short_reply_per_target` in `chezmoi.rs`: real
/// chezmoi never answers a short reply with exit 0.)
#[test]
fn source_paths_answers_in_argument_order_and_never_misaligns() {
    let s = Scratch::new();
    let chezmoi = s.chezmoi();
    let testrc = s.home.join(".testrc");
    let unmanaged = s.home.join(".nope");
    let got = chezmoi
        .source_paths(&[testrc.clone(), unmanaged, testrc.clone()])
        .unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(
        got[0].as_deref(),
        Some(s.source.join("dot_testrc").as_path())
    );
    assert_eq!(got[1], None);
    assert_eq!(got[2], got[0]);
}

/// The scan reads ahead/behind from two `git log --name-only` sets computed
/// once (spec 2026-10-11 §2.3). A committed, unpushed source edit is
/// `local_ahead`; the per-event probe of one file still asks git per file.
///
/// MUTATION: hand `probe_file` an empty `local` set in `scan` (drop the
/// `files_touched_in("origin/main..HEAD")` call) → `local_ahead` reads
/// false here. (`remote_push_is_remote_ahead` above pins the `remote` set
/// the same way: an empty one turns its RemoteAhead into something else.)
#[test]
fn a_committed_unpushed_source_edit_is_local_ahead_in_both_probes() {
    let s = Scratch::new();
    std::fs::write(s.source.join("dot_testrc"), "a=committed\n").unwrap();
    git(&s.source, &["add", "."]);
    git(&s.source, &["commit", "-m", "local edit"]);
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    assert!(
        report.drifted[0].probe.git.local_ahead,
        "{:?}",
        report.drifted[0].probe.git
    );
    assert!(!report.drifted[0].probe.git.remote_ahead);
    let one = s
        .scanner()
        .probe_one(&s.home.join(".testrc"))
        .unwrap()
        .expect("drifted");
    assert_eq!(
        one.probe.git, report.drifted[0].probe.git,
        "both probes agree"
    );
}

/// `git log --name-only` quotes non-ASCII names (`core.quotePath`); the sets
/// are read NUL-separated so they match the unquoted source path.
///
/// MUTATION: drop `-z` and split on newlines → the quoted path never matches
/// and `local_ahead` reads false.
#[test]
fn a_non_ascii_source_name_is_local_ahead() {
    let s = Scratch::new();
    std::fs::write(s.source.join("dot_café"), "x=1\n").unwrap();
    git(&s.source, &["add", "."]);
    git(&s.source, &["commit", "-m", "add cafe"]);
    let report = s.scanner().scan().unwrap();
    let d = report
        .drifted
        .iter()
        .find(|d| d.target.ends_with(".café"))
        .unwrap_or_else(|| panic!("no .café entry: {:?}", report.drifted));
    assert!(d.probe.git.local_ahead, "{:?} {:?}", d.class, d.probe.git);
}

/// A range holding a merge commit is answered per file: the sets would call
/// a file "ahead" that a merge resolved to origin's version.
///
/// MUTATION: drop the `has_merges` check (always use the sets) →
/// `local_ahead` reads true and the two probes disagree.
#[test]
fn a_range_with_a_merge_is_answered_per_file() {
    let s = Scratch::new();
    let other = s.root.path().join("other");
    support::sh(
        s.root.path(),
        "git",
        &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    std::fs::write(other.join("dot_testrc"), "a=origin\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "origin edit"]);
    git(&other, &["push"]);
    std::fs::write(s.source.join("dot_testrc"), "a=local\n").unwrap();
    git(&s.source, &["add", "."]);
    git(&s.source, &["commit", "-m", "local edit"]);
    git(&s.source, &["fetch", "origin"]);
    // Conflicts; keep origin's version.
    let _ = std::process::Command::new("git")
        .current_dir(&s.source)
        .args(["merge", "origin/main"])
        .output()
        .unwrap();
    std::fs::write(s.source.join("dot_testrc"), "a=origin\n").unwrap();
    git(&s.source, &["add", "."]);
    git(&s.source, &["commit", "--no-edit", "-m", "merge"]);
    let report = s.scanner().scan().unwrap();
    let d = report
        .drifted
        .iter()
        .find(|d| d.target.ends_with(".testrc"))
        .unwrap_or_else(|| panic!("no .testrc entry: {:?}", report.drifted));
    assert!(!d.probe.git.local_ahead, "{:?}", d.probe.git);
    let one = s
        .scanner()
        .probe_one(&s.home.join(".testrc"))
        .unwrap()
        .unwrap();
    assert_eq!(one.probe.git, d.probe.git, "both probes agree");
}
