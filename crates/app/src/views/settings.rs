//! Settings view (spec §9; plan 5 Task 7): fetch-interval stepper, 1Password
//! account picker, save-to-settings.toml, read-only paths.
//!
//! gpui has no stock text input, so every control is a stepper or a picker
//! row. All blocking work — the settings.toml read/write and `op account
//! list` — runs on the background executor and lands back in the entity via
//! `WeakEntity::update` (spec §3.2 non-blocking rule). Saving writes the
//! TOML, then asks the daemon to Shutdown; the app's reconnect loop respawns
//! it with the new settings — no user action needed (spec §9).

use std::path::{Path, PathBuf};

use czui_app::theme::Theme;
use czui_core::cmd::{CommandRequest, CommandRunner, SystemRunner};
use gpui::{
    AnyElement, Context, Corner, Div, ElementId, SharedString, Stateful, Window, anchored,
    deferred, div, point, prelude::*, px,
};

use czui_ui::components as ui;
use serde::{Deserialize, Serialize};

/// Fetch-interval bounds and step (spec §9): 5..=120 minutes, 5-minute steps.
pub const INTERVAL_MIN: u64 = 5;
pub const INTERVAL_MAX: u64 = 120;
pub const INTERVAL_STEP: u64 = 5;

/// Clamp a fetch interval into the allowed range.
pub fn clamp_interval(minutes: u64) -> u64 {
    minutes.clamp(INTERVAL_MIN, INTERVAL_MAX)
}

/// One stepper click: ±[`INTERVAL_STEP`] minutes, clamped.
pub fn step_interval(minutes: u64, up: bool) -> u64 {
    let next = if up {
        minutes.saturating_add(INTERVAL_STEP)
    } else {
        minutes.saturating_sub(INTERVAL_STEP)
    };
    clamp_interval(next)
}

/// One `op account list --format=json` entry. Every field is optional — real
/// `op` output varies by CLI version and account type, so absence is data,
/// not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpAccount {
    pub shorthand: Option<String>,
    pub email: Option<String>,
    pub account_uuid: Option<String>,
}

impl OpAccount {
    /// The string stored as `onepassword_account` (the daemon injects it as
    /// OP_ACCOUNT, which accepts any of these): shorthand preferred, then the
    /// stable account UUID, then email.
    pub fn value(&self) -> Option<&str> {
        self.shorthand
            .as_deref()
            .or(self.account_uuid.as_deref())
            .or(self.email.as_deref())
    }

    /// Human row label: shorthand, else email, else UUID.
    pub fn label(&self) -> String {
        self.shorthand
            .clone()
            .or_else(|| self.email.clone())
            .or_else(|| self.account_uuid.clone())
            .unwrap_or_else(|| "(unnamed account)".to_string())
    }
}

/// Tolerant parse of `op account list --format=json`: expects a JSON array;
/// non-object entries and non-string fields are skipped, missing fields stay
/// `None`. Only a non-array (or invalid JSON) is an error.
pub fn parse_op_accounts(json: &str) -> Result<Vec<OpAccount>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid JSON: {e}"))?;
    let entries = value.as_array().ok_or("expected a JSON array")?;
    Ok(entries
        .iter()
        .filter_map(|entry| entry.as_object())
        .map(|obj| {
            let get = |key: &str| obj.get(key).and_then(|v| v.as_str()).map(str::to_owned);
            OpAccount {
                shorthand: get("shorthand"),
                email: get("email"),
                account_uuid: get("account_uuid"),
            }
        })
        .collect())
}

/// On-disk settings shape — field-for-field compatible with
/// `czui_daemon::settings::Settings` (round-trip covered by a test against
/// the daemon crate, which is a dev-dependency).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsDoc {
    pub fetch_interval_minutes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onepassword_account: Option<String>,
}

impl Default for SettingsDoc {
    fn default() -> Self {
        // Mirror of the daemon's defaults.
        Self {
            fetch_interval_minutes: 15,
            onepassword_account: None,
        }
    }
}

/// Serialize the settings the daemon will read at its next boot.
pub fn settings_toml(fetch_interval_minutes: u64, onepassword_account: Option<&str>) -> String {
    toml::to_string(&SettingsDoc {
        fetch_interval_minutes,
        onepassword_account: onepassword_account.map(str::to_owned),
    })
    .expect("a u64 + Option<String> struct always serializes to TOML")
}

/// Read the current settings for the form. Same degrade-to-defaults policy
/// as the daemon's `Settings::load`; the interval is additionally clamped so
/// the stepper starts inside its own bounds.
fn load_settings_blocking(path: &Path) -> SettingsDoc {
    let mut doc: SettingsDoc = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default();
    doc.fetch_interval_minutes = clamp_interval(doc.fetch_interval_minutes);
    doc
}

/// `op account list` lifecycle (spec §10: errors are states, not gaps).
#[derive(Debug, PartialEq)]
enum AccountsState {
    Loading,
    Ready(Vec<OpAccount>),
    /// `op` missing, errored, or emitted garbage — one fixed state row.
    Unavailable,
}

/// Run `op account list --format=json` and map every failure mode to the
/// single [`AccountsState::Unavailable`] state row.
fn load_accounts_blocking(runner: &dyn CommandRunner) -> AccountsState {
    let req = CommandRequest::new("op").args(["account", "list", "--format=json"]);
    match runner.run(req) {
        Ok(out) if out.success() => match parse_op_accounts(&out.stdout_utf8()) {
            Ok(accounts) => AccountsState::Ready(accounts),
            Err(_) => AccountsState::Unavailable,
        },
        _ => AccountsState::Unavailable,
    }
}

/// Save lifecycle for the settings.toml write.
enum SaveState {
    Idle,
    Saving,
    /// Written, and the daemon was asked to restart (the reconnect loop
    /// respawns it with the new settings).
    Saved,
    Error(String),
}

/// Daemon-facing paths shown in the read-only paths section; `settings` is
/// also the save target. Resolved by main.rs (the single source of path
/// policy) and handed through the Shell.
#[derive(Debug, Clone)]
pub struct SettingsPaths {
    pub socket: PathBuf,
    pub journal: PathBuf,
    pub settings: PathBuf,
}

pub struct SettingsView {
    paths: SettingsPaths,
    /// Clamped 5..=120; replaced by the background settings load.
    interval: u64,
    /// The on-disk settings landed — until then every control is inert so
    /// the background load can't clobber user edits (it lands in
    /// milliseconds; spec §10 renders the gap as a "…" value).
    loaded: bool,
    accounts: AccountsState,
    /// The `onepassword_account` value to save; `None` = single account.
    selected: Option<String>,
    save: SaveState,
    /// What's on disk: `(interval, account)` as of load / last save. Save is
    /// inert until edits diverge from this (it restarts the daemon — don't
    /// invite no-op restarts).
    baseline: Option<(u64, Option<String>)>,
    /// The account dropdown's popover is open. `pub(super)` so the gallery
    /// can pose the open state for screenshots.
    pub(super) menu_open: bool,
}

impl SettingsView {
    pub fn new(paths: SettingsPaths, cx: &mut Context<Self>) -> Self {
        // Current settings from disk, off the main thread.
        let settings_path = paths.settings.clone();
        cx.spawn(async move |this, cx| {
            let doc = cx
                .background_executor()
                .spawn(async move { load_settings_blocking(&settings_path) })
                .await;
            this.update(cx, |view, cx| {
                view.interval = doc.fetch_interval_minutes;
                view.selected = doc.onepassword_account;
                view.baseline = Some((view.interval, view.selected.clone()));
                view.loaded = true;
                cx.notify();
            })
            .ok();
        })
        .detach();

        // Account list from `op`, off the main thread.
        cx.spawn(async move |this, cx| {
            let accounts = cx
                .background_executor()
                .spawn(async move { load_accounts_blocking(&SystemRunner) })
                .await;
            this.update(cx, |view, cx| {
                view.accounts = accounts;
                cx.notify();
            })
            .ok();
        })
        .detach();

        Self {
            paths,
            interval: SettingsDoc::default().fetch_interval_minutes,
            loaded: false,
            accounts: AccountsState::Loading,
            selected: None,
            save: SaveState::Idle,
            baseline: None,
            menu_open: false,
        }
    }

    /// Gallery-only: a fully-posed view with NO background loads, so the
    /// screenshot is deterministic (synthetic accounts, optional dirty edit).
    #[doc(hidden)]
    pub fn posed_for_gallery(paths: SettingsPaths, dirty: bool) -> Self {
        let accounts = AccountsState::Ready(vec![
            OpAccount {
                shorthand: Some("personal".into()),
                email: Some("remi@example.com".into()),
                account_uuid: Some("AAAA1111".into()),
            },
            OpAccount {
                shorthand: None,
                email: Some("work@example.com".into()),
                account_uuid: Some("BBBB2222".into()),
            },
        ]);
        Self {
            paths,
            interval: if dirty { 25 } else { 15 },
            loaded: true,
            accounts,
            selected: Some("personal".into()),
            save: SaveState::Idle,
            baseline: Some((15, Some("personal".into()))),
            menu_open: false,
        }
    }

    /// Discard every unsaved edit: current state snaps back to the baseline
    /// (what's on disk).
    fn revert(&mut self, cx: &mut Context<Self>) {
        if let Some((interval, selected)) = self.baseline.clone() {
            self.interval = interval;
            self.selected = selected;
        }
        self.save = SaveState::Idle;
        cx.notify();
    }

    fn bump_interval(&mut self, up: bool, cx: &mut Context<Self>) {
        let next = step_interval(self.interval, up);
        if next != self.interval {
            self.interval = next;
            // Edits invalidate a lingering "Saved" notice.
            self.save = SaveState::Idle;
            cx.notify();
        }
    }

    fn select_account(&mut self, value: Option<String>, cx: &mut Context<Self>) {
        self.selected = value;
        self.save = SaveState::Idle;
        cx.notify();
    }

    /// Serialize and write settings.toml on the background executor, then
    /// show the restart notice (see the module doc for why v0 never restarts
    /// the daemon itself).
    fn write_settings(&mut self, cx: &mut Context<Self>) {
        let text = settings_toml(self.interval, self.selected.as_deref());
        let path = self.paths.settings.clone();
        self.save = SaveState::Saving;
        cx.notify();
        let socket = self.paths.socket.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, text)?;
                    // Restart chezmoid so the new settings apply without user
                    // action: ask it to exit; the app's reconnect loop
                    // respawns it (spec §9). Failure to reach the daemon is
                    // fine — it simply wasn't running.
                    if let Ok(client) = czui_app::ipc::IpcClient::connect(&socket) {
                        let _ = client.request(czui_proto::Request::Shutdown);
                    }
                    Ok::<(), std::io::Error>(())
                })
                .await;
            this.update(cx, |view, cx| {
                view.save = match result {
                    Ok(()) => {
                        view.baseline = Some((view.interval, view.selected.clone()));
                        // The confirmation dismisses itself; edits also clear
                        // it (see bump_interval/select_account).
                        cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(std::time::Duration::from_secs(4))
                                .await;
                            this.update(cx, |view, cx| {
                                if matches!(view.save, SaveState::Saved) {
                                    view.save = SaveState::Idle;
                                    cx.notify();
                                }
                            })
                            .ok();
                        })
                        .detach();
                        SaveState::Saved
                    }
                    Err(e) => SaveState::Error(e.to_string()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Floating toolbar, anchored at the bottom of the pane. Appears only
    /// when there is something to act on (unsaved edits, a save in flight)
    /// or report (saved / failed); Revert discards edits back to disk state.
    fn floating_toolbar(&self, theme: Theme, cx: &mut Context<Self>) -> Option<Div> {
        let saving = matches!(self.save, SaveState::Saving);
        let dirty = self
            .baseline
            .as_ref()
            .is_some_and(|b| *b != (self.interval, self.selected.clone()));
        let show = dirty || !matches!(self.save, SaveState::Idle);
        if !show {
            return None;
        }

        let status: Option<(SharedString, gpui::Rgba)> = match &self.save {
            SaveState::Saved => Some(("Saved · sync daemon restarting".into(), theme.ok)),
            SaveState::Error(e) => Some((format!("save failed: {e}").into(), theme.conflict)),
            SaveState::Saving => Some(("saving…".into(), theme.text_muted)),
            SaveState::Idle => Some((
                "unsaved changes · saving restarts the sync daemon".into(),
                theme.text_muted,
            )),
        };

        let revert = (dirty && !saving).then(|| {
            ui::button(
                theme,
                "revert-settings",
                "Revert".into(),
                ui::ButtonVariant::Ghost,
                ui::ButtonSize::Md,
                cx.listener(|view, _ev, _window, cx| view.revert(cx)),
            )
        });
        let save = (dirty && !saving).then(|| {
            ui::button(
                theme,
                "save-settings",
                "Save".into(),
                ui::ButtonVariant::Outline(theme.accent),
                ui::ButtonSize::Md,
                cx.listener(|view, _ev, _window, cx| view.write_settings(cx)),
            )
        });

        Some(
            ui::toolbar_pill(theme)
                .when_some(status, |el, (text, color)| {
                    el.child(div().text_xs().text_color(color).child(text))
                })
                .when_some(revert, |el, b| el.child(b))
                .when_some(save, |el, b| el.child(b)),
        )
    }

    /// "Fetch interval" row: title + description left, segmented stepper
    /// right (Zed's number-field shape: − │ value │ +).
    fn interval_row(&self, theme: Theme, cx: &mut Context<Self>) -> Div {
        let value: SharedString = if self.loaded {
            format!("{} min", self.interval).into()
        } else {
            "…".into()
        };
        let stepper = ui::stepper(
            theme,
            "interval-minus",
            "interval-plus",
            value,
            self.loaded && self.interval > INTERVAL_MIN,
            self.loaded && self.interval < INTERVAL_MAX,
            cx.listener(|view, _ev, _window, cx| view.bump_interval(false, cx)),
            cx.listener(|view, _ev, _window, cx| view.bump_interval(true, cx)),
        );
        setting_row(
            theme,
            "Fetch interval",
            Some(desc_text(
                theme,
                "How often to check origin for changes. 5–120 minutes.",
            )),
            stepper.into_any_element(),
            false,
        )
    }

    /// Display label for the current account selection.
    fn selected_label(&self) -> SharedString {
        let Some(value) = self.selected.as_deref() else {
            return "None (single account)".into();
        };
        if let AccountsState::Ready(accounts) = &self.accounts
            && let Some(a) = accounts.iter().find(|a| a.value() == Some(value))
        {
            return a.label().into();
        }
        value.to_string().into()
    }

    /// "Account" row: dropdown button right; the popover menu anchors under
    /// its right edge.
    fn account_row(&self, theme: Theme, cx: &mut Context<Self>) -> Div {
        let button = ui::dropdown_button(
            theme,
            "account-dropdown",
            self.selected_label(),
            self.loaded,
            cx.listener(|view, _ev, _window, cx| {
                view.menu_open = !view.menu_open;
                cx.notify();
            }),
        );

        let control = div()
            .flex()
            .flex_col()
            .items_end()
            .child(button)
            .when(self.menu_open, |el| {
                // Zero-size marker: the anchored element positions at its own
                // layout origin, so give it a point (the button's bottom-right
                // corner), not a box with the menu's size.
                el.child(div().h_0().w_0().child(deferred(
                    anchored()
                        .anchor(Corner::TopRight)
                        .offset(point(px(0.), px(4.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(self.account_menu(theme, cx)),
                )))
            });

        let description = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .text_xs()
            .text_color(theme.text_muted)
            .child("Injected as")
            .child(ui::code_chip(theme, "OP_ACCOUNT").py_0())
            .child("into every")
            .child(ui::code_chip(theme, "chezmoi").py_0())
            .child("and")
            .child(ui::code_chip(theme, "op").py_0())
            .child("subprocess.")
            .into_any_element();
        setting_row(theme, "Account", Some(description), control.into_any_element(), false)
    }

    /// The dropdown popover: "None" first (always a real choice), then the
    /// accounts `op` reported — or the loading/unavailable state as an inert
    /// line, so the menu never lies about why the list is short.
    fn account_menu(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut menu = ui::menu(theme)
            .id("account-menu")
            .on_mouse_down_out(cx.listener(|view, _ev, _window, cx| {
                view.menu_open = false;
                cx.notify();
            }))
            .child(self.menu_item(0, None, "None (single account)".into(), None, theme, cx));
        match &self.accounts {
            AccountsState::Loading => {
                menu = menu.child(ui::inert_menu_line(theme, "loading accounts…", theme.text_muted));
            }
            AccountsState::Unavailable => {
                menu = menu.child(ui::inert_menu_line(
                    theme,
                    "1Password CLI not found or errored",
                    theme.drift,
                ));
            }
            AccountsState::Ready(accounts) => {
                let rows: Vec<(Option<String>, SharedString, Option<SharedString>)> = accounts
                    .iter()
                    .filter(|a| a.value().is_some())
                    .map(|a| {
                        let label = a.label();
                        let sublabel = a
                            .email
                            .as_deref()
                            .filter(|email| *email != label)
                            .map(|email| SharedString::from(email.to_owned()));
                        (a.value().map(str::to_owned), label.into(), sublabel)
                    })
                    .collect();
                for (i, (value, label, sublabel)) in rows.into_iter().enumerate() {
                    menu = menu.child(self.menu_item(i + 1, value, label, sublabel, theme, cx));
                }
            }
        }
        menu.into_any_element()
    }

    fn menu_item(
        &self,
        ix: usize,
        value: Option<String>,
        label: SharedString,
        sublabel: Option<SharedString>,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selected = self.selected.as_deref() == value.as_deref();
        ui::menu_row(
            theme,
            ElementId::named_usize("op-account", ix),
            label,
            sublabel,
            selected,
            cx.listener(move |view, _ev, _window, cx| {
                view.select_account(value.clone(), cx);
                view.menu_open = false;
                cx.notify();
            }),
        )
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        let toolbar = self.floating_toolbar(theme, cx);
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .relative()
            .flex()
            .flex_col()
            .when_some(toolbar, |el, toolbar| {
                el.child(
                    div()
                        .absolute()
                        .bottom_4()
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(toolbar),
                )
            })
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .max_w(px(760.))
                            .px_8()
                            .py_5()
                            .flex()
                            .flex_col()
                            .child(ui::section_header(
                                theme,
                                "Sync",
                                ui::SectionHeaderStyle::MonoRuled { spaced: false },
                                None,
                            ))
                            .child(self.interval_row(theme, cx))
                            .child(ui::section_header(
                                theme,
                                "1Password",
                                ui::SectionHeaderStyle::MonoRuled { spaced: true },
                                None,
                            ))
                            .child(self.account_row(theme, cx))
                            .child(ui::section_header(
                                theme,
                                "Paths",
                                ui::SectionHeaderStyle::MonoRuled { spaced: true },
                                None,
                            ))
                            .child(path_row(theme, "Socket", &self.paths.socket, true))
                            .child(path_row(theme, "Journal", &self.paths.journal, true))
                            .child(path_row(theme, "Settings file", &self.paths.settings, false)),
                    ),
            )
    }
}

/// One setting row (Zed's settings-item layout): title + optional muted
/// description left, the control right; optional faded divider below.
fn setting_row(
    theme: Theme,
    title: &'static str,
    description: Option<AnyElement>,
    control: AnyElement,
    divider: bool,
) -> Div {
    div()
        .py_3()
        .when(divider, |el| {
            // In-section separator: dashed and faded, one clear step below
            // the solid section rule (Zed's sub-item treatment).
            el.border_b_1()
                .border_dashed()
                .border_color(Theme::wash(theme.border, 0.7))
        })
        .flex()
        .items_center()
        .justify_between()
        .gap_6()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(div().text_sm().text_color(theme.text).child(title))
                .when_some(description, |el, d| el.child(d)),
        )
        .child(div().flex_none().child(control))
}

/// Plain one-line description under a setting title.
fn desc_text(theme: Theme, text: &'static str) -> AnyElement {
    div()
        .text_xs()
        .text_color(theme.text_muted)
        .child(text)
        .into_any_element()
}

/// Read-only path row: title left, the value as an inline code chip right.
fn path_row(theme: Theme, label: &'static str, path: &Path, divider: bool) -> Div {
    setting_row(
        theme,
        label,
        None,
        div()
            .max_w(px(520.))
            .overflow_hidden()
            .child(ui::code_chip(
                theme,
                super::dashboard::shorten_home(&path.display().to_string()),
            ))
            .into_any_element(),
        divider,
    )
}

#[cfg(test)]
mod tests {
    use czui_core::cmd::CommandError;
    use czui_core::cmd::fake::FakeRunner;

    use super::*;

    #[test]
    fn interval_steps_and_clamps() {
        assert_eq!(step_interval(15, true), 20);
        assert_eq!(step_interval(15, false), 10);
        assert_eq!(step_interval(INTERVAL_MIN, false), INTERVAL_MIN);
        assert_eq!(step_interval(INTERVAL_MAX, true), INTERVAL_MAX);
        assert_eq!(step_interval(118, true), INTERVAL_MAX);
        assert_eq!(step_interval(7, false), INTERVAL_MIN);
        assert_eq!(clamp_interval(0), INTERVAL_MIN);
        assert_eq!(clamp_interval(500), INTERVAL_MAX);
        assert_eq!(clamp_interval(60), 60);
    }

    /// Canned `op account list --format=json` output; the second entry has no
    /// `shorthand` (single-sign-on accounts often don't).
    const OP_FIXTURE: &str = r#"[
      {
        "url": "https://my.1password.com",
        "email": "remi@example.com",
        "user_uuid": "USERUUID",
        "account_uuid": "AAAA1111",
        "shorthand": "personal"
      },
      {
        "url": "https://team.1password.com",
        "email": "work@example.com",
        "account_uuid": "BBBB2222"
      }
    ]"#;

    #[test]
    fn parses_op_accounts_tolerating_missing_shorthand() {
        let accounts = parse_op_accounts(OP_FIXTURE).unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].shorthand.as_deref(), Some("personal"));
        assert_eq!(accounts[0].email.as_deref(), Some("remi@example.com"));
        assert_eq!(accounts[0].account_uuid.as_deref(), Some("AAAA1111"));
        assert_eq!(accounts[1].shorthand, None);
        assert_eq!(accounts[1].email.as_deref(), Some("work@example.com"));
        assert_eq!(accounts[1].account_uuid.as_deref(), Some("BBBB2222"));
    }

    #[test]
    fn parse_op_accounts_edge_shapes() {
        assert!(parse_op_accounts("not json").is_err());
        assert!(parse_op_accounts(r#"{"a": 1}"#).is_err()); // object, not array
        assert_eq!(parse_op_accounts("[]").unwrap(), vec![]);
        // non-object entries and non-string fields are skipped, not fatal
        let accounts = parse_op_accounts(r#"[42, {"shorthand": 7, "email": "e@x"}]"#).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].shorthand, None);
        assert_eq!(accounts[0].email.as_deref(), Some("e@x"));
    }

    #[test]
    fn op_account_value_and_label_precedence() {
        let full = OpAccount {
            shorthand: Some("sh".into()),
            email: Some("e@x".into()),
            account_uuid: Some("U".into()),
        };
        assert_eq!(full.value(), Some("sh"));
        assert_eq!(full.label(), "sh");

        let no_short = OpAccount {
            shorthand: None,
            email: Some("e@x".into()),
            account_uuid: Some("U".into()),
        };
        assert_eq!(no_short.value(), Some("U")); // UUID over email: stable
        assert_eq!(no_short.label(), "e@x"); // email over UUID: human

        let empty = OpAccount {
            shorthand: None,
            email: None,
            account_uuid: None,
        };
        assert_eq!(empty.value(), None);
        assert_eq!(empty.label(), "(unnamed account)");
    }

    #[test]
    fn load_accounts_runs_op_and_maps_states() {
        // success → Ready
        let fake = FakeRunner::new();
        fake.push_ok(0, OP_FIXTURE, "");
        match load_accounts_blocking(&fake) {
            AccountsState::Ready(accounts) => assert_eq!(accounts.len(), 2),
            other => panic!("unexpected: {other:?}"),
        }
        let calls = fake.calls();
        assert_eq!(calls[0].program, "op");
        assert_eq!(calls[0].args, vec!["account", "list", "--format=json"]);

        // op exits non-zero (not signed in, …) → Unavailable
        let fake = FakeRunner::new();
        fake.push_ok(1, "", "no accounts configured");
        assert_eq!(load_accounts_blocking(&fake), AccountsState::Unavailable);

        // op missing entirely → Unavailable
        let fake = FakeRunner::new();
        fake.push_err(CommandError::Spawn {
            program: "op".into(),
            source: std::io::Error::other("not found"),
        });
        assert_eq!(load_accounts_blocking(&fake), AccountsState::Unavailable);

        // garbage stdout → Unavailable
        let fake = FakeRunner::new();
        fake.push_ok(0, "garbage", "");
        assert_eq!(load_accounts_blocking(&fake), AccountsState::Unavailable);
    }

    #[test]
    fn settings_toml_round_trips_through_daemon_settings() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.toml");

        std::fs::write(&p, settings_toml(45, Some("personal"))).unwrap();
        let s = czui_daemon::settings::Settings::load(&p);
        assert_eq!(s.fetch_interval_minutes, 45);
        assert_eq!(s.onepassword_account.as_deref(), Some("personal"));

        // None: the key is omitted entirely and the daemon default kicks in
        std::fs::write(&p, settings_toml(5, None)).unwrap();
        let s = czui_daemon::settings::Settings::load(&p);
        assert_eq!(s.fetch_interval_minutes, 5);
        assert_eq!(s.onepassword_account, None);

        // exotic account values survive TOML string escaping
        std::fs::write(&p, settings_toml(120, Some("we\"ird \\ acct"))).unwrap();
        let s = czui_daemon::settings::Settings::load(&p);
        assert_eq!(s.onepassword_account.as_deref(), Some("we\"ird \\ acct"));
    }

    #[test]
    fn load_settings_defaults_and_clamps() {
        let dir = tempfile::tempdir().unwrap();

        // missing file → daemon defaults
        let doc = load_settings_blocking(&dir.path().join("nope.toml"));
        assert_eq!(doc.fetch_interval_minutes, 15);
        assert_eq!(doc.onepassword_account, None);

        // valid file → parsed
        let p = dir.path().join("settings.toml");
        std::fs::write(
            &p,
            "fetch_interval_minutes = 30\nonepassword_account = \"acct\"\n",
        )
        .unwrap();
        let doc = load_settings_blocking(&p);
        assert_eq!(doc.fetch_interval_minutes, 30);
        assert_eq!(doc.onepassword_account.as_deref(), Some("acct"));

        // out-of-range intervals are clamped for the stepper
        std::fs::write(&p, "fetch_interval_minutes = 500\n").unwrap();
        assert_eq!(load_settings_blocking(&p).fetch_interval_minutes, 120);
        std::fs::write(&p, "fetch_interval_minutes = 1\n").unwrap();
        assert_eq!(load_settings_blocking(&p).fetch_interval_minutes, 5);

        // invalid toml → defaults (same policy as the daemon)
        std::fs::write(&p, "not toml [[[").unwrap();
        assert_eq!(load_settings_blocking(&p).fetch_interval_minutes, 15);
    }
}
