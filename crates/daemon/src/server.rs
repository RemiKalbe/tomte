//! Unix-socket IPC server (spec §3.3): ndjson frames, hello handshake,
//! request/reply with ids, id-less pushes to subscribers.

use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use czui_proto::{
    ClientFrame, EventSummary, PROTOCOL_VERSION, Request, Response, ServerFrame, check_hello,
    write_frame,
};

use crate::core::DaemonCore;

pub fn serve(
    listener: UnixListener,
    core: Arc<Mutex<DaemonCore>>,
    now_fn: fn() -> u64,
    on_shutdown: Arc<dyn Fn() + Send + Sync>,
) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let core = core.clone();
        let on_shutdown = on_shutdown.clone();
        std::thread::spawn(move || {
            let _ = handle_connection(stream, core, now_fn, on_shutdown);
        });
    }
    Ok(())
}

fn reply(out: &Arc<Mutex<UnixStream>>, id: u64, response: Response) -> std::io::Result<()> {
    let mut w = out.lock().expect("socket writer poisoned");
    write_frame(&mut *w, &ServerFrame::Reply { id, response })
}

fn handle_connection(
    stream: UnixStream,
    core: Arc<Mutex<DaemonCore>>,
    now_fn: fn() -> u64,
    on_shutdown: Arc<dyn Fn() + Send + Sync>,
) -> std::io::Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let out = Arc::new(Mutex::new(stream));
    let mut hello_done = false;

    for line in reader.lines() {
        let line = line?;
        let frame: ClientFrame = match czui_proto::read_frame(&line) {
            Ok(f) => f,
            Err(e) => {
                // no id to echo; use 0 per protocol convention for parse errors
                reply(
                    &out,
                    0,
                    Response::Error {
                        message: format!("bad frame: {e}"),
                    },
                )?;
                continue;
            }
        };
        let id = frame.id;

        if !hello_done {
            match frame.request {
                Request::Hello { version } => match check_hello(version) {
                    Ok(()) => {
                        hello_done = true;
                        let machine = core.lock().expect("core poisoned").machine().to_string();
                        reply(
                            &out,
                            id,
                            Response::HelloOk {
                                version: PROTOCOL_VERSION,
                                machine,
                            },
                        )?;
                    }
                    Err(message) => {
                        reply(&out, id, Response::Error { message })?;
                        return Ok(()); // close on mismatch
                    }
                },
                _ => {
                    reply(
                        &out,
                        id,
                        Response::Error {
                            message: "hello required first".into(),
                        },
                    )?;
                }
            }
            continue;
        }

        // Shutdown is special: the reply must reach the client BEFORE the
        // hook runs (the binary's hook is process::exit).
        if matches!(frame.request, Request::Shutdown) {
            reply(&out, id, Response::Ok)?;
            on_shutdown();
            return Ok(());
        }
        let response = dispatch(&core, frame.request, &out, now_fn);
        reply(&out, id, response)?;
    }
    Ok(())
}

fn event_summaries(rows: Vec<czui_journal::EventRow>) -> Vec<EventSummary> {
    rows.into_iter()
        .map(|e| EventSummary {
            id: e.id,
            target: e.target,
            kind: e.kind,
            ts: e.ts,
        })
        .collect()
}

fn dispatch(
    core: &Arc<Mutex<DaemonCore>>,
    request: Request,
    out: &Arc<Mutex<UnixStream>>,
    now_fn: fn() -> u64,
) -> Response {
    let now = now_fn();
    let mut c = match core.lock() {
        Ok(c) => c,
        Err(_) => {
            return Response::Error {
                message: "daemon state poisoned".into(),
            };
        }
    };
    match request {
        Request::Hello { .. } => Response::HelloOk {
            version: PROTOCOL_VERSION,
            machine: c.machine().to_string(),
        },
        Request::Subscribe => {
            let rx = c.subscribe();
            let out = out.clone();
            std::thread::spawn(move || {
                for ev in rx {
                    let Ok(mut w) = out.lock() else { break };
                    let frame = ServerFrame::Push { event: ev };
                    if write_frame(&mut *w, &frame).is_err() {
                        break;
                    }
                }
            });
            Response::Ok
        }
        Request::Status => {
            let (drifted, in_sync, degraded) = c.status_snapshot();
            Response::Status {
                drifted,
                in_sync,
                degraded,
            }
        }
        Request::Timeline { limit, before_id } => match c.journal().timeline(limit, before_id) {
            Ok(rows) => Response::Timeline {
                events: event_summaries(rows),
            },
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        },
        Request::EventsFor { target, limit } => match c.journal().events_for(&target, limit) {
            Ok(rows) => Response::Timeline {
                events: event_summaries(rows),
            },
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        },
        Request::ExpectChanges { paths, ttl_secs } => {
            c.expect_changes(&paths, ttl_secs, now);
            Response::Ok
        }
        Request::Shutdown => Response::Ok, // handled in handle_connection
        Request::Rescan => match c.full_rescan(now) {
            Ok(_) => Response::Ok,
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        },
        Request::Pause => {
            c.set_paused(true);
            Response::Ok
        }
        Request::Resume => {
            c.set_paused(false);
            Response::Ok
        }
        Request::SnapshotBlobs { paths } => match c.snapshot_blobs(&paths, now) {
            Ok(hashes) => Response::Blobs { hashes },
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        },
        Request::SessionStart { ts } => {
            let j = c.journal();
            match j.begin_session(ts) {
                Ok(session) => {
                    let _ = j.record_event(czui_journal::NewEvent {
                        target: None,
                        ts,
                        kind: czui_journal::EventKind::SessionStart,
                        from_hash: None,
                        to_hash: None,
                        meta: Some(serde_json::json!({"session": session})),
                    });
                    Response::SessionStarted { session }
                }
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        Request::SessionDecision { session, decision } => {
            match c.journal().add_decision(session, &decision) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        Request::SessionEnd {
            session,
            ts,
            summary,
        } => {
            let j = c.journal();
            match j.end_session(session, ts, &summary) {
                Ok(()) => {
                    let _ = j.record_event(czui_journal::NewEvent {
                        target: None,
                        ts,
                        kind: czui_journal::EventKind::SessionEnd,
                        from_hash: None,
                        to_hash: None,
                        meta: Some(serde_json::json!({"session": session, "summary": summary})),
                    });
                    Response::Ok
                }
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
    }
}
