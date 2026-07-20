//! chezmoid — chezmoi-ui watcher daemon (spec §3.1).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use notify::{RecursiveMode, Watcher};

use czui_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use czui_core::cmd::SystemRunner;
use czui_core::git::GitClient;
use czui_daemon::core::DaemonCore;
use czui_daemon::debounce::Debouncer;
use czui_daemon::server::serve;
use czui_daemon::settings::{Settings, app_support_dir};
use czui_journal::Journal;

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn env_path(var: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or(default)
}

fn main() -> ExitCode {
    let once = std::env::args().any(|a| a == "--once");
    let support = app_support_dir();
    let settings = Settings::load(&env_path("CZUI_SETTINGS", support.join("settings.toml")));
    let journal_path = env_path("CZUI_JOURNAL", support.join("journal.db"));
    let socket_path = env_path("CZUI_SOCKET", support.join("daemon.sock"));

    if let Some(parent) = journal_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("chezmoid: cannot create {}: {e}", parent.display());
        return ExitCode::FAILURE;
    }

    let runner = Arc::new(SystemRunner);
    let chezmoi = ChezmoiClient::new(
        runner.clone(),
        ChezmoiOptions {
            env: settings.chezmoi_env(),
            ..ChezmoiOptions::default()
        },
    );
    let source_dir = match chezmoi.source_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("chezmoid: cannot locate chezmoi source dir: {e}");
            return ExitCode::FAILURE;
        }
    };
    let git = GitClient::new(runner, source_dir.clone());
    let branch = git.head_branch().unwrap_or_else(|_| "main".into());
    let remote_ref = format!("origin/{branch}");
    let machine = gethostname::gethostname().to_string_lossy().into_owned();
    let journal = match Journal::open(&journal_path, &machine) {
        Ok(j) => j,
        Err(e) => {
            eprintln!(
                "chezmoid: cannot open journal {}: {e}",
                journal_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let mut core = match DaemonCore::new(chezmoi, git, journal, remote_ref) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("chezmoid: init failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    match core.full_rescan(now_ts()) {
        Ok(drifted) => {
            let (list, in_sync, degraded) = core.status_snapshot();
            if let Some(hint) = &degraded {
                eprintln!("degraded scan: {hint}");
            }
            for d in &list {
                println!("{:<22} {}", d.class, d.target.display());
            }
            println!("-- {drifted} drifted, {in_sync} in sync");
        }
        Err(e) => {
            eprintln!("chezmoid: initial scan failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    if once {
        return ExitCode::SUCCESS;
    }

    let core = Arc::new(Mutex::new(core));

    // watcher → debouncer
    let (debouncer, tx) = Debouncer::new(Duration::from_millis(500));
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                for p in event.paths {
                    let _ = tx.send(p);
                }
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("chezmoid: watcher init failed: {e}");
                return ExitCode::FAILURE;
            }
        };
    {
        let c = core.lock().expect("core lock");
        for p in c.watch_paths() {
            let mode = if p == source_dir {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            if let Err(e) = watcher.watch(&p, mode) {
                eprintln!("chezmoid: watch {} failed: {e}", p.display());
            }
        }
    }

    // debounce loop (owns the watcher so watch-set deltas can be applied)
    {
        let core = core.clone();
        std::thread::spawn(move || {
            let mut watcher = watcher; // owned mutably inside the thread
            while let Some(batch) = debouncer.recv_batch() {
                let mut c = match core.lock() {
                    Ok(c) => c,
                    Err(_) => break,
                };
                let before: std::collections::BTreeSet<_> = c.watch_paths().into_iter().collect();
                if let Err(e) = c.handle_paths_changed(&batch, now_ts()) {
                    eprintln!("chezmoid: change handling failed: {e}");
                }
                let after: std::collections::BTreeSet<_> = c.watch_paths().into_iter().collect();
                drop(c);
                for removed in before.difference(&after) {
                    let _ = watcher.unwatch(removed);
                }
                for added in after.difference(&before) {
                    let _ = watcher.watch(added, RecursiveMode::NonRecursive);
                }
            }
        });
    }

    // fetch timer
    {
        let core = core.clone();
        let interval = Duration::from_secs(settings.fetch_interval_minutes.max(1) * 60);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(interval);
                if let Ok(mut c) = core.lock()
                    && let Err(e) = c.handle_fetch(now_ts())
                {
                    eprintln!("chezmoid: fetch failed: {e}");
                }
            }
        });
    }

    // hourly rescan safety net (spec §3.1: FSEvents can drop events)
    {
        let core = core.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
                if let Ok(mut c) = core.lock()
                    && let Err(e) = c.full_rescan(now_ts())
                {
                    eprintln!("chezmoid: rescan failed: {e}");
                }
            }
        });
    }

    // socket server (foreground)
    let _ = std::fs::remove_file(&socket_path);
    let listener = match std::os::unix::net::UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("chezmoid: cannot bind {}: {e}", socket_path.display());
            return ExitCode::FAILURE;
        }
    };
    println!("chezmoid: listening on {}", socket_path.display());
    if let Err(e) = serve(listener, core, now_ts) {
        eprintln!("chezmoid: server failed: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
