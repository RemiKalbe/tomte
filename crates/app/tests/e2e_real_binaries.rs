//! End-to-end connectivity with the REAL compiled binaries — the test the
//! in-process suites couldn't be: `tomte --verify-connectivity` spawning
//! the actual `tomted`, over a real socket, against a scratch chezmoi home.
//! This is the exact path the GUI takes at boot (minus windows).

use std::path::PathBuf;
use std::process::Command;

use tomte_core::testsupport::Scratch;

fn tomte_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tomte"))
}

/// The real tomted binary. CARGO_BIN_EXE_* only exists for the crate's own
/// bins, so build the daemon's and locate it next to ours in target/.
///
/// Built ONCE per test process: these tests run in parallel threads, and
/// concurrent `cargo build` invocations re-linking target/debug/tomted while
/// a sibling test's subprocess exec'd it produced ENOENT on cold CI runners
/// (2026-08-09).
fn tomted_bin() -> PathBuf {
    static TOMTED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    TOMTED
        .get_or_init(|| {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "tomte-daemon", "--bin", "tomted"])
                .status()
                .expect("cargo build tomted");
            assert!(status.success(), "building tomted failed");
            // target/debug/deps/../tomte → target/debug/tomted
            let mut dir = tomte_bin();
            dir.pop();
            if dir.ends_with("deps") {
                dir.pop();
            }
            let bin = dir.join("tomted");
            assert!(bin.is_file(), "tomted not found at {}", bin.display());
            bin
        })
        .clone()
}

struct E2e {
    scratch: Scratch,
    tomted: PathBuf,
}

impl E2e {
    fn new() -> Self {
        let scratch = Scratch::new();
        // chezmoi derives everything from HOME: link the scratch source repo
        // to <home>/.local/share/chezmoi so the REAL binaries need no flags.
        let share = scratch.home.join(".local/share");
        std::fs::create_dir_all(&share).unwrap();
        std::os::unix::fs::symlink(&scratch.source, share.join("chezmoi")).unwrap();
        Self {
            scratch,
            tomted: tomted_bin(),
        }
    }

    fn socket(&self) -> PathBuf {
        self.scratch.root.path().join("e2e.sock")
    }

    /// Run `tomte --verify-connectivity` in a fully scratch environment.
    /// `tomted_override` lets tests prove the no-spawn path.
    fn verify(&self, tomted_override: Option<&str>) -> (bool, String) {
        // HOME=<scratch home>: the real binaries then resolve the scratch
        // source (~/.local/share/chezmoi symlink) and destination (~) with
        // zero flags — exactly like production, hermetically.
        let out = Command::new(tomte_bin())
            .arg("--verify-connectivity")
            .env("TOMTE_SOCKET", self.socket())
            .env(
                "TOMTE_JOURNAL",
                self.scratch.root.path().join("e2e-journal.db"),
            )
            .env(
                "TOMTE_SETTINGS",
                self.scratch.root.path().join("e2e-settings.toml"),
            )
            .env(
                "TOMTE_DAEMON",
                tomted_override.unwrap_or(self.tomted.to_str().unwrap()),
            )
            .env("HOME", &self.scratch.home)
            .output()
            .expect("run tomte --verify-connectivity");
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }
}

#[test]
fn app_spawns_real_daemon_and_full_boot_path_works() {
    let e2e = E2e::new();
    let (ok, log) = e2e.verify(None);
    assert!(ok, "verify-connectivity failed:\n{log}");
    assert!(log.contains("CONNECTIVITY OK"), "{log}");
    assert!(log.contains("[5/5] push received"), "{log}");
}

#[test]
fn app_connects_to_already_running_daemon_without_spawning() {
    let e2e = E2e::new();
    // Start the real daemon ourselves…
    let mut daemon = Command::new(&e2e.tomted)
        .env("TOMTE_SOCKET", e2e.socket())
        .env(
            "TOMTE_JOURNAL",
            e2e.scratch.root.path().join("pre-journal.db"),
        )
        .env(
            "TOMTE_SETTINGS",
            e2e.scratch.root.path().join("pre-settings.toml"),
        )
        .env("HOME", &e2e.scratch.home)
        .spawn()
        .expect("start tomted");
    // Wait for it to bind (it needs a moment to start) before verifying.
    let sock = e2e.socket();
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(sock.exists(), "pre-started daemon never bound its socket");
    // …then verify with a nonexistent tomted path: connecting must succeed
    // with no spawn possible.
    let (ok, log) = e2e.verify(Some("/nonexistent/tomted"));
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert!(ok, "verify against pre-started daemon failed:\n{log}");
    assert!(log.contains("CONNECTIVITY OK"), "{log}");
}

#[test]
fn stale_socket_file_is_reclaimed() {
    let e2e = E2e::new();
    // A socket file with no listener behind it (daemon crashed / machine
    // rebooted). The daemon must reclaim it; the app must then connect.
    let _ = std::os::unix::net::UnixListener::bind(e2e.socket()).expect("create then abandon");
    // dropping the listener leaves the file behind with no accept queue
    let (ok, log) = e2e.verify(None);
    assert!(ok, "verify with stale socket failed:\n{log}");
    assert!(log.contains("CONNECTIVITY OK"), "{log}");
}

#[test]
fn second_daemon_defers_to_the_first() {
    let e2e = E2e::new();
    let (ok, log) = e2e.verify(None);
    assert!(ok, "{log}");
    // A second real daemon started against the same socket must exit
    // "already running" instead of stealing the path.
    let out = Command::new(&e2e.tomted)
        .env("TOMTE_SOCKET", e2e.socket())
        .env(
            "TOMTE_JOURNAL",
            e2e.scratch.root.path().join("dup-journal.db"),
        )
        .env(
            "TOMTE_SETTINGS",
            e2e.scratch.root.path().join("dup-settings.toml"),
        )
        .env("HOME", &e2e.scratch.home)
        .output()
        .expect("second daemon");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "second daemon errored: {text}");
    // Two legitimate defer paths: the flock loser (single-instance lock,
    // pre-socket) or the healthy-daemon probe ("already running").
    assert!(
        text.contains("already running") || text.contains("another instance holds"),
        "{text}"
    );
}

#[test]
fn daemon_serves_degraded_status_while_chezmoi_is_unavailable() {
    // The bug that ate a whole evening: chezmoi hanging (locked secret
    // manager) used to kill tomted at startup, leaving the app retrying a
    // dead socket forever. Now the daemon binds first and reports why it's
    // stuck. Simulate "chezmoi unavailable" with a PATH that has no chezmoi.
    let e2e = E2e::new();
    let mut daemon = Command::new(&e2e.tomted)
        .env("TOMTE_SOCKET", e2e.socket())
        .env(
            "TOMTE_JOURNAL",
            e2e.scratch.root.path().join("deg-journal.db"),
        )
        .env(
            "TOMTE_SETTINGS",
            e2e.scratch.root.path().join("deg-settings.toml"),
        )
        .env("HOME", &e2e.scratch.home)
        .env("PATH", "/usr/bin:/bin") // no chezmoi here
        .spawn()
        .expect("start tomted");
    let sock = e2e.socket();
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(sock.exists(), "daemon must bind BEFORE resolving chezmoi");

    let out = Command::new(tomte_bin())
        .arg("--print-status")
        .env("TOMTE_SOCKET", &sock)
        .env("HOME", &e2e.scratch.home)
        .output()
        .expect("print-status");
    let _ = daemon.kill();
    let _ = daemon.wait();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "print-status failed: {text}");
    assert!(
        text.contains("degraded: tomted starting"),
        "expected a 'starting' degraded status, got: {text}"
    );
}

#[test]
fn fetch_request_updates_freshness_and_pushes_fetch_done() {
    use std::time::Duration;
    use tomte_app::ipc::IpcClient;
    use tomte_proto::{Event, Request, Response};

    let e2e = E2e::new();
    // Boot the real daemon binary against the scratch home (its origin is a
    // local bare repo, so Fetch is a real `git fetch`).
    let mut daemon = Command::new(&e2e.tomted)
        .env("TOMTE_SOCKET", e2e.socket())
        .env(
            "TOMTE_JOURNAL",
            e2e.scratch.root.path().join("fetch-journal.db"),
        )
        .env(
            "TOMTE_SETTINGS",
            e2e.scratch.root.path().join("fetch-settings.toml"),
        )
        .env("HOME", &e2e.scratch.home)
        .spawn()
        .expect("start tomted");
    let sock = e2e.socket();
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let client = IpcClient::connect(&sock).expect("connect");
    let events = client.subscribe().expect("subscribe");

    // Manual fetch: acknowledged immediately; retry through busy/starting.
    for _ in 0..60 {
        match client.request(Request::Fetch) {
            Ok(Response::Ok) => break,
            Ok(Response::Error { message })
                if message.contains("busy") || message.contains("starting") =>
            {
                std::thread::sleep(Duration::from_millis(500));
            }
            other => panic!("fetch request failed: {other:?}"),
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match events.recv_timeout(left) {
            Ok(Event::FetchDone { ts, .. }) => {
                assert!(ts > 0);
                break;
            }
            Ok(_) => continue,
            Err(e) => panic!("no FetchDone push: {e}"),
        }
    }
    // Freshness survives in Status (not just the push) — the perpetual
    // "never fetched" fix. Immediately after the push the fetch thread may
    // still hold the core (busy Status carries no ts), so poll briefly like
    // a real client would.
    let mut carried = false;
    for _ in 0..20 {
        match client.request(Request::Status) {
            Ok(Response::Status {
                last_fetch_ts: Some(_),
                ..
            }) => {
                carried = true;
                break;
            }
            Ok(Response::Status { .. }) => std::thread::sleep(Duration::from_millis(250)),
            other => panic!("status failed: {other:?}"),
        }
    }
    assert!(carried, "Status must carry last_fetch_ts once idle");
    let _ = client.request(Request::Shutdown);
    let _ = daemon.wait();
}
