//! Three-pane merge editor (spec §7.3, approved mockup A; plan 7 Task 3).
//!
//! Full-window takeover routed via [`Route::Merge`]: theirs (chezmoi's
//! rendered source state) / base (last-written snapshot) / ours (disk) over a
//! result pane assembled from per-region choices. Choice-based only — free
//! text editing is deferred (the "open in editor" escape hatch covers it).
//!
//! Everything blocking — `merge_inputs::load` and the engine's
//! `resolve_merged` — runs on the background executor and lands back in the
//! entity via `WeakEntity::update` (spec §3.2 non-blocking rule). Successful
//! saves hand their [`OutcomeBanner`] to Review and route back; protected-span
//! rejections and errors stay here, reported honestly (spec §10).

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use czui_app::merge_inputs::{self, MergeInputs};
use czui_app::merge_state::MergeState;
use czui_app::resolve::{ResolveError, ResolveOutcome};
use czui_app::theme::Theme;
use czui_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use czui_core::cmd::SystemRunner;
use czui_core::merge::{Choice, MergeDocument, RegionKind};
use czui_core::template::anchor::{SpanMap, SpanOrigin};
use gpui::{
    AnyElement, Context, Div, ElementId, FontWeight, Rgba, ScrollStrategy, SharedString, Stateful,
    UniformListScrollHandle, WeakEntity, Window, div, prelude::*, uniform_list,
};

use super::dashboard::TextTooltip;
use super::review::{
    BannerTint, OutcomeBanner, centered_note, display_text, journal_path, message_box,
};
use super::{Route, Shell};

/// Which merge pane a line belongs to. Column order in the panes row is
/// theirs / base / ours, matching the header chips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneSide {
    Theirs,
    Base,
    Ours,
}

/// One uniform-height pane row, precomputed at load so the `uniform_list`
/// closures own plain data (the review preview's `DiffLine` idiom).
#[derive(Debug, Clone, PartialEq)]
struct PaneLine {
    /// Display text: the line minus its trailing newline.
    text: SharedString,
    /// The kind of the region this line sits in — resolved to a wash per
    /// pane at render via [`row_bg`].
    kind: RegionKind,
    /// The line contains a byte of a protected template span (theirs pane
    /// only): amber underlay + 🔒 gutter.
    protected: bool,
}

/// Per-line region tint. The conflict wash lands on every pane; one-sided
/// changes tint only the pane that carries them; `BothSame` gets a subtle ok
/// wash on the two changed panes.
fn row_bg(kind: RegionKind, side: PaneSide, theme: &Theme) -> Option<Rgba> {
    match (kind, side) {
        (RegionKind::Conflict, _) => Some(Theme::wash(theme.conflict, 0.10)),
        (RegionKind::OursOnly, PaneSide::Ours) => Some(Theme::wash(theme.drift, 0.08)),
        (RegionKind::TheirsOnly, PaneSide::Theirs) => Some(Theme::wash(theme.accent, 0.08)),
        (RegionKind::BothSame, PaneSide::Ours | PaneSide::Theirs) => {
            Some(Theme::wash(theme.ok, 0.06))
        }
        _ => None,
    }
}

/// Flatten one side of the document into pane rows. Regions partition each
/// side's lines in order, so concatenating their side ranges covers every
/// line exactly once.
fn pane_lines(doc: &MergeDocument, side: PaneSide, protected: &HashSet<usize>) -> Vec<PaneLine> {
    let lines = match side {
        PaneSide::Theirs => doc.theirs_lines(),
        PaneSide::Base => doc.base_lines(),
        PaneSide::Ours => doc.ours_lines(),
    };
    let mut out = Vec::with_capacity(lines.len());
    for region in &doc.regions {
        let range = match side {
            PaneSide::Theirs => region.theirs.clone(),
            PaneSide::Base => region.base.clone(),
            PaneSide::Ours => region.ours.clone(),
        };
        for ix in range {
            out.push(PaneLine {
                text: display_text(&lines[ix]).to_owned().into(),
                kind: region.kind,
                protected: protected.contains(&ix),
            });
        }
    }
    out
}

/// Whether a rendered span is protected from write-back: templated values
/// (`Action`), unanchored stretches (`Unmapped`), and repeated literals are
/// all rejections in `czui_core::template::writeback`.
fn span_is_protected(origin: &SpanOrigin) -> bool {
    matches!(
        origin,
        SpanOrigin::Action { .. }
            | SpanOrigin::Unmapped
            | SpanOrigin::Literal { repeated: true, .. }
    )
}

/// Line indices (into `lines`, whose concatenation is the rendered text the
/// span map covers) containing at least one protected byte. Approximated to
/// whole lines — precision polish deferred (plan 7 Task 3).
fn protected_line_set(lines: &[String], map: &SpanMap) -> HashSet<usize> {
    let mut starts = Vec::with_capacity(lines.len() + 1);
    let mut offset = 0usize;
    for line in lines {
        starts.push(offset);
        offset += line.len();
    }
    starts.push(offset);
    let mut out = HashSet::new();
    for span in &map.spans {
        if span.range.is_empty() || !span_is_protected(&span.origin) {
            continue;
        }
        for ix in 0..lines.len() {
            if span.range.start < starts[ix + 1] && starts[ix] < span.range.end {
                out.insert(ix);
            }
        }
    }
    out
}

/// One row of the result pane: an assembled line, or the placeholder for an
/// unresolved conflict (with its inline pick buttons).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResultRow {
    Line {
        text: SharedString,
        /// Unchanged context renders muted; chosen/auto-changed lines normal.
        muted: bool,
    },
    Placeholder {
        region: usize,
        /// The cursor (next ↓) sits on this conflict.
        focused: bool,
    },
}

/// The assembled-so-far result: mirrors `MergeDocument::assemble`'s defaults
/// and override rules exactly, but renders unresolved conflicts as
/// placeholder rows instead of failing.
fn result_rows(state: &MergeState) -> Vec<ResultRow> {
    let doc = &state.doc;
    let mut out = Vec::new();
    for (idx, region) in doc.regions.iter().enumerate() {
        let (lines, muted): (&[String], bool) = match (state.resolution.get(idx), region.kind) {
            (Some(Choice::Ours), _) => (&doc.ours_lines()[region.ours.clone()], false),
            (Some(Choice::Theirs), _) => (&doc.theirs_lines()[region.theirs.clone()], false),
            (Some(Choice::Base), _) => (&doc.base_lines()[region.base.clone()], false),
            // The choice-based UI never produces Edited, but assemble
            // supports it — render it rather than mis-render.
            (Some(Choice::Edited(text)), _) => {
                out.extend(text.split_inclusive('\n').map(|line| ResultRow::Line {
                    text: display_text(line).to_owned().into(),
                    muted: false,
                }));
                continue;
            }
            (None, RegionKind::Unchanged) => (&doc.base_lines()[region.base.clone()], true),
            (None, RegionKind::OursOnly | RegionKind::BothSame) => {
                (&doc.ours_lines()[region.ours.clone()], false)
            }
            (None, RegionKind::TheirsOnly) => (&doc.theirs_lines()[region.theirs.clone()], false),
            (None, RegionKind::Conflict) => {
                out.push(ResultRow::Placeholder {
                    region: idx,
                    focused: state.cursor == Some(idx),
                });
                continue;
            }
        };
        out.extend(lines.iter().map(|line| ResultRow::Line {
            text: display_text(line).to_owned().into(),
            muted,
        }));
    }
    out
}

/// Row index of `region`'s placeholder, for the `next ↓` scroll.
fn placeholder_row_ix(rows: &[ResultRow], region: usize) -> Option<usize> {
    rows.iter()
        .position(|row| matches!(row, ResultRow::Placeholder { region: r, .. } if *r == region))
}

/// Map a `resolve_merged` result onto its banner. Done banners are handed to
/// Review (undoable there); protected-span rejections and errors stay in the
/// merge editor and are never undoable (nothing was mutated).
fn merge_banner(result: &Result<ResolveOutcome, ResolveError>) -> OutcomeBanner {
    match result {
        Ok(ResolveOutcome::Done {
            note: None,
            committed,
            pushed,
            ..
        }) => OutcomeBanner {
            text: if *committed && *pushed {
                "Merged · committed & pushed".into()
            } else {
                "merged".into()
            },
            tint: BannerTint::Ok,
            undoable: true,
        },
        Ok(ResolveOutcome::Done {
            note: Some(note), ..
        }) => OutcomeBanner {
            text: format!("Merged · {note}").into(),
            tint: BannerTint::Drift,
            undoable: true,
        },
        Ok(ResolveOutcome::ProtectedSpan { detail }) => OutcomeBanner {
            text: format!("This change touches a templated value · {detail}").into(),
            tint: BannerTint::Drift,
            undoable: false,
        },
        // Defensive: resolve_merged never reports this outcome.
        Ok(ResolveOutcome::NeedsMergeEditor) => OutcomeBanner {
            text: "Unexpected engine outcome · nothing was changed".into(),
            tint: BannerTint::Drift,
            undoable: false,
        },
        Err(e) => OutcomeBanner {
            text: format!("save failed: {e}").into(),
            tint: BannerTint::Conflict,
            undoable: false,
        },
    }
}

/// Everything a loaded target carries: the inputs (Arc'd so the save task can
/// own them), the editor model, and the pane rows precomputed once per load.
pub(super) struct LoadedMerge {
    pub(super) inputs: Arc<MergeInputs>,
    pub(super) state: MergeState,
    theirs_rows: Rc<Vec<PaneLine>>,
    base_rows: Rc<Vec<PaneLine>>,
    ours_rows: Rc<Vec<PaneLine>>,
}

impl LoadedMerge {
    pub(super) fn new(inputs: Arc<MergeInputs>) -> Self {
        let state = MergeState::new(&inputs);
        let protected = inputs
            .span_map
            .as_ref()
            .map(|map| protected_line_set(state.doc.theirs_lines(), map))
            .unwrap_or_default();
        let none = HashSet::new();
        Self {
            theirs_rows: Rc::new(pane_lines(&state.doc, PaneSide::Theirs, &protected)),
            base_rows: Rc::new(pane_lines(&state.doc, PaneSide::Base, &none)),
            ours_rows: Rc::new(pane_lines(&state.doc, PaneSide::Ours, &none)),
            inputs,
            state,
        }
    }
}

pub struct MergeView {
    /// Route-back and banner hand-off; weak because the shell owns this view.
    shell: WeakEntity<Shell>,
    /// Target being merged: set synchronously by `load`, guards stale
    /// background results. `pub(super)` for the render-smoke tests.
    pub(super) target: Option<PathBuf>,
    /// Background `merge_inputs::load` in flight.
    pub(super) loading: bool,
    /// Load failure (binary content, chezmoi/journal/io errors) — rendered
    /// as a message-box state (spec §10).
    pub(super) error: Option<String>,
    pub(super) loaded: Option<LoadedMerge>,
    /// `resolve_merged` in flight on the background executor.
    pub(super) saving: bool,
    /// Banner for saves that STAY here (protected span, errors); successful
    /// saves hand their banner to Review instead.
    pub(super) banner: Option<OutcomeBanner>,
    result_scroll: UniformListScrollHandle,
}

impl MergeView {
    pub fn new(shell: WeakEntity<Shell>) -> Self {
        Self {
            shell,
            target: None,
            loading: false,
            error: None,
            loaded: None,
            saving: false,
            banner: None,
            result_scroll: UniformListScrollHandle::new(),
        }
    }

    /// (Re)load the merge inputs for `target` on the background executor.
    /// Reopening the editor is always a fresh load — earlier choices for the
    /// same file do not survive (the inputs may have changed underneath).
    pub fn load(&mut self, target: PathBuf, cx: &mut Context<Self>) {
        self.target = Some(target.clone());
        self.loading = true;
        self.error = None;
        self.loaded = None;
        self.saving = false;
        self.banner = None;
        cx.notify();

        let journal = journal_path();
        cx.spawn(async move |this, cx| {
            let result = {
                let target = target.clone();
                cx.background_executor()
                    .spawn(async move {
                        let client =
                            ChezmoiClient::new(Arc::new(SystemRunner), ChezmoiOptions::default());
                        merge_inputs::load(&client, &journal, &target)
                    })
                    .await
            };
            this.update(cx, |view, cx| {
                // Stale guard: another target may have been opened meanwhile.
                if view.target.as_deref() != Some(target.as_path()) {
                    return;
                }
                view.loading = false;
                match result {
                    Ok(inputs) => view.loaded = Some(LoadedMerge::new(Arc::new(inputs))),
                    Err(e) => view.error = Some(e.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Record a per-region choice (placeholder buttons in the result pane).
    fn pick(&mut self, region: usize, choice: Choice, cx: &mut Context<Self>) {
        let Some(loaded) = &mut self.loaded else {
            return;
        };
        loaded.state.pick(region, choice);
        cx.notify();
    }

    /// `next ↓`: advance the cursor to the next unresolved conflict and
    /// scroll the result pane to its placeholder row.
    fn next(&mut self, cx: &mut Context<Self>) {
        let Some(loaded) = &mut self.loaded else {
            return;
        };
        if let Some(region) = loaded.state.next_unresolved() {
            let rows = result_rows(&loaded.state);
            if let Some(ix) = placeholder_row_ix(&rows, region) {
                self.result_scroll
                    .scroll_to_item(ix, ScrollStrategy::Center);
            }
        }
        cx.notify();
    }

    /// Cancel: back to Review, discarding in-editor choices.
    fn cancel(&self, cx: &mut Context<Self>) {
        self.shell
            .update(cx, |shell, cx| {
                shell.route = Route::Review;
                cx.notify();
            })
            .ok();
    }

    /// Save: `resolve_merged` on the background executor. On `Done` the
    /// banner is handed to Review and the shell routes back there; protected
    /// spans and errors keep the editor open with the banner shown here.
    fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let Some(loaded) = &self.loaded else {
            return;
        };
        let Some(resolved) = loaded.state.assembled() else {
            return;
        };
        let inputs = loaded.inputs.clone();
        let Some(engine) = cx
            .try_global::<crate::EngineSlot>()
            .and_then(|slot| slot.0.clone())
        else {
            // The button was enabled before a disconnect — report, don't
            // no-op silently (spec §10).
            self.banner = Some(OutcomeBanner {
                text: "Daemon not connected · cannot save".into(),
                tint: BannerTint::Conflict,
                undoable: false,
            });
            cx.notify();
            return;
        };
        self.saving = true;
        self.banner = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { engine.resolve_merged(&inputs, &resolved) })
                .await;
            this.update(cx, |view, cx| {
                view.saving = false;
                let banner = merge_banner(&result);
                if matches!(result, Ok(ResolveOutcome::Done { .. })) {
                    view.shell
                        .update(cx, |shell, cx| shell.merge_done(banner, cx))
                        .ok();
                } else {
                    view.banner = Some(banner);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Header bar: file name + muted path; provenance chips in pane order;
    /// progress; `next ↓`; Cancel; Save.
    fn header(&self, theme: Theme, cx: &mut Context<Self>) -> Div {
        let (name, path): (SharedString, SharedString) = match &self.target {
            Some(target) => (
                target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| target.display().to_string())
                    .into(),
                super::dashboard::shorten_home(&target.display().to_string()).into(),
            ),
            None => ("merge editor".into(), "".into()),
        };
        let mut bar = div()
            .flex()
            .items_center()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(theme.border)
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
                            .font_weight(FontWeight::SEMIBOLD)
                            .whitespace_nowrap()
                            .child(name),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .truncate()
                            .child(path),
                    ),
            );
        if let Some(loaded) = &self.loaded {
            let degraded = loaded.state.degraded_base;
            let (decided, total) = loaded.state.progress();
            let open = total - decided;
            bar = bar
                .child(
                    div()
                        .text_xs()
                        .whitespace_nowrap()
                        .text_color(if open == 0 { theme.ok } else { theme.conflict })
                        .child(if total == 0 {
                            "No conflicts to resolve".to_string()
                        } else {
                            let s = if total == 1 { "" } else { "s" };
                            format!("{total} conflict{s}, {} resolved", total - open)
                        }),
                );
            if open > 0 {
                bar = bar.child(
                    div()
                        .id("merge-next")
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.conflict)
                        .text_xs()
                        .text_color(theme.conflict)
                        .cursor_pointer()
                        .hover(|el| el.bg(Theme::wash(theme.conflict, 0.12)))
                        .child("Next ↓")
                        .on_click(cx.listener(|view, _ev, _window, cx| view.next(cx))),
                );
            }
        }
        bar = bar.child(
            div()
                .id("merge-cancel")
                .px_2()
                .py_0p5()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.text)
                .cursor_pointer()
                .hover(|el| el.bg(Theme::wash(theme.text, 0.05)))
                .child("Cancel")
                .on_click(cx.listener(|view, _ev, _window, cx| view.cancel(cx))),
        );
        if let Some(loaded) = &self.loaded {
            let unresolved = {
                let (decided, total) = loaded.state.progress();
                total - decided
            };
            bar = bar.child(if self.saving {
                div()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child("saving…")
                    .into_any_element()
            } else {
                self.save_button(loaded.state.assembled().is_some(), unresolved, theme, cx)
                    .into_any_element()
            });
        }
        bar
    }

    /// Save is enabled exactly when the document is fully resolved and no
    /// save is in flight (the plan's contract).
    fn save_button(
        &self,
        enabled: bool,
        unresolved: usize,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let base = div()
            .id("merge-save")
            .px_2()
            .py_0p5()
            .rounded_md()
            .border_1()
            .text_xs()
            .child("Save");
        if enabled {
            base.border_color(theme.accent)
                .text_color(theme.accent)
                .cursor_pointer()
                .hover(|el| el.bg(Theme::wash(theme.accent, 0.12)))
                .on_click(cx.listener(|view, _ev, _window, cx| view.save(cx)))
        } else {
            base.border_color(theme.border)
                .text_color(theme.text_muted)
                .tooltip(move |_window, cx| {
                    let s = if unresolved == 1 { "" } else { "s" };
                    let text = format!("{unresolved} conflict{s} left").into();
                    cx.new(|_| TextTooltip { text }).into()
                })
        }
    }

    fn body(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        if self.loading {
            return centered_note(theme, "loading merge inputs…".into(), theme.text_muted);
        }
        if let Some(msg) = &self.error {
            return message_box(
                theme,
                "could not load merge inputs",
                msg.clone(),
                Some("Fix this, then reopen the merge editor from Review."),
            );
        }
        let Some(loaded) = &self.loaded else {
            return centered_note(
                theme,
                "open a file from Review to start a merge".into(),
                theme.text_muted,
            );
        };

        let degraded = loaded.state.degraded_base;
        let rows = Rc::new(result_rows(&loaded.state));
        let view = cx.weak_entity();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .child(
                // Three equal read-only panes; the result pane below is the
                // taller one (mockup A).
                div()
                    .h_2_5()
                    .flex_none()
                    .flex()
                    .gap_2()
                    .child(pane_col(
                        "merge-pane-theirs",
                        "source (rendered)",
                        theme.accent,
                        &loaded.theirs_rows,
                        PaneSide::Theirs,
                        theme,
                    ))
                    .child(if degraded {
                        degraded_base_col(theme)
                    } else {
                        pane_col(
                            "merge-pane-base",
                            "last written",
                            theme.text_muted,
                            &loaded.base_rows,
                            PaneSide::Base,
                            theme,
                        )
                    })
                    .child(pane_col(
                        "merge-pane-ours",
                        "on disk",
                        theme.drift,
                        &loaded.ours_rows,
                        PaneSide::Ours,
                        theme,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        uniform_list("merge-result", rows.len(), move |range, _window, _cx| {
                            range
                                .map(|ix| result_row_el(&rows[ix], degraded, theme, &view))
                                .collect()
                        })
                        .track_scroll(self.result_scroll.clone())
                        .flex_1(),
                    ),
            )
            .into_any_element()
    }
}

impl Render for MergeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.header(theme, cx))
            .when_some(self.banner.clone(), |el, banner| {
                el.child(banner_el(&banner, theme))
            })
            .child(self.body(theme, cx))
    }
}

/// Merge-local banner: same shape as Review's, minus the Undo button (the
/// banners that stay here — protected span, errors — are never undoable).
fn banner_el(banner: &OutcomeBanner, theme: Theme) -> Div {
    let color = banner.tint.color(theme);
    div()
        .mx_3()
        .mt_2()
        .px_2()
        .py_1()
        .rounded_sm()
        .bg(Theme::wash(color, 0.12))
        .text_xs()
        .text_color(color)
        .child(div().min_w_0().truncate().child(banner.text.clone()))
}

/// One read-only pane column: an attached label header (the user should
/// never have to map floating toolbar chips onto panes by position) over a
/// bordered `uniform_list` of its lines.
fn pane_col(
    id: &'static str,
    label: &'static str,
    tint: Rgba,
    rows: &Rc<Vec<PaneLine>>,
    side: PaneSide,
    theme: Theme,
) -> AnyElement {
    let rows = rows.clone();
    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .flex_col()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .child(pane_label(label, tint, theme))
        .child(
            uniform_list(id, rows.len(), move |range, _window, _cx| {
                range.map(|ix| pane_row(&rows[ix], side, theme)).collect()
            })
            .flex_1(),
        )
        .into_any_element()
}

/// The header strip inside a pane: tint dot + muted label.
fn pane_label(label: &'static str, tint: Rgba, theme: Theme) -> Div {
    div()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(theme.border)
        .flex()
        .items_center()
        .gap_1p5()
        .child(div().w_1p5().h_1p5().rounded_full().bg(tint))
        .child(div().text_xs().text_color(theme.text_muted).child(label))
}

/// The base column when no last-written snapshot exists: the merge degraded
/// to 2-way, and the pane says so instead of duplicating theirs (spec §10).
fn degraded_base_col(theme: Theme) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .flex()
        .flex_col()
        .child(pane_label("last written", theme.text_muted, theme))
        .child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_2()
                .text_xs()
                .text_color(theme.text_muted)
                .child("no snapshot · merging 2-way"),
        )
        .into_any_element()
}

fn pane_row(line: &PaneLine, side: PaneSide, theme: Theme) -> Div {
    // Protected spans get the amber underlay regardless of region kind — the
    // write-back rejection is the stronger fact.
    let bg = if line.protected {
        Some(Theme::wash(theme.drift, 0.15))
    } else {
        row_bg(line.kind, side, &theme)
    };
    div()
        .h_5()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .font_family("Menlo")
        .text_xs()
        .text_color(if line.kind == RegionKind::Unchanged {
            theme.text_muted
        } else {
            theme.text
        })
        .whitespace_nowrap()
        .when_some(bg, |el, bg| el.bg(bg))
        .child(
            div()
                .w_4()
                .flex_shrink_0()
                .text_color(theme.drift)
                .child(if line.protected { "⚿" } else { "" }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(line.text.clone()),
        )
}

fn result_row_el(
    row: &ResultRow,
    degraded_base: bool,
    theme: Theme,
    view: &WeakEntity<MergeView>,
) -> AnyElement {
    match row {
        ResultRow::Line { text, muted } => div()
            .h_5()
            .flex()
            .items_center()
            .px_2()
            .font_family("Menlo")
            .text_xs()
            .text_color(if *muted { theme.text_muted } else { theme.text })
            .whitespace_nowrap()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(text.clone()),
            )
            .into_any_element(),
        ResultRow::Placeholder { region, focused } => {
            let region = *region;
            div()
                .h_5()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .text_xs()
                .bg(Theme::wash(
                    theme.conflict,
                    if *focused { 0.15 } else { 0.08 },
                ))
                .child(div().text_color(theme.conflict).child("‹pick one›"))
                .child(pick_button(
                    "merge-pick-ours",
                    "ours",
                    region,
                    Choice::Ours,
                    theme.drift,
                    view,
                ))
                .child(pick_button(
                    "merge-pick-theirs",
                    "theirs",
                    region,
                    Choice::Theirs,
                    theme.accent,
                    view,
                ))
                .when(!degraded_base, |el| {
                    el.child(pick_button(
                        "merge-pick-base",
                        "base",
                        region,
                        Choice::Base,
                        theme.text_muted,
                        view,
                    ))
                })
                .into_any_element()
        }
    }
}

/// Inline pick button on a placeholder row, tinted like the pane it takes
/// from (ours → drift, theirs → accent, base → muted).
fn pick_button(
    id: &'static str,
    label: &'static str,
    region: usize,
    choice: Choice,
    color: Rgba,
    view: &WeakEntity<MergeView>,
) -> Stateful<Div> {
    let view = view.clone();
    div()
        .id(ElementId::named_usize(id, region))
        .flex_none()
        .px_1p5()
        .rounded_sm()
        .border_1()
        .border_color(color)
        .text_color(color)
        .cursor_pointer()
        .hover(move |el| el.bg(Theme::wash(color, 0.12)))
        .child(label)
        .on_click(move |_ev, _window, cx| {
            let choice = choice.clone();
            view.update(cx, |merge, cx| merge.pick(region, choice, cx))
                .ok();
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use czui_app::merge_inputs::MergeInputs;
    use czui_app::merge_state::MergeState;
    use czui_app::resolve::{ResolveError, ResolveOutcome};
    use czui_app::theme::Theme;
    use czui_core::merge::{Choice, RegionKind};
    use czui_core::template::{anchor::anchor, lexer::lex};

    use super::super::review::BannerTint;
    use super::{
        LoadedMerge, PaneSide, ResultRow, merge_banner, pane_lines, placeholder_row_ix,
        protected_line_set, result_rows, row_bg,
    };

    fn inputs(base: Option<&str>, ours: &str, theirs: &str) -> MergeInputs {
        MergeInputs {
            target: PathBuf::from("/home/u/.testrc"),
            ours: ours.to_string(),
            theirs: theirs.to_string(),
            base: base.map(str::to_string),
            source_path: PathBuf::from("/src/dot_testrc"),
            templated: false,
            span_map: None,
        }
    }

    /// One conflict on the middle line, context around it.
    fn conflict_state() -> MergeState {
        MergeState::new(&inputs(
            Some("a\nv = 1\nz\n"),
            "a\nv = 2\nz\n",
            "a\nv = 3\nz\n",
        ))
    }

    #[test]
    fn pane_lines_cover_every_side_line_with_region_kinds() {
        let state = conflict_state();
        let none = std::collections::HashSet::new();
        for side in [PaneSide::Theirs, PaneSide::Base, PaneSide::Ours] {
            let rows = pane_lines(&state.doc, side, &none);
            assert_eq!(rows.len(), 3, "{side:?} pane covers all lines");
            assert_eq!(
                rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
                [
                    RegionKind::Unchanged,
                    RegionKind::Conflict,
                    RegionKind::Unchanged
                ]
            );
            assert!(rows.iter().all(|r| !r.protected));
        }
        let ours = pane_lines(&state.doc, PaneSide::Ours, &none);
        assert_eq!(ours[1].text.as_ref(), "v = 2", "newline stripped");
    }

    #[test]
    fn row_bg_tints_conflicts_everywhere_and_one_sided_changes_on_their_pane() {
        let t = Theme::dark();
        for side in [PaneSide::Theirs, PaneSide::Base, PaneSide::Ours] {
            assert_eq!(
                row_bg(RegionKind::Conflict, side, &t),
                Some(Theme::wash(t.conflict, 0.10))
            );
            assert_eq!(row_bg(RegionKind::Unchanged, side, &t), None);
        }
        assert_eq!(
            row_bg(RegionKind::OursOnly, PaneSide::Ours, &t),
            Some(Theme::wash(t.drift, 0.08))
        );
        assert_eq!(row_bg(RegionKind::OursOnly, PaneSide::Theirs, &t), None);
        assert_eq!(row_bg(RegionKind::OursOnly, PaneSide::Base, &t), None);
        assert_eq!(
            row_bg(RegionKind::TheirsOnly, PaneSide::Theirs, &t),
            Some(Theme::wash(t.accent, 0.08))
        );
        assert_eq!(row_bg(RegionKind::TheirsOnly, PaneSide::Ours, &t), None);
        assert_eq!(
            row_bg(RegionKind::BothSame, PaneSide::Ours, &t),
            Some(Theme::wash(t.ok, 0.06))
        );
        assert_eq!(row_bg(RegionKind::BothSame, PaneSide::Base, &t), None);
    }

    #[test]
    fn protected_line_set_marks_lines_touching_templated_values() {
        let template = "email = {{ .email }}\neditor = hx\n";
        let rendered = "email = a@b.c\neditor = hx\n";
        let map = anchor(template, &lex(template).unwrap(), rendered);
        let lines: Vec<String> = rendered.split_inclusive('\n').map(str::to_owned).collect();
        let protected = protected_line_set(&lines, &map);
        assert!(
            protected.contains(&0),
            "the {{{{ .email }}}} value line is protected: {protected:?}"
        );
        assert!(
            !protected.contains(&1),
            "the literal editor line is not protected: {protected:?}"
        );
    }

    #[test]
    fn loaded_merge_marks_theirs_pane_protected_rows_only() {
        let template = "email = {{ .email }}\neditor = hx\n";
        let rendered = "email = a@b.c\neditor = hx\n";
        let map = anchor(template, &lex(template).unwrap(), rendered);
        let loaded = LoadedMerge::new(Arc::new(MergeInputs {
            target: PathBuf::from("/home/u/.testrc"),
            ours: "email = a@b.c\neditor = nvim\n".into(),
            theirs: rendered.into(),
            base: Some(rendered.into()),
            source_path: PathBuf::from("/src/dot_testrc.tmpl"),
            templated: true,
            span_map: Some(map),
        }));
        assert!(loaded.theirs_rows[0].protected);
        assert!(!loaded.theirs_rows[1].protected);
        assert!(loaded.base_rows.iter().all(|r| !r.protected));
        assert!(loaded.ours_rows.iter().all(|r| !r.protected));
    }

    #[test]
    fn result_rows_render_placeholder_then_chosen_lines_after_pick() {
        let mut state = conflict_state();
        let region = state.conflicts()[0];
        let rows = result_rows(&state);
        assert_eq!(
            rows,
            [
                ResultRow::Line {
                    text: "a".into(),
                    muted: true
                },
                ResultRow::Placeholder {
                    region,
                    focused: true // cursor starts on the first conflict
                },
                ResultRow::Line {
                    text: "z".into(),
                    muted: true
                },
            ]
        );
        assert_eq!(placeholder_row_ix(&rows, region), Some(1));

        state.pick(region, Choice::Theirs);
        let rows = result_rows(&state);
        assert_eq!(
            rows[1],
            ResultRow::Line {
                text: "v = 3".into(),
                muted: false
            }
        );
        assert_eq!(placeholder_row_ix(&rows, region), None);
    }

    #[test]
    fn result_rows_auto_regions_take_their_side_without_a_choice() {
        // ours-only edit: the result shows the disk line, normal tint.
        let state = MergeState::new(&inputs(Some("a\nb\n"), "a\nB\n", "a\nb\n"));
        assert_eq!(
            result_rows(&state),
            [
                ResultRow::Line {
                    text: "a".into(),
                    muted: true
                },
                ResultRow::Line {
                    text: "B".into(),
                    muted: false
                },
            ]
        );
    }

    fn done(committed: bool, pushed: bool, note: Option<&str>) -> ResolveOutcome {
        ResolveOutcome::Done {
            session: 9,
            committed,
            pushed,
            note: note.map(str::to_owned),
        }
    }

    #[test]
    fn merge_banner_full_success_is_ok_and_undoable() {
        let b = merge_banner(&Ok(done(true, true, None)));
        assert_eq!(b.text.as_ref(), "Merged · committed & pushed");
        assert_eq!(b.tint, BannerTint::Ok);
        assert!(b.undoable);

        // apply-only convergence (nothing to commit) must not claim a push
        let b = merge_banner(&Ok(done(true, false, None)));
        assert_eq!(b.text.as_ref(), "merged");
        assert_eq!(b.tint, BannerTint::Ok);
    }

    #[test]
    fn merge_banner_degraded_commit_carries_the_note_in_drift_tint() {
        let b = merge_banner(&Ok(done(true, false, Some("push failed: locked"))));
        assert_eq!(b.text.as_ref(), "Merged · push failed: locked");
        assert_eq!(b.tint, BannerTint::Drift);
        assert!(b.undoable, "the merge itself succeeded");
    }

    #[test]
    fn merge_banner_protected_span_and_error_stay_not_undoable() {
        let b = merge_banner(&Ok(ResolveOutcome::ProtectedSpan {
            detail: "edit at rendered bytes 8..13 touches a protected template span".into(),
        }));
        assert!(
            b.text
                .as_ref()
                .starts_with("This change touches a templated value · "),
            "text: {}",
            b.text
        );
        assert_eq!(b.tint, BannerTint::Drift);
        assert!(!b.undoable, "nothing was mutated");

        let b = merge_banner(&Err(ResolveError::Failed("daemon gone".into())));
        assert_eq!(b.text.as_ref(), "save failed: daemon gone");
        assert_eq!(b.tint, BannerTint::Conflict);
        assert!(!b.undoable);
    }
}
