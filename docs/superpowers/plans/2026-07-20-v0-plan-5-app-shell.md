# chezmoi-ui v0 — Plan 5: GPUI App Shell

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The `czui-app` crate: a menubar-resident GPUI app (spec §3.2, §7) — NSStatusItem with live drift state, Accessory activation policy, GitHub-palette light/dark theme, IPC client to `chezmoid`, dashboard (health tiles + chronological actionable timeline), review shell, settings (incl. the 1Password account picker), and osascript notifications. Mutating quick-actions and the merge editor are Plan 6.

**Architecture:** One window whose root view routes between Dashboard/Review/Settings; the status item is pure AppKit (objc2) bridged into gpui over an mpsc channel polled on the background executor — this exact integration (define_class! menu target + Accessory policy + gpui window + channel bridge) was **compile-verified in a spike against gpui 0.2.2 + objc2-app-kit 0.3.2 before this plan was written**; Task 1's platform code is that spike, verbatim where possible. Domain state lives in a pure `SyncModel` (unit-testable without gpui) wrapped in an `Entity`. All subprocess work (chezmoi cat for previews, `op account list`) runs on the background executor — never the main thread (spec §3.2 non-blocking rule).

**Tech Stack:** gpui = "=0.2.2" (exact pin), objc2 0.6, objc2-app-kit 0.3, objc2-foundation 0.3, existing czui-{core,journal,proto} (+ czui-daemon as dev-dep for IPC tests).

**Prerequisites:** Plans 1–4 complete (84 tests green). **Build requirement:** full Xcode + Metal Toolchain component (`xcodebuild -downloadComponent MetalToolchain`) — installed and verified on this machine 2026-07-20; gpui's build script fails without it (spec §12).

**GPUI facts verified from the pinned crate source (do not re-derive from the stale skill):**
- `Application::new().run(|cx: &mut App| …)`; `App::open_window(WindowOptions, |window, cx| cx.new(|cx| view)) -> anyhow::Result<WindowHandle<V>>`
- `trait Render { fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement; }` (element.rs:131)
- `App::spawn(async move |cx: &mut AsyncApp| …) -> Task<R>` (app.rs:1417) — detach or store; `cx.update(|cx| …)` from AsyncApp
- `BackgroundExecutor::timer(Duration) -> Task<()>` (executor.rs:357)
- `App::{activate(bool), quit(), hide()}`; `WindowOptions { window_bounds, titlebar, focus, show, kind, is_movable, is_resizable, is_minimizable, display_id, window_background, app_id, window_min_size, … }` (platform.rs:1089)
- `uniform_list(...)` exists (elements/uniform_list.rs:22); `Stateful<Div>::on_click(impl Fn(&ClickEvent, &mut Window, &mut App))` (div.rs:484); interactive elements need `.id(…)` before `.on_click`
- gpui has **no stock text input** — settings UI must use steppers/pickers, never a text field (Zed's inputs are Zed-internal).
- objc2-app-kit 0.3.2: `NSStatusBar::systemStatusBar()`, `statusItemWithLength(NSVariableStatusItemLength)`, `NSStatusItem::{button(mtm), setMenu}`, `NSButton::setTitle`, `NSApplication::sharedApplication(mtm).setActivationPolicy(…)`, `NSMenu/NSMenuItem::{new(mtm), setTitle, setTarget, setAction, setEnabled, separatorItem(mtm), addItem}`.

## Global Constraints

Identical to Plan 1's, plus:

- New workspace deps: `gpui = "=0.2.2"`, `objc2 = "0.6"`, `objc2-app-kit = "0.3"`, `objc2-foundation = "0.3"`. Nothing else.
- **No `CommandRunner` call, journal open, or file read >1 KiB on the GPUI main thread** (spec §3.2). Pattern: `cx.background_executor().spawn(async move { blocking_work() })` or `cx.spawn(async move |cx| { … cx.update(...) })`.
- The app mutates nothing in v0 Plan 5 (mutating actions arrive in Plan 6): buttons for apply/re-add/merge render disabled with tooltip "arrives with the sync pipeline". Exceptions: writing `settings.toml` and daemon restart from Settings (spec §9), which are app-owned.
- Theme values ONLY through the `Theme` struct (spec §7.5) — no raw hex in view code.
- All view code compiles against local rustdoc at `scratchpad gpui-probe/target/doc/gpui/index.html` and the crate source — when an element builder method is missing, check the source, not the skill.
- 1Password may be LOCKED during implementation: if `git commit` fails with "1Password: failed to fill whole buffer" → stage with `git add`, verify the gate, report "staged, commit blocked". Never `--no-gpg-sign`, never config changes, never retry loops.

## File Structure

```
Cargo.toml                      # + member crates/app, + gpui/objc2* workspace deps
crates/app/
  Cargo.toml                    # package czui-app; bin chezmoi-ui
  src/main.rs                   # boot: policy, status item, IPC, entity wiring
  src/theme.rs                  # GitHub-palette light/dark semantic tokens
  src/model.rs                  # SyncModel (pure) + TimelineRow
  src/ipc.rs                    # IpcClient (connect/hello/request/subscribe/spawn-daemon)
  src/platform_mac.rs           # spike code: MenuTarget, status item, policy, MenuSpec
  src/views/mod.rs              # Shell root view + Route
  src/views/dashboard.rs
  src/views/review.rs
  src/views/settings.rs
  src/notify_osa.rs             # osascript notifications, coalesced
  tests/ipc_client.rs           # against real daemon serve() in scratch
```

---

### Task 1: Crate scaffold, theme, mac platform layer, shell window

**Files:**
- Modify: `Cargo.toml` (member + workspace deps above)
- Create: `crates/app/Cargo.toml`, `src/main.rs`, `src/theme.rs`, `src/platform_mac.rs`, `src/views/mod.rs` (placeholder views land in Tasks 5–7; `model.rs`, `ipc.rs`, `notify_osa.rs` as `//! see plan task N` placeholders)

**Interfaces:**
- Produces:
  - `theme::Theme { bg, surface, border, text, text_muted, accent, ok, drift, conflict: gpui::Rgba }` with `Theme::dark()`, `Theme::light()`, `Theme::for_appearance(gpui::WindowAppearance) -> Theme`
  - `platform_mac::MenuCommand::{OpenApp, Review, SyncAll, Settings, Quit}`
  - `platform_mac::MenuSpec { header: String, freshness: String, review_label: Option<String>, sync_all_enabled: bool }`
  - `platform_mac::StatusItem` with `install(mtm) -> (StatusItem, Receiver<MenuCommand>)`, `set_title(&self, mtm, title: &str)`, `set_menu(&self, mtm, spec: &MenuSpec)`
  - `platform_mac::set_accessory_policy(mtm)`
  - `views::Shell` root view with `Route::{Dashboard, Review, Settings}` and a top nav bar; Task 1 renders placeholder bodies.
- The `Cargo.toml`:

```toml
[package]
name = "czui-app"
version = "0.0.1"
edition.workspace = true
license.workspace = true

[[bin]]
name = "chezmoi-ui"
path = "src/main.rs"

[dependencies]
czui-core = { path = "../core" }
czui-journal = { path = "../journal" }
czui-proto = { path = "../proto" }
gpui.workspace = true
objc2.workspace = true
objc2-app-kit.workspace = true
objc2-foundation.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
toml.workspace = true

[dev-dependencies]
czui-daemon = { path = "../daemon" }
czui-core = { path = "../core", features = ["test-support"] }
tempfile.workspace = true
```

- [ ] **Step 1: Write the theme + its test**

`crates/app/src/theme.rs`:
```rust
//! GitHub-palette semantic tokens (spec §7.5). Light and dark from day one.

use gpui::{rgb, Rgba, WindowAppearance};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub bg: Rgba,
    pub surface: Rgba,
    pub border: Rgba,
    pub text: Rgba,
    pub text_muted: Rgba,
    pub accent: Rgba,
    pub ok: Rgba,
    pub drift: Rgba,
    pub conflict: Rgba,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            bg: rgb(0x0d1117),
            surface: rgb(0x161b22),
            border: rgb(0x30363d),
            text: rgb(0xc9d1d9),
            text_muted: rgb(0x8b949e),
            accent: rgb(0x58a6ff),
            ok: rgb(0x3fb950),
            drift: rgb(0xd29922),
            conflict: rgb(0xf85149),
        }
    }

    pub fn light() -> Self {
        Self {
            bg: rgb(0xffffff),
            surface: rgb(0xf6f8fa),
            border: rgb(0xd0d7de),
            text: rgb(0x1f2328),
            text_muted: rgb(0x656d76),
            accent: rgb(0x0969da),
            ok: rgb(0x1a7f37),
            drift: rgb(0x9a6700),
            conflict: rgb(0xcf222e),
        }
    }

    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
        }
    }

    pub fn class_color(&self, class: &str) -> Rgba {
        match class {
            "conflict" | "local_source_diverged" | "eval_failed" => self.conflict,
            "destination_drift" | "source_ahead" => self.drift,
            "remote_ahead" => self.accent,
            _ => self.ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_colors_are_distinct_per_severity() {
        let t = Theme::dark();
        assert_eq!(t.class_color("conflict"), t.conflict);
        assert_eq!(t.class_color("destination_drift"), t.drift);
        assert_eq!(t.class_color("remote_ahead"), t.accent);
        assert_eq!(t.class_color("in_sync"), t.ok);
    }
}
```

- [ ] **Step 2: Write the mac platform layer** (spike-derived; the `define_class!` block, `MenuTarget::new`, and policy flip are compile-verified)

`crates/app/src/platform_mac.rs`:
```rust
//! AppKit integration: status item + menu + activation policy (spec §3.2).
//! Compile-verified as a spike against gpui 0.2.2 + objc2-app-kit 0.3.2.

use std::sync::mpsc::{channel, Receiver, Sender};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSObject, NSString};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCommand {
    OpenApp,
    Review,
    SyncAll,
    Settings,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuSpec {
    pub header: String,
    pub freshness: String,
    pub review_label: Option<String>,
    pub sync_all_enabled: bool,
}

struct TargetIvars {
    tx: Sender<MenuCommand>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CzuiMenuTarget"]
    #[ivars = TargetIvars]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(openApp:))]
        fn open_app(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().tx.send(MenuCommand::OpenApp);
        }

        #[unsafe(method(review:))]
        fn review(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().tx.send(MenuCommand::Review);
        }

        #[unsafe(method(syncAll:))]
        fn sync_all(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().tx.send(MenuCommand::SyncAll);
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().tx.send(MenuCommand::Settings);
        }

        #[unsafe(method(quitApp:))]
        fn quit_app(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().tx.send(MenuCommand::Quit);
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker, tx: Sender<MenuCommand>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars { tx });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct StatusItem {
    item: Retained<NSStatusItem>,
    target: Retained<MenuTarget>,
}

pub fn set_accessory_policy(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}

impl StatusItem {
    pub fn install(mtm: MainThreadMarker) -> (Self, Receiver<MenuCommand>) {
        let (tx, rx) = channel();
        let target = MenuTarget::new(mtm, tx);
        let bar = unsafe { NSStatusBar::systemStatusBar() };
        let item = unsafe { bar.statusItemWithLength(NSVariableStatusItemLength) };
        let this = Self { item, target };
        this.set_title(mtm, "cz");
        (this, rx)
    }

    pub fn set_title(&self, mtm: MainThreadMarker, title: &str) {
        if let Some(button) = self.item.button(mtm) {
            button.setTitle(&NSString::from_str(title));
        }
    }

    pub fn set_menu(&self, mtm: MainThreadMarker, spec: &MenuSpec) {
        let menu = NSMenu::new(mtm);
        unsafe {
            let add_info = |text: &str| {
                let it = NSMenuItem::new(mtm);
                it.setTitle(&NSString::from_str(text));
                it.setEnabled(false);
                menu.addItem(&it);
            };
            add_info(&spec.header);
            add_info(&spec.freshness);
            menu.addItem(&NSMenuItem::separatorItem(mtm));

            if let Some(label) = &spec.review_label {
                let it = NSMenuItem::new(mtm);
                it.setTitle(&NSString::from_str(label));
                it.setTarget(Some(&self.target));
                it.setAction(Some(sel!(review:)));
                menu.addItem(&it);
            }
            let sync = NSMenuItem::new(mtm);
            sync.setTitle(&NSString::from_str("Sync all"));
            if spec.sync_all_enabled {
                sync.setTarget(Some(&self.target));
                sync.setAction(Some(sel!(syncAll:)));
            } else {
                sync.setEnabled(false);
            }
            menu.addItem(&sync);
            menu.addItem(&NSMenuItem::separatorItem(mtm));

            let open = NSMenuItem::new(mtm);
            open.setTitle(&NSString::from_str("Open chezmoi UI"));
            open.setTarget(Some(&self.target));
            open.setAction(Some(sel!(openApp:)));
            menu.addItem(&open);

            let settings = NSMenuItem::new(mtm);
            settings.setTitle(&NSString::from_str("Settings…"));
            settings.setTarget(Some(&self.target));
            settings.setAction(Some(sel!(openSettings:)));
            menu.addItem(&settings);

            let quit = NSMenuItem::new(mtm);
            quit.setTitle(&NSString::from_str("Quit chezmoi UI"));
            quit.setTarget(Some(&self.target));
            quit.setAction(Some(sel!(quitApp:)));
            menu.addItem(&quit);
        }
        self.item.setMenu(Some(&menu));
    }
}
```

- [ ] **Step 3: Shell root view and main**

`crates/app/src/views/mod.rs`:
```rust
pub mod dashboard;
pub mod review;
pub mod settings;

use gpui::{div, prelude::*, Context, SharedString, Window};

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Dashboard,
    Review,
    Settings,
}

pub struct Shell {
    pub route: Route,
}

impl Shell {
    fn nav_button(
        &self,
        theme: &Theme,
        label: &'static str,
        route: Route,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.route == route;
        div()
            .id(label)
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .when(active, |el| el.bg(theme.surface))
            .text_color(if active { theme.text } else { theme.text_muted })
            .child(SharedString::from(label))
            .on_click(cx.listener(move |shell, _ev, _window, cx| {
                shell.route = route;
                cx.notify();
            }))
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg)
            .text_color(theme.text)
            .child(
                div()
                    .flex()
                    .gap_1()
                    .p_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(self.nav_button(&theme, "Dashboard", Route::Dashboard, cx))
                    .child(self.nav_button(&theme, "Review", Route::Review, cx))
                    .child(self.nav_button(&theme, "Settings", Route::Settings, cx)),
            )
            .child(match self.route {
                Route::Dashboard => div().p_4().child("dashboard — task 5"),
                Route::Review => div().p_4().child("review — task 6"),
                Route::Settings => div().p_4().child("settings — task 7"),
            })
    }
}
```

Implementer notes: `cx.listener` gives `Fn(&mut Shell, &ClickEvent, &mut Window, &mut Context<Shell>)` — verify the exact closure arity against `Context::listener` in the local source and adjust the parameter list if it differs (the pattern is standard; the arity has changed between gpui versions). `window.appearance()` — confirm method name via `grep -n "pub fn appearance" src/window.rs`; if it's `window_appearance()`, use that.

`crates/app/src/main.rs` (Task 1 version — IPC arrives in Task 2/4):
```rust
//! chezmoi-ui — menubar-resident GPUI app (spec §3.2).

mod ipc;
mod model;
mod notify_osa;
mod platform_mac;
mod theme;
mod views;

use gpui::{px, size, App, Application, Bounds, WindowBounds, WindowOptions};
use objc2::MainThreadMarker;

use platform_mac::{set_accessory_policy, MenuCommand, MenuSpec, StatusItem};
use views::{Route, Shell};

fn open_shell(cx: &mut App, route: Route) {
    cx.activate(true);
    let bounds = Bounds::centered(None, size(px(980.), px(640.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |_, cx| cx.new(|_| Shell { route }),
    )
    .ok();
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let mtm = MainThreadMarker::new().expect("gpui runs on the main thread");
        set_accessory_policy(mtm);
        let (status, rx) = StatusItem::install(mtm);
        status.set_menu(
            mtm,
            &MenuSpec {
                header: "starting…".into(),
                freshness: "daemon not connected yet".into(),
                review_label: None,
                sync_all_enabled: false,
            },
        );
        // status item must live for the app's lifetime
        let status = std::rc::Rc::new(status);

        cx.spawn(async move |cx| {
            loop {
                match rx.try_recv() {
                    Ok(MenuCommand::OpenApp) => {
                        let _ = cx.update(|cx| open_shell(cx, Route::Dashboard));
                    }
                    Ok(MenuCommand::Review) => {
                        let _ = cx.update(|cx| open_shell(cx, Route::Review));
                    }
                    Ok(MenuCommand::Settings) => {
                        let _ = cx.update(|cx| open_shell(cx, Route::Settings));
                    }
                    Ok(MenuCommand::SyncAll) => { /* Plan 6 */ }
                    Ok(MenuCommand::Quit) => {
                        let _ = cx.update(|cx| cx.quit());
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(100))
                            .await;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
        })
        .detach();

        let _keep_alive = status; // moved Rc keeps the item retained
    });
}
```

Implementer note: the `let _keep_alive = status;` line as written drops at the closure's end, which is the app's lifetime — if the status item vanishes from the bar at launch, leak it instead: `std::mem::forget(status);` (acceptable: one item for the process lifetime). `Bounds::centered` — verify against source (`grep -n "pub fn centered" src/geometry.rs`); fall back to `WindowOptions::default()` if absent.

- [ ] **Step 4: Verify it builds and the unit test passes**

Run: `cargo test -p czui-app` (theme test) and `cargo build -p czui-app`
Expected: 1 test passed; binary builds. Do NOT `cargo run` the GUI (it would flash windows on the user's desktop while they're away); the runtime smoke is Step 6 of Task 7, flagged for the user.

- [ ] **Step 5: Full gate + commit**

Gate (separate commands): `cargo fmt --all`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace`.

```bash
git add Cargo.toml Cargo.lock crates/app
git commit -m "feat(app): czui-app shell — theme, status item, accessory policy, routed window"
```

---

### Task 2: IPC client

**Files:**
- Modify: `crates/app/src/ipc.rs`
- Create: `crates/app/tests/ipc_client.rs`

**Interfaces:**
- Produces:
  - `IpcClient::connect(socket: &Path) -> Result<IpcClient, IpcError>` — connects, performs Hello, starts the reader thread
  - `IpcClient::request(&self, req: Request) -> Result<Response, IpcError>` — blocking, 10s timeout, id-correlated (safe from any thread; NEVER call on the main thread — callers wrap in background executor)
  - `IpcClient::subscribe(&self) -> Result<Receiver<Event>, IpcError>` — sends `Subscribe`, returns the push channel (one subscription per client)
  - `IpcClient::connect_or_spawn(socket: &Path, chezmoid_bin: &Path) -> Result<IpcClient, IpcError>` — dev convenience: if connect fails, spawns `chezmoid` as a child process and retries for ~5s
  - `IpcError::{Io(std::io::Error), Proto(String), Timeout, Rejected(String)}`
- Internals: writer half behind `Mutex<UnixStream>`; reader thread parses `ServerFrame` lines — `Reply` routed to a `Mutex<HashMap<u64, mpsc::Sender<Response>>>`, `Push` forwarded to the events channel.

- [ ] **Step 1: Write the failing integration test** — mirror `crates/daemon/tests/server_ipc.rs`'s setup (scratch + `DaemonCore` + `serve` on a thread), then:

```rust
//! IpcClient against the real daemon server.

use std::sync::{Arc, Mutex};

use czui_app::ipc::IpcClient;
use czui_core::chezmoi::ChezmoiClient;
use czui_core::cmd::SystemRunner;
use czui_core::git::GitClient;
use czui_core::testsupport::Scratch;
use czui_daemon::core::DaemonCore;
use czui_daemon::server::serve;
use czui_journal::Journal;
use czui_proto::{Event, Request, Response};

#[test]
fn connect_status_subscribe_roundtrip() {
    let s = Scratch::new();
    let chezmoi: ChezmoiClient = s.chezmoi();
    let git = GitClient::new(Arc::new(SystemRunner), s.source.clone());
    let journal = Journal::open_in_memory("ipc").unwrap();
    let core = Arc::new(Mutex::new(
        DaemonCore::new(chezmoi, git, journal, "origin/main".into()).unwrap(),
    ));
    let sock = s.root.path().join("d.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let served = core.clone();
    std::thread::spawn(move || serve(listener, served, || 42));

    let client = IpcClient::connect(&sock).unwrap();
    match client.request(Request::Status).unwrap() {
        Response::Status { drifted, .. } => assert!(drifted.is_empty()),
        other => panic!("unexpected: {other:?}"),
    }
    let events = client.subscribe().unwrap();
    let target = s.home.join(".testrc");
    std::fs::write(&target, "a=live\n").unwrap();
    core.lock().unwrap().handle_paths_changed(std::slice::from_ref(&target), 77).unwrap();
    match events.recv_timeout(std::time::Duration::from_secs(5)).unwrap() {
        Event::Drift { target: t, ts: 77, .. } => assert_eq!(t, target),
        other => panic!("unexpected push: {other:?}"),
    }
}

#[test]
fn version_rejection_surfaces_as_error() {
    // connect() performs Hello with PROTOCOL_VERSION, so this tests the happy
    // handshake; rejection is covered by the daemon's own tests. Here: bad socket.
    assert!(IpcClient::connect(std::path::Path::new("/nonexistent.sock")).is_err());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p czui-app --test ipc_client`
Expected: compile errors (`IpcClient` undefined). (The test file needs `czui_app` as a lib — add to `crates/app/Cargo.toml`: `[lib] name = "czui_app"` plus `src/lib.rs` re-exporting `pub mod ipc; pub mod model; pub mod theme;` — main.rs then uses `czui_app::…` imports for those modules instead of `mod` declarations for the shared ones. Views/platform stay bin-only modules.)

- [ ] **Step 3: Implement**

`crates/app/src/ipc.rs`:
```rust
//! Blocking IPC client for chezmoid (spec §3.3). Off-main-thread only.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::time::Duration;

use czui_proto::{
    read_frame, write_frame, ClientFrame, Event, Request, Response, ServerFrame, PROTOCOL_VERSION,
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
                let Ok(frame) = read_frame::<ServerFrame>(&line) else { continue };
                match frame {
                    ServerFrame::Reply { id, response } => {
                        if let Some(tx) = pending.lock().ok().and_then(|mut p| p.remove(&id)) {
                            let _ = tx.send(response);
                        }
                    }
                    ServerFrame::Push { event } => {
                        if let Ok(guard) = events_tx.lock() {
                            if let Some(tx) = guard.as_ref() {
                                let _ = tx.send(event);
                            }
                        }
                    }
                }
            }
        });

        match client.request(Request::Hello { version: PROTOCOL_VERSION })? {
            Response::HelloOk { .. } => Ok(client),
            Response::Error { message } => Err(IpcError::Rejected(message)),
            other => Err(IpcError::Proto(format!("unexpected hello reply: {other:?}"))),
        }
    }

    pub fn request(&self, request: Request) -> Result<Response, IpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        if let Ok(mut p) = self.pending.lock() {
            p.insert(id, tx);
        }
        {
            let mut w = self.writer.lock().map_err(|_| IpcError::Proto("writer poisoned".into()))?;
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
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p czui-app --test ipc_client`
Expected: 2 passed.

- [ ] **Step 5: Full gate + commit**

```bash
git add crates/app
git commit -m "feat(app): blocking IPC client with id correlation and push subscription"
```

---

### Task 3: `SyncModel` + read-only journal access

**Files:**
- Modify: `crates/journal/src/lib.rs` (one method + test), `crates/app/src/model.rs`

**Interfaces:**
- Produces:
  - `Journal::open_read_only(path: &Path, machine: &str) -> Result<Journal, JournalError>` (rusqlite `OpenFlags::SQLITE_OPEN_READ_ONLY`; fails cleanly if the file doesn't exist)
  - `model::SyncModel { pub drifted: Vec<DriftSummary>, pub in_sync: u64, pub degraded: Option<String>, pub timeline: Vec<TimelineRow>, pub last_fetch_ts: Option<u64>, pub connected: bool }`
  - `TimelineRow { pub ts: u64, pub kind: String, pub target: Option<PathBuf>, pub machine: String, pub class: Option<String> }`
  - `SyncModel::hydrate_status(&mut self, drifted, in_sync, degraded)`, `hydrate_timeline(&mut self, rows: Vec<czui_journal::EventRow>)` (maps meta.class), `apply_event(&mut self, ev: Event)` (updates drifted list, prepends synthetic timeline rows, tracks last_fetch_ts), `needs_attention(&self) -> usize` (conflict-ish classes count), `status_title(&self) -> String` ("cz" / "cz ●N"), `menu_spec(&self, now_ts: u64) -> MenuSpec`-shaped data as `(String, String, Option<String>, bool)` (header, freshness, review label, sync-all enabled = `drifted.is_empty() && connected`)
- Pure — no gpui imports in `model.rs`; fully unit-tested.

- [ ] **Steps 1–4: TDD** — write table-driven tests first (hydrate → apply Drift/LeftManagement/FetchDone events → assert drifted list dedups by target, needs_attention counts conflict+local_source_diverged+eval_failed, status_title formats, timeline caps at 500 rows), verify failure, implement, verify 5+ model tests pass. Test `open_read_only`: open a temp journal read-write, write an event, reopen read-only, read timeline ✓, and assert a `record_event` through the read-only handle errors.

- [ ] **Step 5: Full gate + commit**

```bash
git add crates/journal/src/lib.rs crates/app/src/model.rs
git commit -m "feat(app): pure SyncModel with event ingestion; read-only journal access"
```

---

### Task 4: Live wiring — IPC → entity → status item

**Files:**
- Modify: `crates/app/src/main.rs`

**Behavior:**
- Boot: resolve socket/journal paths (same env overrides as chezmoid: `CZUI_SOCKET`, `CZUI_JOURNAL`, plus `CZUI_CHEZMOID` for the binary path, defaulting to a sibling `chezmoid` next to the app binary, falling back to `chezmoid` on PATH).
- On the background executor: `IpcClient::connect_or_spawn`, `Request::Status`, hydrate timeline from `Journal::open_read_only`, `subscribe()`.
- A `SyncState` entity (`cx.new(|_| SyncModel::default())`) holds the model; the event loop task applies pushes via `cx.update_entity`.
- After every model change: recompute `status_title()` + menu data and apply to the `StatusItem` inside `cx.update` (main thread — safe for AppKit calls).
- `MenuCommand::Review` opens/focuses the shell on the Review route; `OpenApp` → Dashboard; windows observe the entity (`cx.observe`) so views re-render on changes (views consume it in Tasks 5–6).
- Every 30s, refresh freshness (menu rebuild) from the latest model.

No new tests (wiring); the gate is `cargo build` + all prior tests + a `--version`-style headless check: add `--print-status` flag to the binary that connects, prints status counts to stdout, and exits without any UI (this validates the whole boot path headlessly in CI/agents).

- [ ] Implement per the behavior list, keeping ALL IpcClient/journal calls on the background executor. Run: `cargo build -p czui-app`, then with a scratch daemon running (reuse the ipc test setup pattern manually if needed) `CZUI_SOCKET=… cargo run -p czui-app --bin chezmoi-ui -- --print-status`.
- [ ] Full gate + commit:

```bash
git add crates/app/src/main.rs
git commit -m "feat(app): live status item wired to daemon over IPC with print-status mode"
```

---

### Task 5: Dashboard view

**Files:**
- Modify: `crates/app/src/views/dashboard.rs`, `crates/app/src/views/mod.rs` (route to it)

**Content (spec §7.1, approved mockup B+C):**
- Health tile row: needs-attention count (conflict color), origin freshness ("fetched Nm ago" from last_fetch_ts vs now), in-sync count (ok color). Degraded banner (drift color) with the hint text when `degraded.is_some()`.
- Below: **one chronological timeline** (`uniform_list`) of `TimelineRow`s, newest first: relative time, kind glyph (`Δ` dest_changed, `↓` remote_advanced, `✓` applied/resolved, `⛔` eval_failed, `−` left_management), target file name (muted full path), machine label, class chip colored via `theme.class_color`.
- Actionable rows (current drifted targets) render a right-aligned button group: `Review →` (enabled; routes to Review with that target selected) plus `keep disk` / `keep source` / `Merge…` rendered disabled with tooltip "arrives with the sync pipeline" (Plan 6).
- Empty state: centered "everything in sync" with the ok color.

**Interfaces:** `DashboardView { state: Entity<SyncModel>, now_ts: fn() -> u64 }` (clock injected for testable relative-time formatting — the formatter itself is a pure fn `fn time_ago(now: u64, ts: u64) -> String` with unit tests: "just now", "3h ago", "2d ago").

- [ ] TDD the pure parts (`time_ago`, glyph mapping); build the view; wire `Route::Dashboard`; gate; commit `feat(app): dashboard with health tiles and chronological timeline`.

---

### Task 6: Review shell view

**Files:**
- Modify: `crates/app/src/views/review.rs`, `views/mod.rs`

**Content (spec §7.2, approved mockup A):**
- Left sidebar (fixed 260px): severity groups — "Needs you" (conflict/local_source_diverged/eval_failed), "One click" (destination_drift/source_ahead/remote_ahead), collapsed "In sync (N)". Rows: class dot + file name + since-time.
- Right detail for the selected target: provenance timeline (`events_for` from read-only journal, background-loaded), then a **read-only diff preview**: destination bytes (fs read) vs rendered (`chezmoi cat` — via `CommandRunner` on the background executor; `EvalFailed` renders the hint + remediation text instead), computed through `MergeDocument::compute` and rendered region-by-region with `word_diff` intra-region highlights (added/removed tints from theme). Buttons: `Open merge editor` disabled ("Plan 6"), `open in editor` enabled — `open -t <source-path>` (read-only escape hatch, allowed: it opens an editor, mutates nothing itself).
- Loading and error states for the preview (spec §10: errors are states, not gaps).

**Interfaces:** `ReviewView { state: Entity<SyncModel>, selected: Option<PathBuf>, preview: PreviewState }`, `PreviewState::{Empty, Loading, Ready(MergeDocument), EvalFailed(String), Error(String)}`.

- [ ] TDD the pure grouping fn (`fn severity_groups(&[DriftSummary]) -> (Vec<_>, Vec<_>)`); build the view with background preview loading; gate; commit `feat(app): review shell with severity sidebar and read-only diff preview`.

---

### Task 7: Settings view + notifications + runtime smoke

**Files:**
- Modify: `crates/app/src/views/settings.rs`, `crates/app/src/notify_osa.rs`, `views/mod.rs`, `main.rs` (notification hookup)

**Settings (spec §9; NO text inputs — gpui has none):**
- Fetch interval: `−` / `+` stepper (5-min increments, min 5, max 120) around a value label.
- **1Password account picker:** background-load `op account list --format=json` via `CommandRunner` (parse: array of objects with `shorthand`/`email`/`account_uuid` — tolerate missing fields); render as selectable rows; "None (single account)" row on top. Selection + Save → write `settings.toml` (same schema as `czui_daemon::settings::Settings` — serialize via `toml`), then restart the daemon: `Request::Rescan` is NOT enough (daemon reads settings at boot), so: if the daemon was spawned by us, kill child + `connect_or_spawn` again; otherwise show "restart chezmoid to apply" notice. `op` missing → picker shows "1Password CLI not found" (state, not gap).
- Paths section: read-only display of socket/journal/settings paths.

**Notifications (spec §7.6):** `notify_osa::notify(title, body)` shells `osascript -e 'display notification …'` (args escaped, via CommandRunner, background executor). Hook into the event loop: coalesce `Drift` events in 5s windows → one notification ("3 files drifted"); `RemoteAdvanced` → "machine X pushed…" when not self-caused. Never for `applied`/expected events (the daemon already filtered those).

- [ ] TDD pure parts (interval clamping, op-account JSON parsing with a canned fixture, osascript arg escaping); implement; gate; commit `feat(app): settings with 1Password account picker, osascript notifications`.

- [ ] **Final step — runtime smoke (REQUIRES THE USER):** this needs a human at the machine: `cargo run -p czui-app --bin chezmoi-ui` → verify the menubar item appears with live counts, no Dock icon, menu opens the window on each route, notifications fire on a manufactured drift (`echo test >> ~/.claude/settings.json` style — pick an already-drifted file to avoid new noise). Do NOT run this while the user is away; leave it as the handoff item in the final report.

---

## Self-Review Notes (completed during plan writing)

- **Spec coverage:** §3.2 status item + accessory policy (Task 1, spike-verified), never-block rule (constraint + patterns), notifications (Task 7), pre-announce/mutations deferred to Plan 6 by design; §7.1 dashboard = approved B+C mockup incl. chronological order; §7.2 review shell = mockup A minus merge editor (Plan 6); §7.4 menu = status glance, Sync-all disabled until zero-decision logic exists (Plan 6), Review-N opens app — one resolution surface preserved; §7.5 theme tokens light+dark; §9 settings incl. op account picker with non-interactive `op account list`; §12 Metal Toolchain requirement recorded.
- **Honest deviation from the §7.4 mockup:** the glance popup is a native NSMenu rather than a styled GPUI popup — same information architecture, dramatically less platform risk; a styled popup can replace it post-v0. Flag this to the user in the completion report.
- **Type consistency:** `MenuSpec` produced by model ↔ consumed by platform_mac; `DriftSummary`/`Event` from czui-proto everywhere; `EventRow` mapping in one place (`hydrate_timeline`).
- **Known risks for implementers:** `cx.listener` arity and `window.appearance()`/`Bounds::centered` names may differ — verify against local source as noted inline; `define_class!` syntax is exactly as spike-verified, don't "modernize" it.
