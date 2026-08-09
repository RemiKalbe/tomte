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

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use czui_app::merge_inputs::{self, MergeInputs};
use czui_app::merge_state::MergeState;
use czui_app::resolve::{ResolveError, ResolveOutcome};
use czui_app::theme::Theme;
use czui_ui::components as ui;
use czui_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use czui_core::cmd::SystemRunner;
use czui_core::merge::{Choice, MergeDocument, RegionKind};
use czui_core::template::anchor::{SpanMap, SpanOrigin};
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, FocusHandle, KeyBinding, Rgba, ScrollHandle,
    SharedString, Stateful, WeakEntity, Window, actions, div, prelude::*, uniform_list,
};

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
    /// only). Payload: the template's own line(s), shown on hover — the
    /// point is "this text is GENERATED, here is the template behind it".
    protected: Option<SharedString>,
}

/// Per-line region tint. The conflict wash lands on every pane; one-sided
/// changes tint only the pane that carries them; `BothSame` gets a subtle ok
/// wash on the two changed panes.
/// Changed lines are tinted by THE SIDE THEY BELONG TO — amber in the
/// on-disk pane, blue in the source pane, muted in the base pane — matching
/// the provenance blocks in the result. Conflict rows use a stronger wash of
/// the same side tint so they still read hotter than one-sided changes.
fn row_bg(kind: RegionKind, side: PaneSide, theme: &Theme) -> Option<Rgba> {
    let side_tint = match side {
        PaneSide::Ours => theme.drift,
        PaneSide::Theirs => theme.accent,
        PaneSide::Base => theme.text_muted,
    };
    match (kind, side) {
        (RegionKind::Conflict, _) => Some(Theme::wash(side_tint, 0.16)),
        (RegionKind::OursOnly, PaneSide::Ours) => Some(Theme::wash(side_tint, 0.08)),
        (RegionKind::TheirsOnly, PaneSide::Theirs) => Some(Theme::wash(side_tint, 0.08)),
        (RegionKind::BothSame, PaneSide::Ours | PaneSide::Theirs) => {
            Some(Theme::wash(theme.ok, 0.06))
        }
        _ => None,
    }
}

/// Flatten one side of the document into pane rows. Regions partition each
/// side's lines in order, so concatenating their side ranges covers every
/// line exactly once.
fn pane_lines(
    doc: &MergeDocument,
    side: PaneSide,
    protected: &HashMap<usize, SharedString>,
) -> Vec<PaneLine> {
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
                protected: protected.get(&ix).cloned(),
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

/// The template line containing byte `pos` of `template`, trimmed.
fn template_line_at(template: &str, pos: usize) -> &str {
    let start = template[..pos.min(template.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = template[start..]
        .find('\n')
        .map(|i| start + i)
        .unwrap_or(template.len());
    template[start..end].trim_end()
}

/// Per-line protection info: line index (into `lines`, whose concatenation
/// is the rendered text the span map covers) → hover text explaining WHY,
/// preferably the template's own line for that span. Approximated to whole
/// lines — precision polish deferred (plan 7 Task 3).
fn protected_line_info(
    lines: &[String],
    map: &SpanMap,
    template: Option<&str>,
) -> HashMap<usize, SharedString> {
    // Re-lex for segment source ranges: cheap, and it succeeded at load
    // time or there would be no span map.
    let segments = template.and_then(|t| czui_core::template::lexer::lex(t).ok());
    let mut starts = Vec::with_capacity(lines.len() + 1);
    let mut offset = 0usize;
    for line in lines {
        starts.push(offset);
        offset += line.len();
    }
    starts.push(offset);
    let mut reasons: HashMap<usize, Vec<String>> = HashMap::new();
    for span in &map.spans {
        if span.range.is_empty() || !span_is_protected(&span.origin) {
            continue;
        }
        let reason = match (&span.origin, template, &segments) {
            (SpanOrigin::Action { segment }, Some(t), Some(segs)) => segs
                .get(*segment)
                .map(|seg| template_line_at(t, seg.src.start).to_string())
                .unwrap_or_else(|| "template expression".to_string()),
            (SpanOrigin::Action { .. }, _, _) => "template expression".to_string(),
            (SpanOrigin::Literal { src, .. }, Some(t), _) => {
                format!("repeated literal: {}", template_line_at(t, src.start))
            }
            (SpanOrigin::Literal { .. }, _, _) => "repeated template literal".to_string(),
            (SpanOrigin::Unmapped, _, _) => "unanchored template output".to_string(),
        };
        for ix in 0..lines.len() {
            if span.range.start < starts[ix + 1] && starts[ix] < span.range.end {
                let list = reasons.entry(ix).or_default();
                if !list.contains(&reason) {
                    list.push(reason.clone());
                }
            }
        }
    }
    reasons
        .into_iter()
        .map(|(ix, list)| {
            (
                ix,
                SharedString::from(format!("generated by template:\n{}", list.join("\n"))),
            )
        })
        .collect()
}

actions!(
    merge_editor,
    [
        PickDisk,
        PickSource,
        PickBase,
        PickBoth,
        NextConflict,
        UndoChoice,
        RedoChoice
    ]
);

/// Key bindings for the merge editor (active only while its pane has
/// focus via the "MergeEditor" key context). Registered by every app entry
/// point (main / gallery / live) so behavior is identical everywhere.
pub fn register_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("1", PickDisk, Some("MergeEditor")),
        KeyBinding::new("2", PickSource, Some("MergeEditor")),
        KeyBinding::new("3", PickBase, Some("MergeEditor")),
        KeyBinding::new("b", PickBoth, Some("MergeEditor")),
        KeyBinding::new("n", NextConflict, Some("MergeEditor")),
        KeyBinding::new("cmd-z", UndoChoice, Some("MergeEditor")),
        KeyBinding::new("cmd-shift-z", RedoChoice, Some("MergeEditor")),
    ]);
}

/// What one region renders as (merge-editor-v2 spec step 3). Pure and
/// testable; the render walk consumes it directly.
#[derive(Debug, PartialEq)]
enum RegionDisplay {
    /// Unchanged context lines (muted).
    Context { lines: Vec<SharedString> },
    /// Open decision: a conflict with no choice, a revisited decision, or an
    /// auto-resolved region (current = the engine default, overridable).
    /// `provenance` carries both sides for true re-deciding; auto regions
    /// show their materialized default instead (spec: one strip line per
    /// changed region, full provenance only where a human must look).
    Deciding {
        current: Option<ui::ChoiceKind>,
        focused: bool,
        has_base: bool,
        /// Some => show ours/theirs provenance blocks (with the theirs
        /// side's region-local protected map); None => show `lines`.
        provenance: Option<(
            Vec<SharedString>,
            Vec<SharedString>,
            HashMap<usize, SharedString>,
        )>,
        lines: Vec<(SharedString, Option<ui::Side>)>,
    },
    /// Explicit decision, collapsed: strip names it, lines show the result.
    Decided {
        choice: ui::ChoiceKind,
        focused: bool,
        lines: Vec<(SharedString, Option<ui::Side>)>,
    },
}

fn choice_kind(choice: &Choice) -> ui::ChoiceKind {
    match choice {
        Choice::Ours => ui::ChoiceKind::Ours,
        Choice::Theirs => ui::ChoiceKind::Theirs,
        Choice::Base => ui::ChoiceKind::Base,
        Choice::Both => ui::ChoiceKind::Both,
        Choice::Edited(_) => ui::ChoiceKind::Edited,
    }
}

fn side_lines(
    doc: &MergeDocument,
    region: &czui_core::merge::MergeRegion,
    side: ui::Side,
) -> Vec<SharedString> {
    let lines = match side {
        ui::Side::Ours => &doc.ours_lines()[region.ours.clone()],
        ui::Side::Theirs => &doc.theirs_lines()[region.theirs.clone()],
        ui::Side::Base => &doc.base_lines()[region.base.clone()],
    };
    lines
        .iter()
        .map(|l| SharedString::from(display_text(l).to_owned()))
        .collect()
}

/// Materialized lines for a choice, each tagged with its provenance side
/// (None = edited text, tinted as such).
fn choice_lines(
    doc: &MergeDocument,
    region: &czui_core::merge::MergeRegion,
    choice: &Choice,
) -> Vec<(SharedString, Option<ui::Side>)> {
    let tag = |side: ui::Side| {
        side_lines(doc, region, side)
            .into_iter()
            .map(move |l| (l, Some(side)))
            .collect::<Vec<_>>()
    };
    match choice {
        Choice::Ours => tag(ui::Side::Ours),
        Choice::Theirs => tag(ui::Side::Theirs),
        Choice::Base => tag(ui::Side::Base),
        Choice::Both => {
            let mut out = tag(ui::Side::Ours);
            out.extend(tag(ui::Side::Theirs));
            out
        }
        Choice::Edited(text) => text
            .split_inclusive('\n')
            .map(|l| (SharedString::from(display_text(l).to_owned()), None))
            .collect(),
    }
}

/// The engine's default choice for a region that needs no human decision.
fn auto_choice(kind: RegionKind) -> Option<Choice> {
    match kind {
        RegionKind::OursOnly | RegionKind::BothSame => Some(Choice::Ours),
        RegionKind::TheirsOnly => Some(Choice::Theirs),
        RegionKind::Unchanged | RegionKind::Conflict => None,
    }
}

/// Compute the display for region `idx` from the single source of truth.
fn region_display(
    state: &MergeState,
    idx: usize,
    revisiting: &std::collections::HashSet<usize>,
    protected: &HashMap<usize, SharedString>,
) -> RegionDisplay {
    let doc = &state.doc;
    let region = &doc.regions[idx];
    let has_base = !state.degraded_base;
    // Protected map is keyed by GLOBAL theirs-line index; provenance rows
    // are region-local.
    let theirs_protected = || -> HashMap<usize, SharedString> {
        protected
            .iter()
            .filter(|(gix, _)| region.theirs.contains(gix))
            .map(|(gix, tip)| (gix - region.theirs.start, tip.clone()))
            .collect()
    };
    match (state.resolution.get(idx), region.kind) {
        (None, RegionKind::Unchanged) => RegionDisplay::Context {
            lines: side_lines(doc, region, ui::Side::Base),
        },
        (None, RegionKind::Conflict) => RegionDisplay::Deciding {
            current: None,
            focused: state.cursor == Some(idx),
            has_base,
            provenance: Some((
                side_lines(doc, region, ui::Side::Ours),
                side_lines(doc, region, ui::Side::Theirs),
                theirs_protected(),
            )),
            lines: Vec::new(),
        },
        (None, kind) => {
            let auto = auto_choice(kind).expect("changed non-conflict region has a default");
            RegionDisplay::Deciding {
                current: Some(choice_kind(&auto)),
                focused: false,
                has_base,
                provenance: None,
                lines: choice_lines(doc, region, &auto),
            }
        }
        (Some(choice), _) if revisiting.contains(&idx) => RegionDisplay::Deciding {
            current: Some(choice_kind(choice)),
            focused: state.cursor == Some(idx),
            has_base,
            provenance: Some((
                side_lines(doc, region, ui::Side::Ours),
                side_lines(doc, region, ui::Side::Theirs),
                theirs_protected(),
            )),
            lines: Vec::new(),
        },
        (Some(choice), _) => RegionDisplay::Decided {
            choice: choice_kind(choice),
            focused: state.cursor == Some(idx),
            lines: choice_lines(doc, region, choice),
        },
    }
}

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
    /// Theirs-pane line index → template hover text (protected spans).
    protected: Rc<HashMap<usize, SharedString>>,
    /// Per-region first-line index per column [theirs, base, ours], plus a
    /// totals sentinel row — the basis for region-aligned scroll sync.
    region_line_starts: Rc<Vec<[usize; 3]>>,
}

impl LoadedMerge {
    pub(super) fn new(inputs: Arc<MergeInputs>) -> Self {
        let state = MergeState::new(&inputs);
        let protected = inputs
            .span_map
            .as_ref()
            .map(|map| {
                protected_line_info(state.doc.theirs_lines(), map, inputs.template.as_deref())
            })
            .unwrap_or_default();
        let none = HashMap::new();
        let mut region_line_starts = Vec::with_capacity(state.doc.regions.len() + 1);
        let mut acc = [0usize; 3];
        for region in &state.doc.regions {
            region_line_starts.push(acc);
            acc[0] += region.theirs.len();
            acc[1] += region.base.len();
            acc[2] += region.ours.len();
        }
        region_line_starts.push(acc);
        Self {
            region_line_starts: Rc::new(region_line_starts),
            theirs_rows: Rc::new(pane_lines(&state.doc, PaneSide::Theirs, &protected)),
            base_rows: Rc::new(pane_lines(&state.doc, PaneSide::Base, &none)),
            ours_rows: Rc::new(pane_lines(&state.doc, PaneSide::Ours, &none)),
            protected: Rc::new(protected),
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
    result_scroll: ScrollHandle,
    /// One scroll handle per input pane (theirs/base/ours); wheel on any
    /// pane mirrors its offset to the others so the three columns compare
    /// without independent scrolling.
    pane_scrolls: [gpui::UniformListScrollHandle; 3],
    /// Decided regions whose strip the user reopened (non-destructive).
    revisiting: std::collections::HashSet<usize>,
    /// Region displays + analytic block geometry, rebuilt ONLY on state
    /// mutations (pick/undo/revisit/load) — never per frame. Scrolling at
    /// 120Hz re-renders the window every tick (gpui refresh-on-wheel), so
    /// per-frame recomputation of every line was the 2026-08-08 lag.
    display_cache: Vec<RegionDisplay>,
    /// (top, height) per region block, content-relative; parallel to
    /// `display_cache`.
    geometry: Vec<(f32, f32)>,
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    focus_handle: FocusHandle,
}

/// One document-level undo step: a choice change on one region, with the
/// cursor restored so undo never "jumps" (merge-editor-v2 spec).
struct UndoEntry {
    region: usize,
    prev: Option<Choice>,
    next: Choice,
    prev_cursor: Option<usize>,
}

impl MergeView {
    pub fn new(shell: WeakEntity<Shell>, cx: &mut Context<Self>) -> Self {
        Self {
            shell,
            target: None,
            loading: false,
            error: None,
            loaded: None,
            saving: false,
            banner: None,
            result_scroll: ScrollHandle::new(),
            pane_scrolls: std::array::from_fn(|_| gpui::UniformListScrollHandle::new()),
            revisiting: std::collections::HashSet::new(),
            display_cache: Vec::new(),
            geometry: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            focus_handle: cx.focus_handle(),
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
                view.rebuild_display();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Recompute the display cache + analytic geometry from the current
    /// resolution. Called on every state mutation; render only reads.
    fn rebuild_display(&mut self) {
        let Some(loaded) = &self.loaded else {
            self.display_cache.clear();
            self.geometry.clear();
            return;
        };
        self.display_cache = (0..loaded.state.doc.regions.len())
            .map(|idx| {
                region_display(&loaded.state, idx, &self.revisiting, &loaded.protected)
            })
            .collect();
        let mut top = RESULT_PAD_TOP;
        self.geometry = self
            .display_cache
            .iter()
            .map(|d| {
                let h = block_height(d);
                let entry = (top, h);
                top += h;
                entry
            })
            .collect();
    }

    /// The single mutation funnel (merge-editor-v2 spec): record the undo
    /// entry, apply the choice, close any revisit, advance the cursor.
    /// Every pick path — click, keybinding, redo — lands here.
    fn apply(&mut self, region: usize, choice: Choice, cx: &mut Context<Self>) {
        let Some(loaded) = &mut self.loaded else {
            return;
        };
        let prev = loaded.state.resolution.get(region).cloned();
        let prev_cursor = loaded.state.cursor;
        loaded.state.pick(region, choice.clone());
        self.revisiting.remove(&region);
        self.undo.push(UndoEntry {
            region,
            prev,
            next: choice,
            prev_cursor,
        });
        self.redo.clear();
        self.rebuild_display();
        cx.notify();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.undo.pop() else {
            return;
        };
        let Some(loaded) = &mut self.loaded else {
            return;
        };
        match &entry.prev {
            Some(choice) => loaded.state.resolution.set(entry.region, choice.clone()),
            None => loaded.state.resolution.unset(entry.region),
        }
        loaded.state.cursor = entry.prev_cursor;
        self.redo.push(entry);
        self.rebuild_display();
        cx.notify();
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.redo.pop() else {
            return;
        };
        let Some(loaded) = &mut self.loaded else {
            return;
        };
        loaded.state.pick(entry.region, entry.next.clone());
        self.undo.push(entry);
        self.rebuild_display();
        cx.notify();
    }

    /// Reopen a decided region's strip (non-destructive: the choice stands
    /// until a new pick replaces it — Resolution is truth, so revisiting is
    /// free, unlike Zed's dissolve-on-pick).
    fn revisit(&mut self, region: usize, cx: &mut Context<Self>) {
        self.revisiting.insert(region);
        if let Some(loaded) = &mut self.loaded {
            loaded.state.cursor = Some(region);
        }
        self.rebuild_display();
        cx.notify();
    }

    /// Pick at the cursor (keybindings route here).
    fn pick_at_cursor(&mut self, choice: Choice, cx: &mut Context<Self>) {
        let Some(region) = self.loaded.as_ref().and_then(|l| l.state.cursor) else {
            return;
        };
        self.apply(region, choice, cx);
    }

    /// Region-aligned scroll sync: `leader` is 0..=2 for the input panes or
    /// 3 for the result view. Maps the leader's viewport-top to (region,
    /// fraction) and positions every other surface at ITS version of that
    /// point. Pure offset math over the cached analytic geometry — no
    /// notify, no layout reads: the wheel's own refresh paints the new
    /// offsets in the same frame (2026-08-08: notifying per wheel tick was
    /// part of the lag).
    fn sync_from(&mut self, leader: usize) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let starts = &loaded.region_line_starts;
        if starts.len() < 2 || self.geometry.len() + 1 != starts.len() {
            return;
        }

        let (k, f) = if leader == 3 {
            let scroll_top = -f32::from(self.result_scroll.offset().y);
            let mut found = (self.geometry.len() - 1, 1.0f32);
            for (k, &(top, height)) in self.geometry.iter().enumerate() {
                if scroll_top < top + height {
                    let f = if height > 0.0 {
                        ((scroll_top - top) / height).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    found = (k, f);
                    break;
                }
            }
            found
        } else {
            let line =
                -f32::from(self.pane_scrolls[leader].0.borrow().base_handle.offset().y)
                    / PANE_ROW_H;
            region_at(starts, leader, line)
        };

        for col in 0..3 {
            if leader == col {
                continue;
            }
            let y = gpui::px(-(line_at(starts, col, k, f) * PANE_ROW_H));
            let handle = self.pane_scrolls[col].0.borrow();
            let mut offset = handle.base_handle.offset();
            offset.y = y;
            handle.base_handle.set_offset(offset);
        }
        if leader != 3 {
            let (top, height) = self.geometry[k];
            let mut offset = self.result_scroll.offset();
            offset.y = gpui::px(-(top + f * height));
            self.result_scroll.set_offset(offset);
        }
    }

    /// `Next ↓`: advance the cursor to the next unresolved conflict — or,
    /// once everything is resolved, to the next changed region, so every
    /// decision point stays reachable for revisiting (2026-08-08 feedback).
    /// Target selection lives in [`next_target`] (pure, tested).
    fn next(&mut self, cx: &mut Context<Self>) {
        let Some(loaded) = &mut self.loaded else {
            return;
        };
        if let Some(region) = next_target(&mut loaded.state) {
            if let Some(&(top, _)) = self.geometry.get(region) {
                let mut offset = self.result_scroll.offset();
                offset.y = gpui::px(-(top - RESULT_PAD_TOP));
                self.result_scroll.set_offset(offset);
            }
            // Bring the input panes along to the same region.
            self.sync_from(3);
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
        let mut bar = ui::detail_header(theme, name, path, Vec::new());
        if let Some(loaded) = &self.loaded {
            let (decided, total) = loaded.state.progress();
            let open = total - decided;
            bar = bar
                .child(
                    div()
                        .text_xs()
                        .whitespace_nowrap()
                        .text_color(if open == 0 { theme.ok } else { theme.conflict })
                        .child(if total == 0 {
                            let changed = loaded
                                .state
                                .doc
                                .regions
                                .iter()
                                .filter(|r| r.kind != RegionKind::Unchanged)
                                .count();
                            if changed == 0 {
                                "Nothing to merge".to_string()
                            } else {
                                format!(
                                    "auto-merged · {changed} changed region{} — review below",
                                    if changed == 1 { "" } else { "s" }
                                )
                            }
                        } else {
                            let s = if total == 1 { "" } else { "s" };
                            format!("{total} conflict{s}, {} resolved", total - open)
                        }),
                );
            let changed = loaded
                .state
                .doc
                .regions
                .iter()
                .any(|r| r.kind != RegionKind::Unchanged);
            if changed {
                // Conflict-red while decisions are owed; neutral afterwards
                // (it then walks decided/auto regions for revisiting).
                let tint = if open > 0 { theme.conflict } else { theme.text };
                bar = bar.child(ui::button(
                    theme,
                    "merge-next",
                    "Next ↓".into(),
                    ui::ButtonVariant::Outline(tint),
                    ui::ButtonSize::Sm,
                    cx.listener(|view, _ev, _window, cx| view.next(cx)),
                ));
            }
        }
        bar = bar.child(ui::button(
            theme,
            "merge-cancel",
            "Cancel".into(),
            ui::ButtonVariant::Outline(theme.text),
            ui::ButtonSize::Sm,
            cx.listener(|view, _ev, _window, cx| view.cancel(cx)),
        ));
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
        if enabled {
            ui::button(
                theme,
                "merge-save",
                "Save".into(),
                ui::ButtonVariant::Outline(theme.accent),
                ui::ButtonSize::Sm,
                cx.listener(|view, _ev, _window, cx| view.save(cx)),
            )
        } else {
            let s = if unresolved == 1 { "" } else { "s" };
            ui::disabled_button(
                theme,
                "merge-save",
                "Save".into(),
                ui::ButtonSize::Sm,
                Some(format!("{unresolved} conflict{s} left").into()),
            )
        }
    }

    fn body(&self, viewport_h: f32, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
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

        let state = &loaded.state;
        let degraded = state.degraded_base;
        let cursor = state.cursor;
        let view = cx.weak_entity();
        // Virtualized result: only blocks near the viewport materialize;
        // spacers hold the geometry. O(viewport) per frame instead of
        // O(document) — with gpui re-rendering on every wheel tick, this is
        // what makes synced scrolling smooth.
        let scroll_top = -f32::from(self.result_scroll.offset().y);
        let overscan = 600.0f32;
        let lo = scroll_top - overscan;
        let hi = scroll_top + viewport_h + overscan;
        let total: f32 = self.geometry.last().map(|&(t, h)| t + h).unwrap_or(0.);
        let mut lead = 0.0f32;
        let mut trail = 0.0f32;
        let mut blocks: Vec<AnyElement> = Vec::new();
        for (idx, display) in self.display_cache.iter().enumerate() {
            let (top, height) = self.geometry[idx];
            if top + height < lo {
                lead += height;
            } else if top > hi {
                trail += height;
            } else {
                blocks.push(region_block(
                    idx,
                    display,
                    cursor == Some(idx),
                    theme,
                    &view,
                ));
            }
        }
        let _ = total;
        let blocks = {
            let mut v: Vec<AnyElement> = Vec::with_capacity(blocks.len() + 2);
            v.push(div().h(gpui::px(lead)).flex_none().into_any_element());
            v.extend(blocks);
            v.push(div().h(gpui::px(trail)).flex_none().into_any_element());
            v
        };

        // One shared grid of hairlines (2026-08-08 feedback): three input
        // columns split by vertical dividers, one horizontal rule, then the
        // result flowing as the pane's own content — no floating boxes.
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h_2_5()
                    .flex_none()
                    .flex()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(pane_col(
                        "merge-pane-theirs",
                        "source (rendered)",
                        theme.accent,
                        &loaded.theirs_rows,
                        PaneSide::Theirs,
                        false,
                        0,
                        &self.pane_scrolls,
                        &view,
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
                            true,
                            1,
                            &self.pane_scrolls,
                            &view,
                            theme,
                        )
                    })
                    .child(pane_col(
                        "merge-pane-ours",
                        "on disk",
                        theme.drift,
                        &loaded.ours_rows,
                        PaneSide::Ours,
                        true,
                        2,
                        &self.pane_scrolls,
                        &view,
                        theme,
                    )),
            )
            .child(
                div()
                    .id("merge-result")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.result_scroll)
                    .flex()
                    .flex_col()
                    .pt_1()
                    .on_scroll_wheel({
                        let view = view.clone();
                        move |_event, _window, cx| {
                            view.update(cx, |merge, _cx| merge.sync_from(3)).ok();
                        }
                    })
                    .children(blocks),
            )
            .into_any_element()
    }
}

impl Render for MergeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::time("merge-render", || self.render_inner(window, cx))
    }
}

impl MergeView {
    fn render_inner(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        // Cache safety net: poses (gallery/tests) set `loaded` directly
        // without going through load(); rebuild lazily when stale.
        let regions = self
            .loaded
            .as_ref()
            .map(|l| l.state.doc.regions.len())
            .unwrap_or(0);
        if self.display_cache.len() != regions {
            self.rebuild_display();
        }
        // Claim focus on arrival so the keybindings work immediately.
        if self.loaded.is_some() && !self.focus_handle.is_focused(window) {
            self.focus_handle.focus(window);
        }
        div()
            .track_focus(&self.focus_handle)
            .key_context("MergeEditor")
            .on_action(cx.listener(|this, _: &PickDisk, _w, cx| {
                this.pick_at_cursor(Choice::Ours, cx)
            }))
            .on_action(cx.listener(|this, _: &PickSource, _w, cx| {
                this.pick_at_cursor(Choice::Theirs, cx)
            }))
            .on_action(cx.listener(|this, _: &PickBase, _w, cx| {
                if !this.loaded.as_ref().is_some_and(|l| l.state.degraded_base) {
                    this.pick_at_cursor(Choice::Base, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &PickBoth, _w, cx| {
                this.pick_at_cursor(Choice::Both, cx)
            }))
            .on_action(cx.listener(|this, _: &NextConflict, _w, cx| this.next(cx)))
            .on_action(cx.listener(|this, _: &UndoChoice, _w, cx| this.undo(cx)))
            .on_action(cx.listener(|this, _: &RedoChoice, _w, cx| this.redo(cx)))
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.header(theme, cx))
            .when_some(self.banner.clone(), |el, banner| {
                el.child(banner_el(&banner, theme))
            })
            .child(self.body(f32::from(window.viewport_size().height), theme, cx))
    }
}

/// Merge-local banner: same shape as Review's, minus the Undo button (the
/// banners that stay here — protected span, errors — are never undoable).
fn banner_el(banner: &OutcomeBanner, theme: Theme) -> Div {
    ui::banner(theme, banner.tint, banner.text.clone(), None)
}

/// One read-only pane column: an attached label header (the user should
/// never have to map floating toolbar chips onto panes by position) over a
/// bordered `uniform_list` of its lines.
#[allow(clippy::too_many_arguments)]
fn pane_col(
    id: &'static str,
    label: &'static str,
    tint: Rgba,
    rows: &Rc<Vec<PaneLine>>,
    side: PaneSide,
    divider: bool,
    me: usize,
    scrolls: &[gpui::UniformListScrollHandle; 3],
    view: &WeakEntity<MergeView>,
    theme: Theme,
) -> AnyElement {
    let rows = rows.clone();
    let view = view.clone();
    div()
        .id(ElementId::named_usize("merge-pane-wrap", me))
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .flex_col()
        .when(divider, |el| el.border_l_1().border_color(theme.border))
        .child(ui::pane_label(theme, label, tint))
        .child(
            uniform_list(id, rows.len(), move |range, _window, _cx| {
                range
                    .map(|ix| pane_row(ix, &rows[ix], side, theme))
                    .collect()
            })
            .track_scroll(scrolls[me].clone())
            .flex_1(),
        )
        // Scroll sync: this wrapper's wheel handler runs AFTER the inner
        // list already applied the wheel (children dispatch first), so the
        // leader's fresh position drives the region-aligned mapping.
        .on_scroll_wheel(move |_event, _window, cx| {
            view.update(cx, |merge, _cx| merge.sync_from(me)).ok();
        })
        .into_any_element()
}

/// The base column when no last-written snapshot exists: the merge degraded
/// to 2-way, and the pane says so instead of duplicating theirs (spec §10).
fn degraded_base_col(theme: Theme) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .flex_col()
        .border_l_1()
        .border_color(theme.border)
        .child(ui::pane_label(theme, "last written", theme.text_muted))
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

/// Choose Next ↓'s target: unresolved conflicts first; once none remain,
/// cycle ALL changed regions. Branch explicitly — `next_unresolved()`
/// clears the cursor when everything is resolved, so chaining it in front
/// of `next_changed()` resets the cycle to the first region on every press.
fn next_target(state: &mut MergeState) -> Option<usize> {
    if state.unresolved().is_empty() {
        state.next_changed()
    } else {
        state.next_unresolved()
    }
}

/// Heights of the result column's fixed-size pieces — MUST mirror the
/// render code (mono_line h_5, decision_strip h_6, provenance_label h_5,
/// block my_1 margins). The analytic geometry powers both virtualization
/// and region-aligned scroll sync.
const RESULT_ROW_H: f32 = 20.;
const RESULT_STRIP_H: f32 = 24.;
const RESULT_LABEL_H: f32 = 20.;
const RESULT_BLOCK_MARGIN: f32 = 8.;
/// The result container's pt_1.
const RESULT_PAD_TOP: f32 = 4.;

fn block_height(display: &RegionDisplay) -> f32 {
    match display {
        RegionDisplay::Context { lines } => lines.len() as f32 * RESULT_ROW_H,
        RegionDisplay::Deciding {
            provenance, lines, ..
        } => {
            let body = match provenance {
                Some((ours, theirs, _)) => {
                    2. * RESULT_LABEL_H + (ours.len() + theirs.len()) as f32 * RESULT_ROW_H
                }
                None => lines.len() as f32 * RESULT_ROW_H,
            };
            RESULT_BLOCK_MARGIN + RESULT_STRIP_H + body
        }
        RegionDisplay::Decided { lines, .. } => {
            RESULT_BLOCK_MARGIN + RESULT_STRIP_H + lines.len() as f32 * RESULT_ROW_H
        }
    }
}

/// Fixed mono row height (`h_5` = 20px) — the unit converting pane scroll
/// offsets to line positions for region-aligned sync.
const PANE_ROW_H: f32 = 20.;

/// Region-aligned scroll mapping (2026-08-08 feedback: raw offset mirroring
/// is not 1:1 across panes). `starts` has one row per region plus a totals
/// sentinel; `starts[k][col]` is the first line of region k in that column.
/// Returns which region the given line sits in and the fraction through it.
fn region_at(starts: &[[usize; 3]], col: usize, line: f32) -> (usize, f32) {
    let regions = starts.len().saturating_sub(1);
    if regions == 0 {
        return (0, 0.0);
    }
    let mut k = regions - 1; // saturate at the last region on overscroll
    for i in 0..regions {
        if (starts[i][col] as f32) <= line && line < (starts[i + 1][col] as f32) {
            k = i;
            break;
        }
    }
    let len = (starts[k + 1][col] - starts[k][col]) as f32;
    let f = if len > 0.0 {
        ((line - starts[k][col] as f32) / len).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (k, f)
}

/// The line position in `col` for region `k` at fraction `f` — zero-length
/// regions pin at their boundary (the "know when to stop" behavior).
fn line_at(starts: &[[usize; 3]], col: usize, k: usize, f: f32) -> f32 {
    let len = (starts[k + 1][col] - starts[k][col]) as f32;
    starts[k][col] as f32 + f * len
}

fn pane_row(ix: usize, line: &PaneLine, side: PaneSide, theme: Theme) -> AnyElement {
    // Protected spans get the amber underlay regardless of region kind — the
    // write-back rejection is the stronger fact.
    let bg = if line.protected.is_some() {
        Some(Theme::wash(theme.drift, 0.15))
    } else {
        row_bg(line.kind, side, &theme)
    };
    // Compact template marker (2026-08-08: a wide chip column shifted the
    // panes' root indentation apart): EVERY mono row in the merge editor
    // carries the same 16px gutter, protected lines fill it with an amber
    // {} and the hover tooltip tells the full story.
    let row = ui::mono_line(theme)
        .text_color(if line.kind == RegionKind::Unchanged {
            theme.text_muted
        } else {
            theme.text
        })
        .when_some(bg, |el, bg| el.bg(bg))
        .child(ui::line_gutter(
            theme.drift,
            if line.protected.is_some() { "{}" } else { "" },
        ))
        .child(ui::line_text(line.text.clone()));
    match &line.protected {
        Some(tip) => row
            .id(ElementId::named_usize("tmpl-line", ix))
            .tooltip(ui::text_tooltip(tip.clone()))
            .into_any_element(),
        None => row.into_any_element(),
    }
}

/// One region's block in the result column: strip (when the region changed)
/// over its lines. One column child per region so `Next ↓` can
/// scroll_to_item(region index).
fn region_block(
    idx: usize,
    display: &RegionDisplay,
    cursor_here: bool,
    theme: Theme,
    view: &WeakEntity<MergeView>,
) -> AnyElement {
    let tinted_lines = move |lines: Vec<(SharedString, Option<ui::Side>)>| {
        div().flex().flex_col().children(lines.into_iter().map(move |(text, side)| {
            let row = ui::mono_line(theme)
                .child(ui::line_gutter(theme.text_muted, ""))
                .child(ui::line_text(text));
            match side {
                Some(side) => row.bg(Theme::wash(side.tint(theme), 0.07)),
                None => row.bg(Theme::wash(theme.ok, 0.07)),
            }
        }))
    };
    let strip_for = |current, focused, has_base| {
        let on_pick = {
            let view = view.clone();
            move |kind: ui::ChoiceKind, _ev: &ClickEvent, _window: &mut Window, cx: &mut App| {
                let choice = match kind {
                    ui::ChoiceKind::Ours => Choice::Ours,
                    ui::ChoiceKind::Theirs => Choice::Theirs,
                    ui::ChoiceKind::Base => Choice::Base,
                    ui::ChoiceKind::Both => Choice::Both,
                    ui::ChoiceKind::Edited => return,
                };
                view.update(cx, |merge, cx| merge.apply(idx, choice, cx)).ok();
            }
        };
        // Hand-editing lands with the TextArea integration (spec step 7).
        let on_edit = |_: &ClickEvent, _: &mut Window, _: &mut App| {};
        let on_revisit = {
            let view = view.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                view.update(cx, |merge, cx| merge.revisit(idx, cx)).ok();
            }
        };
        ui::decision_strip(
            theme,
            idx,
            ui::StripState::Deciding {
                has_base,
                current,
                focused,
            },
            on_pick,
            on_edit,
            on_revisit,
        )
    };

    match display {
        RegionDisplay::Context { lines } => div()
            .flex()
            .flex_col()
            .children(lines.iter().map(move |text| {
                ui::mono_line(theme)
                    .text_color(theme.text_muted)
                    .child(ui::line_gutter(theme.text_muted, ""))
                    .child(ui::line_text(text.clone()))
            }))
            .into_any_element(),
        RegionDisplay::Deciding {
            current,
            has_base,
            provenance,
            lines,
            ..
        } => {
            let block = div()
                .flex()
                .flex_col()
                .my_1()
                .child(strip_for(*current, cursor_here, *has_base));
            match provenance {
                Some((ours, theirs, theirs_protected)) => block
                    .child(ui::provenance_label(theme, ui::Side::Ours))
                    .child(ui::provenance_rows(
                        theme,
                        ui::Side::Ours,
                        ours,
                        &HashMap::new(),
                    ))
                    .child(ui::provenance_label(theme, ui::Side::Theirs))
                    .child(ui::provenance_rows(
                        theme,
                        ui::Side::Theirs,
                        theirs,
                        theirs_protected,
                    ))
                    .into_any_element(),
                None => block.child(tinted_lines(lines.clone())).into_any_element(),
            }
        }
        RegionDisplay::Decided { choice, lines, .. } => {
            let on_revisit = {
                let view = view.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    view.update(cx, |merge, cx| merge.revisit(idx, cx)).ok();
                }
            };
            div()
                .flex()
                .flex_col()
                .my_1()
                .child(ui::decision_strip(
                    theme,
                    idx,
                    ui::StripState::Decided {
                        choice: *choice,
                        focused: cursor_here,
                    },
                    |_, _, _, _| {},
                    |_, _, _| {},
                    on_revisit,
                ))
                .child(tinted_lines(lines.clone()))
                .into_any_element()
        }
    }
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
        LoadedMerge, PaneSide, RegionDisplay, line_at, merge_banner, next_target, pane_lines,
        protected_line_info, region_at, region_display, row_bg,
    };
    use gpui::SharedString;
    use std::collections::HashMap;
    use czui_ui::components::{ChoiceKind, Side};
    #[allow(unused_imports)]
    use super::{
    };

    fn inputs(base: Option<&str>, ours: &str, theirs: &str) -> MergeInputs {
        MergeInputs {
            target: PathBuf::from("/home/u/.testrc"),
            ours: ours.to_string(),
            theirs: theirs.to_string(),
            base: base.map(str::to_string),
            source_path: PathBuf::from("/src/dot_testrc"),
            templated: false,
            template: None,
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
        let none = std::collections::HashMap::new();
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
            assert!(rows.iter().all(|r| r.protected.is_none()));
        }
        let ours = pane_lines(&state.doc, PaneSide::Ours, &none);
        assert_eq!(ours[1].text.as_ref(), "v = 2", "newline stripped");
    }

    #[test]
    fn row_bg_tints_by_owning_side() {
        let t = Theme::dark();
        // Conflict rows carry each pane's OWN tint (stronger wash), so a
        // line's color always names where it comes from (user feedback,
        // 2026-07-31).
        assert_eq!(
            row_bg(RegionKind::Conflict, PaneSide::Ours, &t),
            Some(Theme::wash(t.drift, 0.16))
        );
        assert_eq!(
            row_bg(RegionKind::Conflict, PaneSide::Theirs, &t),
            Some(Theme::wash(t.accent, 0.16))
        );
        assert_eq!(
            row_bg(RegionKind::Conflict, PaneSide::Base, &t),
            Some(Theme::wash(t.text_muted, 0.16))
        );
        for side in [PaneSide::Theirs, PaneSide::Base, PaneSide::Ours] {
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
    fn protected_line_info_marks_lines_and_names_the_template_line() {
        let template = "email = {{ .email }}\neditor = hx\n";
        let rendered = "email = a@b.c\neditor = hx\n";
        let map = anchor(template, &lex(template).unwrap(), rendered);
        let lines: Vec<String> = rendered.split_inclusive('\n').map(str::to_owned).collect();
        let protected = protected_line_info(&lines, &map, Some(template));
        // The hover text carries the template's OWN line for that span
        // (2026-08-08 feedback: the glyph alone said nothing).
        let tip = protected.get(&0).expect("value line is protected");
        assert!(
            tip.contains("email = {{ .email }}"),
            "tooltip names the template line: {tip}"
        );
        assert!(
            !protected.contains_key(&1),
            "the literal editor line is not protected"
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
            template: Some(template.into()),
            span_map: Some(map),
        }));
        assert!(loaded.theirs_rows[0].protected.is_some());
        assert!(loaded.theirs_rows[1].protected.is_none());
        assert!(loaded.base_rows.iter().all(|r| r.protected.is_none()));
        assert!(loaded.ours_rows.iter().all(|r| r.protected.is_none()));
    }

    #[test]
    fn next_target_cycles_changed_regions_when_no_conflicts_remain() {
        // Auto-merged doc: two changed regions (ours-only + theirs-only),
        // zero conflicts — the 2026-08-08 "Next only goes to the first" bug.
        let mut state = MergeState::new(&inputs(
            Some("a\nb\nc\n"),
            "A\nb\nc\n", // ours changed region around line 1
            "a\nb\nC\n", // theirs changed region around line 3
        ));
        assert!(state.unresolved().is_empty(), "no conflicts in this doc");
        let changed = state.changed_regions();
        assert!(changed.len() >= 2, "need at least two stops: {changed:?}");

        let first = next_target(&mut state).expect("first stop");
        let second = next_target(&mut state).expect("second stop");
        assert_ne!(first, second, "Next must advance, not restart");
        // Full cycle wraps back to the first stop.
        let mut cur = second;
        for _ in 0..changed.len() {
            cur = next_target(&mut state).expect("cycling");
            if cur == first {
                break;
            }
        }
        assert_eq!(cur, first, "cycle wraps to the first stop");
    }

    #[test]
    fn region_scroll_mapping_is_piecewise_and_pins_empty_sides() {
        // Two regions: region 0 has 4 theirs-lines but 0 ours-lines
        // (theirs-only insertion); region 1 has 2 lines everywhere.
        let starts = vec![[0usize, 0, 0], [4, 4, 0], [6, 6, 2]];

        // Halfway through region 0 in theirs…
        let (k, f) = region_at(&starts, 0, 2.0);
        assert_eq!(k, 0);
        assert!((f - 0.5).abs() < 1e-6);
        // …maps to ours PINNED at its region-0 boundary (empty side).
        assert_eq!(line_at(&starts, 2, k, f), 0.0);
        // …and base advances proportionally.
        assert!((line_at(&starts, 1, k, f) - 2.0).abs() < 1e-6);

        // A line inside region 1 of ours maps back to region 1 (the
        // zero-length region 0 is skipped for that column).
        let (k, f) = region_at(&starts, 2, 1.0);
        assert_eq!(k, 1);
        assert!((f - 0.5).abs() < 1e-6);
        assert!((line_at(&starts, 0, k, f) - 5.0).abs() < 1e-6);

        // Overscroll saturates at the last region, fraction clamped.
        let (k, f) = region_at(&starts, 0, 99.0);
        assert_eq!(k, 1);
        assert!((f - 1.0).abs() < 1e-6);
    }

    #[test]
    fn region_display_conflict_lifecycle() {
        use std::collections::HashSet;
        let mut state = conflict_state();
        let region = state.conflicts()[0];
        let none = HashSet::new();

        // Undecided conflict: open decision with both sides' provenance.
        match region_display(&state, region, &none, &HashMap::new()) {
            RegionDisplay::Deciding {
                current: None,
                focused: true,
                provenance: Some((ours, theirs, _)),
                ..
            } => {
                assert_eq!(ours, vec![SharedString::from("v = 2")]);
                assert_eq!(theirs, vec![SharedString::from("v = 3")]);
            }
            other => panic!("expected open conflict, got {other:?}"),
        }

        // Picked: collapsed decided state with the chosen side's lines.
        state.pick(region, Choice::Theirs);
        match region_display(&state, region, &none, &HashMap::new()) {
            RegionDisplay::Decided {
                choice: ChoiceKind::Theirs,
                focused: false,
                lines,
            } => assert_eq!(
                lines,
                vec![(
                    SharedString::from("v = 3"),
                    Some(Side::Theirs)
                )]
            ),
            other => panic!("expected decided, got {other:?}"),
        }

        // Revisiting reopens with the current choice marked.
        let mut revisiting = HashSet::new();
        revisiting.insert(region);
        match region_display(&state, region, &revisiting, &HashMap::new()) {
            RegionDisplay::Deciding {
                current: Some(ChoiceKind::Theirs),
                provenance: Some(_),
                ..
            } => {}
            other => panic!("expected revisit, got {other:?}"),
        }

        // Both: ours half then theirs half, each with its own provenance.
        state.pick(region, Choice::Both);
        match region_display(&state, region, &none, &HashMap::new()) {
            RegionDisplay::Decided {
                choice: ChoiceKind::Both,
                focused: false,
                lines,
            } => assert_eq!(
                lines,
                vec![
                    (
                        SharedString::from("v = 2"),
                        Some(Side::Ours)
                    ),
                    (
                        SharedString::from("v = 3"),
                        Some(Side::Theirs)
                    ),
                ]
            ),
            other => panic!("expected both, got {other:?}"),
        }
    }

    #[test]
    fn auto_region_shows_overridable_default() {
        use std::collections::HashSet;
        // ours-only edit: auto-resolved to disk, shown as an open (auto)
        // decision so the user can override it — the merge-editor-v2 fix.
        let state = MergeState::new(&inputs(Some("a\nb\n"), "a\nB\n", "a\nb\n"));
        let none = HashSet::new();
        let region_ix = state
            .doc
            .regions
            .iter()
            .position(|r| r.kind == RegionKind::OursOnly)
            .expect("ours-only region");
        match region_display(&state, region_ix, &none, &HashMap::new()) {
            RegionDisplay::Deciding {
                current: Some(ChoiceKind::Ours),
                focused: false,
                provenance: None,
                lines,
                ..
            } => assert_eq!(
                lines,
                vec![(
                    SharedString::from("B"),
                    Some(Side::Ours)
                )]
            ),
            other => panic!("expected auto decision, got {other:?}"),
        }
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
