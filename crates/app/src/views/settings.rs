//! Settings view (spec §9; plan 5 Task 7): fetch-interval stepper, 1Password
//! account picker, save-to-settings.toml, read-only paths.
//!
//! gpui has no stock text input, so every control is a stepper or a picker
//! row. All blocking work — the settings.toml read/write and `op account
//! list` — runs on the background executor and lands back in the entity via
//! `WeakEntity::update` (spec §3.2 non-blocking rule). Saving shows a
//! "restart chezmoid to apply" notice instead of restarting the daemon: it
//! reads settings at boot only, and v0 never kills a daemon it may not own.

use std::path::{Path, PathBuf};

use czui_app::theme::Theme;
use czui_core::cmd::{CommandRequest, CommandRunner, SystemRunner};
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, FontWeight, SharedString, Stateful,
    Window, div, prelude::*,
};
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
    /// Written. The daemon reads settings at boot, so the user must restart
    /// chezmoid — v0 shows the notice rather than auto-restarting.
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
        }
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
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, text)
                })
                .await;
            this.update(cx, |view, cx| {
                view.save = match result {
                    Ok(()) => SaveState::Saved,
                    Err(e) => SaveState::Error(e.to_string()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn interval_section(&self, theme: Theme, cx: &mut Context<Self>) -> Div {
        let value: SharedString = if self.loaded {
            format!("{} min", self.interval).into()
        } else {
            "…".into()
        };
        section(theme, "Fetch interval")
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(stepper_button(
                        "interval-minus",
                        "−",
                        self.loaded && self.interval > INTERVAL_MIN,
                        theme,
                        cx.listener(|view, _ev, _window, cx| view.bump_interval(false, cx)),
                    ))
                    .child(div().w_20().text_sm().text_center().child(value))
                    .child(stepper_button(
                        "interval-plus",
                        "+",
                        self.loaded && self.interval < INTERVAL_MAX,
                        theme,
                        cx.listener(|view, _ev, _window, cx| view.bump_interval(true, cx)),
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child("how often chezmoid fetches origin (5–120 minutes, 5-minute steps)"),
            )
    }

    fn accounts_section(&self, theme: Theme, cx: &mut Context<Self>) -> Div {
        // "None (single account)" always leads: it is a real choice even when
        // `op` is unavailable.
        let mut rows: Vec<AnyElement> = vec![
            self.picker_row(0, None, "None (single account)".into(), None, theme, cx)
                .into_any_element(),
        ];
        match &self.accounts {
            AccountsState::Loading => rows.push(
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(theme.text_muted)
                    .child("loading accounts…")
                    .into_any_element(),
            ),
            AccountsState::Unavailable => rows.push(
                div()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.drift)
                    .text_sm()
                    .text_color(theme.drift)
                    .child("1Password CLI not found or errored")
                    .into_any_element(),
            ),
            AccountsState::Ready(accounts) => {
                // Entries with no usable identifier can't be selected (there
                // is nothing to store), so they don't render as rows.
                let data: Vec<(Option<String>, SharedString, Option<SharedString>)> = accounts
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
                for (i, (value, label, sublabel)) in data.into_iter().enumerate() {
                    rows.push(
                        self.picker_row(i + 1, value, label, sublabel, theme, cx)
                            .into_any_element(),
                    );
                }
            }
        }
        section(theme, "1Password account")
            .child(
                div()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child("injected as OP_ACCOUNT into every chezmoi/op subprocess"),
            )
            .children(rows)
    }

    /// One selectable picker row; selection shows as an accent border.
    fn picker_row(
        &self,
        ix: usize,
        value: Option<String>,
        label: SharedString,
        sublabel: Option<SharedString>,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selected = self.selected.as_deref() == value.as_deref();
        div()
            .id(ElementId::named_usize("op-account", ix))
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(if selected { theme.accent } else { theme.border })
            .flex()
            .items_baseline()
            .gap_2()
            .child(div().text_sm().text_color(theme.text).child(label))
            .when_some(sublabel, |el, s| {
                el.child(div().text_xs().text_color(theme.text_muted).child(s))
            })
            .when(self.loaded, |el| {
                el.cursor_pointer()
                    .on_click(cx.listener(move |view, _ev, _window, cx| {
                        view.select_account(value.clone(), cx);
                    }))
            })
    }

    fn save_section(&self, theme: Theme, cx: &mut Context<Self>) -> Div {
        let saving = matches!(self.save, SaveState::Saving);
        let ready = self.loaded && !saving;
        let button = div()
            .id("save-settings")
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(if ready { theme.accent } else { theme.border })
            .text_sm()
            .text_color(if ready {
                theme.accent
            } else {
                theme.text_muted
            })
            .child(if saving { "Saving…" } else { "Save" })
            .when(ready, |el| {
                el.cursor_pointer()
                    .on_click(cx.listener(|view, _ev, _window, cx| view.write_settings(cx)))
            });
        let row = div().flex().items_center().gap_3().child(button);
        match &self.save {
            SaveState::Saved => row.child(
                div()
                    .text_sm()
                    .text_color(theme.drift)
                    .child("Saved — restart chezmoid to apply"),
            ),
            SaveState::Error(e) => row.child(
                div()
                    .text_sm()
                    .text_color(theme.conflict)
                    .child(format!("save failed: {e}")),
            ),
            SaveState::Idle | SaveState::Saving => row,
        }
    }

    fn paths_section(&self, theme: Theme) -> Div {
        section(theme, "Paths")
            .child(path_row(theme, "socket", &self.paths.socket))
            .child(path_row(theme, "journal", &self.paths.journal))
            .child(path_row(theme, "settings", &self.paths.settings))
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        div()
            .id("settings-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .child(self.interval_section(theme, cx))
            .child(self.accounts_section(theme, cx))
            .child(self.save_section(theme, cx))
            .child(self.paths_section(theme))
    }
}

/// One settings card: surface background, bordered, titled.
fn section(theme: Theme, title: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_md()
        .bg(theme.surface)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
}

/// Square − / + button; disabled renders muted with no click handler.
fn stepper_button(
    id: &'static str,
    label: &'static str,
    enabled: bool,
    theme: Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .w_7()
        .h_7()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .text_sm()
        .text_color(if enabled {
            theme.text
        } else {
            theme.text_muted
        })
        .when(enabled, |el| el.cursor_pointer().on_click(on_click))
        .child(label)
}

/// Read-only path line: muted label + monospaced path.
fn path_row(theme: Theme, label: &'static str, path: &Path) -> Div {
    div()
        .flex()
        .items_baseline()
        .gap_2()
        .child(
            div()
                .w_16()
                .flex_shrink_0()
                .text_xs()
                .text_color(theme.text_muted)
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .text_xs()
                .font_family("Menlo")
                .text_color(theme.text)
                .truncate()
                .child(SharedString::from(path.display().to_string())),
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
