//! Dashboard: health tiles (approved mockup B) + chronological activity list
//! (Zed-git-panel density). Rows with a target open Review; consecutive
//! scan/fetch noise collapses into one expandable line.
//!
//! Pure logic (relative time, glyphs, labels, grouping) lives in
//! `czui_app::model`; this module renders and never blocks.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use czui_app::model::{
    SyncModel, TimelineItem, TimelineRow, class_label, group_timeline, kind_glyph, kind_label,
    time_ago,
};
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
    fn event(row: &TimelineRow, now: u64, indented: bool) -> Self {
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
        }
    }
}

/// Flatten grouped timeline items into display lines, honoring expansion.
fn build_lines(model: &SyncModel, now: u64, expanded: &HashSet<u64>) -> Vec<Line> {
    let mut lines = Vec::new();
    for item in group_timeline(&model.timeline) {
        match item {
            TimelineItem::Row(row) => lines.push(Line::event(&row, now, false)),
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
                        lines.push(Line::event(row, now, true));
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
        let freshness: SharedString = match model.last_fetch_ts {
            Some(ts) => format!("fetched {}", time_ago(now, ts)).into(),
            None => "never fetched".into(),
        };

        let lines: Rc<Vec<Line>> = Rc::new(build_lines(model, now, &expanded));
        let shell = cx.weak_entity();

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
                        "need attention",
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
                        theme.text,
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
                        theme.ok,
                        None,
                    )),
            )
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
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text_muted)
                            .child("ACTIVITY"),
                    )
                    .when(scanning && have_data, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(theme.text_muted)
                                .child("rescanning…"),
                        )
                    }),
            )
            .child(if lines.is_empty() {
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
                    lines.len(),
                    move |range, _window, _cx| {
                        range
                            .map(|ix| render_line(ix, &lines[ix], theme, shell.clone()))
                            .collect()
                    },
                )
                .flex_1()
                .px_2()
                .into_any_element()
            })
    }
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
    div()
        .id("tile-review")
        .px_2()
        .py_0p5()
        .rounded_sm()
        .bg(Theme::wash(theme.accent, 0.15))
        .text_xs()
        .text_color(theme.accent)
        .cursor_pointer()
        .hover(|el| el.bg(Theme::wash(theme.accent, 0.25)))
        .child(format!("Review {drifted} →"))
        .on_click(move |_event, _window, cx| {
            let _ = shell.update(cx, |shell, cx| {
                shell.open_review(None, cx);
            });
        })
        .into_any_element()
}

fn render_line(ix: usize, line: &Line, theme: Theme, shell: WeakEntity<Shell>) -> gpui::AnyElement {
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
                        .w_12()
                        .flex_none()
                        .text_xs()
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
        } => {
            let base = div()
                .h_7()
                .w_full()
                .px_2()
                .when(*indented, |el| el.pl_8())
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
                        }),
                )
                .when_some(chip.clone(), |el, label| {
                    let color = class
                        .as_deref()
                        .map(|c| theme.class_color(c))
                        .unwrap_or(theme.text_muted);
                    el.child(
                        div()
                            .flex_none()
                            .px_1p5()
                            .rounded_sm()
                            .bg(Theme::wash(color, 0.12))
                            .text_xs()
                            .text_color(color)
                            .child(label),
                    )
                });

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
