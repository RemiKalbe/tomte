//! Dashboard: health tiles (approved mockup B) + chronological activity list
//! (Zed-git-panel density). Rows with a target open Review; consecutive
//! scan/fetch noise collapses into one expandable line.
//!
//! Pure logic (relative time, glyphs, labels, grouping) lives in
//! `tomte_app::model`; this module renders and never blocks.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Context, Div, ElementId, Entity, FontWeight, Rgba, SharedString, Stateful, WeakEntity,
    div, prelude::*, uniform_list,
};
use tomte_app::action_queue::{Decision, QueuedAction};
use tomte_app::model::{
    SyncModel, TimelineItem, TimelineRow, class_label, group_timeline, kind_glyph, kind_label,
    time_ago,
};
use tomte_app::resolve::{ResolveEngine, ResolveError, ResolveOutcome};
use tomte_app::theme::Theme;
use tomte_core::cmd::SystemRunner;
use tomte_ui::components as ui;

use super::review::ResolveAction;
use super::{RepoOpKind, Shell};

/// Seconds since the Unix epoch — the production clock injected into
/// [`DashboardView::now_ts`] (tests inject fixed values into the pure fns).
pub fn system_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the degraded banner's action button should do — matched against our
/// own hint texts from `tomte_core::chezmoi::classify_eval_stderr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DegradedRemedy {
    /// 1Password is locked: the fix is an unlock prompt, not a setting.
    UnlockOnePassword,
    /// Configuration problem (account selection): Settings is the fix.
    OpenSettings,
}

pub(super) fn degraded_remedy(hint: &str) -> Option<DegradedRemedy> {
    if hint.contains("Unlock 1Password") {
        Some(DegradedRemedy::UnlockOnePassword)
    } else if hint.contains("Settings") {
        Some(DegradedRemedy::OpenSettings)
    } else {
        None
    }
}

/// Moved to tomte-ui; re-exported so sibling views keep their import paths.
pub(super) use tomte_ui::components::shorten_home;

/// One flattened display line for the uniform list.
enum Line {
    /// A real event row.
    Event {
        time: SharedString,
        glyph: &'static str,
        name: SharedString,
        dir: Option<SharedString>,
        chip: Option<SharedString>,
        class: Option<SharedString>,
        target: Option<PathBuf>,
        /// Rendered indented under an expanded scan group.
        indented: bool,
        /// The target is CURRENTLY drifted (in `model.drifted`) → the row
        /// carries inline keep-disk / keep-source quick actions.
        drifted: bool,
    },
    /// Collapsed run of scans/fetches; click toggles expansion.
    Group {
        count: usize,
        time: SharedString,
        key: u64,
        expanded: bool,
    },
}

impl Line {
    fn event(row: &TimelineRow, now: u64, indented: bool, drifted: bool) -> Self {
        let (name, dir): (SharedString, Option<SharedString>) = match &row.target {
            Some(target) => (
                super::review::display_name(target),
                target
                    .parent()
                    .map(|p| SharedString::from(shorten_home(&p.display().to_string()))),
            ),
            None => (kind_label(&row.kind).into(), None),
        };
        Self::Event {
            time: time_ago(now, row.ts).into(),
            glyph: kind_glyph(&row.kind),
            name,
            dir,
            chip: row.class.as_ref().map(|class| match &row.label {
                Some(label) => SharedString::from(label.clone()),
                None => SharedString::from(class_label(class)),
            }),
            class: row.class.clone().map(SharedString::from),
            target: row.target.clone(),
            indented,
            drifted,
        }
    }
}

/// Whether a timeline row's target is currently drifted — those rows carry
/// the inline quick actions (plan 6 Task 3).
fn is_drifted(model: &SyncModel, row: &TimelineRow) -> bool {
    row.target
        .as_ref()
        .is_some_and(|t| model.drifted.iter().any(|d| &d.target == t))
}

/// Flatten grouped timeline items into display lines, honoring expansion.
fn build_lines(model: &SyncModel, now: u64, expanded: &HashSet<u64>) -> Vec<Line> {
    let mut lines = Vec::new();
    for item in group_timeline(&model.timeline) {
        match item {
            TimelineItem::Row(row) => {
                lines.push(Line::event(&row, now, false, is_drifted(model, &row)))
            }
            TimelineItem::ScanGroup {
                count,
                newest_ts,
                rows,
            } => {
                let is_open = expanded.contains(&newest_ts);
                lines.push(Line::Group {
                    count,
                    time: time_ago(now, newest_ts).into(),
                    key: newest_ts,
                    expanded: is_open,
                });
                if is_open {
                    for row in &rows {
                        lines.push(Line::event(row, now, true, is_drifted(model, row)));
                    }
                }
            }
        }
    }
    lines
}

pub struct DashboardView {
    pub state: Entity<SyncModel>,
    pub now_ts: fn() -> u64,
    /// Cloned from `Shell::expanded_scans` by the caller — reading the Shell
    /// entity from inside its own render panics ("already being updated").
    pub expanded_scans: HashSet<u64>,
    /// Copied from `Shell::unlock_in_flight`: the 1Password unlock probe is
    /// waiting on the user's approval.
    pub unlock_in_flight: bool,
    /// Copied from `Shell::pending_merge`: a merge with origin is resolved
    /// but uncommitted, and nothing else can surface it — no conflicted file
    /// is left to be a review row.
    pub pending_merge: bool,
    /// `Shell::deferred_repo_op`'s kind: a repo-level answer the user gave
    /// that has not started yet, because the latch is held.
    pub pending_repo_op: Option<RepoOpKind>,
    /// `Shell::running_repo_op`: the one that IS running.
    pub running_repo_op: Option<RepoOpKind>,
    /// The targets whose decision is in flight under the latch, from
    /// `Shell::queue.running()`. Their quick actions go inert — a second press
    /// cannot replace a decision that has already started, it can only queue a
    /// duplicate (see `quick_action_button`).
    ///
    /// Copied, like everything else here: the dashboard body is rebuilt from
    /// Shell state on every render and cannot borrow the Shell entity from
    /// inside its own render.
    pub running: Vec<PathBuf>,
}

/// The Finish button's label and whether it is live, for one repo-op state.
///
/// A pure function so the four cases are assertable without a window — and
/// so the rule lives in one place rather than inside a `when_some` chain in
/// the middle of a banner.
///
/// The abort arm is the one that matters. `Shell::request_repo_op` already
/// REFUSES a finish that would displace a confirmed roll-back, but a button
/// that stays bright and does nothing when pressed teaches the user that the
/// app is broken. This stops the press from ever reaching that guard, and the
/// guard stays as the last line of defence for the click that lands in the
/// same frame as the confirmation.
///
/// ONE source of truth for "a finish is running": `running`. There used to be
/// a third parameter, `Shell::merge_finish_in_flight`, set and cleared over
/// exactly the window `running == Some(Finish)` already spans — so it could
/// only ever agree, until the day something set one without the other
/// (Ruling 41).
pub(super) fn finish_button_state(
    pending: Option<RepoOpKind>,
    running: Option<RepoOpKind>,
) -> (&'static str, bool) {
    if pending == Some(RepoOpKind::Abort) || running == Some(RepoOpKind::Abort) {
        // Pending or running, the merge this banner is about is being thrown
        // away. Offering to commit it would be offering the opposite of the
        // answer the user already confirmed.
        return ("Rolling back…", false);
    }
    if pending == Some(RepoOpKind::Finish) {
        // Pressed, and waiting behind a batch that can take seconds. Saying
        // "Finish merge" still would invite a second press that does nothing.
        return ("Finish merge · queued", false);
    }
    if running == Some(RepoOpKind::Finish) {
        return ("Finishing…", false);
    }
    ("Finish merge", true)
}

impl DashboardView {
    /// Render the dashboard body. Constructed fresh in each `Shell` render;
    /// the shell re-renders whenever the model entity notifies.
    pub fn render(self, theme: Theme, cx: &mut Context<Shell>) -> impl IntoElement + use<> {
        let now = (self.now_ts)();
        let expanded = self.expanded_scans;
        let model = self.state.read(cx);

        let needs_attention = model.needs_attention();
        let drifted_count = model.drifted.len();
        let in_sync = model.in_sync;
        let scanning = model.scanning;
        let connected = model.connected;
        let degraded = model.degraded.clone();
        let have_data = drifted_count > 0 || in_sync > 0;
        let counts_known = connected && (have_data || !scanning);
        // Freshness carries its own severity: recent is plain fact, stale is
        // drift-tinted, unknown is muted (never a bold alarm headline).
        // A manual fetch in flight (60s validity — a lost push must not
        // stick the tile) and a failed fetch each say so plainly.
        let fetching = model
            .fetch_started_ts
            .is_some_and(|t| now.saturating_sub(t) < 60);
        // Errors are a STATE, not a headline: the tile says "fetch failed",
        // the full message lives in a hover tooltip (2026-08-17: raw git
        // stderr crammed into the tile header read as broken UI).
        let fetch_error_detail: Option<SharedString> =
            model.fetch_failed.clone().map(SharedString::from);
        let (freshness, freshness_color): (SharedString, Rgba) = if fetching {
            ("fetching…".into(), theme.text_muted)
        } else if model.fetch_failed.is_some() {
            ("fetch failed".into(), theme.conflict)
        } else {
            match model.last_fetch_ts {
                Some(ts) => (
                    format!("fetched {}", time_ago(now, ts)).into(),
                    // 2x the default 15-min fetch interval before stale.
                    if now.saturating_sub(ts) > 30 * 60 {
                        theme.drift
                    } else {
                        theme.text
                    },
                ),
                None => ("never fetched".into(), theme.text_muted),
            }
        };

        let lines: Rc<Vec<Line>> = Rc::new(build_lines(model, now, &expanded));
        let shell = cx.weak_entity();
        // The engine, for the two things on this screen that still call it
        // directly: the origin tile's "Fetch now" and the degraded banner's
        // 1Password unlock. NOT the quick actions — those record a
        // `Decision` on the queue and never touch the engine, which is why
        // they stay live with no daemon (the press parks).
        let engine = cx
            .try_global::<crate::EngineSlot>()
            .and_then(|slot| slot.0.clone());
        let running = Rc::new(self.running);
        let unlock_in_flight = self.unlock_in_flight;
        let pending_merge = self.pending_merge;
        let (finish_label, finish_live) =
            finish_button_state(self.pending_repo_op, self.running_repo_op);

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                // Health tiles — the approved mockup B: big value over muted
                // label, bordered cards, Review shortcut in the first tile.
                div()
                    .flex()
                    .gap_3()
                    .px_4()
                    .pt_4()
                    .pb_2()
                    .flex_none()
                    .child(tile(
                        theme,
                        if counts_known {
                            needs_attention.to_string()
                        } else {
                            "–".into()
                        },
                        "need a decision",
                        if needs_attention > 0 {
                            theme.conflict
                        } else {
                            theme.text_muted
                        },
                        (drifted_count > 0)
                            .then(|| review_link(theme, drifted_count, shell.clone())),
                    ))
                    .child({
                        let t = tile(
                            theme,
                            freshness.to_string(),
                            "origin",
                            freshness_color,
                            Some(fetch_now_button(
                                theme,
                                engine.clone(),
                                self.state.clone(),
                                fetching,
                                model.fetch_failed.is_some(),
                            )),
                        );
                        match &fetch_error_detail {
                            Some(detail) => t
                                .id("origin-tile")
                                .tooltip(ui::text_tooltip(detail.clone()))
                                .into_any_element(),
                            None => t.into_any_element(),
                        }
                    })
                    .child(tile(
                        theme,
                        if counts_known {
                            in_sync.to_string()
                        } else {
                            "–".into()
                        },
                        "in sync",
                        if counts_known {
                            theme.ok
                        } else {
                            theme.text_muted
                        },
                        None,
                    )),
            )
            .when_some(degraded, |el, hint| {
                let shell = shell.clone();
                let remedy = degraded_remedy(&hint);
                // The button IS the instruction — drop our own hint's
                // redundant trailing imperative so the text stays short
                // (the banner clips rather than ellipsizes; gpui quirk).
                let hint = if remedy == Some(DegradedRemedy::UnlockOnePassword) {
                    hint.trim_end()
                        .trim_end_matches("Unlock 1Password and retry.")
                        .trim_end()
                        .to_string()
                } else {
                    hint
                };
                let action = match remedy {
                    _ if unlock_in_flight => Some(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child("waiting for 1Password…")
                            .into_any_element(),
                    ),
                    Some(DegradedRemedy::UnlockOnePassword) => Some(
                        ui::button(
                            theme,
                            "degraded-unlock-1p",
                            "Unlock 1Password".into(),
                            ui::ButtonVariant::Solid(theme.drift),
                            ui::ButtonSize::Micro,
                            {
                                let shell = shell.clone();
                                let engine = engine.clone();
                                move |_event, _window, cx| {
                                    trigger_onepassword_unlock(shell.clone(), engine.clone(), cx);
                                }
                            },
                        )
                        .into_any_element(),
                    ),
                    Some(DegradedRemedy::OpenSettings) => Some(
                        ui::button(
                            theme,
                            "degraded-open-settings",
                            "Open Settings".into(),
                            ui::ButtonVariant::Solid(theme.drift),
                            ui::ButtonSize::Micro,
                            move |_event, _window, cx| {
                                let _ = shell.update(cx, |shell, cx| {
                                    shell.route = super::Route::Settings;
                                    cx.notify();
                                });
                            },
                        )
                        .into_any_element(),
                    ),
                    None => None,
                };
                // Self-healing is real but was invisible — say it in the
                // non-truncating action slot (long hints ellipsize; the
                // promise and the button never do).
                let action = Some(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(Theme::wash(theme.drift, 0.7))
                                .child("re-checks every minute"),
                        )
                        .when_some(action, |el, a| el.child(a))
                        .into_any_element(),
                );
                el.child(
                    ui::banner(theme, ui::BannerTint::Drift, hint.into(), action)
                        .mx_4()
                        .mb_2()
                        .mt_0(),
                )
            })
            // A merge with origin that is fully resolved but uncommitted has
            // no drifted row left to speak for it — every file in it is
            // answered. Without this banner the repo sits mid-merge in
            // silence until someone opens a terminal.
            .when(pending_merge, |el| {
                let shell = shell.clone();
                let action = Some(if finish_live {
                    ui::button(
                        theme,
                        "pending-merge-finish",
                        finish_label.into(),
                        ui::ButtonVariant::Solid(theme.conflict),
                        ui::ButtonSize::Micro,
                        move |_event, _window, cx| {
                            let _ = shell.update(cx, |shell, cx| shell.finish_pending_merge(cx));
                        },
                    )
                    .into_any_element()
                } else {
                    // Still a button, not a bare word: it says what the app is
                    // doing to this merge and stays in the place the live one
                    // occupies, so the banner does not reflow under the cursor.
                    ui::disabled_button(
                        theme,
                        "pending-merge-finish",
                        finish_label.into(),
                        ui::ButtonSize::Micro,
                        None,
                    )
                    .flex_none()
                    .into_any_element()
                });
                el.child(
                    ui::banner(
                        theme,
                        ui::BannerTint::Conflict,
                        "a merge with origin is resolved but not committed — its commit \
                         did not go through"
                            .into(),
                        action,
                    )
                    .mx_4()
                    .mb_2()
                    .mt_0(),
                )
            })
            .when(!lines.is_empty() || scanning, |el| {
                el.child(
                    div()
                        .px_4()
                        .pb_1()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme.text_muted)
                                        .child("ACTIVITY"),
                                )
                                // Scan in flight: a quiet spinner, no words
                                // (2026-08-08: the banner + "rescanning…"
                                // pair was loud for a routine background op).
                                .when(scanning, |el| {
                                    el.child(ui::spinner(theme, "activity-scan-spinner"))
                                }),
                        ),
                )
            })
            .child(if lines.is_empty() {
                if scanning {
                    skeleton_rows(theme)
                } else if !connected {
                    // Spell out WHAT tomted is and WHY it's down — "daemon
                    // not connected" alone reads as jargon with no next step
                    // (2026-08-10, fresh second-machine install).
                    let why = model
                        .connect_error
                        .as_deref()
                        .and_then(|e| e.lines().last())
                        .map(|e| format!("last error: {e}"))
                        .unwrap_or_else(|| "reconnecting automatically…".into());
                    ui::empty_state(
                        theme,
                        "●",
                        theme.conflict,
                        "Tomte's background watcher (tomted) isn't running",
                        format!(
                            "it scans your dotfiles for drift · restarts automatically · {why}"
                        ),
                    )
                    .into_any_element()
                } else {
                    ui::empty_state(
                        theme,
                        "✓",
                        theme.ok,
                        "Everything in sync",
                        format!("{in_sync} files tracked"),
                    )
                    .into_any_element()
                }
            } else {
                uniform_list(
                    "dashboard-timeline",
                    lines.len(),
                    move |range, _window, _cx| {
                        range
                            .map(|ix| render_line(ix, &lines[ix], theme, shell.clone(), &running))
                            .collect()
                    },
                )
                .flex_1()
                .px_2()
                .into_any_element()
            })
    }
}

/// Skeleton rows while the first scan runs: the shape of the activity list,
/// not a sentence about it.
fn skeleton_rows(theme: Theme) -> gpui::AnyElement {
    div()
        .px_4()
        .pt_1()
        .flex()
        .flex_col()
        .gap_3()
        .children([0.40_f32, 0.65, 0.52].into_iter().map(|w| {
            div()
                .h_5()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(gpui::px(56.))
                        .h_2()
                        .flex_none()
                        .rounded_sm()
                        .bg(Theme::wash(theme.text, 0.06)),
                )
                .child(
                    div()
                        .h_2()
                        .w(gpui::relative(w))
                        .rounded_sm()
                        .bg(Theme::wash(theme.text, 0.09)),
                )
        }))
        .into_any_element()
}

/// Run a cheap `op` command: with 1Password desktop-app integration this
/// pops the system unlock prompt (Touch ID / password). Whatever the
/// outcome, ask the daemon to rescan — if the user authorized, the next
/// scan heals; if not, the banner honestly stays.
fn trigger_onepassword_unlock(
    shell: WeakEntity<Shell>,
    engine: Option<Arc<ResolveEngine>>,
    cx: &mut App,
) {
    let set_flag = |cx: &mut App, value: bool| {
        let _ = shell.update(cx, |shell, cx| {
            shell.unlock_in_flight = value;
            cx.notify();
        });
    };
    set_flag(cx, true);
    let shell2 = shell.clone();
    cx.spawn(async move |cx| {
        let _ = cx
            .background_executor()
            .spawn(async move {
                use tomte_core::cmd::{CommandRequest, CommandRunner as _};
                // Generous timeout: this intentionally waits for a human to
                // approve the 1Password prompt.
                let _ = SystemRunner.run(
                    CommandRequest::new("op")
                        .args(["whoami", "--format=json"])
                        .timeout(std::time::Duration::from_secs(120)),
                );
                if let Some(engine) = engine {
                    // Best-effort, but never silent: the unlock's whole point
                    // is the rescan that follows it.
                    match engine.ipc.request_patiently(
                        tomte_proto::Request::Rescan,
                        tomte_app::ipc::DAEMON_PATIENCE,
                        "rescan after unlock",
                    ) {
                        Ok(tomte_proto::Response::Ok) => {}
                        Ok(other) => tomte_core::log_warn!(
                            "unlock",
                            "rescan after unlock: unexpected reply {other:?}"
                        ),
                        Err(e) => {
                            tomte_core::log_warn!("unlock", "rescan after unlock failed: {e}")
                        }
                    }
                }
            })
            .await;
        let _ = cx.update(|cx| {
            let _ = shell2.update(cx, |shell, cx| {
                shell.unlock_in_flight = false;
                cx.notify();
            });
        });
    })
    .detach();
}

/// One mockup-B health tile: big value, muted label, optional action slot.
/// "Fetch now" in the origin tile: ask the daemon to fetch immediately
/// (ack now, FetchDone push updates the freshness). Muted+tooltip while
/// disconnected — same contract as every other action.
fn fetch_now_button(
    theme: Theme,
    engine: Option<Arc<ResolveEngine>>,
    state: Entity<SyncModel>,
    fetching: bool,
    failed: bool,
) -> gpui::AnyElement {
    if fetching {
        // In flight: the spinner IS the feedback (2026-08-08: clicking gave
        // none); the FetchDone/FetchFailed push resolves it.
        return div()
            .flex()
            .items_center()
            .gap_1p5()
            .child(ui::spinner(theme, "tile-fetch-spinner"))
            .into_any_element();
    }
    let label: SharedString = if failed {
        "Retry".into()
    } else {
        "Fetch now".into()
    };
    match engine {
        Some(engine) => ui::button(
            theme,
            "tile-fetch-now",
            label,
            ui::ButtonVariant::Ring(theme.accent),
            ui::ButtonSize::Sm,
            move |_event, _window, cx| {
                let engine = engine.clone();
                state.update(cx, |model, cx| {
                    model.fetch_started_ts = Some(system_now());
                    model.fetch_failed = None;
                    cx.notify();
                });
                let state = state.clone();
                cx.spawn(async move |cx| {
                    let outcome = cx
                        .background_executor()
                        .spawn(async move {
                            // Retried across the daemon's busy bounces like
                            // every decision is: a scan in progress is not a
                            // failed fetch. A single bounce never reaches the
                            // tile; only a daemon that stayed busy past the
                            // budget does, in those words, the bounce quoted
                            // inside.
                            engine.ipc.request_patiently(
                                tomte_proto::Request::Fetch,
                                tomte_app::ipc::DAEMON_PATIENCE,
                                "fetch now",
                            )
                        })
                        .await;
                    // Only failures resolve the spinner here — on an ack the
                    // FetchDone/FetchFailed push is the truth. (2026-08-08: a
                    // stale-protocol daemon rejected the request and the
                    // swallowed error left the spinner up forever.)
                    let error = match outcome {
                        Ok(tomte_proto::Response::Error { message }) => Some(message),
                        Ok(_) => None,
                        Err(e) => Some(e.to_string()),
                    };
                    if let Some(error) = error {
                        let _ = cx.update(|cx| {
                            state.update(cx, |model, cx| {
                                model.fetch_started_ts = None;
                                model.fetch_failed = Some(error);
                                cx.notify();
                            });
                        });
                    }
                })
                .detach();
            },
        )
        .into_any_element(),
        None => ui::disabled_button(
            theme,
            "tile-fetch-now",
            label,
            ui::ButtonSize::Sm,
            Some("daemon not connected".into()),
        )
        .into_any_element(),
    }
}

fn tile(
    theme: Theme,
    value: String,
    label: &'static str,
    color: Rgba,
    action: Option<gpui::AnyElement>,
) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_md()
        .bg(theme.surface)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color)
                        .child(value),
                )
                .when_some(action, |el, action| el.child(action)),
        )
        .child(div().text_xs().text_color(theme.text_muted).child(label))
}

/// The "Review →" shortcut inside the attention tile (mockup B).
fn review_link(theme: Theme, drifted: usize, shell: WeakEntity<Shell>) -> gpui::AnyElement {
    ui::button(
        theme,
        "tile-review",
        format!("Review {drifted} →").into(),
        ui::ButtonVariant::Ring(theme.accent),
        ui::ButtonSize::Sm,
        move |_event, _window, cx| {
            let _ = shell.update(cx, |shell, cx| {
                shell.open_review(None, cx);
            });
        },
    )
    .into_any_element()
}

fn render_line(
    ix: usize,
    line: &Line,
    theme: Theme,
    shell: WeakEntity<Shell>,
    running: &[PathBuf],
) -> gpui::AnyElement {
    match line {
        Line::Group {
            count,
            time,
            key,
            expanded,
        } => {
            let key = *key;
            let chevron = if *expanded { "▾" } else { "▸" };
            div()
                .id(ElementId::named_usize("scan-group", ix))
                .h_7()
                .w_full()
                .px_2()
                .rounded_sm()
                .flex()
                .items_center()
                .gap_2()
                .cursor_pointer()
                .hover(|el| el.bg(Theme::wash(theme.text, 0.05)))
                .child(
                    div()
                        .w(gpui::px(56.))
                        .flex_none()
                        .text_xs()
                        .text_right()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_color(theme.text_muted)
                        .child(time.clone()),
                )
                .child(
                    div()
                        .w_4()
                        .flex_none()
                        .text_xs()
                        .text_center()
                        .text_color(theme.text_muted)
                        .child(chevron),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child(format!("{count} background events")),
                )
                .on_click(move |_event, _window, cx| {
                    let _ = shell.update(cx, |shell, cx| {
                        if !shell.expanded_scans.remove(&key) {
                            shell.expanded_scans.insert(key);
                        }
                        cx.notify();
                    });
                })
                .into_any_element()
        }
        Line::Event {
            time,
            glyph,
            name,
            dir,
            chip,
            class,
            target,
            indented,
            drifted,
        } => {
            // Inline quick actions on currently-drifted rows (right-aligned,
            // before the class chip). `drifted` implies a target.
            let quick = (*drifted)
                .then_some(target.as_ref())
                .flatten()
                .map(|target| {
                    quick_actions(
                        ix,
                        target,
                        theme,
                        running.iter().any(|r| r == target),
                        shell.clone(),
                    )
                });
            let base = div()
                .h_7()
                .w_full()
                .px_2()
                .when(*indented, |el| el.pl_8())
                .rounded_sm()
                .flex()
                .items_center()
                .gap_2()
                .group("activity-row")
                .child(
                    div()
                        .w(gpui::px(56.))
                        .flex_none()
                        .text_xs()
                        .text_right()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_color(theme.text_muted)
                        .child(time.clone()),
                )
                .child(
                    div()
                        .w_4()
                        .flex_none()
                        .text_sm()
                        .text_center()
                        .text_color(match class {
                            Some(class) => theme.class_color(class),
                            None => theme.text_muted,
                        })
                        .child(*glyph),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(if *indented {
                                    theme.text_muted
                                } else {
                                    theme.text
                                })
                                .whitespace_nowrap()
                                .child(name.clone()),
                        )
                        .when_some(dir.clone(), |el, dir| {
                            el.child(
                                div()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(theme.text_muted)
                                    .truncate()
                                    .child(dir),
                            )
                        })
                        .when_some(chip.clone(), |el, label| {
                            let color = class
                                .as_deref()
                                .map(|c| theme.class_color(c))
                                .unwrap_or(theme.text_muted);
                            el.child(
                                ui::chip(theme, label, ui::ChipVariant::Wash(color)).flex_none(),
                            )
                        }),
                )
                .when_some(quick, |el, actions| el.child(actions));

            match target.clone() {
                Some(target) => base
                    .id(ElementId::named_usize("activity-row", ix))
                    .cursor_pointer()
                    .hover(|el| el.bg(Theme::wash(theme.text, 0.05)))
                    .on_click(move |_event, _window, cx| {
                        let target = target.clone();
                        let _ = shell.update(cx, |shell, cx| {
                            shell.open_review(Some(target.clone()), cx);
                        });
                    })
                    .into_any_element(),
                None => base.into_any_element(),
            }
        }
    }
}

/// The two decisions this surface is able to make.
///
/// A TYPE, not a `ResolveAction` with a wildcard arm. Everything here is
/// built from a timeline row: no file content has been loaded, so "keep
/// source · origin" — which needs origin's text exactly as it was SHOWN, and
/// refuses without it — cannot be offered honestly. Spelling that as
/// `_ => Decision::KeepSourceLocal` would mean the day a third button is
/// added it silently writes THIS MACHINE's source over the user's file, with
/// no compile error anywhere. Here a third button is a third variant, and
/// [`Self::decision`] does not compile until it says what that variant writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QuickAction {
    KeepDisk,
    KeepSourceLocal,
}

impl QuickAction {
    /// The shared button vocabulary — label, id, and the words a failure uses.
    fn action(self) -> ResolveAction {
        match self {
            Self::KeepDisk => ResolveAction::KeepDisk,
            Self::KeepSourceLocal => ResolveAction::KeepSourceLocal,
        }
    }

    /// What this button writes. Total: no wildcard, by construction.
    fn decision(self) -> Decision {
        match self {
            Self::KeepDisk => Decision::KeepDisk,
            Self::KeepSourceLocal => Decision::KeepSourceLocal,
        }
    }
}

/// The inline quick-action cluster for one drifted row: keep-disk /
/// keep-source buttons. Outcomes surface as an osascript notification (the
/// dashboard is transient — no banner, and no processing group either).
///
/// No "working…" marker any more, and no shared busy flag behind it: a
/// decision here is queued like every other, so the buttons on THIS row and
/// on every other row stay live while it drains. That flag existing at all
/// was defect §1.1 — one boolean that made every other file's button a no-op.
fn quick_actions(
    ix: usize,
    target: &Path,
    theme: Theme,
    running: bool,
    shell: WeakEntity<Shell>,
) -> Div {
    // Ghost until the row is hovered (Zed idiom): the chip carries the
    // information at rest; the response appears when the user reaches for it.
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap_1()
        .opacity(0.)
        .group_hover("activity-row", |el| el.opacity(1.))
        .child(quick_action_button(
            ix,
            QuickAction::KeepDisk,
            target,
            theme,
            running,
            shell.clone(),
        ))
        // Row actions carry no origin split: nothing here has loaded the
        // file's content, and `DriftSummary::origin_differs` alone cannot
        // tell a file origin has its own version of from one origin has
        // never seen (added locally, never pushed). The review view, which
        // does load the content, is where that decision is offered.
        .child(quick_action_button(
            ix,
            QuickAction::KeepSourceLocal,
            target,
            theme,
            running,
            shell,
        ))
}

/// Why this row's quick actions are inert, or `None` when they are live.
///
/// ONE rule, the same one `review::blocked_reason` applies and in the same
/// words: this file's decision is already RUNNING, so a second press could
/// not replace it, only duplicate it. MERELY QUEUED is not the same thing —
/// `ActionQueue::push` replaces a pending choice, so that row stays live and
/// re-choosable (spec §2.4) — and every OTHER file's buttons stay live
/// throughout, which is the whole point of the queue.
///
/// A MISSING DAEMON IS NOT A REASON, on this surface either. Parking is the
/// queue's model: `Shell::pump` leaves everything queued when no engine is
/// published, and the connect latch releases it on the next (re)connect. The
/// press is recorded and runs when the daemon is back — refusing it would be
/// the discarded click this whole plan exists to remove.
///
/// Pure so the rule is a value a test can assert rather than something only a
/// tooltip in a screenshot could show.
pub(super) fn quick_action_blocked(running: bool) -> Option<SharedString> {
    if running {
        return Some("applying this file's decision now".into());
    }
    None
}

/// One inline quick action: record the decision on the Shell's queue and be
/// done with it.
///
/// The click used to spawn the engine call itself, behind a shell-wide
/// `dashboard_action_in_flight` boolean — beside whatever the queue was
/// draining, on the same git repository. Now it is a [`Decision`] like every
/// other, and the press returns the instant the choice is remembered.
///
/// The one thing that makes a button inert, and why a missing daemon is not
/// on that list, both live in [`quick_action_blocked`] — the duplicate a
/// second press would queue is not harmless: it re-adds identical content,
/// commits and pushes again, and opens a SECOND journal session, which is
/// then the one Undo reverts.
fn quick_action_button(
    ix: usize,
    action: QuickAction,
    target: &Path,
    theme: Theme,
    running: bool,
    shell: WeakEntity<Shell>,
) -> Stateful<Div> {
    let id = ElementId::named_usize(action.action().button_id(), ix);
    if let Some(reason) = quick_action_blocked(running) {
        return ui::disabled_button(
            theme,
            id,
            action.action().label().into(),
            ui::ButtonSize::Micro,
            Some(reason),
        )
        .flex_none();
    }
    let target = target.to_path_buf();
    ui::button(
        theme,
        id,
        action.action().label().into(),
        ui::ButtonVariant::Tonal(theme.accent),
        ui::ButtonSize::Micro,
        move |_event, _window, cx| {
            // The row underneath opens Review on click — quick actions must
            // not also navigate.
            cx.stop_propagation();
            let target = target.clone();
            let _ = shell.update(cx, |shell, cx| {
                shell.enqueue(
                    QueuedAction {
                        target,
                        decision: action.decision(),
                        // The one surface that asks to be told: this row just
                        // stops being drifted, there is no banner here, and
                        // the answer can be minutes away.
                        notify_on_done: true,
                    },
                    cx,
                );
            });
        },
    )
    .flex_none()
}

/// Notification body for a queued dashboard decision's outcome — same honesty
/// rules as the review banner (`note` = degraded commit/push, spelled out).
///
/// Takes the [`Decision`], not the button: by the time this is built the
/// press is minutes old, the queue is what remembers it, and the queue speaks
/// in decisions. `verb` is what the choice did, `label` names the button that
/// failed — the same two words the row and the banner use.
pub(super) fn quick_action_body(
    decision: &Decision,
    target: &Path,
    result: &Result<super::BatchResult, ResolveError>,
) -> String {
    let name = super::review::display_name(target);
    let outcome = match result {
        Ok(super::BatchResult::Resolved(outcome)) => outcome,
        // Defensive: only a merge-editor Save stages a repo conflict, and
        // that surface never asks for a notification.
        Ok(super::BatchResult::RepoConflictStaged) => {
            return format!("{name}: recorded into the merge with origin");
        }
        Err(e) => return format!("{} {name} failed: {e}", decision.label()),
    };
    match outcome {
        ResolveOutcome::Done { note: None, .. } => format!("{}: {name}", decision.verb()),
        ResolveOutcome::Done {
            note: Some(note), ..
        } => format!("{}: {name} · {note}", decision.verb()),
        // A templated keep-disk whose on-disk edit changed a templated VALUE:
        // the write-back refused it, and only the merge editor can say what
        // that value should be.
        ResolveOutcome::ProtectedSpan { .. } => {
            format!("{name}: this change touches a templated value · open the merge editor")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tomte_app::action_queue::Decision;
    use tomte_app::resolve::{ResolveError, ResolveOutcome};

    use super::super::{BatchResult, RepoOpKind};
    use super::{
        DegradedRemedy, QuickAction, degraded_remedy, finish_button_state, quick_action_blocked,
        quick_action_body,
    };

    fn done(note: Option<&str>) -> Result<BatchResult, ResolveError> {
        Ok(BatchResult::Resolved(ResolveOutcome::Done {
            session: 1,
            committed: true,
            pushed: note.is_none(),
            note: note.map(str::to_owned),
        }))
    }

    #[test]
    fn degraded_remedy_matches_cause() {
        // Locked 1P: the fix is an unlock prompt, not Settings.
        assert_eq!(
            degraded_remedy("1Password CLI could not authenticate. Unlock 1Password and retry."),
            Some(DegradedRemedy::UnlockOnePassword)
        );
        // Config problem: Settings is right.
        assert_eq!(
            degraded_remedy(
                "Select a 1Password account in Settings (sets OP_ACCOUNT for all chezmoi calls)."
            ),
            Some(DegradedRemedy::OpenSettings)
        );
        // Unknown causes get no button, just the honest text.
        assert_eq!(degraded_remedy("age decryption failed"), None);
    }

    #[test]
    fn quick_action_body_reports_success_degraded_templated_and_error() {
        let t = Path::new("/Users/x/.zshrc");
        assert_eq!(
            quick_action_body(&Decision::KeepDisk, t, &done(None)),
            "Kept disk version: .zshrc"
        );
        assert_eq!(
            quick_action_body(&Decision::KeepSourceLocal, t, &done(Some("push failed"))),
            "Restored chezmoi's version: .zshrc · push failed"
        );
        assert_eq!(
            quick_action_body(
                &Decision::KeepDisk,
                t,
                &Err(ResolveError::Failed("daemon gone".into()))
            ),
            "Keep disk .zshrc failed: daemon gone",
            "a failure names the button that was pressed, not what it would have done"
        );
        assert_eq!(
            quick_action_body(
                &Decision::KeepDisk,
                t,
                &Ok(BatchResult::Resolved(ResolveOutcome::ProtectedSpan {
                    detail: "touches a protected template span".into()
                }))
            ),
            ".zshrc: this change touches a templated value · open the merge editor"
        );
    }

    /// The dashboard's Finish button must never offer to COMMIT a merge the
    /// user confirmed throwing away, and must never look pressable while the
    /// press it already took is queued behind a batch. `request_repo_op`
    /// refuses such a press, but a bright button that silently does nothing
    /// reads as a broken app.
    ///
    /// MUTATION: drop the `running == Some(Abort)` half of the first arm →
    /// the third case offers "Finish merge" while the roll-back is actually
    /// rewriting the source tree. MUTATION: return `true` for the pending
    /// finish → the second case invites a press that can only be dropped.
    /// MUTATION: make the "Finishing…" arm read `pending` instead of
    /// `running` → the third case says "Finish merge" while a finish is under
    /// way, which is the double-press this button exists to prevent.
    #[test]
    fn the_finish_button_says_what_is_happening_to_this_merge() {
        assert_eq!(
            finish_button_state(None, None),
            ("Finish merge", true),
            "nothing owed: the banner's one way out is live"
        );
        assert_eq!(
            finish_button_state(Some(RepoOpKind::Finish), None),
            ("Finish merge · queued", false),
            "already pressed, waiting for the latch"
        );
        assert_eq!(
            finish_button_state(None, Some(RepoOpKind::Finish)),
            ("Finishing…", false),
            "and running: `running_repo_op` alone says so now"
        );
        assert_eq!(
            finish_button_state(Some(RepoOpKind::Abort), None),
            ("Rolling back…", false),
            "a confirmed roll-back is queued: committing is the opposite answer"
        );
        assert_eq!(
            finish_button_state(None, Some(RepoOpKind::Abort)),
            ("Rolling back…", false),
            "and while it actually runs"
        );
        // An undo is about the journal, not about this merge: it defers the
        // finish behind itself like any other op, so the button stays live.
        assert_eq!(
            finish_button_state(Some(RepoOpKind::Undo), None),
            ("Finish merge", true)
        );
    }

    /// A second press on a target whose decision is already RUNNING must not
    /// be possible. `ActionQueue::push` replaces a PENDING choice, but it
    /// cannot recall one in flight, so the press would queue a DUPLICATE: a
    /// no-op re-add, a second commit and push, and a second journal session —
    /// which is then the one Undo reverts. Review locks such a row
    /// (`QueuedStatus::locks_row`); this surface has to do the same.
    ///
    /// Only RUNNING, not merely queued: a pending decision can still be
    /// changed, and re-choosing it is how a mis-click is corrected (spec
    /// §2.4). The dashboard passes `running` from `Shell::queue.running()`,
    /// which holds exactly the batch under the latch.
    ///
    /// MUTATION: drop the `running` arm → the second case goes live and a
    /// double press duplicates the decision. MUTATION: pass the whole
    /// `processing` list instead of `running` → a queued row goes dead and a
    /// mis-click becomes uncorrectable.
    #[test]
    fn a_quick_action_is_inert_while_that_files_decision_is_running() {
        assert_eq!(quick_action_blocked(false), None, "idle: live");
        assert_eq!(
            quick_action_blocked(true).map(|r| r.to_string()),
            Some("applying this file's decision now".to_string()),
            "in flight under the latch: a second press could only duplicate it"
        );
    }

    /// The dashboard offers exactly the two decisions it can make HONESTLY,
    /// and each writes what its label says.
    ///
    /// This surface builds its buttons from a timeline row and has loaded no
    /// file content, so "keep source · origin" — which needs origin's text as
    /// it was SHOWN, and refuses without it — cannot be among them. The
    /// mapping used to be `_ => Decision::KeepSourceLocal`: a third button
    /// added later would have silently written THIS MACHINE's source over the
    /// user's file, with no compile error anywhere.
    ///
    /// THE EXHAUSTIVENESS GUARD IS THE COMPILER'S, NOT THIS TEST'S. Neither
    /// `action` nor `decision` has a wildcard arm, so a new `QuickAction`
    /// variant is `error[E0004]` at both sites until it says what it renders
    /// and what it writes. A wildcard added back would compile and pass here,
    /// and no test can change that — what this test pins is the OTHER half:
    /// that the two variants which exist map to the decision their label
    /// promises, so a mapping swapped by hand is caught.
    ///
    /// MUTATION: swap the two arms of `decision` (or route either through a
    /// wildcard that changes what it writes) → this fails.
    #[test]
    fn the_dashboards_two_quick_actions_write_what_their_labels_say() {
        assert!(matches!(
            QuickAction::KeepDisk.decision(),
            Decision::KeepDisk
        ));
        assert_eq!(QuickAction::KeepDisk.action().label(), "Keep disk");
        assert!(matches!(
            QuickAction::KeepSourceLocal.decision(),
            Decision::KeepSourceLocal
        ));
        assert_eq!(
            QuickAction::KeepSourceLocal.action().label(),
            "Keep source",
            "the row has no origin split to disambiguate against"
        );
    }
}
