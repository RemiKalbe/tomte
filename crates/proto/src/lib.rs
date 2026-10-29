//! IPC wire types (spec §3.3): newline-delimited JSON with request ids.
//! Shared verbatim by tomted (server) and the app (client).

use std::io;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 4;

/// The daemon's reply when a request could not take the core within the
/// daemon's own wait for it (a scan, a fetch, an auto-join holding it).
/// Nothing executed: the request is safe to send again.
pub const BUSY_REPLY: &str = "daemon busy (scan in progress) — retry shortly";

/// Prefix of the daemon's reply while it has no core yet — `tomted
/// starting…`, or `tomted starting: <why>` once a startup attempt has
/// failed. Nothing executed: the request is safe to send again.
pub const STARTING_PREFIX: &str = "tomted starting";

/// Whether a `Response::Error` message is one of the two replies that
/// executed nothing and invite a retry. Both sides of the socket read this
/// so the client never has to guess at the daemon's wording.
pub fn is_daemon_bounce(message: &str) -> bool {
    message == BUSY_REPLY || message.starts_with(STARTING_PREFIX)
}

/// The binary a daemon process was started from: its resolved path and the
/// file's modification time, read ONCE at startup. The app compares it with
/// the `tomted` it would spawn and takes over a daemon from any other
/// build — the installed bundle's daemon still serving beside a development
/// app, or the previous bundle's after an in-place install (2026-10-12: a
/// 0.1.20 daemon served a branch build for two days, and every "the daemon
/// stayed busy" it produced looked like a bug the branch had already
/// fixed). Read at startup, not on demand: after an in-place replace the
/// file's mtime is the NEW build's while the running code is the old one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildId {
    pub exe: PathBuf,
    /// Modification time of `exe`, milliseconds since the Unix epoch.
    pub modified_ms: u64,
}

impl BuildId {
    /// The identity of the binary at `path`; `None` when it cannot be
    /// resolved or read (the caller then has nothing to compare against).
    pub fn of(path: &std::path::Path) -> Option<Self> {
        let exe = std::fs::canonicalize(path).ok()?;
        let modified = std::fs::metadata(&exe).ok()?.modified().ok()?;
        let modified_ms = modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis();
        Some(Self {
            exe,
            modified_ms: u64::try_from(modified_ms).ok()?,
        })
    }

    /// The identity of the running process's own binary.
    pub fn of_current_exe() -> Option<Self> {
        Self::of(&std::env::current_exe().ok()?)
    }
}

impl std::fmt::Display for BuildId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (mtime {}ms)", self.exe.display(), self.modified_ms)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        version: u32,
    },
    Subscribe,
    Status,
    Timeline {
        limit: u32,
        before_id: Option<i64>,
    },
    EventsFor {
        target: PathBuf,
        limit: u32,
    },
    ExpectChanges {
        paths: Vec<PathBuf>,
        ttl_secs: u32,
    },
    Rescan,
    /// Fetch origin now (acknowledged immediately; completion arrives as a
    /// FetchDone push, like Rescan/ScanDone).
    Fetch,
    /// Ask the daemon to exit cleanly (the app respawns it after settings
    /// changes; spec §9 restart-to-apply without user action).
    Shutdown,
    Pause,
    Resume,
    SnapshotBlobs {
        paths: Vec<PathBuf>,
    },
    SessionStart {
        ts: u64,
    },
    SessionDecision {
        session: i64,
        decision: serde_json::Value,
    },
    SessionEnd {
        session: i64,
        ts: u64,
        summary: String,
    },
}

/// What a managed target is, on the wire (mirrors core's `EntryKind`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireEntryKind {
    Absent,
    File,
    Folder,
    /// The link text as written, not resolved.
    Link {
        text: String,
    },
}

/// A type change between source and disk, on the wire (mirrors core's
/// `TypeChange`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireTypeChange {
    pub source: WireEntryKind,
    pub disk: WireEntryKind,
    /// False means the source changed and the disk is what chezmoi last wrote.
    pub disk_moved: bool,
}

impl WireTypeChange {
    /// The one wording table for a type change, shared by the journal meta
    /// (daemon) and the UI (app).
    pub fn label(&self) -> &'static str {
        use WireEntryKind::*;
        if self.disk_moved {
            match (&self.source, &self.disk) {
                (Link { .. }, Link { .. }) => "link points elsewhere",
                (Link { .. }, Absent) => "link removed",
                (_, Link { .. }) => "replaced by a link",
                (_, Folder) => "replaced by a folder",
                (_, File) => "replaced by a file",
                _ => "type changed",
            }
        } else {
            match (&self.source, &self.disk) {
                (Link { .. }, Link { .. }) => "source link points elsewhere",
                (Link { .. }, _) => "source is now a link",
                (Folder, _) => "source is now a folder",
                (File, _) => "source is now a file",
                _ => "type changed",
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftSummary {
    pub target: PathBuf,
    pub class: String,
    pub since_ts: Option<u64>,
    /// Origin's version of this target's source differs from local's,
    /// regardless of which side moved (decision 6.4).
    pub origin_differs: bool,
    /// Set iff `class` is `type_changed`.
    #[serde(default)]
    pub type_change: Option<WireTypeChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventSummary {
    pub id: i64,
    pub target: Option<PathBuf>,
    pub kind: String,
    pub ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    HelloOk {
        version: u32,
        machine: String,
        /// Which binary this daemon runs — see [`BuildId`]. Absent from
        /// daemons older than 2026-10-12, which the app treats as "another
        /// build" and takes over.
        #[serde(default)]
        build: Option<BuildId>,
    },
    Ok,
    Error {
        message: String,
    },
    Status {
        drifted: Vec<DriftSummary>,
        in_sync: u64,
        degraded: Option<String>,
        /// True while the daemon cannot serve real data yet (starting, or a
        /// scan holds the core) — clients must render "scanning", never
        /// "all in sync" (first-launch data-consistency bug).
        #[serde(default)]
        scanning: bool,
        /// Timestamp of the last SUCCESSFUL origin fetch, daemon-side — so
        /// freshness survives app restarts instead of living only in pushes
        /// (the 2026-08-08 perpetual "never fetched" bug).
        #[serde(default)]
        last_fetch_ts: Option<u64>,
    },
    Timeline {
        events: Vec<EventSummary>,
    },
    SessionStarted {
        session: i64,
    },
    Blobs {
        hashes: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Drift {
        target: PathBuf,
        class: String,
        ts: u64,
        #[serde(default)]
        type_change: Option<WireTypeChange>,
    },
    RemoteAdvanced {
        target: PathBuf,
        ts: u64,
    },
    EvalFailed {
        target: Option<PathBuf>,
        hint: String,
        ts: u64,
    },
    LeftManagement {
        target: PathBuf,
        ts: u64,
    },
    /// An origin fetch FAILED (locked agent, network, auth) — pushed so
    /// clients can show honest feedback instead of silence.
    FetchFailed {
        ts: u64,
        error: String,
    },
    FetchDone {
        ts: u64,
        behind: u32,
    },
    ScanDone {
        ts: u64,
        drifted: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientFrame {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    Reply {
        id: u64,
        #[serde(flatten)]
        response: Response,
    },
    Push {
        #[serde(flatten)]
        event: Event,
    },
}

pub fn write_frame<W: io::Write, T: Serialize>(w: &mut W, frame: &T) -> io::Result<()> {
    let json = serde_json::to_string(frame)?;
    w.write_all(json.as_bytes())?;
    w.write_all(b"\n")
}

pub fn read_frame<T: DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line)
}

pub fn check_hello(client_version: u32) -> Result<(), String> {
    if client_version == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(format!(
            "protocol version mismatch: daemon speaks {PROTOCOL_VERSION}, client speaks {client_version}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MUTATION: loosen to `contains("busy")` or `contains("starting")` and the
    /// two negative cases below no longer hold the line.
    #[test]
    fn is_daemon_bounce_matches_exactly_the_two_replies_that_executed_nothing() {
        assert!(is_daemon_bounce(BUSY_REPLY));
        assert!(is_daemon_bounce("tomted starting…"));
        assert!(is_daemon_bounce("tomted starting: chezmoi not found"));
        assert!(!is_daemon_bounce("daemon state poisoned"));
        assert!(!is_daemon_bounce("session decision: no such session 7"));
        assert!(!is_daemon_bounce("git busy: index.lock"));
        assert!(!is_daemon_bounce("restarting the watcher"));
    }
    use std::path::PathBuf;

    #[test]
    fn request_roundtrips_and_wire_shape_is_stable() {
        let req = ClientFrame {
            id: 7,
            request: Request::ExpectChanges {
                paths: vec![PathBuf::from("/a"), PathBuf::from("/b")],
                ttl_secs: 30,
            },
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let line = String::from_utf8(buf).unwrap();
        assert!(line.ends_with('\n'));
        assert!(
            line.contains("\"type\":\"expect_changes\""),
            "wire tag must be snake_case: {line}"
        );
        let back: ClientFrame = read_frame(line.trim_end()).unwrap();
        assert_eq!(back.id, 7);
        assert!(matches!(
            back.request,
            Request::ExpectChanges { ttl_secs: 30, .. }
        ));
    }

    #[test]
    fn server_frames_distinguish_reply_and_push() {
        let reply = ServerFrame::Reply {
            id: 3,
            response: Response::Ok,
        };
        let push = ServerFrame::Push {
            event: Event::Drift {
                target: PathBuf::from("/x"),
                class: "conflict".into(),
                ts: 42,
                type_change: None,
            },
        };
        let r = serde_json::to_string(&reply).unwrap();
        let p = serde_json::to_string(&push).unwrap();
        assert!(r.contains("\"frame\":\"reply\""));
        assert!(p.contains("\"frame\":\"push\""));
        let back: ServerFrame = read_frame(&p).unwrap();
        match back {
            ServerFrame::Push {
                event: Event::Drift { ts: 42, .. },
            } => {}
            other => panic!("bad roundtrip: {other:?}"),
        }
    }

    #[test]
    fn hello_check() {
        assert!(check_hello(PROTOCOL_VERSION).is_ok());
        let err = check_hello(PROTOCOL_VERSION + 1).unwrap_err();
        assert!(err.contains(&PROTOCOL_VERSION.to_string()));
        assert!(err.contains(&(PROTOCOL_VERSION + 1).to_string()));
    }

    #[test]
    fn every_request_variant_roundtrips() {
        let variants = vec![
            Request::Hello { version: 1 },
            Request::Subscribe,
            Request::Status,
            Request::Timeline {
                limit: 50,
                before_id: Some(9),
            },
            Request::EventsFor {
                target: PathBuf::from("/t"),
                limit: 5,
            },
            Request::ExpectChanges {
                paths: vec![],
                ttl_secs: 1,
            },
            Request::Rescan,
            Request::Shutdown,
            Request::Pause,
            Request::Resume,
            Request::SnapshotBlobs {
                paths: vec![PathBuf::from("/s")],
            },
            Request::SessionStart { ts: 1 },
            Request::SessionDecision {
                session: 2,
                decision: serde_json::json!({"c": "ours"}),
            },
            Request::SessionEnd {
                session: 2,
                ts: 3,
                summary: "done".into(),
            },
        ];
        for v in variants {
            let s = serde_json::to_string(&v).unwrap();
            let back: Request = read_frame(&s).unwrap();
            assert_eq!(
                std::mem::discriminant(&v),
                std::mem::discriminant(&back),
                "variant changed through roundtrip: {s}"
            );
        }
    }

    /// MUTATION: replace `#[serde(default)]` on `DriftSummary.type_change` with
    /// `#[serde(deserialize_with = "Option::deserialize")]` (required key; plain
    /// removal is a no-op for an Option);
    /// a line from an older daemon then fails to parse.
    #[test]
    fn drift_summary_without_type_change_still_parses() {
        let old = r#"{"target":"/t","class":"conflict","since_ts":1,"origin_differs":false}"#;
        let d: DriftSummary = serde_json::from_str(old).unwrap();
        assert!(d.type_change.is_none());
        let old_ev = r#"{"type":"drift","target":"/t","class":"conflict","ts":1}"#;
        let e: Event = serde_json::from_str(old_ev).unwrap();
        assert!(matches!(
            e,
            Event::Drift {
                type_change: None,
                ..
            }
        ));
    }

    /// MUTATION: rename the `text` field of `WireEntryKind::Link` (or drop
    /// the `Link` arm's payload); the roundtrip no longer matches.
    #[test]
    fn type_change_roundtrips() {
        let tc = WireTypeChange {
            source: WireEntryKind::Folder,
            disk: WireEntryKind::Link {
                text: "/elsewhere".into(),
            },
            disk_moved: true,
        };
        let line = serde_json::to_string(&tc).unwrap();
        assert!(line.contains("\"kind\":\"link\""), "{line}");
        let back: WireTypeChange = serde_json::from_str(&line).unwrap();
        assert_eq!(back, tc);
    }
}
