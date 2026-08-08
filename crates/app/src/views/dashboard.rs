//! Dashboard: health tiles (approved mockup B) + chronological activity list
//! (Zed-git-panel density). Rows with a target open Review; consecutive
//! scan/fetch noise collapses into one expandable line.
//!
//! Pure logic (relative time, glyphs, labels, grouping) lives in
//! `czui_app::model`; this module renders and never blocks.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use czui_app::model::{
    SyncModel, TimelineItem, TimelineRow, class_label, group_timeline, kind_glyph, kind_label,
    time_ago,
};
use czui_app::resolve::{ResolveEngine, ResolveError, ResolveOutcome};
use czui_app::theme::Theme;
use czui_ui::components as ui;
use czui_core::cmd::SystemRunner;
use gpui::{
    App, Context, Div, ElementId, Entity, FontWeight, Rgba, SharedString, Stateful, WeakEntity,
    div, prelude::*, uniform_list,
};

use crate::notify_osa::notify;

use super::Shell;
use super::review::ResolveAction;

/// Seconds since the Unix epoch — the production clock injected into
/// [`DashboardView::now_ts`] (tests inject fixed values into the pure fns).
pub fn system_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the degraded banner's action button should do — matched against our
/// own hint texts from `czui_core::chezmoi::classify_eval_stderr`.
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

/// Moved to czui-ui; re-exported so sibling views keep their import paths.
pub(super) use czui_ui::components::{TextTooltip, shorten_home};

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
        let (name, dir) = match &row.target {
            Some(target) => (
                target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| target.display().to_string()),
                target
                    .parent()
                    .map(|p| SharedString::from(shorten_home(&p.display().to_string()))),
            ),
            None => (kind_label(&row.kind).to_string(), None),
        };
        Self::Event {
            time: time_ago(now, row.ts).into(),
            glyph: kind_glyph(&row.kind),
            name: name.into(),
            dir,
            chip: row
                .class
                .as_ref()
                .map(|class| SharedString::from(class_label(class))),
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
    /// Copied from `Shell::dashboard_action_in_flight`: a quick action is
    /// running, so the inline row buttons yield to a "working…" marker.
    pub action_in_flight: bool,
    /// Copied from `Shell::unlock_in_flight`: the 1Password unlock probe is
    /// waiting on the user's approval.
    pub unlock_in_flight: bool,
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
        let (freshness, freshness_color): (SharedString, Rgba) = match model.last_fetch_ts {
            Some(ts) => (
                format!("fetched {}", time_ago(now, ts)).into(),
                // 2x the default 15-min fetch interval before it reads stale.
                if now.saturating_sub(ts) > 30 * 60 {
                    theme.drift
                } else {
                    theme.text
                },
            ),
            None => ("never fetched".into(), theme.text_muted),
        };

        let lines: Rc<Vec<Line>> = Rc::new(build_lines(model, now, &expanded));
        let shell = cx.weak_entity();
        // Resolve engine for the inline quick actions: None (disconnected /
        // not yet built) renders them disabled.
        let engine = cx
            .try_global::<crate::EngineSlot>()
            .and_then(|slot| slot.0.clone());
        let busy = self.action_in_flight;
        let unlock_in_flight = self.unlock_in_flight;

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
                    .child(tile(
                        theme,
                        freshness.to_string(),
                        "origin",
                        freshness_color,
                        None,
                    ))
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
                            ui::ButtonVariant::Outline(theme.drift),
                            ui::ButtonSize::Micro,
                            {
                                let shell = shell.clone();
                                let engine = engine.clone();
                                move |_event, _window, cx| {
                                    trigger_onepassword_unlock(
                                        shell.clone(),
                                        engine.clone(),
                                        cx,
                                    );
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
                            ui::ButtonVariant::Outline(theme.drift),
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
                    ui::empty_state(
                        theme,
                        "●",
                        theme.conflict,
                        "Sync daemon not connected",
                        "reconnecting automatically…",
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
                            .map(|ix| {
                                render_line(
                                    ix,
                                    &lines[ix],
                                    theme,
                                    shell.clone(),
                                    engine.clone(),
                                    busy,
                                )
                            })
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
                use czui_core::cmd::{CommandRequest, CommandRunner as _};
                // Generous timeout: this intentionally waits for a human to
                // approve the 1Password prompt.
                let _ = SystemRunner.run(
                    CommandRequest::new("op")
                        .args(["whoami", "--format=json"])
                        .timeout(std::time::Duration::from_secs(120)),
                );
                if let Some(engine) = engine {
                    let _ = engine.ipc.request(czui_proto::Request::Rescan);
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
        ui::ButtonVariant::Wash(theme.accent),
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
    engine: Option<Arc<ResolveEngine>>,
    busy: bool,
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
                        .child(format!("{count} scans")),
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
                .map(|target| quick_actions(ix, target, theme, engine, busy, shell.clone()));
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
                            el.child(ui::chip(theme, label, ui::ChipVariant::Wash(color)).flex_none())
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

/// The inline quick-action cluster for one drifted row: keep-disk /
/// keep-source buttons, or a "working…" marker while any dashboard action
/// runs. Outcomes surface as an osascript notification (the dashboard is
/// transient — no banner).
fn quick_actions(
    ix: usize,
    target: &Path,
    theme: Theme,
    engine: Option<Arc<ResolveEngine>>,
    busy: bool,
    shell: WeakEntity<Shell>,
) -> Div {
    if busy {
        return div()
            .flex_none()
            .text_xs()
            .text_color(theme.text_muted)
            .child("working…");
    }
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
            ResolveAction::KeepDisk,
            target,
            theme,
            engine.clone(),
            shell.clone(),
        ))
        .child(quick_action_button(
            ix,
            ResolveAction::KeepSource,
            target,
            theme,
            engine,
            shell,
        ))
}

fn quick_action_button(
    ix: usize,
    action: ResolveAction,
    target: &Path,
    theme: Theme,
    engine: Option<Arc<ResolveEngine>>,
    shell: WeakEntity<Shell>,
) -> Stateful<Div> {
    let id = match action {
        ResolveAction::KeepDisk => "row-keep-disk",
        ResolveAction::KeepSource => "row-keep-source",
    };
    let id = ElementId::named_usize(id, ix);
    let Some(engine) = engine else {
        return ui::disabled_button(
            theme,
            id,
            action.label().into(),
            ui::ButtonSize::Micro,
            Some("daemon not connected".into()),
        )
        .flex_none();
    };
    let target = target.to_path_buf();
    ui::button(
        theme,
        id,
        action.label().into(),
        ui::ButtonVariant::Outline(theme.accent),
        ui::ButtonSize::Micro,
        move |_event, _window, cx| {
            // The row underneath opens Review on click — quick actions must
            // not also navigate.
            cx.stop_propagation();
            run_quick_action(action, engine.clone(), target.clone(), shell.clone(), cx);
        },
    )
    .flex_none()
}

/// Run one quick action: flag the shell busy, do the blocking engine call +
/// notification on the background executor, then clear the flag. No call
/// ever runs on the main thread.
fn run_quick_action(
    action: ResolveAction,
    engine: Arc<ResolveEngine>,
    target: PathBuf,
    shell: WeakEntity<Shell>,
    cx: &mut App,
) {
    let flag = |cx: &mut App, shell: &WeakEntity<Shell>, value: bool| {
        let _ = shell.update(cx, |shell, cx| {
            shell.dashboard_action_in_flight = value;
            cx.notify();
        });
    };
    flag(cx, &shell, true);
    cx.spawn(async move |cx| {
        cx.background_executor()
            .spawn(async move {
                let result = action.run(&engine, &target);
                let body = quick_action_body(action, &target, &result);
                notify(&SystemRunner, "chezmoi-ui", &body);
            })
            .await;
        let _ = cx.update(|cx| flag(cx, &shell, false));
    })
    .detach();
}

/// Notification body for a quick action's outcome — same honesty rules as
/// the review banner (`note` = degraded commit/push, spelled out).
fn quick_action_body(
    action: ResolveAction,
    target: &Path,
    result: &Result<ResolveOutcome, ResolveError>,
) -> String {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.display().to_string());
    match result {
        Ok(ResolveOutcome::Done { note: None, .. }) => format!("{}: {name}", action.verb()),
        Ok(ResolveOutcome::Done {
            note: Some(note), ..
        }) => format!("{}: {name} · {note}", action.verb()),
        Ok(ResolveOutcome::NeedsMergeEditor) => {
            format!("{name} is templated · use the merge editor")
        }
        // Defensive: quick actions never run `resolve_merged`, the only
        // producer of this outcome.
        Ok(ResolveOutcome::ProtectedSpan { .. }) => {
            format!("{name}: this change touches a templated value · open the merge editor")
        }
        Err(e) => format!("{} {name} failed: {e}", action.label()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use czui_app::resolve::{ResolveError, ResolveOutcome};

    use super::super::review::ResolveAction;
    use super::{DegradedRemedy, degraded_remedy, quick_action_body};

    fn done(note: Option<&str>) -> ResolveOutcome {
        ResolveOutcome::Done {
            session: 1,
            committed: true,
            pushed: note.is_none(),
            note: note.map(str::to_owned),
        }
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
            quick_action_body(ResolveAction::KeepDisk, t, &Ok(done(None))),
            "Kept disk version: .zshrc"
        );
        assert_eq!(
            quick_action_body(ResolveAction::KeepSource, t, &Ok(done(Some("push failed")))),
            "Restored chezmoi's version: .zshrc · push failed"
        );
        assert_eq!(
            quick_action_body(
                ResolveAction::KeepDisk,
                t,
                &Ok(ResolveOutcome::NeedsMergeEditor)
            ),
            ".zshrc is templated · use the merge editor"
        );
        assert_eq!(
            quick_action_body(
                ResolveAction::KeepDisk,
                t,
                &Err(ResolveError::Failed("daemon gone".into()))
            ),
            "Keep disk .zshrc failed: daemon gone"
        );
        assert_eq!(
            quick_action_body(
                ResolveAction::KeepDisk,
                t,
                &Ok(ResolveOutcome::ProtectedSpan {
                    detail: "touches a protected template span".into()
                })
            ),
            ".zshrc: this change touches a templated value · open the merge editor"
        );
    }
}
