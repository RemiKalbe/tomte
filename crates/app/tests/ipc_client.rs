//! IpcClient against the real daemon server.

use std::sync::{Arc, Mutex};

use czui_app::ipc::IpcClient;
use czui_core::chezmoi::ChezmoiClient;
use czui_core::cmd::SystemRunner;
use czui_core::git::GitClient;
use czui_core::testsupport::Scratch;
use czui_daemon::core::DaemonCore;
use czui_daemon::server::serve;
use czui_journal::Journal;
use czui_proto::{Event, Request, Response};

#[test]
fn connect_status_subscribe_roundtrip() {
    let s = Scratch::new();
    let chezmoi: ChezmoiClient = s.chezmoi();
    let git = GitClient::new(Arc::new(SystemRunner), s.source.clone());
    let journal = Journal::open_in_memory("ipc").unwrap();
    let core = Arc::new(Mutex::new(
        DaemonCore::new(chezmoi, git, journal, "origin/main".into()).unwrap(),
    ));
    let sock = s.root.path().join("d.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let served = core.clone();
    std::thread::spawn(move || {
        serve(
            listener,
            czui_daemon::server::ServeCtx::new(served, || 42, std::sync::Arc::new(|| {})),
        )
    });

    let client = IpcClient::connect(&sock).unwrap();
    match client.request(Request::Status).unwrap() {
        Response::Status { drifted, .. } => assert!(drifted.is_empty()),
        other => panic!("unexpected: {other:?}"),
    }
    let events = client.subscribe().unwrap();
    let target = s.home.join(".testrc");
    std::fs::write(&target, "a=live\n").unwrap();
    core.lock()
        .unwrap()
        .handle_paths_changed(std::slice::from_ref(&target), 77)
        .unwrap();
    match events
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap()
    {
        Event::Drift {
            target: t, ts: 77, ..
        } => assert_eq!(t, target),
        other => panic!("unexpected push: {other:?}"),
    }
}

#[test]
fn version_rejection_surfaces_as_error() {
    // connect() performs Hello with PROTOCOL_VERSION, so this tests the happy
    // handshake; rejection is covered by the daemon's own tests. Here: bad socket.
    assert!(IpcClient::connect(std::path::Path::new("/nonexistent.sock")).is_err());
}
