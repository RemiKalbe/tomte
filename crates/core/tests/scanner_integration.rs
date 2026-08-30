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
