//! `tomte --print-status` against a real daemon server: the headless
//! probe of the whole boot path (path resolution → connect → Status → print).

use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use tomte_core::chezmoi::ChezmoiClient;
use tomte_core::cmd::SystemRunner;
use tomte_core::git::GitClient;
use tomte_core::testsupport::Scratch;
use tomte_daemon::core::DaemonCore;
use tomte_daemon::server::serve;
use tomte_journal::Journal;

fn print_status(socket: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tomte"))
        .arg("--print-status")
        .env("TOMTE_SOCKET", socket)
        // --print-status must never spawn a daemon: point the spawn path at
        // a nonexistent binary so any regression fails loudly instead of
        // silently launching a real tomted from PATH.
        .env("TOMTE_DAEMON", "/nonexistent/tomted")
        .output()
        .expect("run tomte --print-status")
}

#[test]
fn print_status_reports_counts_from_a_live_daemon() {
    let s = Scratch::new();
    let chezmoi: ChezmoiClient = s.chezmoi();
    let git = GitClient::new(Arc::new(SystemRunner), s.source.clone());
    let journal = Journal::open_in_memory("print-status").unwrap();
    let mut core = DaemonCore::new(chezmoi, git, journal, "origin/main".into()).unwrap();

    // Drift the one managed file (~/.testrc) so the counts are nontrivial.
    std::fs::write(s.home.join(".testrc"), "a=live\n").unwrap();
    core.full_rescan(77).unwrap();

    let sock = s.root.path().join("d.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let core = Arc::new(Mutex::new(core));
    std::thread::spawn(move || {
        serve(
            listener,
            tomte_daemon::server::ServeCtx::ready(core, || 42, std::sync::Arc::new(|| {})),
        )
    });

    let out = print_status(&sock);
    assert!(
        out.status.success(),
        "exit: {:?}, stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout, "1 drifted, 0 in sync\n");
}

#[test]
fn print_status_fails_cleanly_without_a_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let out = print_status(&dir.path().join("no-daemon.sock"));
    assert_eq!(out.status.code(), Some(1), "connection failure exits 1");
    assert!(
        out.stdout.is_empty(),
        "no stdout on failure, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("cannot connect to tomted"),
        "stderr: {stderr}"
    );
}
