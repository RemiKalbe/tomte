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
    #[error("request timed out")]
    Timeout,
    #[error("daemon rejected connection: {0}")]
    Rejected(String),
}

pub struct IpcClient {
    writer: Mutex<UnixStream>,
    next_id: AtomicU64,
    pending: std::sync::Arc<Mutex<HashMap<u64, Sender<Response>>>>,
    events_tx: std::sync::Arc<Mutex<Option<Sender<Event>>>>,
}

impl IpcClient {
    pub fn connect(socket: &Path) -> Result<Self, IpcError> {
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

        match client.request(Request::Hello {
            version: PROTOCOL_VERSION,
        })? {
            Response::HelloOk { .. } => Ok(client),
            Response::Error { message } => Err(IpcError::Rejected(message)),
            other => Err(IpcError::Proto(format!(
                "unexpected hello reply: {other:?}"
            ))),
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
        if let Ok(c) = Self::connect(socket) {
            return Ok(c);
        }
        let _child = std::process::Command::new(chezmoid_bin)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        for _ in 0..25 {
            std::thread::sleep(Duration::from_millis(200));
            if let Ok(c) = Self::connect(socket) {
                return Ok(c);
            }
        }
        Err(IpcError::Timeout)
    }
}
