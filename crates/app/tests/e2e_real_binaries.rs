//! End-to-end connectivity with the REAL compiled binaries — the test the
//! in-process suites couldn't be: `chezmoi-ui --verify-connectivity` spawning
//! the actual `chezmoid`, over a real socket, against a scratch chezmoi home.
//! This is the exact path the GUI takes at boot (minus windows).

use std::path::PathBuf;
use std::process::Command;

use czui_core::testsupport::Scratch;

fn chezmoi_ui_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_chezmoi-ui"))
}

/// The real chezmoid binary. CARGO_BIN_EXE_* only exists for the crate's own
/// bins, so build the daemon's and locate it next to ours in target/.
fn chezmoid_bin() -> PathBuf {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "czui-daemon", "--bin", "chezmoid"])
        .status()
        .expect("cargo build chezmoid");
    assert!(status.success(), "building chezmoid failed");
    // target/debug/deps/../chezmoi-ui → target/debug/chezmoid
    let mut dir = chezmoi_ui_bin();
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let bin = dir.join("chezmoid");
    assert!(bin.is_file(), "chezmoid not found at {}", bin.display());
    bin
}

struct E2e {
    scratch: Scratch,
    chezmoid: PathBuf,
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
            chezmoid: chezmoid_bin(),
        }
    }

    fn socket(&self) -> PathBuf {
        self.scratch.root.path().join("e2e.sock")
    }

    /// Run `chezmoi-ui --verify-connectivity` in a fully scratch environment.
    /// `chezmoid_override` lets tests prove the no-spawn path.
    fn verify(&self, chezmoid_override: Option<&str>) -> (bool, String) {
        // HOME=<scratch home>: the real binaries then resolve the scratch
        // source (~/.local/share/chezmoi symlink) and destination (~) with
        // zero flags — exactly like production, hermetically.
        let out = Command::new(chezmoi_ui_bin())
            .arg("--verify-connectivity")
            .env("CZUI_SOCKET", self.socket())
            .env(
                "CZUI_JOURNAL",
                self.scratch.root.path().join("e2e-journal.db"),
            )
            .env(
                "CZUI_SETTINGS",
                self.scratch.root.path().join("e2e-settings.toml"),
            )
            .env(
                "CZUI_CHEZMOID",
                chezmoid_override.unwrap_or(self.chezmoid.to_str().unwrap()),
            )
            .env("HOME", &self.scratch.home)
            .output()
            .expect("run chezmoi-ui --verify-connectivity");
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
    let mut daemon = Command::new(&e2e.chezmoid)
        .env("CZUI_SOCKET", e2e.socket())
        .env(
            "CZUI_JOURNAL",
            e2e.scratch.root.path().join("pre-journal.db"),
        )
        .env(
            "CZUI_SETTINGS",
            e2e.scratch.root.path().join("pre-settings.toml"),
        )
        .env("HOME", &e2e.scratch.home)
        .spawn()
        .expect("start chezmoid");
    // Wait for it to bind (it needs a moment to start) before verifying.
    let sock = e2e.socket();
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(sock.exists(), "pre-started daemon never bound its socket");
    // …then verify with a nonexistent chezmoid path: connecting must succeed
    // with no spawn possible.
    let (ok, log) = e2e.verify(Some("/nonexistent/chezmoid"));
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
    let out = Command::new(&e2e.chezmoid)
        .env("CZUI_SOCKET", e2e.socket())
        .env(
            "CZUI_JOURNAL",
            e2e.scratch.root.path().join("dup-journal.db"),
        )
        .env(
            "CZUI_SETTINGS",
            e2e.scratch.root.path().join("dup-settings.toml"),
        )
        .env("HOME", &e2e.scratch.home)
        .output()
        .expect("second daemon");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "second daemon errored: {text}");
    assert!(text.contains("already running"), "{text}");
}
