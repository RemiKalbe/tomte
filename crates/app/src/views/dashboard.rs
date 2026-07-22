//! Dashboard: compact stat strip + chronological activity list (spec §7.1,
//! restyled after Zed's git panel: fixed-height rows, ghost hover, filename +
//! muted path, humanized status chips). Rows with a target are clickable and
//! open Review with that file selected.
//!
//! Pure logic (relative time, glyphs, labels) lives in `czui_app::model`;
//! this module is rendering only and never blocks.

use std::path::PathBuf;
use std::rc::Rc;

use czui_app::model::{SyncModel, class_label, kind_glyph, kind_label, time_ago};
use czui_app::theme::Theme;
use gpui::{
    Context, Div, ElementId, Entity, FontWeight, Rgba, SharedString, WeakEntity, Window, div,
    prelude::*, uniform_list,
};

use super::Shell;

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
    /// Muted parent directory (target rows only), truncated in render.
    dir: Option<SharedString>,
    /// (label, colored) chip. Colored=false renders muted (info rows).
    chip: (SharedString, bool),
    class: Option<SharedString>,
    /// Click opens Review focused on this target.
    target: Option<PathBuf>,
}

impl RowData {
    fn new(row: &czui_app::model::TimelineRow, now: u64) -> Self {
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
        let chip = match &row.class {
            Some(class) => (SharedString::from(class_label(class)), true),
            None => (SharedString::from(kind_label(&row.kind)), false),
        };
        Self {
            time: time_ago(now, row.ts).into(),
            glyph: kind_glyph(&row.kind),
            name: name.into(),
            dir,
            chip,
            class: row.class.clone().map(SharedString::from),
            target: row.target.clone(),
        }
    }
}

/// `$HOME/...` → `~/...` — paths read better and truncate less.
fn shorten_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

/// Hover tooltip (shared with review/settings): the minimal themed bubble.
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
        let drifted_count = model.drifted.len();
        let in_sync = model.in_sync;
        let scanning = model.scanning;
        let connected = model.connected;
        let degraded = model.degraded.clone();

        let rows: Rc<Vec<RowData>> = Rc::new(
            model
                .timeline
                .iter()
                .map(|row| RowData::new(row, now))
                .collect(),
        );
        let shell = cx.weak_entity();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(stat_strip(
                theme,
                connected,
                scanning,
                needs_attention,
                drifted_count,
                in_sync,
            ))
            .when_some(degraded, |el, hint| {
                el.child(
                    div()
                        .mx_4()
                        .mb_2()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .bg(Theme::wash(theme.drift, 0.12))
                        .text_xs()
                        .text_color(theme.drift)
                        .child(hint),
                )
            })
            .child(
                div()
                    .px_4()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child("ACTIVITY"),
            )
            .child(if rows.is_empty() {
                let (text, color) = if scanning {
                    ("scanning your dotfiles…", theme.text_muted)
                } else if !connected {
                    ("waiting for chezmoid…", theme.text_muted)
                } else {
                    ("everything in sync", theme.ok)
                };
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(color)
                    .child(text)
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
                .px_2()
                .into_any_element()
            })
    }
}

/// One inline stat: colored value + muted label, Zed-git-panel density.
fn stat(theme: Theme, value: String, label: &'static str, color: Rgba) -> Div {
    div()
        .flex()
        .items_baseline()
        .gap_1()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(value),
        )
        .child(div().text_xs().text_color(theme.text_muted).child(label))
}

fn stat_strip(
    theme: Theme,
    connected: bool,
    scanning: bool,
    needs_attention: usize,
    drifted: usize,
    in_sync: u64,
) -> Div {
    let strip = div()
        .flex()
        .items_center()
        .gap_4()
        .px_4()
        .py_3()
        .flex_none();
    if scanning || !connected {
        return strip.child(
            div()
                .text_sm()
                .text_color(theme.text_muted)
                .child(if scanning {
                    "scanning…"
                } else {
                    "connecting to chezmoid…"
                }),
        );
    }
    strip
        .child(stat(
            theme,
            needs_attention.to_string(),
            "need attention",
            if needs_attention > 0 {
                theme.conflict
            } else {
                theme.text_muted
            },
        ))
        .child(stat(
            theme,
            drifted.to_string(),
            "drifted",
            if drifted > 0 {
                theme.drift
            } else {
                theme.text_muted
            },
        ))
        .child(stat(theme, in_sync.to_string(), "in sync", theme.ok))
}

/// One compact activity row. The whole row is clickable when it has a target.
fn timeline_row(
    ix: usize,
    row: &RowData,
    theme: Theme,
    shell: WeakEntity<Shell>,
) -> gpui::AnyElement {
    let base = div()
        .h_7()
        .px_2()
        .rounded_sm()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w_12()
                .flex_none()
                .text_xs()
                .text_color(theme.text_muted)
                .child(row.time.clone()),
        )
        .child(
            div()
                .w_4()
                .flex_none()
                .text_sm()
                .text_center()
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
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .whitespace_nowrap()
                        .child(row.name.clone()),
                )
                .when_some(row.dir.clone(), |el, dir| {
                    el.child(
                        div()
                            .min_w_0()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .truncate()
                            .child(dir),
                    )
                }),
        )
        .child({
            let (label, colored) = row.chip.clone();
            let color = if colored {
                row.class
                    .as_deref()
                    .map(|c| theme.class_color(c))
                    .unwrap_or(theme.text_muted)
            } else {
                theme.text_muted
            };
            div()
                .flex_none()
                .px_1p5()
                .rounded_sm()
                .when(colored, |el| el.bg(Theme::wash(color, 0.12)))
                .text_xs()
                .text_color(color)
                .child(label)
        });

    match row.target.clone() {
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
