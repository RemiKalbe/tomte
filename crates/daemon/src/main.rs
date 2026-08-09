//! tomted — tomte watcher daemon (spec §3.1).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use notify::{RecursiveMode, Watcher};

use tomte_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use tomte_core::cmd::SystemRunner;
use tomte_core::git::GitClient;
use tomte_daemon::core::DaemonCore;
use tomte_daemon::debounce::Debouncer;
use tomte_daemon::server::serve;
use tomte_daemon::settings::{Settings, app_support_dir};
use tomte_journal::Journal;

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Probe the socket: connect, send Hello, expect a reply within 1s.
fn healthy_daemon_at(socket: &std::path::Path) -> bool {
    use std::io::{BufRead, BufReader, Write};
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let frame = tomte_proto::ClientFrame {
        id: 0,
        request: tomte_proto::Request::Hello {
            version: tomte_proto::PROTOCOL_VERSION,
        },
    };
    if tomte_proto::write_frame(&mut stream, &frame).is_err() || stream.flush().is_err() {
        return false;
    }
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).is_ok() && line.contains("hello_ok")
}

fn env_path(var: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or(default)
}

/// Everything that needs a working chezmoi: clients, source dir, journal,
/// core. Fails fast; the caller decides whether to retry.
fn build_core(
    settings: &Settings,
    journal_path: &std::path::Path,
    machine: &str,
    subscribers: Option<Arc<Mutex<Vec<std::sync::mpsc::Sender<tomte_proto::Event>>>>>,
) -> Result<DaemonCore, Box<dyn std::error::Error>> {
    let runner = Arc::new(SystemRunner);
    let chezmoi = ChezmoiClient::new(
        runner.clone(),
        ChezmoiOptions {
            env: settings.chezmoi_env(),
            ..ChezmoiOptions::default()
        },
    );
    let source_dir = chezmoi.source_dir()?;
    let git = GitClient::new(runner, source_dir);
    let branch = git.head_branch().unwrap_or_else(|_| "main".into());
    let remote_ref = format!("origin/{branch}");
    let journal = Journal::open(journal_path, machine)?;
    let core = match subscribers {
        Some(subs) => DaemonCore::new_with_subscribers(chezmoi, git, journal, remote_ref, subs)?,
        None => DaemonCore::new(chezmoi, git, journal, remote_ref)?,
    };
    Ok(core)
}

/// Drop PATH entries under /var/folders (per-session temp shims, e.g.
/// terminal-app CLI wrappers): a long-lived daemon inheriting them ends up
/// resolving tools through shims whose session died — the 24h-wedge bug.
fn sanitize_path() {
    let Some(path) = std::env::var_os("PATH") else {
        return;
    };
    let kept: Vec<_> = std::env::split_paths(&path)
        .filter(|p| {
            let ephemeral = p.starts_with("/var/folders") || p.starts_with("/private/var/folders");
            if ephemeral {
                eprintln!("tomted: dropping ephemeral PATH entry {}", p.display());
            }
            !ephemeral
        })
        .collect();
    if let Ok(joined) = std::env::join_paths(kept) {
        // Single-threaded at this point in main; no concurrent env access.
        unsafe { std::env::set_var("PATH", joined) };
    }
}

fn main() -> ExitCode {
    // SIG_IGN dispositions survive exec, and whoever spawned us may have
    // been launched with SIGTERM ignored (observed 2026-08-08: such daemons
    // shrugged off pkill and squatted the socket). A daemon must die on
    // TERM no matter its ancestry.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
    }
    let once = std::env::args().any(|a| a == "--once");
    sanitize_path();
    let support = app_support_dir();
    let settings = Settings::load(&env_path("TOMTE_SETTINGS", support.join("settings.toml")));
    let journal_path = env_path("TOMTE_JOURNAL", support.join("journal.db"));
    let socket_path = env_path("TOMTE_SOCKET", support.join("daemon.sock"));

    if let Some(parent) = journal_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("tomted: cannot create {}: {e}", parent.display());
        return ExitCode::FAILURE;
    }

    let machine = gethostname::gethostname().to_string_lossy().into_owned();

    if once {
        // CLI smoke mode: build everything inline, failing fast is correct.
        let mut core = match build_core(&settings, &journal_path, &machine, None) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("tomted: init failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        return match core.full_rescan(now_ts()) {
            Ok(drifted) => {
                let (list, in_sync, degraded) = core.status_snapshot();
                if let Some(hint) = &degraded {
                    eprintln!("degraded scan: {hint}");
                }
                for d in &list {
                    println!("{:<22} {}", d.class, d.target.display());
                }
                println!("-- {drifted} drifted, {in_sync} in sync");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("tomted: initial scan failed: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // Single-instance lock, held for the process lifetime: concurrent
    // spawns race unlink+bind and orphan each other (observed 2026-08-02:
    // nine daemons each serving a socket inode nobody could reach). flock
    // makes losers exit before touching the socket path at all.
    let lock_path = socket_path.with_extension("sock.lock");
    let lock_file = match std::fs::File::create(&lock_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("tomted: cannot create {}: {e}", lock_path.display());
            return ExitCode::FAILURE;
        }
    };
    {
        use std::os::fd::AsRawFd as _;
        // SAFETY: flock on a fd we own; LOCK_NB never blocks.
        if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            println!(
                "tomted: another instance holds {} — exiting",
                lock_path.display()
            );
            return ExitCode::SUCCESS;
        }
    }
    // Keep the lock for the daemon's lifetime.
    std::mem::forget(lock_file);

    // Single-instance guard: if a healthy daemon already answers on the
    // socket, exit instead of stealing the path from it (spawn races from
    // the app must converge on ONE daemon). A stale socket file (no
    // listener / no Hello reply) is reclaimed below.
    if healthy_daemon_at(&socket_path) {
        println!("tomted: already running at {}", socket_path.display());
        return ExitCode::SUCCESS;
    }

    // Bind the socket BEFORE the initial scan: the scan takes seconds on
    // large dotfile sets and clients must be able to connect immediately
    // (their requests block on the core mutex until the scan finishes).
    let _ = std::fs::remove_file(&socket_path);
    let listener = match std::os::unix::net::UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tomted: cannot bind {}: {e}", socket_path.display());
            return ExitCode::FAILURE;
        }
    };
    println!("tomted: listening on {}", socket_path.display());

    // Self-reap when displaced: if the socket path ever stops pointing at
    // OUR bound inode (a successor re-bound it, or the file vanished), this
    // process is an unreachable orphan — exit so it can't accumulate.
    {
        use std::os::unix::fs::MetadataExt as _;
        let our_ino = match std::fs::metadata(&socket_path) {
            Ok(m) => m.ino(),
            Err(_) => 0,
        };
        let path = socket_path.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(30));
                let current = std::fs::metadata(&path).map(|m| m.ino()).unwrap_or(0);
                if current != our_ino {
                    eprintln!(
                        "tomted: socket path no longer ours (displaced by a successor) — exiting"
                    );
                    std::process::exit(0);
                }
            }
        });
    }
    // Serve immediately with an empty core: clients get instant Hello and an
    // honest "starting" status while we fight a possibly-slow chezmoi below
    // (a locked secret manager can stall it for minutes).
    let subscribers: Arc<Mutex<Vec<std::sync::mpsc::Sender<tomte_proto::Event>>>> = Arc::default();
    let on_shutdown: Arc<dyn Fn() + Send + Sync> = Arc::new(|| std::process::exit(0));
    let ctx = tomte_daemon::server::ServeCtx::starting(
        subscribers.clone(),
        machine.clone(),
        now_ts,
        on_shutdown,
    );
    let server = {
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Err(e) = serve(listener, ctx) {
                eprintln!("tomted: server failed: {e}");
            }
        })
    };

    // Build the core with retries — but NOT forever: a daemon that cannot
    // reach chezmoi is squatting the socket and blocking healthy respawns
    // (the single-instance guard defers to it). Better dead: the app's
    // reconnect loop respawns us in a fresh context.
    let startup_deadline = std::time::Instant::now() + Duration::from_secs(300);
    let core = loop {
        match build_core(
            &settings,
            &journal_path,
            &machine,
            Some(subscribers.clone()),
        ) {
            Ok(core) => break Arc::new(Mutex::new(core)),
            Err(e) => {
                eprintln!(
                    "tomted[t={}]: startup blocked ({e}); retrying in 10s",
                    now_ts()
                );
                ctx.set_starting_error(e.to_string());
                if std::time::Instant::now() > startup_deadline {
                    eprintln!(
                        "tomted[t={}]: startup failed for 5 minutes — exiting so a fresh spawn can take over",
                        now_ts()
                    );
                    return ExitCode::FAILURE;
                }
                std::thread::sleep(Duration::from_secs(10));
            }
        }
    };
    ctx.set_core(core.clone());
    println!("tomted: core ready");

    // Initial scan. A failure must not kill the daemon — the hourly rescan
    // and manual Rescan requests can recover it (spec §10).
    match core.lock().expect("core lock").full_rescan(now_ts()) {
        Ok(drifted) => println!("tomted: initial scan done, {drifted} drifted"),
        Err(e) => eprintln!("tomted: initial scan failed: {e}"),
    }

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
                eprintln!("tomted: watcher init failed: {e}");
                return ExitCode::FAILURE;
            }
        };
    let source_dir = core.lock().expect("core lock").source_dir().to_path_buf();
    {
        let c = core.lock().expect("core lock");
        for p in c.watch_paths() {
            let mode = if p == source_dir {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            if let Err(e) = watcher.watch(&p, mode) {
                eprintln!("tomted: watch {} failed: {e}", p.display());
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
                    eprintln!("tomted: change handling failed: {e}");
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

    // fetch timer — first fetch RIGHT AWAY (a fresh daemon used to show
    // "never fetched" for a whole interval, 2026-08-08), then the interval.
    {
        let core = core.clone();
        let interval = Duration::from_secs(settings.fetch_interval_minutes.max(1) * 60);
        std::thread::spawn(move || {
            loop {
                if let Ok(mut c) = core.lock()
                    && let Err(e) = c.handle_fetch(now_ts())
                {
                    eprintln!("tomted: fetch failed: {e}");
                }
                std::thread::sleep(interval);
            }
        });
    }

    // hourly rescan safety net (spec §3.1: FSEvents can drop events)
    {
        let core = core.clone();
        std::thread::spawn(move || {
            // Hourly safety rescan — but while DEGRADED (locked 1Password,
            // broken decryption…), re-check every minute so recovery is
            // visible moments after the user fixes the cause, not up to an
            // hour later (2026-08-03 UX report: self-heal felt like magic).
            let mut ticks_since_scan = 0u32;
            loop {
                std::thread::sleep(Duration::from_secs(60));
                ticks_since_scan += 1;
                let degraded = core
                    .try_lock()
                    .map(|c| c.status_snapshot().2.is_some())
                    .unwrap_or(false);
                if !degraded && ticks_since_scan < 60 {
                    continue;
                }
                ticks_since_scan = 0;
                if let Ok(mut c) = core.lock()
                    && let Err(e) = c.full_rescan(now_ts())
                {
                    eprintln!("tomted: rescan failed: {e}");
                }
            }
        });
    }

    // The accept loop owns the process from here.
    let _ = server.join();
    ExitCode::SUCCESS
}
