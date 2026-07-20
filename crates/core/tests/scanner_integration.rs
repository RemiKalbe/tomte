mod support;

use czui_core::drift::DriftClass;
use support::{Scratch, git};

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
