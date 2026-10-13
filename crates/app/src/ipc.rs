//! Blocking IPC client for tomted (spec §3.3). Off-main-thread only.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use tomte_proto::{
    BuildId, ClientFrame, Event, PROTOCOL_VERSION, Request, Response, ServerFrame, read_frame,
    write_frame,
};

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Proto(String),
    #[error(
        "request timed out (connected, but no reply within 30s) — the daemon may still \
         have run it; check the file after the next scan before pressing again"
    )]
    Timeout,
    /// The daemon answered "busy" (or "starting") for longer than the caller
    /// was prepared to wait. Every one of those replies executed nothing, so
    /// nothing happened — but nothing will until the daemon frees up.
    #[error("the daemon stayed busy for {waited_secs:.1}s ({message}) — try again")]
    Busy { waited_secs: f64, message: String },
    #[error("daemon rejected connection: {0}")]
    Rejected(String),
    #[error(
        "spawned tomted but could not connect within {waited_secs}s; last error: {last}. \
         Daemon stderr (if any) is in {log}"
    )]
    SpawnedButUnreachable {
        waited_secs: u64,
        last: String,
        log: String,
    },
}

/// Pull the daemon's advertised protocol version out of its hello-rejection
/// message ("protocol version mismatch: daemon speaks 2, client speaks 3").
fn parse_daemon_version(message: &str) -> Option<u32> {
    let rest = message.split("daemon speaks ").nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// How long a user-initiated request keeps retrying a daemon that answers
/// "busy" (or "starting") before that becomes the failure the user sees.
/// Long enough to outlast a fetch (30s ceiling) plus a scan, short enough
/// that a daemon that is genuinely stuck is reported within the minute and a
/// half the user would give it anyway.
pub const DAEMON_PATIENCE: Duration = Duration::from_secs(90);

/// Pause between retries. The daemon already waited its own bound before
/// bouncing, so this only needs to avoid a hot loop.
const DAEMON_RETRY_PAUSE: Duration = Duration::from_millis(500);

/// Is the daemon on the socket a different build from the `tomted` this
/// app would spawn? `expected` is `None` when that binary cannot be read
/// (nothing to compare against — and nothing to spawn either); `running`
/// is `None` for a daemon too old to say, which IS another build.
fn stale_daemon(expected: Option<&BuildId>, running: Option<&BuildId>) -> bool {
    match expected {
        None => false,
        Some(want) => running != Some(want),
    }
}

/// Poll until nothing accepts on `socket` any more, then give the old
/// process a beat to actually exit: the listener dies before the process
/// releases the sock.lock flock, and a fresh daemon spawned in that window
/// would lose the lock and quit.
fn wait_for_socket_death(socket: &Path) {
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if UnixStream::connect(socket).is_err() {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(500));
}

pub struct IpcClient {
    writer: Mutex<UnixStream>,
    next_id: AtomicU64,
    pending: std::sync::Arc<Mutex<HashMap<u64, Sender<Response>>>>,
    events_tx: std::sync::Arc<Mutex<Option<Sender<Event>>>>,
    /// What the daemon said it runs in its hello reply.
    daemon_build: Option<BuildId>,
}

impl IpcClient {
    pub fn connect(socket: &Path) -> Result<Self, IpcError> {
        let mut client = Self::connect_raw(socket)?;
        match client.request(Request::Hello {
            version: PROTOCOL_VERSION,
        })? {
            Response::HelloOk { build, .. } => {
                client.daemon_build = build;
                Ok(client)
            }
            Response::Error { message } => {
                // Version skew: an OLD daemon owns the socket (it holds the
                // flock, so a fresh spawn can't displace it — 2026-08-08:
                // a stale daemon silently ate every new-protocol request).
                // Take over: hello at ITS version, ask it to shut down, and
                // report the mismatch so connect_or_spawn respawns.
                if let Some(theirs) = parse_daemon_version(&message)
                    && theirs != PROTOCOL_VERSION
                {
                    eprintln!(
                        "tomte: daemon speaks protocol {theirs}, we speak {PROTOCOL_VERSION} — shutting the old daemon down"
                    );
                    let _ = Self::shutdown_old_daemon(socket, theirs);
                }
                Err(IpcError::Rejected(message))
            }
            other => Err(IpcError::Proto(format!(
                "unexpected hello reply: {other:?}"
            ))),
        }
    }

    fn connect_raw(socket: &Path) -> Result<Self, IpcError> {
        let stream = UnixStream::connect(socket)?;
        let reader_stream = stream.try_clone()?;
        let pending: std::sync::Arc<Mutex<HashMap<u64, Sender<Response>>>> = Default::default();
        let events_tx: std::sync::Arc<Mutex<Option<Sender<Event>>>> = Default::default();

        let client = Self {
            writer: Mutex::new(stream),
            next_id: AtomicU64::new(1),
            pending: pending.clone(),
            events_tx: events_tx.clone(),
            daemon_build: None,
        };

        std::thread::spawn(move || {
            let reader = BufReader::new(reader_stream);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(frame) = read_frame::<ServerFrame>(&line) else {
                    continue;
                };
                match frame {
                    ServerFrame::Reply { id, response } => {
                        if let Some(tx) = pending.lock().ok().and_then(|mut p| p.remove(&id)) {
                            let _ = tx.send(response);
                        }
                    }
                    ServerFrame::Push { event } => {
                        if let Ok(guard) = events_tx.lock()
                            && let Some(tx) = guard.as_ref()
                        {
                            let _ = tx.send(event);
                        }
                    }
                }
            }
        });

        Ok(client)
    }

    /// Best-effort shutdown of an old-protocol daemon: hello at ITS version
    /// (all versions have Hello + Shutdown), then Shutdown, then wait for
    /// the socket to actually die.
    fn shutdown_old_daemon(socket: &Path, version: u32) -> Result<(), IpcError> {
        let client = Self::connect_raw(socket)?;
        match client.request(Request::Hello { version })? {
            Response::HelloOk { .. } => {}
            other => return Err(IpcError::Proto(format!("old hello failed: {other:?}"))),
        }
        let _ = client.request(Request::Shutdown);
        wait_for_socket_death(socket);
        Ok(())
    }

    /// The binary the daemon on the other end reported at hello; `None`
    /// for a daemon too old to report one.
    pub fn daemon_build(&self) -> Option<&BuildId> {
        self.daemon_build.as_ref()
    }

    /// [`Self::request`], retried across the daemon's busy bounces for up to
    /// `patience`, each retry logged under `what`. The bounce (see
    /// [`tomte_proto::is_daemon_bounce`]) is the daemon saying "I did nothing,
    /// ask again": a scan, a fetch or an auto-join held its core past its own
    /// wait. Retrying it is free, and a decision the user already recorded
    /// must not fail because one scan ran long. When the budget runs out the
    /// error is [`IpcError::Busy`], in those words.
    ///
    /// Only the bounce is retried. A client-side [`IpcError::Timeout`] is
    /// NOT: the daemon may have executed that request after the client gave
    /// up, and a second `SessionStart` or `SessionDecision` would then be a
    /// duplicate.
    pub fn request_patiently(
        &self,
        request: Request,
        patience: Duration,
        what: &str,
    ) -> Result<Response, IpcError> {
        let started = Instant::now();
        let mut attempt = 1u32;
        loop {
            match self.request(request.clone())? {
                Response::Error { message } if tomte_proto::is_daemon_bounce(&message) => {
                    let waited = started.elapsed();
                    if waited >= patience {
                        tomte_core::log_error!(
                            "ipc",
                            "{what}: daemon stayed busy for {:.1}s over {attempt} attempt(s): {message}",
                            waited.as_secs_f64()
                        );
                        return Err(IpcError::Busy {
                            waited_secs: waited.as_secs_f64(),
                            message,
                        });
                    }
                    tomte_core::log_info!(
                        "ipc",
                        "{what}: daemon busy (attempt {attempt}, {:.1}s so far), retrying: {message}",
                        waited.as_secs_f64()
                    );
                    attempt += 1;
                    std::thread::sleep(DAEMON_RETRY_PAUSE);
                }
                other => return Ok(other),
            }
        }
    }

    pub fn request(&self, request: Request) -> Result<Response, IpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        if let Ok(mut p) = self.pending.lock() {
            p.insert(id, tx);
        }
        {
            let mut w = self
                .writer
                .lock()
                .map_err(|_| IpcError::Proto("writer poisoned".into()))?;
            write_frame(&mut *w, &ClientFrame { id, request })?;
            w.flush()?;
        }
        // 30s: requests may legitimately WAIT inside the daemon for a full
        // scan to release the core (~9s over ~1k files) — see server.rs.
        rx.recv_timeout(Duration::from_secs(30)).map_err(|_| {
            if let Ok(mut p) = self.pending.lock() {
                p.remove(&id);
            }
            IpcError::Timeout
        })
    }

    pub fn subscribe(&self) -> Result<Receiver<Event>, IpcError> {
        let (tx, rx) = channel();
        if let Ok(mut guard) = self.events_tx.lock() {
            *guard = Some(tx);
        }
        match self.request(Request::Subscribe)? {
            Response::Ok => Ok(rx),
            other => Err(IpcError::Proto(format!("subscribe failed: {other:?}"))),
        }
    }

    /// Connect to the daemon on `socket`, spawning `tomted_bin` when
    /// nothing answers — or when what answers is a daemon from ANOTHER
    /// build: the installed bundle's beside a development app, the previous
    /// bundle's after an install. It speaks our protocol, so nothing would
    /// ever displace it, and every fix in this build would look unapplied
    /// (2026-10-12). That daemon is asked to shut down and ours spawned.
    pub fn connect_or_spawn(socket: &Path, tomted_bin: &Path) -> Result<Self, IpcError> {
        let expected = BuildId::of(tomted_bin);
        match Self::connect(socket) {
            Ok(c) if !stale_daemon(expected.as_ref(), c.daemon_build()) => return Ok(c),
            Ok(c) => {
                tomte_core::log_info!(
                    "ipc",
                    "the daemon at {} runs {}; this build ships {} — shutting it down to spawn ours",
                    socket.display(),
                    c.daemon_build().map_or_else(
                        || "a build with no identity (older than 2026-10-12)".to_string(),
                        |b| b.to_string()
                    ),
                    expected
                        .as_ref()
                        .map_or_else(|| "?".to_string(), |b| b.to_string())
                );
                let _ = c.request(Request::Shutdown);
                drop(c);
                wait_for_socket_death(socket);
            }
            Err(first) => {
                eprintln!(
                    "tomte: no daemon at {} ({first}); spawning tomted",
                    socket.display()
                );
            }
        }
        // Capture the daemon's output — a silently dying child was
        // undiagnosable when this was Stdio::null() (first-launch bug).
        let log_path = socket.with_file_name("tomted.spawn.log");
        // The socket's directory may not exist yet (fresh install, custom
        // TOMTE_SOCKET): creating the log must not be the reason boot fails.
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Append: the previous daemon's dying words are exactly what a
        // "why is it down" investigation needs (2026-08-10: truncation on
        // every respawn left nothing to read).
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let log_err = log.try_clone()?;
        let mut child = std::process::Command::new(tomted_bin)
            .stdout(log)
            .stderr(log_err)
            .spawn()?;
        let mut last = String::from("never attempted");
        for _ in 0..75 {
            std::thread::sleep(Duration::from_millis(200));
            // A dead child will never bind — fail fast with its exit status.
            if let Ok(Some(status)) = child.try_wait() {
                return Err(IpcError::SpawnedButUnreachable {
                    waited_secs: 0,
                    last: format!("tomted exited at startup with {status}"),
                    log: log_path.display().to_string(),
                });
            }
            match Self::connect(socket) {
                Ok(c) => return Ok(c),
                Err(e) => last = e.to_string(),
            }
        }
        Err(IpcError::SpawnedButUnreachable {
            waited_secs: 15,
            last,
            log: log_path.display().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_daemon_version, stale_daemon};
    use std::path::PathBuf;
    use tomte_proto::BuildId;

    fn build(exe: &str, modified_ms: u64) -> BuildId {
        BuildId {
            exe: PathBuf::from(exe),
            modified_ms,
        }
    }

    /// A daemon is stale when it is not exactly the binary we would spawn:
    /// another path (the bundle's daemon beside a dev build), the same path
    /// rebuilt (an in-place install), or one too old to say. With nothing
    /// to compare against there is nothing to take over.
    /// MUTATION: compare only `exe` → the same-path-rebuilt case passes as
    /// fresh and the third assertion fails.
    /// MUTATION: `running.is_none()` treated as fresh → the fourth fails.
    #[test]
    fn stale_daemon_means_any_build_but_ours() {
        let ours = build("/app/tomted", 1_000);
        assert!(!stale_daemon(Some(&ours), Some(&ours)));
        assert!(stale_daemon(
            Some(&ours),
            Some(&build("/Applications/tomted", 1_000))
        ));
        assert!(stale_daemon(
            Some(&ours),
            Some(&build("/app/tomted", 2_000))
        ));
        assert!(stale_daemon(Some(&ours), None));
        assert!(!stale_daemon(None, Some(&ours)));
        assert!(!stale_daemon(None, None));
    }

    #[test]
    fn parses_version_from_mismatch_message() {
        assert_eq!(
            parse_daemon_version("protocol version mismatch: daemon speaks 2, client speaks 3"),
            Some(2)
        );
        assert_eq!(parse_daemon_version("hello required first"), None);
        assert_eq!(parse_daemon_version("daemon speaks garbage"), None);
    }
}
