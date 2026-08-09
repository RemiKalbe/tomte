//! DaemonCore behavior against a real scratch chezmoi home.

use tomte_core::chezmoi::ChezmoiClient;
use tomte_core::cmd::SystemRunner;
use tomte_core::git::GitClient;
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
fn fetch_detects_remote_changes() {
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
    let target = s.home.join(".testrc");
    assert!(kinds(&core, &target).contains(&"remote_advanced".to_string()));
    let events: Vec<Event> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Drift { class, .. } if class == "remote_ahead"))
    );
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
