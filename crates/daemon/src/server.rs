//! Unix-socket IPC server (spec §3.3): ndjson frames, hello handshake,
//! request/reply with ids, id-less pushes to subscribers.

use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use czui_proto::{
    ClientFrame, Event, EventSummary, PROTOCOL_VERSION, Request, Response, ServerFrame,
    check_hello, write_frame,
};

use crate::core::DaemonCore;

/// Everything a connection needs that must NOT wait on the core — or on the
/// core even EXISTING: the daemon binds its socket before it can talk to
/// chezmoi at all (a locked secret manager can stall chezmoi for minutes),
/// so Hello/Subscribe answer instantly and Status degrades honestly until
/// the core is ready (spec §3.2, §10).
#[derive(Clone)]
pub struct ServeCtx {
    /// Set once startup manages to build the core; empty while starting.
    pub core: Arc<std::sync::OnceLock<Arc<Mutex<DaemonCore>>>>,
    pub subscribers: Arc<Mutex<Vec<std::sync::mpsc::Sender<Event>>>>,
    pub machine: String,
    /// Last startup error, shown as degraded status while `core` is empty.
    pub starting_error: Arc<Mutex<String>>,
    pub now_fn: fn() -> u64,
    pub on_shutdown: Arc<dyn Fn() + Send + Sync>,
}

impl ServeCtx {
    /// A context whose core arrives later via [`ServeCtx::set_core`].
    pub fn starting(
        subscribers: Arc<Mutex<Vec<std::sync::mpsc::Sender<Event>>>>,
        machine: String,
        now_fn: fn() -> u64,
        on_shutdown: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            core: Arc::new(std::sync::OnceLock::new()),
            subscribers,
            machine,
            starting_error: Arc::new(Mutex::new("starting".to_string())),
            now_fn,
            on_shutdown,
        }
    }

    /// Build from an already-ready core (tests; call while uncontended).
    pub fn ready(
        core: Arc<Mutex<DaemonCore>>,
        now_fn: fn() -> u64,
        on_shutdown: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let (subscribers, machine) = {
            let c = core.lock().expect("core uncontended at setup");
            (c.subscriber_handle(), c.machine().to_string())
        };
        let ctx = Self::starting(subscribers, machine, now_fn, on_shutdown);
        ctx.set_core(core);
        ctx
    }

    pub fn set_core(&self, core: Arc<Mutex<DaemonCore>>) {
        let _ = self.core.set(core);
    }

    pub fn set_starting_error(&self, message: String) {
        if let Ok(mut e) = self.starting_error.lock() {
            *e = message;
        }
    }
}

pub fn serve(listener: UnixListener, ctx: ServeCtx) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = handle_connection(stream, ctx);
        });
    }
    Ok(())
}

fn reply(out: &Arc<Mutex<UnixStream>>, id: u64, response: Response) -> std::io::Result<()> {
    let mut w = out.lock().expect("socket writer poisoned");
    write_frame(&mut *w, &ServerFrame::Reply { id, response })
}

fn handle_connection(stream: UnixStream, ctx: ServeCtx) -> std::io::Result<()> {
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
                        let machine = ctx.machine.clone();
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
            (ctx.on_shutdown)();
            return Ok(());
        }
        let response = dispatch(&ctx, frame.request, &out);
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

fn dispatch(ctx: &ServeCtx, request: Request, out: &Arc<Mutex<UnixStream>>) -> Response {
    let now = (ctx.now_fn)();

    // Requests that must never wait behind a long-running scan:
    match &request {
        Request::Hello { .. } => {
            return Response::HelloOk {
                version: PROTOCOL_VERSION,
                machine: ctx.machine.clone(),
            };
        }
        Request::Subscribe => {
            let (tx, rx) = std::sync::mpsc::channel();
            if let Ok(mut subs) = ctx.subscribers.lock() {
                subs.push(tx);
            }
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
            return Response::Ok;
        }
        _ => {}
    }

    // Startup may still be fighting a slow chezmoi (locked secret manager):
    // no core yet means "starting", not an outage.
    let Some(core) = ctx.core.get() else {
        let why = ctx
            .starting_error
            .lock()
            .map(|e| e.clone())
            .unwrap_or_else(|_| "starting".to_string());
        // Before the first failure the placeholder reason is "starting" —
        // don't render the comical "chezmoid starting: starting".
        let why = if why == "starting" {
            "chezmoid starting…".to_string()
        } else {
            format!("chezmoid starting: {why}")
        };
        return match request {
            Request::Status => Response::Status {
                drifted: Vec::new(),
                in_sync: 0,
                degraded: Some(why.clone()),
                scanning: true,
            },
            _ => Response::Error { message: why },
        };
    };

    // Everything else needs the core. Never block indefinitely: if a scan
    // holds the lock, degrade honestly (spec §10) instead of timing out the
    // client.
    let mut c = match core.try_lock() {
        Ok(c) => c,
        Err(std::sync::TryLockError::WouldBlock) => {
            return match request {
                // A running scan is NOT a degradation — scanning:true is the
                // whole story, and clients keep their last real degraded
                // hint (2026-08-04: the busy text leaked into the degraded
                // banner as "scan in progress… · re-checks every minute").
                Request::Status => Response::Status {
                    drifted: Vec::new(),
                    in_sync: 0,
                    degraded: None,
                    scanning: true,
                },
                _ => Response::Error {
                    message: "daemon busy (scan in progress) — retry shortly".into(),
                },
            };
        }
        Err(std::sync::TryLockError::Poisoned(_)) => {
            return Response::Error {
                message: "daemon state poisoned".into(),
            };
        }
    };
    match request {
        Request::Hello { .. } | Request::Subscribe => unreachable!("handled above"),
        Request::Status => {
            let (drifted, in_sync, degraded) = c.status_snapshot();
            Response::Status {
                drifted,
                in_sync,
                degraded,
                scanning: false,
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
        Request::Rescan => {
            // Scans take ~15s on real dotfile trees — far beyond the client's
            // request timeout. Acknowledge now, scan on a thread; completion
            // arrives as the ScanDone push.
            drop(c);
            let core = ctx.core.get().cloned();
            let now_fn = ctx.now_fn;
            std::thread::spawn(move || {
                if let Some(core) = core
                    && let Ok(mut c) = core.lock()
                    && let Err(e) = c.full_rescan(now_fn())
                {
                    eprintln!("chezmoid: requested rescan failed: {e}");
                }
            });
            Response::Ok
        }
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
