//! Dashboard: health tiles, degraded banner, chronological timeline
//! (spec §7.1, approved mockup B+C; plan 5 Task 5).
//!
//! Pure logic (relative time, kind glyphs) lives in `czui_app::model` where it
//! is unit-tested; this module is rendering only. Never blocks: everything it
//! shows is already in the [`SyncModel`] entity.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use czui_app::model::{SyncModel, kind_glyph, time_ago};
use czui_app::theme::Theme;
use gpui::{
    AppContext as _, Context, Div, ElementId, Entity, FontWeight, Rgba, SharedString, Stateful,
    WeakEntity, Window, div, prelude::*, uniform_list,
};

use super::{Route, Shell};

/// Seconds since the Unix epoch — the production clock injected into
/// [`DashboardView::now_ts`] (tests inject fixed values into the pure fns).
pub fn system_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Everything a timeline row needs, precomputed so the `uniform_list` item
/// closure ('static) owns plain data instead of borrowing the model.
struct RowData {
    time: SharedString,
    glyph: &'static str,
    name: SharedString,
    /// Muted full path shown next to the file name (target rows only).
    path: Option<SharedString>,
    machine: SharedString,
    class: Option<SharedString>,
    /// Target is currently in `SyncModel::drifted` → show the action buttons.
    actionable: bool,
}

impl RowData {
    fn new(row: &czui_app::model::TimelineRow, now: u64, drifted: &HashSet<PathBuf>) -> Self {
        let (name, path) = match &row.target {
            Some(target) => (
                target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| target.display().to_string()),
                Some(SharedString::from(target.display().to_string())),
            ),
            // Target-less rows (fetch, global eval failures) show the kind.
            None => (row.kind.clone(), None),
        };
        Self {
            time: time_ago(now, row.ts).into(),
            glyph: kind_glyph(&row.kind),
            name: name.into(),
            path,
            machine: SharedString::from(row.machine.clone()),
            class: row.class.clone().map(SharedString::from),
            actionable: row.target.as_ref().is_some_and(|t| drifted.contains(t)),
        }
    }
}

/// Hover tooltip for the disabled Plan-6 action stubs (shared with the
/// review view). gpui tooltips are arbitrary views
/// (`StatefulInteractiveElement::tooltip` returns `AnyView`), so this is the
/// minimal themed text bubble.
pub(super) struct TextTooltip {
    pub(super) text: SharedString,
}

impl Render for TextTooltip {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme.surface)
            .border_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.text_muted)
            .child(self.text.clone())
    }
}

pub struct DashboardView {
    pub state: Entity<SyncModel>,
    pub now_ts: fn() -> u64,
}

impl DashboardView {
    /// Render the dashboard body. Constructed fresh in each `Shell` render;
    /// the shell re-renders whenever the model entity notifies.
    pub fn render(self, theme: Theme, cx: &mut Context<Shell>) -> impl IntoElement + use<> {
        let now = (self.now_ts)();
        let model = self.state.read(cx);

        let needs_attention = model.needs_attention();
        let in_sync = model.in_sync;
        let degraded = model.degraded.clone();
        let freshness: SharedString = match model.last_fetch_ts {
            Some(ts) => format!("fetched {}", time_ago(now, ts)).into(),
            None => "never fetched".into(),
        };
        let all_clear = model.drifted.is_empty() && model.timeline.is_empty();

        let drifted: HashSet<PathBuf> = model.drifted.iter().map(|d| d.target.clone()).collect();
        let rows: Rc<Vec<RowData>> = Rc::new(
            model
                .timeline
                .iter()
                .map(|row| RowData::new(row, now, &drifted))
                .collect(),
        );
        let shell = cx.weak_entity();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_3()
            .p_4()
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(tile(
                        theme,
                        needs_attention.to_string(),
                        "need attention",
                        if needs_attention > 0 {
                            theme.conflict
                        } else {
                            theme.text_muted
                        },
                    ))
                    .child(tile(theme, freshness.to_string(), "origin", theme.text))
                    .child(tile(theme, in_sync.to_string(), "in sync", theme.ok)),
            )
            .when_some(degraded, |el, hint| {
                el.child(
                    div()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(theme.surface)
                        .border_1()
                        .border_color(theme.drift)
                        .text_sm()
                        .text_color(theme.drift)
                        .child(format!("degraded: {hint}")),
                )
            })
            .child(if all_clear {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme.ok)
                    .child("everything in sync")
                    .into_any_element()
            } else {
                uniform_list(
                    "dashboard-timeline",
                    rows.len(),
                    move |range, _window, _cx| {
                        range
                            .map(|ix| timeline_row(ix, &rows[ix], theme, shell.clone()))
                            .collect()
                    },
                )
                .flex_1()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .into_any_element()
            })
    }
}

/// One health tile: a big value over a muted label.
fn tile(theme: Theme, value: String, label: &'static str, color: Rgba) -> Div {
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
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(value),
        )
        .child(div().text_xs().text_color(theme.text_muted).child(label))
}

/// One uniform-height timeline row: time · glyph · name + muted path · machine
/// · class chip, plus the action button group when the target is drifted.
fn timeline_row(ix: usize, row: &RowData, theme: Theme, shell: WeakEntity<Shell>) -> Div {
    div()
        .h_9()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .w_16()
                .flex_shrink_0()
                .text_xs()
                .text_color(theme.text_muted)
                .child(row.time.clone()),
        )
        .child(
            div()
                .w_4()
                .flex_shrink_0()
                .text_sm()
                .text_color(match &row.class {
                    Some(class) => theme.class_color(class),
                    None => theme.text_muted,
                })
                .child(row.glyph),
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
                        .text_color(theme.text)
                        .whitespace_nowrap()
                        .child(row.name.clone()),
                )
                .when_some(row.path.clone(), |el, path| {
                    el.child(
                        div()
                            .min_w_0()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .truncate()
                            .child(path),
                    )
                }),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(theme.text_muted)
                .child(row.machine.clone()),
        )
        .when_some(row.class.clone(), |el, class| {
            let color = theme.class_color(&class);
            el.child(
                div()
                    .flex_shrink_0()
                    .px_1p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(color)
                    .text_xs()
                    .text_color(color)
                    .child(class),
            )
        })
        .when(row.actionable, |el| {
            el.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .ml_auto()
                    .child(review_button(ix, theme, shell))
                    .child(disabled_button(ix, "keep-disk", "keep disk", theme))
                    .child(disabled_button(ix, "keep-source", "keep source", theme))
                    .child(disabled_button(ix, "merge", "Merge…", theme)),
            )
        })
}

/// Enabled per-row action: routes the shell to the Review view. Target
/// selection is Task 6's state; v0 routing switches the view only.
fn review_button(ix: usize, theme: Theme, shell: WeakEntity<Shell>) -> Stateful<Div> {
    div()
        .id(ElementId::named_usize("review", ix))
        .px_2()
        .py_0p5()
        .rounded_md()
        .border_1()
        .border_color(theme.accent)
        .text_xs()
        .text_color(theme.accent)
        .cursor_pointer()
        .child("Review →")
        .on_click(move |_event, _window, cx| {
            let _ = shell.update(cx, |shell, cx| {
                shell.route = Route::Review;
                cx.notify();
            });
        })
}

/// Disabled Plan-6 stub: muted, no click handler, hover tooltip explains why.
fn disabled_button(
    ix: usize,
    id: &'static str,
    label: &'static str,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id(ElementId::named_usize(id, ix))
        .px_2()
        .py_0p5()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.text_muted)
        .child(label)
        .tooltip(|_window, cx| {
            cx.new(|_| TextTooltip {
                text: "arrives with the sync pipeline".into(),
            })
            .into()
        })
}
