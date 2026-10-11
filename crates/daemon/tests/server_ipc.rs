//! Socket round-trip: handshake, request/reply, subscribe/push.

use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use tomte_core::chezmoi::ChezmoiClient;
use tomte_core::cmd::SystemRunner;
use tomte_core::git::GitClient;
use tomte_core::testsupport::Scratch;
use tomte_daemon::core::DaemonCore;
use tomte_daemon::server::serve;
use tomte_journal::Journal;
use tomte_proto::{
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
            tomte_daemon::server::ServeCtx::ready(served, || 1000, Arc::new(|| {})),
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
                tomte_proto::Event::Drift {
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
            tomte_daemon::server::ServeCtx::ready(
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

/// The busy bounce is what the app retries, so it has to be prompt and
/// nameable: a request that needs the core waits `busy_wait`, then answers
/// "daemon busy" instead of hanging into the client's timeout.
/// MUTATION: ignore `busy_wait` (hard-code the 20s) → the 5s read timeout
/// below trips first and the test fails instead of hanging.
#[test]
fn a_shortened_busy_wait_bounces_a_core_request_promptly() {
    let s = Scratch::new();
    let chezmoi: ChezmoiClient = s.chezmoi();
    let git = GitClient::new(Arc::new(SystemRunner), s.source.clone());
    let journal = Journal::open_in_memory("busy-wait-test").unwrap();
    let core = Arc::new(Mutex::new(
        DaemonCore::new(chezmoi, git, journal, "origin/main".into()).unwrap(),
    ));
    let sock = s.root.path().join("bw.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let mut ctx = tomte_daemon::server::ServeCtx::ready(core.clone(), || 7, Arc::new(|| {}));
    ctx.busy_wait = std::time::Duration::from_millis(200);
    std::thread::spawn(move || serve(listener, ctx));
    let _scan_guard = core.lock().unwrap();

    let stream = UnixStream::connect(&sock).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    send(
        &mut writer,
        1,
        Request::Hello {
            version: PROTOCOL_VERSION,
        },
    );
    let _ = recv(&mut reader);

    let started = std::time::Instant::now();
    send(&mut writer, 2, Request::Rescan);
    match recv(&mut reader) {
        ServerFrame::Reply {
            id: 2,
            response: Response::Error { message },
        } => assert!(message.contains("daemon busy"), "{message}"),
        other => panic!("expected the busy bounce, got {other:?}"),
    }
    let waited = started.elapsed();
    assert!(
        waited >= std::time::Duration::from_millis(200)
            && waited < std::time::Duration::from_secs(3),
        "bounced after {waited:?}, expected ~200ms"
    );
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
    let ctx = tomte_daemon::server::ServeCtx::ready(core.clone(), || 7, Arc::new(|| {}));
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
            response:
                Response::Status {
                    drifted,
                    degraded,
                    scanning,
                    ..
                },
        } => {
            assert!(drifted.is_empty());
            // Busy is NOT a degradation: scanning carries the state and the
            // hint stays clear so clients keep their last real one.
            assert!(scanning, "busy status must report scanning");
            assert_eq!(degraded, None);
        }
        other => panic!("expected busy status, got {other:?}"),
    }
}

/// A scan in progress is a FLAG, not a held lock (spec 2026-10-11 §2.2):
/// requests are answered meanwhile, and `Status` says `scanning: true`
/// next to the real drift list rather than placeholder zeros.
///
/// MUTATION: report `scanning: false` from the locked Status arm (drop
/// `c.scan_in_progress()`) → the scanning assertion fails.
#[test]
fn status_reports_scanning_with_real_data_while_a_scan_is_in_progress() {
    let s = Scratch::new();
    let chezmoi: ChezmoiClient = s.chezmoi();
    let git = GitClient::new(Arc::new(SystemRunner), s.source.clone());
    let journal = Journal::open_in_memory("scanning-test").unwrap();
    let core = Arc::new(Mutex::new(
        DaemonCore::new(chezmoi, git, journal, "origin/main".into()).unwrap(),
    ));
    std::fs::write(s.home.join(".testrc"), "a=drift\n").unwrap();
    core.lock().unwrap().full_rescan(1).unwrap();
    // A scan "in progress" without running it: deterministic.
    // Held to the end of the test so the scan stays "in progress".
    let _in_flight = core.lock().unwrap().begin_scan(2).unwrap();

    let sock = s.root.path().join("sc.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let mut ctx = tomte_daemon::server::ServeCtx::ready(core.clone(), || 7, Arc::new(|| {}));
    ctx.busy_wait = std::time::Duration::from_millis(200);
    std::thread::spawn(move || serve(listener, ctx));

    let stream = UnixStream::connect(&sock).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    send(
        &mut writer,
        1,
        Request::Hello {
            version: PROTOCOL_VERSION,
        },
    );
    let _ = recv(&mut reader);

    send(&mut writer, 2, Request::SessionStart { ts: 3 });
    assert!(
        matches!(
            recv(&mut reader),
            ServerFrame::Reply {
                id: 2,
                response: Response::SessionStarted { .. }
            }
        ),
        "answered, not bounced, while a scan is in progress"
    );
    send(&mut writer, 3, Request::Status);
    match recv(&mut reader) {
        ServerFrame::Reply {
            id: 3,
            response: Response::Status {
                scanning, drifted, ..
            },
        } => {
            assert!(scanning, "a scan is in progress");
            assert_eq!(drifted.len(), 1, "and the real list is still served");
        }
        other => panic!("expected Status, got {other:?}"),
    }
}

/// The Rescan arm runs the scan with the lock RELEASED: a request sent
/// right after it is answered at once, long before the scan is done.
///
/// MUTATION: in `dispatch`'s Rescan arm, lock the core on the thread and
/// call `full_rescan` (the 0.1.20 shape) → the scratch scan holds the lock
/// for its ~1 s of subprocesses, `SessionStart` waits past the 200 ms
/// `busy_wait` and bounces, and the match below fails.
#[test]
fn a_rescan_request_does_not_hold_the_core_for_the_scan() {
    let s = Scratch::new();
    let chezmoi: ChezmoiClient = s.chezmoi();
    let git = GitClient::new(Arc::new(SystemRunner), s.source.clone());
    let journal = Journal::open_in_memory("rescan-off-lock-test").unwrap();
    let core = Arc::new(Mutex::new(
        DaemonCore::new(chezmoi, git, journal, "origin/main".into()).unwrap(),
    ));
    let sock = s.root.path().join("rs.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let mut ctx = tomte_daemon::server::ServeCtx::ready(core.clone(), || 7, Arc::new(|| {}));
    ctx.busy_wait = std::time::Duration::from_millis(200);
    std::thread::spawn(move || serve(listener, ctx));

    let stream = UnixStream::connect(&sock).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    send(
        &mut writer,
        1,
        Request::Hello {
            version: PROTOCOL_VERSION,
        },
    );
    let _ = recv(&mut reader);
    send(&mut writer, 2, Request::Subscribe);
    let _ = recv(&mut reader);

    send(&mut writer, 3, Request::Rescan);
    assert!(matches!(
        recv(&mut reader),
        ServerFrame::Reply {
            id: 3,
            response: Response::Ok
        }
    ));
    send(&mut writer, 4, Request::SessionStart { ts: 5 });
    // Pushes and replies share the stream: skip pushes until reply 4, but
    // remember a ScanDone that lands first (a quick scratch scan can finish
    // before the reply is read).
    let mut scan_done = false;
    loop {
        match recv(&mut reader) {
            ServerFrame::Reply {
                id: 4,
                response: Response::SessionStarted { .. },
            } => break,
            ServerFrame::Reply { id: 4, response } => {
                panic!("expected SessionStarted, got {response:?}")
            }
            ServerFrame::Push {
                event: tomte_proto::Event::ScanDone { .. },
            } => scan_done = true,
            ServerFrame::Push { .. } => continue,
            other => panic!("unexpected {other:?}"),
        }
    }
    // Let the scan finish before the scratch is torn down.
    while !scan_done {
        if let ServerFrame::Push {
            event: tomte_proto::Event::ScanDone { .. },
        } = recv(&mut reader)
        {
            scan_done = true;
        }
    }
}
