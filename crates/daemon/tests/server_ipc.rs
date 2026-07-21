//! Socket round-trip: handshake, request/reply, subscribe/push.

use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use czui_core::chezmoi::ChezmoiClient;
use czui_core::cmd::SystemRunner;
use czui_core::git::GitClient;
use czui_core::testsupport::Scratch;
use czui_daemon::core::DaemonCore;
use czui_daemon::server::serve;
use czui_journal::Journal;
use czui_proto::{
    ClientFrame, PROTOCOL_VERSION, Request, Response, ServerFrame, read_frame, write_frame,
};

fn setup() -> (Scratch, Arc<Mutex<DaemonCore>>, UnixStream) {
    let s = Scratch::new();
    let chezmoi: ChezmoiClient = s.chezmoi();
    let git = GitClient::new(Arc::new(SystemRunner), s.source.clone());
    let journal = Journal::open_in_memory("ipc-test").unwrap();
    let core = Arc::new(Mutex::new(
        DaemonCore::new(chezmoi, git, journal, "origin/main".into()).unwrap(),
    ));
    let sock = s.root.path().join("d.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let served = core.clone();
    std::thread::spawn(move || {
        serve(
            listener,
            czui_daemon::server::ServeCtx::new(served, || 1000, Arc::new(|| {})),
        )
    });
    let stream = UnixStream::connect(&sock).unwrap();
    (s, core, stream)
}

fn send(stream: &mut UnixStream, id: u64, request: Request) {
    write_frame(stream, &ClientFrame { id, request }).unwrap();
}

fn recv(reader: &mut BufReader<UnixStream>) -> ServerFrame {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    read_frame(line.trim_end()).unwrap()
}

#[test]
fn handshake_then_status_and_push() {
    let (s, core, mut stream) = setup();
    let mut reader = BufReader::new(stream.try_clone().unwrap());

    send(
        &mut stream,
        1,
        Request::Hello {
            version: PROTOCOL_VERSION,
        },
    );
    match recv(&mut reader) {
        ServerFrame::Reply {
            id: 1,
            response: Response::HelloOk { version, machine },
        } => {
            assert_eq!(version, PROTOCOL_VERSION);
            assert_eq!(machine, "ipc-test");
        }
        other => panic!("bad hello reply: {other:?}"),
    }

    send(&mut stream, 2, Request::Subscribe);
    assert!(matches!(
        recv(&mut reader),
        ServerFrame::Reply {
            id: 2,
            response: Response::Ok
        }
    ));

    send(&mut stream, 3, Request::Status);
    match recv(&mut reader) {
        ServerFrame::Reply {
            id: 3,
            response: Response::Status { drifted, .. },
        } => {
            assert!(drifted.is_empty());
        }
        other => panic!("bad status: {other:?}"),
    }

    // trigger a drift; the push must arrive on this connection
    let target = s.home.join(".testrc");
    std::fs::write(&target, "a=pushed\n").unwrap();
    core.lock()
        .unwrap()
        .handle_paths_changed(std::slice::from_ref(&target), 1234)
        .unwrap();
    match recv(&mut reader) {
        ServerFrame::Push {
            event:
                czui_proto::Event::Drift {
                    target: t,
                    ts: 1234,
                    ..
                },
        } => {
            assert_eq!(t, target);
        }
        other => panic!("expected push, got {other:?}"),
    }
}

#[test]
fn version_mismatch_is_rejected() {
    let (_s, _core, mut stream) = setup();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    send(
        &mut stream,
        1,
        Request::Hello {
            version: PROTOCOL_VERSION + 9,
        },
    );
    match recv(&mut reader) {
        ServerFrame::Reply {
            id: 1,
            response: Response::Error { message },
        } => {
            assert!(message.contains("mismatch"));
        }
        other => panic!("expected error, got {other:?}"),
    }
}

#[test]
fn session_flow_over_ipc() {
    let (_s, core, mut stream) = setup();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    send(
        &mut stream,
        1,
        Request::Hello {
            version: PROTOCOL_VERSION,
        },
    );
    recv(&mut reader);
    send(&mut stream, 2, Request::SessionStart { ts: 50 });
    let session = match recv(&mut reader) {
        ServerFrame::Reply {
            response: Response::SessionStarted { session },
            ..
        } => session,
        other => panic!("{other:?}"),
    };
    send(
        &mut stream,
        3,
        Request::SessionDecision {
            session,
            decision: serde_json::json!({"c": "ours"}),
        },
    );
    recv(&mut reader);
    send(
        &mut stream,
        4,
        Request::SessionEnd {
            session,
            ts: 60,
            summary: "done".into(),
        },
    );
    recv(&mut reader);
    let tl = core.lock().unwrap().journal().timeline(10, None).unwrap();
    let kinds: Vec<_> = tl.iter().map(|e| e.kind.as_str().to_string()).collect();
    assert!(kinds.contains(&"session_start".to_string()));
    assert!(kinds.contains(&"session_end".to_string()));
}

#[test]
fn shutdown_replies_ok_then_fires_hook() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let s = Scratch::new();
    let chezmoi: ChezmoiClient = s.chezmoi();
    let git = GitClient::new(Arc::new(SystemRunner), s.source.clone());
    let journal = Journal::open_in_memory("shutdown-test").unwrap();
    let core = Arc::new(Mutex::new(
        DaemonCore::new(chezmoi, git, journal, "origin/main".into()).unwrap(),
    ));
    let sock = s.root.path().join("sd.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let fired = Arc::new(AtomicBool::new(false));
    let hook_flag = fired.clone();
    std::thread::spawn(move || {
        serve(
            listener,
            czui_daemon::server::ServeCtx::new(
                core,
                || 1000,
                Arc::new(move || hook_flag.store(true, Ordering::SeqCst)),
            ),
        )
    });

    let mut stream = UnixStream::connect(&sock).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    send(
        &mut stream,
        1,
        Request::Hello {
            version: PROTOCOL_VERSION,
        },
    );
    recv(&mut reader);
    send(&mut stream, 2, Request::Shutdown);
    // the Ok reply must arrive BEFORE the hook (binary's hook is exit(0))
    assert!(matches!(
        recv(&mut reader),
        ServerFrame::Reply {
            id: 2,
            response: Response::Ok
        }
    ));
    // hook fires right after the reply; give the thread a beat
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(fired.load(Ordering::SeqCst));
}

#[test]
fn handshake_and_status_survive_a_long_scan_holding_the_core() {
    let s = Scratch::new();
    let chezmoi: ChezmoiClient = s.chezmoi();
    let git = GitClient::new(Arc::new(SystemRunner), s.source.clone());
    let journal = Journal::open_in_memory("busy-test").unwrap();
    let core = Arc::new(Mutex::new(
        DaemonCore::new(chezmoi, git, journal, "origin/main".into()).unwrap(),
    ));
    let sock = s.root.path().join("busy.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    // ServeCtx must be built BEFORE the lock is contended (as the binary does).
    let ctx = czui_daemon::server::ServeCtx::new(core.clone(), || 7, Arc::new(|| {}));
    std::thread::spawn(move || serve(listener, ctx));

    // Simulate a minutes-long initial scan: hold the core lock for the whole test.
    let _scan_guard = core.lock().unwrap();

    let mut stream = UnixStream::connect(&sock).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());

    // Hello must answer instantly (no core lock involved).
    send(
        &mut stream,
        1,
        Request::Hello {
            version: PROTOCOL_VERSION,
        },
    );
    assert!(matches!(
        recv(&mut reader),
        ServerFrame::Reply {
            id: 1,
            response: Response::HelloOk { .. }
        }
    ));

    // Subscribe must succeed instantly too.
    send(&mut stream, 2, Request::Subscribe);
    assert!(matches!(
        recv(&mut reader),
        ServerFrame::Reply {
            id: 2,
            response: Response::Ok
        }
    ));

    // Status degrades honestly instead of hanging until the client times out.
    send(&mut stream, 3, Request::Status);
    match recv(&mut reader) {
        ServerFrame::Reply {
            id: 3,
            response: Response::Status {
                drifted, degraded, ..
            },
        } => {
            assert!(drifted.is_empty());
            assert_eq!(degraded.as_deref(), Some("initial scan in progress…"));
        }
        other => panic!("expected degraded status, got {other:?}"),
    }
}
