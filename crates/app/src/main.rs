//! chezmoi-ui — menubar-resident GPUI app (spec §3.2).

// Shared modules (ipc, model, theme) live in the czui_app lib target;
// views and the AppKit platform layer stay bin-only.
mod notify_osa;
mod platform_mac;
mod views;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use czui_app::ipc::{IpcClient, IpcError};
use czui_app::model::{SyncModel, TIMELINE_CAP};
use czui_app::resolve::ResolveEngine;
use czui_core::chezmoi::{ChezmoiClient, ChezmoiError, ChezmoiOptions};
use czui_core::cmd::{CommandRunner, SystemRunner};
use czui_core::git::GitClient;
use czui_journal::Journal;
use czui_proto::{Event, Request, Response};
use gpui::{
    App, AppContext as _, Application, Bounds, Entity, WindowBounds, WindowOptions, px, size,
};
use objc2::MainThreadMarker;

use notify_osa::{drift_body, notify, remote_advanced_body};
use platform_mac::{MenuCommand, MenuSpec, StatusItem, set_accessory_policy};
use views::settings::SettingsPaths;
use views::{Route, Shell};

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// gpui global holding the resolve engine for the CURRENT daemon connection
/// (plan 6 Task 3). Absent or `None` while disconnected — views read it
/// lazily each render (`cx.try_global`) and disable their action buttons.
/// Rebuilt on every successful (re)connect because the engine's
/// `Arc<IpcClient>` dies with the connection; an action racing a reconnect
/// fails with an IpcError outcome, which the UI reports honestly.
pub struct EngineSlot(pub Option<Arc<ResolveEngine>>);

impl gpui::Global for EngineSlot {}

/// Build the resolve engine for one daemon connection. Blocking — the
/// `chezmoi source-path`-style `source_dir` lookup is a subprocess — so
/// callers run it on the background executor. The app builds its own
/// chezmoi/git clients (options default; the app reads no settings today),
/// mirroring czui-daemon's `build_core` shape.
fn build_engine(ipc: Arc<IpcClient>, journal_path: PathBuf) -> Result<ResolveEngine, ChezmoiError> {
    let runner: Arc<dyn CommandRunner> = Arc::new(SystemRunner);
    let chezmoi = ChezmoiClient::new(runner.clone(), ChezmoiOptions::default());
    let source_dir = chezmoi.source_dir()?;
    let git = GitClient::new(runner, source_dir);
    Ok(ResolveEngine {
        chezmoi,
        git,
        ipc,
        journal_path,
    })
}

/// Resolved daemon-facing paths. Env overrides and defaults must match
/// chezmoid's (`czui_daemon::settings`): both sides of the socket have to
/// agree on where it lives.
struct Paths {
    socket: PathBuf,
    journal: PathBuf,
    /// Written and displayed by the Settings view; resolved here so all
    /// path policy lives in one place.
    settings: PathBuf,
    /// The chezmoid binary `connect_or_spawn` launches when the daemon is
    /// not already running.
    chezmoid: PathBuf,
}

impl Paths {
    /// The subset the Settings view displays and writes (plan Task 7),
    /// handed through the Shell so views never re-derive path policy.
    fn view_paths(&self) -> SettingsPaths {
        SettingsPaths {
            socket: self.socket.clone(),
            journal: self.journal.clone(),
            settings: self.settings.clone(),
        }
    }
}

fn env_path(var: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or(default)
}

/// Mirror of `czui_daemon::settings::app_support_dir` (the daemon crate is a
/// dev-dependency only, so the three lines are duplicated rather than linked).
fn app_support_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Library/Application Support/ChezmoiUI")
}

/// CZUI_CHEZMOID env override, else a `chezmoid` sibling of this binary
/// (how a bundled .app ships), else bare `chezmoid` resolved via PATH.
fn resolve_chezmoid() -> PathBuf {
    if let Some(p) = std::env::var_os("CZUI_CHEZMOID") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("chezmoid");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("chezmoid")
}

fn resolve_paths() -> Paths {
    let support = app_support_dir();
    Paths {
        socket: env_path("CZUI_SOCKET", support.join("daemon.sock")),
        journal: env_path("CZUI_JOURNAL", support.join("journal.db")),
        settings: env_path("CZUI_SETTINGS", support.join("settings.toml")),
        chezmoid: resolve_chezmoid(),
    }
}

/// End-to-end connectivity diagnostic (also driven by the e2e test): the
/// exact GUI boot path — spawn if needed, hello, status, subscribe, first
/// push — with one line per step so failures name their step.
fn verify_connectivity(paths: &Paths) -> ExitCode {
    use std::time::Instant;
    println!("[1/5] socket: {}", paths.socket.display());
    println!("      chezmoid: {}", paths.chezmoid.display());
    let t0 = Instant::now();
    let client = match IpcClient::connect_or_spawn(&paths.socket, &paths.chezmoid) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[2/5] FAIL connect_or_spawn: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("[2/5] connected + hello ok in {:?}", t0.elapsed());
    match client.request(Request::Status) {
        Ok(Response::Status {
            drifted,
            in_sync,
            degraded,
            ..
        }) => println!(
            "[3/5] status ok: {} drifted, {in_sync} in sync, degraded: {}",
            drifted.len(),
            degraded.as_deref().unwrap_or("no")
        ),
        Ok(other) => {
            eprintln!("[3/5] FAIL status: unexpected reply {other:?}");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("[3/5] FAIL status: {e}");
            return ExitCode::FAILURE;
        }
    }
    let events = match client.subscribe() {
        Ok(rx) => {
            println!("[4/5] subscribe ok");
            rx
        }
        Err(e) => {
            eprintln!("[4/5] FAIL subscribe: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Force a fresh scan so a push is guaranteed even if the initial scan
    // finished before we subscribed. The daemon may be mid-scan (busy) —
    // retry until it accepts.
    println!("[5/5] requesting rescan and waiting for a push, up to 120s…");
    for _ in 0..60 {
        match client.request(Request::Rescan) {
            Ok(Response::Ok) => break,
            Ok(Response::Error { message })
                if message.contains("busy") || message.contains("starting") =>
            {
                std::thread::sleep(Duration::from_secs(1));
            }
            Ok(other) => {
                eprintln!("[5/5] FAIL rescan: unexpected reply {other:?}");
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("[5/5] FAIL rescan: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    match events.recv_timeout(Duration::from_secs(120)) {
        Ok(ev) => println!("[5/5] push received after {:?}: {ev:?}", t0.elapsed()),
        Err(e) => {
            eprintln!("[5/5] FAIL: no push within 120s ({e})");
            return ExitCode::FAILURE;
        }
    }
    println!("CONNECTIVITY OK");
    ExitCode::SUCCESS
}

/// Headless status probe for CI/agents: connect (never spawn a daemon),
/// print the drift counts, exit. No windows, no status item.
fn print_status(socket: &Path) -> ExitCode {
    let client = match IpcClient::connect(socket) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "chezmoi-ui: cannot connect to chezmoid at {}: {e}",
                socket.display()
            );
            return ExitCode::FAILURE;
        }
    };
    match client.request(Request::Status) {
        Ok(Response::Status {
            drifted,
            in_sync,
            degraded,
            scanning,
        }) => {
            let _ = scanning;
            let mut line = format!("{} drifted, {} in sync", drifted.len(), in_sync);
            if let Some(hint) = degraded {
                line.push_str(&format!(", degraded: {hint}"));
            }
            println!("{line}");
            ExitCode::SUCCESS
        }
        Ok(other) => {
            eprintln!("chezmoi-ui: unexpected status reply: {other:?}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("chezmoi-ui: status request failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Value following a `--flag` argument, if present.
fn arg_value(flag: &str) -> Option<String> {
    let mut args = std::env::args();
    while let Some(a) = args.next() {
        if a == flag {
            return args.next();
        }
    }
    None
}

/// Gallery mode: open a normal window posed to a named synthetic state so a
/// screenshot tool (scripts/shoot.sh) can capture any UI state on demand —
/// no daemon, no real data. Prints `GALLERY_WINDOW_ID: <n>` for
/// `screencapture -l`. `dark`: force appearance; None follows the system.
fn run_gallery(state_name: String, dark: Option<bool>, paths: SettingsPaths) -> ExitCode {
    use objc2_app_kit::{
        NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    };

    if !views::fixtures::STATES.iter().any(|(n, _)| *n == state_name) {
        eprintln!("unknown gallery state: {state_name} (try --gallery-list)");
        return ExitCode::FAILURE;
    }

    Application::new().run(move |cx: &mut App| {
        let mtm = MainThreadMarker::new().expect("gpui runs on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        if let Some(dark) = dark {
            let name = if dark {
                unsafe { NSAppearanceNameDarkAqua }
            } else {
                unsafe { NSAppearanceNameAqua }
            };
            let appearance = NSAppearance::appearanceNamed(name);
            app.setAppearance(appearance.as_deref());
        }

        cx.activate(true);
        let bounds = Bounds::centered(None, size(px(980.), px(640.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("chezmoi ui — gallery".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(gpui::point(px(12.), px(12.))),
                }),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|cx| {
                    views::fixtures::build(&state_name, paths.clone(), cx)
                        .expect("state name validated before run()")
                })
            },
        );
        if opened.is_err() {
            eprintln!("gallery: failed to open window");
            std::process::exit(1);
        }

        // The capture script waits for this line, then shoots the window.
        let windows = app.windows();
        if let Some(win) = windows.iter().last() {
            println!("GALLERY_WINDOW_ID: {}", win.windowNumber());
            use std::io::Write as _;
            std::io::stdout().flush().ok();
        } else {
            eprintln!("gallery: no NSWindow found after open");
            std::process::exit(1);
        }
    });
    ExitCode::SUCCESS
}

/// Live mode: the REAL boot path — connect to (or spawn) the actual daemon,
/// hydrate from the real journal and chezmoi state — but open the window
/// immediately on `route` and print `GALLERY_WINDOW_ID:` for the capture
/// script. No status item (a second transient menubar icon would flicker);
/// this instance is a read-only observer that shoot.sh kills after the shot.
/// Prints `LIVE_CONNECTED` once the daemon hello lands so the script can
/// wait for real data instead of guessing with a sleep.
fn run_live(route: Route, paths: Paths) -> ExitCode {
    use objc2_app_kit::NSApplication;

    Application::new().run(move |cx: &mut App| {
        let mtm = MainThreadMarker::new().expect("gpui runs on the main thread");
        set_accessory_policy(mtm);

        let state: Entity<SyncModel> = cx.new(|_| SyncModel::default());
        cx.observe(&state, {
            let mut announced = false;
            move |state, cx| {
                if !announced && state.read(cx).connected {
                    announced = true;
                    println!("LIVE_CONNECTED");
                    use std::io::Write as _;
                    std::io::stdout().flush().ok();
                }
            }
        })
        .detach();

        let view_paths = paths.view_paths();
        spawn_boot_and_event_loop(cx, state.clone(), paths);
        open_shell(cx, route, state, view_paths);

        let app = NSApplication::sharedApplication(mtm);
        if let Some(win) = app.windows().iter().last() {
            println!("GALLERY_WINDOW_ID: {}", win.windowNumber());
            use std::io::Write as _;
            std::io::stdout().flush().ok();
        } else {
            eprintln!("live: no NSWindow found after open");
            std::process::exit(1);
        }
    });
    ExitCode::SUCCESS
}

fn open_shell(cx: &mut App, route: Route, state: Entity<SyncModel>, paths: SettingsPaths) {
    cx.activate(true);
    let bounds = Bounds::centered(None, size(px(980.), px(640.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // Zed-style chrome: the sidebar runs to the top of the window,
            // traffic lights float over it (sidebar reserves pt_10).
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("chezmoi ui".into()),
                appears_transparent: true,
                traffic_light_position: Some(gpui::point(px(12.), px(12.))),
            }),
            ..Default::default()
        },
        |_, cx| {
            cx.new(|cx| {
                // Views read the model in render; re-render this window
                // whenever the entity notifies.
                cx.observe(&state, |_, _, cx| cx.notify()).detach();
                Shell {
                    route,
                    state,
                    review: None,
                    settings: None,
                    merge: None,
                    paths,
                    expanded_scans: Default::default(),
                    dashboard_action_in_flight: false,
                }
            })
        },
    )
    .ok();
}

/// Rebuild the status item title + menu from the current model. Main thread
/// only (AppKit): callers hold `&mut App`, so the marker always exists.
fn refresh_status_item(status: &StatusItem, model: &SyncModel) {
    let mtm = MainThreadMarker::new().expect("status item is only touched with App access");
    status.set_title(mtm, &model.status_title());
    let (header, freshness, review_label, sync_all_enabled) = model.menu_spec(now_ts());
    status.set_menu(
        mtm,
        &MenuSpec {
            header,
            freshness,
            review_label,
            sync_all_enabled,
        },
    );
}

/// Poll the AppKit menu channel and route commands (v0: open a fresh shell
/// window per command; focusing an existing one arrives with the views work).
fn spawn_menu_command_loop(
    cx: &mut App,
    rx: Receiver<MenuCommand>,
    state: Entity<SyncModel>,
    paths: SettingsPaths,
) {
    cx.spawn(async move |cx| {
        loop {
            match rx.try_recv() {
                Ok(MenuCommand::OpenApp) => {
                    let _ = cx.update(|cx| {
                        open_shell(cx, Route::Dashboard, state.clone(), paths.clone())
                    });
                }
                Ok(MenuCommand::Review) => {
                    let _ =
                        cx.update(|cx| open_shell(cx, Route::Review, state.clone(), paths.clone()));
                }
                Ok(MenuCommand::Settings) => {
                    let _ = cx
                        .update(|cx| open_shell(cx, Route::Settings, state.clone(), paths.clone()));
                }
                Ok(MenuCommand::SyncAll) => {
                    let _ = cx.update(|cx| run_sync_all(cx, &state));
                }
                Ok(MenuCommand::Quit) => {
                    let _ = cx.update(|cx| cx.quit());
                    break;
                }
                Err(TryRecvError::Empty) => {
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                }
                Err(TryRecvError::Disconnected) => break,
            }
        }
    })
    .detach();
}

/// Menubar "Sync all" (spec §7.4, plan 6 Task 3): pull + apply when zero
/// decisions are pending. `menu_spec` already disables the item unless the
/// tree is clean, but menus can go stale — re-check the model here and drop
/// the command silently when the gate no longer holds. The engine call runs
/// on the background executor; the result surfaces as a notification.
fn run_sync_all(cx: &mut App, state: &Entity<SyncModel>) {
    let model = state.read(cx);
    let gate_open =
        model.connected && !model.scanning && model.degraded.is_none() && model.drifted.is_empty();
    if !gate_open {
        return;
    }
    let Some(engine) = cx
        .try_global::<EngineSlot>()
        .and_then(|slot| slot.0.clone())
    else {
        return;
    };
    cx.background_executor()
        .spawn(async move {
            let body = match engine.sync_all() {
                Ok(_) => "synced with origin".to_string(),
                Err(e) => format!("sync all failed: {e}"),
            };
            notify(&SystemRunner, "chezmoi-ui", &body);
        })
        .detach();
}

/// Coalescing window for drift notifications (spec §7.6, plan Task 7): every
/// gated new-drift push inside an open window folds into one
/// "N file(s) drifted" notification.
const DRIFT_NOTIFY_WINDOW: Duration = Duration::from_secs(5);

/// What one applied push means for the notifier (plan 5 Task 7).
#[derive(Debug, PartialEq, Eq)]
enum NotifySignal {
    /// Nothing user-facing: applied/expected events, fetches, scans, and
    /// repeat drift observations stay silent.
    None,
    /// A drift push carried news → count it into the coalescing window.
    NewDrift,
    /// Origin moved for this target → notify immediately (the daemon already
    /// filtered self-caused pushes).
    RemoteAdvanced(PathBuf),
}

/// Apply one push to the model and classify it for the notifier. The drift
/// gate rides on `SyncModel::apply_event`'s own dedup: a Drift/EvalFailed
/// push only notifies when it actually changed the drifted picture — the set
/// grew, or a class escalated into needs-attention territory. Repeat
/// observations, de-escalations, and back-in-sync transitions are silent.
fn apply_and_signal(model: &mut SyncModel, ev: Event) -> NotifySignal {
    if let Event::RemoteAdvanced { target, .. } = &ev {
        let target = target.clone();
        model.apply_event(ev);
        return NotifySignal::RemoteAdvanced(target);
    }
    let driftish = matches!(ev, Event::Drift { .. } | Event::EvalFailed { .. });
    let (len_before, attention_before) = (model.drifted.len(), model.needs_attention());
    model.apply_event(ev);
    if driftish && (model.drifted.len() > len_before || model.needs_attention() > attention_before)
    {
        NotifySignal::NewDrift
    } else {
        NotifySignal::None
    }
}

/// Boot the daemon link on the background executor (connect-or-spawn, Status,
/// read-only journal hydrate, subscribe), then apply live pushes to the model
/// entity. Every blocking IpcClient/journal call stays off the main thread.
/// Re-request Status and hydrate the model. Returns false when the app has
/// been released and the caller should stop.
async fn refresh_status(
    client: &std::sync::Arc<IpcClient>,
    state: &Entity<SyncModel>,
    cx: &mut gpui::AsyncApp,
) -> bool {
    let c = client.clone();
    let resp = cx
        .background_executor()
        .spawn(async move { c.request(Request::Status) })
        .await;
    if let Ok(Response::Status {
        drifted,
        in_sync,
        degraded,
        scanning,
    }) = resp
    {
        return cx
            .update_entity(state, |model, cx| {
                model.hydrate_status(drifted, in_sync, degraded, scanning);
                cx.notify();
            })
            .is_ok();
    }
    true
}

fn spawn_boot_and_event_loop(cx: &mut App, state: Entity<SyncModel>, paths: Paths) {
    cx.spawn(async move |cx| {
        // Reconnect forever: chezmoid may still be mid-initial-scan at app
        // launch, may be restarted after a settings save (Shutdown request),
        // or may crash — the app heals the connection on its own.
        let mut backoff_secs: u64 = 1;
        let mut last_spawn: Option<Instant> = None;
        loop {
            let (socket, journal, chezmoid) = (
                paths.socket.clone(),
                paths.journal.clone(),
                paths.chezmoid.clone(),
            );
            // Spawn at most once per minute — a dying daemon must not turn
            // the reconnect loop into a spawn storm.
            let may_spawn = last_spawn.is_none_or(|t| t.elapsed() > Duration::from_secs(60));
            if may_spawn {
                last_spawn = Some(Instant::now());
            }
            let boot = cx
                .background_executor()
                .spawn(async move {
                    let client = if may_spawn {
                        IpcClient::connect_or_spawn(&socket, &chezmoid)?
                    } else {
                        IpcClient::connect(&socket)?
                    };
                    let status = client.request(Request::Status)?;
                    // Read-only timeline hydrate: a missing journal (fresh
                    // install, daemon not yet scanned) is fine — start empty.
                    let rows = Journal::open_read_only(&journal, "app")
                        .and_then(|j| j.timeline(TIMELINE_CAP as u32, None))
                        .unwrap_or_default();
                    let events = client.subscribe()?;
                    Ok::<_, IpcError>((client, status, rows, events))
                })
                .await;

            let (raw_client, status_resp, rows, events) = match boot {
                Ok(parts) => {
                    backoff_secs = 1;
                    parts
                }
                Err(e) => {
                    eprintln!(
                        "chezmoi-ui: daemon connection failed (retrying in {backoff_secs}s): {e}"
                    );
                    cx.background_executor()
                        .timer(Duration::from_secs(backoff_secs))
                        .await;
                    backoff_secs = (backoff_secs * 2).min(15);
                    continue;
                }
            };
            // Arc: the event loop issues background Status refreshes on
            // ScanDone while the connection stays owned by this task.
            let client = std::sync::Arc::new(raw_client);
            let hydrated = cx.update_entity(&state, |model, cx| {
                model.connected = true;
                if let Response::Status {
                    drifted,
                    in_sync,
                    degraded,
                    scanning,
                } = status_resp
                {
                    model.hydrate_status(drifted, in_sync, degraded, scanning);
                }
                model.hydrate_timeline(rows);
                cx.notify();
            });
            if hydrated.is_err() {
                return; // app released
            }

            // Resolve engine for THIS connection (plan 6 Task 3): the
            // source_dir lookup is a subprocess, so build off the main
            // thread, then publish through the EngineSlot global. A failure
            // leaves the slot None — action buttons stay disabled, honestly.
            let engine = {
                let ipc = client.clone();
                let journal = paths.journal.clone();
                cx.background_executor()
                    .spawn(async move { build_engine(ipc, journal) })
                    .await
            };
            let slot = match engine {
                Ok(engine) => EngineSlot(Some(Arc::new(engine))),
                Err(e) => {
                    eprintln!("chezmoi-ui: resolve actions unavailable (source dir lookup failed): {e}");
                    EngineSlot(None)
                }
            };
            if cx.update(|cx| cx.set_global(slot)).is_err() {
                return; // app released
            }

            // Live pushes: poll without blocking the main thread (same pattern
            // as the menu-command loop; push rates are low). New drift coalesces
            // into 5s notification windows; osascript runs on the background
            // executor (plan Task 7).
            let mut pending_drift: usize = 0;
            let mut drift_window_started: Option<Instant> = None;
            // Periodic safety net: a ScanDone push can be missed (app booted
            // after the scan), so poll Status every 30s regardless — the
            // first-launch bug was exactly this staleness.
            let mut last_status_refresh = Instant::now();
            // Sick-daemon detector: a daemon reporting scanning/starting for
            // this long is stuck (healthy full scans take seconds; startup
            // retries are capped at 5min daemon-side). Shoot it — the
            // reconnect loop respawns a fresh one. This is what finally kills
            // the "zombie daemon squats the socket forever" class.
            const STUCK_LIMIT: Duration = Duration::from_secs(180);
            let mut stuck_since: Option<Instant> = None;
            loop {
                // Flush an expired coalescing window: exactly one notification
                // per window, no matter how many pushes landed inside it.
                if let Some(started) = drift_window_started
                    && started.elapsed() >= DRIFT_NOTIFY_WINDOW
                {
                    let body = drift_body(pending_drift);
                    pending_drift = 0;
                    drift_window_started = None;
                    cx.background_executor()
                        .spawn(async move { notify(&SystemRunner, "chezmoi-ui", &body) })
                        .detach();
                }
                match events.try_recv() {
                    Ok(ev) => {
                        let is_scan_done = matches!(ev, Event::ScanDone { .. });
                        let signal = cx.update_entity(&state, |model, cx| {
                            let signal = apply_and_signal(model, ev);
                            cx.notify();
                            signal
                        });
                        if is_scan_done {
                            last_status_refresh = Instant::now();
                            if !refresh_status(&client, &state, cx).await {
                                return;
                            }
                        }
                        match signal {
                            Err(_) => break, // app released
                            Ok(NotifySignal::NewDrift) => {
                                pending_drift += 1;
                                drift_window_started.get_or_insert_with(Instant::now);
                            }
                            Ok(NotifySignal::RemoteAdvanced(target)) => {
                                cx.background_executor()
                                    .spawn(async move {
                                        notify(
                                            &SystemRunner,
                                            "chezmoi-ui",
                                            &remote_advanced_body(&target),
                                        );
                                    })
                                    .detach();
                            }
                            Ok(NotifySignal::None) => {}
                        }
                    }
                    Err(TryRecvError::Empty) => {
                        cx.background_executor()
                            .timer(Duration::from_millis(100))
                            .await;
                        if last_status_refresh.elapsed() >= Duration::from_secs(30) {
                            last_status_refresh = Instant::now();
                            if !refresh_status(&client, &state, cx).await {
                                return;
                            }
                            let scanning = cx
                                .update_entity(&state, |model, _| model.scanning)
                                .unwrap_or(false);
                            match (scanning, stuck_since) {
                                (false, _) => stuck_since = None,
                                (true, None) => stuck_since = Some(Instant::now()),
                                (true, Some(since)) if since.elapsed() > STUCK_LIMIT => {
                                    eprintln!(
                                        "chezmoi-ui: daemon stuck in scanning/starting for {}s — restarting it",
                                        since.elapsed().as_secs()
                                    );
                                    let c = client.clone();
                                    cx.background_executor()
                                        .spawn(async move {
                                            let _ = c.request(Request::Shutdown);
                                        })
                                        .detach();
                                    stuck_since = None;
                                }
                                (true, Some(_)) => {}
                            }
                        }
                    }
                    Err(TryRecvError::Disconnected) => {
                        // Daemon went away (settings restart, crash): show it,
                        // then fall through to the reconnect loop.
                        if cx
                            .update_entity(&state, |model, cx| {
                                model.connected = false;
                                cx.notify();
                            })
                            .is_err()
                        {
                            return; // app released
                        }
                        break;
                    }
                }
            }
            drop(client);
        }
    })
    .detach();
}

/// Every 30s rebuild the menu from the latest model so relative freshness
/// ("fetched 3m ago") stays honest even when no events arrive.
fn spawn_freshness_loop(cx: &mut App, state: Entity<SyncModel>, status: Rc<StatusItem>) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_secs(30))
                .await;
            if cx
                .update(|cx| refresh_status_item(&status, state.read(cx)))
                .is_err()
            {
                break; // app released
            }
        }
    })
    .detach();
}

fn main() -> ExitCode {
    let paths = resolve_paths();
    if std::env::args().any(|a| a == "--verify-connectivity") {
        return verify_connectivity(&resolve_paths());
    }
    if std::env::args().any(|a| a == "--print-status") {
        return print_status(&paths.socket);
    }
    if std::env::args().any(|a| a == "--gallery-list") {
        for (name, desc) in views::fixtures::STATES {
            println!("{name}\t{desc}");
        }
        return ExitCode::SUCCESS;
    }
    if let Some(route) = arg_value("--live") {
        let route = match route.as_str() {
            "dashboard" => Route::Dashboard,
            "review" => Route::Review,
            "settings" => Route::Settings,
            other => {
                eprintln!("unknown live route: {other} (dashboard|review|settings)");
                return ExitCode::FAILURE;
            }
        };
        return run_live(route, paths);
    }
    if let Some(state) = arg_value("--gallery") {
        let dark = match (
            std::env::args().any(|a| a == "--dark"),
            std::env::args().any(|a| a == "--light"),
        ) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            _ => None,
        };
        return run_gallery(state, dark, paths.view_paths());
    }

    Application::new().run(move |cx: &mut App| {
        let mtm = MainThreadMarker::new().expect("gpui runs on the main thread");
        set_accessory_policy(mtm);
        let (status, menu_rx) = StatusItem::install(mtm);
        let status = Rc::new(status);
        // The status item must live for the app's lifetime, but this closure
        // is `on_finish_launching` — it returns right after launch (gpui
        // app.rs:180), so leak one strong ref (plan Task 1 implementer note):
        // one item for the process lifetime.
        std::mem::forget(Rc::clone(&status));

        let state: Entity<SyncModel> = cx.new(|_| SyncModel::default());

        // Initial (disconnected) title + menu straight from the default
        // model: header reads "chezmoid not connected" until hello lands.
        refresh_status_item(&status, state.read(cx));

        // Single choke point for "after every model change, update AppKit":
        // everything else just updates the entity and notifies.
        cx.observe(&state, {
            let status = Rc::clone(&status);
            move |state, cx| refresh_status_item(&status, state.read(cx))
        })
        .detach();

        spawn_menu_command_loop(cx, menu_rx, state.clone(), paths.view_paths());
        spawn_boot_and_event_loop(cx, state.clone(), paths);
        spawn_freshness_loop(cx, state, status);
    });
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use czui_app::model::SyncModel;
    use czui_proto::Event;

    use super::{NotifySignal, apply_and_signal};

    fn drift(target: &str, class: &str, ts: u64) -> Event {
        Event::Drift {
            target: PathBuf::from(target),
            class: class.into(),
            ts,
        }
    }

    #[test]
    fn drift_notifies_only_on_news() {
        let mut m = SyncModel::default();
        // brand-new drift → notify
        assert_eq!(
            apply_and_signal(&mut m, drift("/a", "destination_drift", 1)),
            NotifySignal::NewDrift
        );
        // repeat observation of the same state → silent (model dedup)
        assert_eq!(
            apply_and_signal(&mut m, drift("/a", "destination_drift", 2)),
            NotifySignal::None
        );
        // class escalation into needs-attention → notify
        assert_eq!(
            apply_and_signal(&mut m, drift("/a", "conflict", 3)),
            NotifySignal::NewDrift
        );
        // de-escalation → silent
        assert_eq!(
            apply_and_signal(&mut m, drift("/a", "destination_drift", 4)),
            NotifySignal::None
        );
        // back in sync → silent
        assert_eq!(
            apply_and_signal(&mut m, drift("/a", "in_sync", 5)),
            NotifySignal::None
        );
        assert!(m.drifted.is_empty(), "model still ingested every event");
    }

    #[test]
    fn eval_failed_counts_as_drift_news_once() {
        let mut m = SyncModel::default();
        assert_eq!(
            apply_and_signal(
                &mut m,
                Event::EvalFailed {
                    target: Some(PathBuf::from("/tpl")),
                    hint: "bad template".into(),
                    ts: 1,
                }
            ),
            NotifySignal::NewDrift
        );
        // the follow-up Drift push for the same class is not news
        assert_eq!(
            apply_and_signal(&mut m, drift("/tpl", "eval_failed", 2)),
            NotifySignal::None
        );
        // a target-less eval failure can't join the drifted set → silent
        assert_eq!(
            apply_and_signal(
                &mut m,
                Event::EvalFailed {
                    target: None,
                    hint: "doctor".into(),
                    ts: 3,
                }
            ),
            NotifySignal::None
        );
    }

    #[test]
    fn remote_advanced_notifies_and_lifecycle_events_stay_silent() {
        let mut m = SyncModel::default();
        assert_eq!(
            apply_and_signal(
                &mut m,
                Event::RemoteAdvanced {
                    target: PathBuf::from("/b"),
                    ts: 1,
                }
            ),
            NotifySignal::RemoteAdvanced(PathBuf::from("/b"))
        );
        assert_eq!(
            apply_and_signal(&mut m, Event::FetchDone { ts: 2, behind: 3 }),
            NotifySignal::None
        );
        assert_eq!(
            apply_and_signal(&mut m, Event::ScanDone { ts: 3, drifted: 0 }),
            NotifySignal::None
        );
        assert_eq!(
            apply_and_signal(
                &mut m,
                Event::LeftManagement {
                    target: PathBuf::from("/b"),
                    ts: 4,
                }
            ),
            NotifySignal::None
        );
        // the model still ingested the pushes
        assert_eq!(m.last_fetch_ts, Some(2));
    }
}
