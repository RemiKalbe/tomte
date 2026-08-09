//! Blocking IPC client for chezmoid (spec §3.3). Off-main-thread only.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use czui_proto::{
    ClientFrame, Event, PROTOCOL_VERSION, Request, Response, ServerFrame, read_frame, write_frame,
};

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Proto(String),
    #[error("request timed out (connected, but no reply within 10s)")]
    Timeout,
    #[error("daemon rejected connection: {0}")]
    Rejected(String),
    #[error(
        "spawned chezmoid but could not connect within {waited_secs}s; last error: {last}. \
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

pub struct IpcClient {
    writer: Mutex<UnixStream>,
    next_id: AtomicU64,
    pending: std::sync::Arc<Mutex<HashMap<u64, Sender<Response>>>>,
    events_tx: std::sync::Arc<Mutex<Option<Sender<Event>>>>,
}

impl IpcClient {
    pub fn connect(socket: &Path) -> Result<Self, IpcError> {
        let client = Self::connect_raw(socket)?;
        match client.request(Request::Hello {
            version: PROTOCOL_VERSION,
        })? {
            Response::HelloOk { .. } => Ok(client),
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
                        "chezmoi-ui: daemon speaks protocol {theirs}, we speak {PROTOCOL_VERSION} — shutting the old daemon down"
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
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if UnixStream::connect(socket).is_err() {
                break;
            }
        }
        // The listener dies before the process exits and releases the
        // sock.lock flock; a fresh daemon spawned in that window would lose
        // the lock and quit. Give the old process a beat to actually die.
        std::thread::sleep(std::time::Duration::from_millis(500));
        Ok(())
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
        rx.recv_timeout(Duration::from_secs(10)).map_err(|_| {
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

    pub fn connect_or_spawn(socket: &Path, chezmoid_bin: &Path) -> Result<Self, IpcError> {
        match Self::connect(socket) {
            Ok(c) => return Ok(c),
            Err(first) => {
                eprintln!(
                    "chezmoi-ui: no daemon at {} ({first}); spawning chezmoid",
                    socket.display()
                );
            }
        }
        // Capture the daemon's output — a silently dying child was
        // undiagnosable when this was Stdio::null() (first-launch bug).
        let log_path = socket.with_file_name("chezmoid.spawn.log");
        let log = std::fs::File::create(&log_path)?;
        let log_err = log.try_clone()?;
        let mut child = std::process::Command::new(chezmoid_bin)
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
                    last: format!("chezmoid exited at startup with {status}"),
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
    use super::parse_daemon_version;

    #[test]
    fn parses_version_from_mismatch_message() {
        assert_eq!(
            parse_daemon_version(
                "protocol version mismatch: daemon speaks 2, client speaks 3"
            ),
            Some(2)
        );
        assert_eq!(parse_daemon_version("hello required first"), None);
        assert_eq!(parse_daemon_version("daemon speaks garbage"), None);
    }
}
