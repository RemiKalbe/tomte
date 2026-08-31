//! Multi-pane merge editor (spec §7.3, approved mockup A; plan 7 Task 3).
//!
//! Full-window takeover routed via [`Route::Merge`]: origin (the dotfiles
//! remote's rendered source, only when it HAS its own version) / theirs
//! (chezmoi's rendered source state) / base (last-written snapshot) / ours
//! (disk) over a result pane, left to right as one provenance gradient. The
//! result IS a persistent [`tomte_ui::TextArea`]
//! (2026-08-22, Zed's conflict-view model): read-only by default so every
//! line is selectable, with the decision strips riding INSIDE it as blocks
//! above each changed region. "edit" flips the same editor editable in
//! place — no view swap; Apply line-diffs the text back onto the regions it
//! touched as `Choice::Edited` (one document-undo step).
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

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, FocusHandle, KeyBinding, Rgba,
    SharedString, Stateful, WeakEntity, Window, actions, div, prelude::*, px,
};
use tomte_app::merge_inputs::{self, MergeInputs};
use tomte_app::merge_state::MergeState;
use tomte_app::resolve::{ResolveError, ResolveOutcome};
use tomte_app::theme::Theme;
use tomte_core::merge::{Choice, MergeDocument, RegionKind};
use tomte_core::template::anchor::{SpanMap, SpanOrigin};
use tomte_ui::components as ui;

use super::review::{
    BannerTint, OutcomeBanner, centered_note, display_text, journal_path, message_box,
};
use super::{Route, Shell};

/// Which side of the merge document a pane line belongs to. Origin is NOT
/// one: it is not an input to the 3-way merge but a fourth text projected
/// onto its regions, so it has its own row/decor builders
/// ([`origin_pane_lines`], [`origin_pane_decors`]).
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
    /// Origin pane only: origin's text for this line's region differs from
    /// the local source's. `kind` describes ours/theirs/base and says
    /// nothing about the remote, so that is what the remote wash keys off
    /// (see [`origin_pane_decors`]). Always false for the other three panes.
    differs: bool,
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
                differs: false,
            });
        }
    }
    out
}

/// Flatten origin into pane rows, sliced by its projected per-region
/// boundaries (`bounds[k]..bounds[k + 1]` are region `k`'s origin lines) so
/// the fourth column shares the other three's region geometry. `texts` is
/// [`origin_region_texts`] for the same projection: a region with text is one
/// where origin differs from the local source, and that — not the merge's
/// region kind — is what the remote wash means in this pane.
fn origin_pane_lines(
    origin: &str,
    bounds: &[usize],
    texts: &[Option<String>],
    regions: &[tomte_core::merge::MergeRegion],
) -> Vec<PaneLine> {
    let lines = tomte_core::merge::split_lines(origin);
    let mut out = Vec::with_capacity(lines.len());
    for (ix, region) in regions.iter().enumerate() {
        let differs = texts.get(ix).is_some_and(Option::is_some);
        for line in &lines[bounds[ix]..bounds[ix + 1]] {
            out.push(PaneLine {
                text: display_text(line).to_owned().into(),
                kind: region.kind,
                protected: None,
                differs,
            });
        }
    }
    out
}

/// One input pane's precomputed content: its rows, and the decorations the
/// pane's read-only editor hands its decorator closure.
type PaneContent = (Rc<Vec<PaneLine>>, Rc<Vec<ui::text_area::LineDecor>>);

/// Whether a rendered span is protected from write-back: templated values
/// (`Action`), unanchored stretches (`Unmapped`), and repeated literals are
/// all rejections in `tomte_core::template::writeback`.
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
    let segments = template.and_then(|t| tomte_core::template::lexer::lex(t).ok());
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
        RedoChoice,
        ApplyEdit,
        CancelEdit
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
        // Region editor session (the root swaps its context to
        // MergeEditorEditing while one is open, so the single-letter picks
        // above can't shadow typing).
        KeyBinding::new("escape", CancelEdit, Some("RegionEditor")),
        KeyBinding::new("cmd-enter", ApplyEdit, Some("RegionEditor")),
    ]);
    cx.bind_keys(tomte_ui::components::text_area::bindings());
}

/// What one region renders as (merge-editor-v2 spec step 3). Pure and
/// testable; the render walk consumes it directly.
/// Both sides of a re-decidable region: (ours lines, theirs lines, theirs
/// region-local protected map).
type ProvenanceSides = (
    Vec<SharedString>,
    Vec<SharedString>,
    HashMap<usize, SharedString>,
);

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
        provenance: Option<ProvenanceSides>,
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
        Choice::Origin(_) => ui::ChoiceKind::Origin,
    }
}

fn side_lines(
    doc: &MergeDocument,
    region: &tomte_core::merge::MergeRegion,
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
    region: &tomte_core::merge::MergeRegion,
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
        // Origin reads the same as Edited here: free-form text, no side tag.
        // The strip label and result-row tint are where its provenance shows.
        Choice::Edited(text) | Choice::Origin(text) => text
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

/// One region's contribution to the editor seed: its CURRENT resolution
/// text (raw, `\n`-terminated lines — exactly what `assemble` would emit).
/// An undecided conflict seeds disk-then-source, the `Both` order, so the
/// user edits from all the material instead of a blank.
fn region_seed_text(state: &MergeState, idx: usize) -> String {
    let doc = &state.doc;
    let region = &doc.regions[idx];
    let ours = || doc.ours_lines()[region.ours.clone()].concat();
    let theirs = || doc.theirs_lines()[region.theirs.clone()].concat();
    let effective = state
        .resolution
        .get(idx)
        .cloned()
        .or_else(|| auto_choice(region.kind));
    match effective {
        Some(Choice::Edited(text) | Choice::Origin(text)) => text,
        Some(Choice::Ours) => ours(),
        Some(Choice::Theirs) => theirs(),
        Some(Choice::Base) => doc.base_lines()[region.base.clone()].concat(),
        Some(Choice::Both) => ours() + &theirs(),
        None => match region.kind {
            RegionKind::Conflict => ours() + &theirs(),
            // Unchanged (and anything else undecided): one copy, not both.
            _ => ours(),
        },
    }
}

/// The whole-document editor seed: every region's current text, in order.
/// Concatenated it IS the result as it currently stands (unresolved
/// conflicts inlined disk-then-source).
fn document_seed(state: &MergeState) -> Vec<String> {
    (0..state.doc.regions.len())
        .map(|idx| region_seed_text(state, idx))
        .collect()
}

/// Project seed-line boundary `b` into the edited text: shift by the net
/// line delta of every hunk strictly before it; a boundary inside a
/// replaced hunk clamps to the hunk's start (the replacement lands in the
/// later region — the concatenation is identical either way). An insertion
/// exactly AT a boundary belongs to the later region too.
fn map_line(b: usize, hunks: &[(std::ops::Range<usize>, std::ops::Range<usize>)]) -> usize {
    let mut delta = 0isize;
    for (before, after) in hunks {
        if before.end <= b && before.start < b {
            delta += after.len() as isize - before.len() as isize;
        } else if before.start >= b {
            break;
        } else {
            // before.start < b < before.end: inside the hunk.
            return after.start;
        }
    }
    (b as isize + delta).max(0) as usize
}

/// Project every region boundary of the seed into `edited` (identity when
/// the text is untouched, else through the line diff), with the sentinel
/// pinned to the end so appended lines belong to the last region.
fn mapped_boundaries(seed: &str, seed_line_starts: &[usize], edited: &str) -> Vec<usize> {
    if edited == seed {
        return seed_line_starts.to_vec();
    }
    let hunks = tomte_core::merge::line_hunks(seed, edited);
    let edited_len = tomte_core::merge::split_lines(edited).len();
    let mut mapped: Vec<usize> = seed_line_starts
        .iter()
        .map(|&b| map_line(b, &hunks).min(edited_len))
        .collect();
    if let Some(last) = mapped.last_mut() {
        *last = edited_len;
    }
    mapped
}

/// The editor's per-line decorations (Zed's conflict-view idiom: provenance
/// washes and rails live IN the buffer and follow it as it changes). Each
/// region shows its result-pane pattern while its text matches the seed;
/// the moment it diverges the whole region reads as "edited" (ok-green).
fn editor_decors(
    theme: Theme,
    seed: &str,
    seed_line_starts: &[usize],
    patterns: &[Vec<ui::text_area::LineDecor>],
    content: &str,
) -> Vec<ui::text_area::LineDecor> {
    let seed_lines = tomte_core::merge::split_lines(seed);
    let edited_lines = tomte_core::merge::split_lines(content);
    let mapped = mapped_boundaries(seed, seed_line_starts, content);
    let mut out = vec![ui::text_area::LineDecor::default(); edited_lines.len()];
    for region in 0..seed_line_starts.len().saturating_sub(1) {
        let old = seed_lines[seed_line_starts[region]..seed_line_starts[region + 1]].concat();
        let new = edited_lines[mapped[region]..mapped[region + 1]].concat();
        let untouched = old == new;
        for (k, row) in (mapped[region]..mapped[region + 1]).enumerate() {
            out[row] = if untouched {
                patterns
                    .get(region)
                    .and_then(|p| p.get(k))
                    .cloned()
                    .unwrap_or_default()
            } else {
                ui::text_area::LineDecor {
                    bg: Some(Theme::wash(theme.ok, 0.07)),
                    label: (k == 0).then(|| ("edited".into(), theme.ok)),
                    ..Default::default()
                }
            };
        }
    }
    out
}

/// One region's untouched decoration pattern, mirroring the result pane
/// exactly: muted context, tinted+railed provenance blocks for open
/// conflicts (with the protected `{}` gutter), side washes for decided and
/// auto regions with a pill naming the current state.
fn region_pattern(
    state: &MergeState,
    protected: &HashMap<usize, SharedString>,
    theme: Theme,
    idx: usize,
) -> Vec<ui::text_area::LineDecor> {
    use ui::text_area::LineDecor;
    let doc = &state.doc;
    let region = &doc.regions[idx];
    let explicit = state.resolution.get(idx).cloned();
    let auto = explicit.is_none();
    let effective = explicit.or_else(|| auto_choice(region.kind));
    match effective {
        None if region.kind == RegionKind::Conflict => {
            let mut rows = Vec::new();
            for k in 0..region.ours.len() {
                rows.push(LineDecor {
                    bg: Some(Theme::wash(theme.drift, 0.07)),
                    rail: Some(theme.drift),
                    label: (k == 0).then(|| ("on disk".into(), theme.drift)),
                    ..Default::default()
                });
            }
            for k in 0..region.theirs.len() {
                let prot = protected.contains_key(&(region.theirs.start + k));
                let tip = protected.get(&(region.theirs.start + k)).cloned();
                rows.push(LineDecor {
                    bg: Some(if prot {
                        Theme::wash(theme.drift, 0.15)
                    } else {
                        Theme::wash(theme.accent, 0.07)
                    }),
                    rail: Some(theme.accent),
                    gutter: prot.then(|| ("{}".into(), theme.drift)),
                    tooltip: tip,
                    label: (k == 0).then(|| ("source".into(), theme.accent)),
                    ..Default::default()
                });
            }
            rows
        }
        // Unchanged context: muted, undecorated — plain surroundings.
        None => vec![
            LineDecor {
                text_color: Some(theme.text_muted),
                ..Default::default()
            };
            region.base.len()
        ],
        Some(choice) => {
            let (label, tint): (SharedString, gpui::Rgba) = match &choice {
                Choice::Ours => ("disk".into(), theme.drift),
                Choice::Theirs => ("source".into(), theme.accent),
                Choice::Base => ("last written".into(), theme.text_muted),
                Choice::Both => ("both".into(), theme.ok),
                Choice::Edited(_) => ("edited".into(), theme.ok),
                Choice::Origin(_) => ("origin".into(), theme.remote),
            };
            let label: SharedString = if auto {
                format!("auto · {label}").into()
            } else {
                label
            };
            // Side-tagged lines wash by their side; untagged ones (edited,
            // origin) wash by the choice's own tint, so an origin region
            // does not read as a hand edit.
            choice_lines(doc, region, &choice)
                .into_iter()
                .enumerate()
                .map(|(k, (_, side))| LineDecor {
                    bg: Some(Theme::wash(
                        side.map(|s| s.tint(theme)).unwrap_or(tint),
                        0.07,
                    )),
                    label: (k == 0).then(|| (label.clone(), tint)),
                    ..Default::default()
                })
                .collect()
        }
    }
}

/// Map a free-form edit of the seeded document back onto regions: line-diff
/// seed → edited, project each region boundary through the hunks, and emit
/// an `Edited` pick for every region whose projected text changed. The
/// projected boundaries tile the edited text exactly, so assembling the
/// picks reproduces what the user saw byte for byte. Untouched regions keep
/// their state — in particular an untouched conflict stays undecided.
fn edited_region_picks(
    seed: &str,
    seed_line_starts: &[usize],
    edited: &str,
) -> Vec<(usize, Choice)> {
    let seed_lines = tomte_core::merge::split_lines(seed);
    let edited_lines = tomte_core::merge::split_lines(edited);
    let regions = seed_line_starts.len().saturating_sub(1);
    let mapped = mapped_boundaries(seed, seed_line_starts, edited);
    let mut picks = Vec::new();
    for region in 0..regions {
        let old = seed_lines[seed_line_starts[region]..seed_line_starts[region + 1]].concat();
        let new = edited_lines[mapped[region]..mapped[region + 1]].concat();
        if old != new {
            picks.push((region, Choice::Edited(new)));
        }
    }
    picks
}

/// Origin projected onto the merge's regions: ONE line-diff yielding both
/// halves, so the fourth pane's rows and the text its pick applies can never
/// come from different projections.
struct OriginProjection {
    /// Region `k`'s origin lines are `bounds[k]..bounds[k + 1]` — one entry
    /// per region plus the totals sentinel, i.e. `ORIGIN_COL`'s geometry.
    bounds: Vec<usize>,
    /// What each region's origin pick applies, or `None` where there is
    /// nothing origin can honestly contribute there (see below).
    texts: Vec<Option<String>>,
}

/// Project origin onto the regions. Same machinery as a free-text edit:
/// line-diff origin against the anchor — the source pane's text, whose
/// per-region line starts the merge already owns — map each region boundary
/// through the hunks, and slice.
///
/// The anchor is `theirs` (the local source), NOT the editor seed: origin is
/// the remote's version of that same source, so it is what origin has to be
/// compared against for "differs" to mean anything. Anchoring on the seed
/// would offer an origin button on every undecided conflict (whose seed is
/// ours+theirs) even where origin and the source are identical, and would
/// make the button vanish from the very strip that reads "origin" once the
/// pick landed.
///
/// A region's text is `None` when origin agrees with the source there — and
/// ALSO when the projected slice came out empty while the source has lines:
/// such a pick could only delete. That happens both when origin genuinely
/// dropped the lines and when its diff straddled a region boundary and
/// [`map_line`] clamped the content into the neighbour, and the two are
/// indistinguishable here. Either way a purple button that reads like every
/// other pick must not silently empty a region; `edit` still shows the
/// deletion in the text before anyone applies it.
fn origin_projection(anchor: &str, anchor_line_starts: &[usize], origin: &str) -> OriginProjection {
    let anchor_lines = tomte_core::merge::split_lines(anchor);
    let origin_lines = tomte_core::merge::split_lines(origin);
    let bounds = mapped_boundaries(anchor, anchor_line_starts, origin);
    let texts = (0..anchor_line_starts.len().saturating_sub(1))
        .map(|r| {
            let old = anchor_lines[anchor_line_starts[r]..anchor_line_starts[r + 1]].concat();
            let new = origin_lines[bounds[r]..bounds[r + 1]].concat();
            if new.is_empty() && !old.is_empty() {
                return None;
            }
            (old != new).then_some(new)
        })
        .collect();
    OriginProjection { bounds, texts }
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
    /// Origin's rows, projected onto the regions — `Some` exactly when this
    /// file HAS an origin version that differs (`inputs.origin`), which is
    /// what gates the fourth pane. A file added locally and never pushed has
    /// no origin blob at all, so rendering an empty pane for it would lie.
    origin_rows: Option<Rc<Vec<PaneLine>>>,
    /// Origin's text per region, `None` where origin matches the local
    /// source there (and empty when there is no origin at all): the origin
    /// pick's payload, and what washes the origin pane.
    origin_texts: Vec<Option<String>>,
    /// Theirs-pane line index → template hover text (protected spans).
    protected: Rc<HashMap<usize, SharedString>>,
    /// Per-region first-line index per column, indexed by `*_COL`, plus a
    /// totals sentinel row — the basis for region-aligned scroll sync. With
    /// no origin, `ORIGIN_COL` mirrors `OURS_COL` so the column stays
    /// well-defined for [`region_at`] / [`line_at`] (its pane is not drawn,
    /// and syncing its scroll handle is harmless).
    region_line_starts: Rc<Vec<[usize; PANE_COLS]>>,
    /// One read-only editor per input pane, indexed by geometry column — so
    /// `ORIGIN_COL` is the fourth entry and exists only when origin does.
    /// Read-only so their text is selectable and horizontally scrollable
    /// like the result (2026-08-26 feedback). Built lazily in render, which
    /// owns the theme.
    pane_editors: Option<Vec<Entity<ui::TextArea>>>,
    pane_appearance: Option<gpui::WindowAppearance>,
}

/// Per-line decorations for one input pane, in the same vocabulary the
/// result editor uses: the region wash for that side, the amber
/// protected-template underlay, and the `{}` marker.
fn pane_decors(rows: &[PaneLine], side: PaneSide, theme: Theme) -> Vec<ui::text_area::LineDecor> {
    rows.iter()
        .map(|line| {
            // Protected spans get the amber underlay regardless of region
            // kind — the write-back rejection is the stronger fact.
            let bg = if line.protected.is_some() {
                Some(Theme::wash(theme.drift, 0.15))
            } else {
                row_bg(line.kind, side, &theme)
            };
            ui::text_area::LineDecor {
                bg,
                // Text darkens only when THIS pane is washed, i.e. this side
                // actually changed (2026-08-08: a side with no change
                // deserves no emphasis at all).
                text_color: Some(if bg.is_some() {
                    theme.text
                } else {
                    theme.text_muted
                }),
                gutter: line.protected.is_some().then(|| ("{}".into(), theme.drift)),
                ..Default::default()
            }
        })
        .collect()
}

/// Per-line decorations for the origin pane. The merge's region kinds are
/// statements about ours/theirs/base, so [`row_bg`] has nothing to say here:
/// the remote wash marks exactly the regions where origin's text differs
/// from the local source's, and nothing else is emphasized. Origin carries
/// no span map, so no `{}` gutter either — claiming protection we never
/// computed would be a lie.
fn origin_pane_decors(rows: &[PaneLine], theme: Theme) -> Vec<ui::text_area::LineDecor> {
    rows.iter()
        .map(|line| {
            let bg = line.differs.then(|| Theme::wash(theme.remote, 0.16));
            ui::text_area::LineDecor {
                bg,
                text_color: Some(if bg.is_some() {
                    theme.text
                } else {
                    theme.text_muted
                }),
                ..Default::default()
            }
        })
        .collect()
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
        let mut acc = [0usize; PANE_COLS];
        for region in &state.doc.regions {
            region_line_starts.push(acc);
            acc[THEIRS_COL] += region.theirs.len();
            acc[BASE_COL] += region.base.len();
            acc[OURS_COL] += region.ours.len();
        }
        region_line_starts.push(acc);
        // Origin is projected onto the regions through the source pane it is
        // the remote's copy of: one line-diff gives both the fourth column's
        // geometry and the per-region text the origin pick applies.
        let theirs_text = state.doc.theirs_lines().concat();
        let theirs_starts: Vec<usize> = region_line_starts.iter().map(|s| s[THEIRS_COL]).collect();
        let (origin_rows, origin_texts) = match inputs.origin.as_deref() {
            Some(origin) => {
                let projected = origin_projection(&theirs_text, &theirs_starts, origin);
                let rows = origin_pane_lines(
                    origin,
                    &projected.bounds,
                    &projected.texts,
                    &state.doc.regions,
                );
                for (starts, &b) in region_line_starts.iter_mut().zip(projected.bounds.iter()) {
                    starts[ORIGIN_COL] = b;
                }
                (Some(Rc::new(rows)), projected.texts)
            }
            None => {
                // No fourth pane: mirror the disk column so no region is
                // zero-length in a way the scroll mapping never expects.
                for starts in region_line_starts.iter_mut() {
                    starts[ORIGIN_COL] = starts[OURS_COL];
                }
                (None, Vec::new())
            }
        };
        Self {
            region_line_starts: Rc::new(region_line_starts),
            theirs_rows: Rc::new(pane_lines(&state.doc, PaneSide::Theirs, &protected)),
            base_rows: Rc::new(pane_lines(&state.doc, PaneSide::Base, &none)),
            ours_rows: Rc::new(pane_lines(&state.doc, PaneSide::Ours, &none)),
            origin_rows,
            origin_texts,
            protected: Rc::new(protected),
            pane_editors: None,
            pane_appearance: None,
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
    /// Repo-reconcile mode: the inputs are a mid-merge source conflict;
    /// Save writes the SOURCE file + `git add` (no chezmoi apply), Cancel
    /// aborts the whole merge via the Shell.
    pub(super) repo_reconcile: bool,
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
    /// One scroll handle per input pane column (see the `*_COL` constants);
    /// wheel on any pane mirrors its offset to the others so the columns
    /// compare without independent scrolling. The origin handle exists even
    /// when its pane is not drawn — syncing it is harmless.
    pane_scrolls: [gpui::ScrollHandle; PANE_COLS],
    /// Decided regions whose strip the user reopened (non-destructive).
    revisiting: std::collections::HashSet<usize>,
    /// The result IS the editor (2026-08-22): one persistent `TextArea`,
    /// read-only by default so every line is selectable and the decision
    /// strips ride inside it as blocks; `edit` flips it editable in place.
    /// Created lazily (needs a `Window` for nothing, but `cx.new`), re-seeded
    /// from the resolution on every pick/undo.
    result_editor: Option<Entity<ui::TextArea>>,
    /// Whether the result editor is currently editable (vs read-only).
    editing: bool,
    /// The text the editor was last seeded with (== the assembled result of
    /// the current resolution) and per-region first-line indices into it —
    /// the basis for mapping a free edit back onto regions.
    seed: String,
    seed_line_starts: Vec<usize>,
    /// The decorator must be (re)installed on the editor: set on seed, mode
    /// toggle, or appearance change. Render owns installation (it has the
    /// window appearance).
    decor_dirty: bool,
    decor_appearance: Option<gpui::WindowAppearance>,
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    /// Scrolls the result editor (Next ↓ jumps it to a region).
    editor_scroll: gpui::ScrollHandle,
    /// Draggable layout: px width per input pane column (indexed by the
    /// `*_COL` constants — the last-rendered column flexes and ignores its
    /// entry) and the height of the panes row. `None` until the first render
    /// sizes them from the viewport.
    pane_widths: Option<[f32; PANE_COLS]>,
    /// How many panes `pane_widths` was sized for. A load that adds or drops
    /// the origin pane re-divides the row rather than leaving the flexing
    /// column squeezed to nothing.
    pane_cols: usize,
    panes_height: Option<f32>,
    /// The divider currently under the pointer's button, if any.
    dragging: Option<ui::DragState<Divider>>,
    /// Last known body height/width, captured at render — the drag handlers
    /// run outside render and still need to clamp against the window.
    viewport_h: f32,
    viewport_w: f32,
    focus_handle: FocusHandle,
}

/// One document-level undo step: choice changes applied together (a strip
/// pick is one; an editor Apply may touch several regions), with the cursor
/// restored so undo never "jumps" (merge-editor-v2 spec).
struct UndoEntry {
    /// `(region, before, after)` in application order.
    changes: Vec<(usize, Option<Choice>, Choice)>,
    prev_cursor: Option<usize>,
}

impl MergeView {
    pub fn new(shell: WeakEntity<Shell>, cx: &mut Context<Self>) -> Self {
        Self {
            shell,
            target: None,
            repo_reconcile: false,
            loading: false,
            error: None,
            loaded: None,
            saving: false,
            banner: None,
            pane_scrolls: std::array::from_fn(|_| gpui::ScrollHandle::new()),
            revisiting: std::collections::HashSet::new(),
            result_editor: None,
            editing: false,
            seed: String::new(),
            seed_line_starts: Vec::new(),
            decor_dirty: true,
            decor_appearance: None,
            undo: Vec::new(),
            redo: Vec::new(),
            editor_scroll: gpui::ScrollHandle::new(),
            pane_widths: None,
            pane_cols: 0,
            panes_height: None,
            dragging: None,
            viewport_h: 0.,
            viewport_w: 0.,
            focus_handle: cx.focus_handle(),
        }
    }

    /// (Re)load the merge inputs for `target` on the background executor.
    /// Reopening the editor is always a fresh load — earlier choices for the
    /// same file do not survive (the inputs may have changed underneath).
    /// Present prepared repo-conflict inputs (reconcile flow): no
    /// background load — the engine already read the merge stages.
    pub fn present_repo_conflict(
        &mut self,
        inputs: std::sync::Arc<MergeInputs>,
        cx: &mut Context<Self>,
    ) {
        self.target = Some(inputs.target.clone());
        self.repo_reconcile = true;
        self.banner = None;
        self.saving = false;
        self.editing = false;
        self.loaded = Some(LoadedMerge::new(inputs));
        self.seed_editor(cx);
        cx.notify();
    }

    pub fn load(&mut self, target: PathBuf, cx: &mut Context<Self>) {
        self.repo_reconcile = false;
        self.target = Some(target.clone());
        self.loading = true;
        self.error = None;
        self.loaded = None;
        self.saving = false;
        self.banner = None;
        self.editing = false;
        self.result_editor = None;
        cx.notify();

        let journal = journal_path();
        cx.spawn(async move |this, cx| {
            let result = {
                let target = target.clone();
                cx.background_executor()
                    .spawn(async move {
                        let client = crate::app_chezmoi_client();
                        let git = crate::app_git_client(&client)?;
                        // Same fallback as `review.rs`'s `auto_merge_blocking`
                        // and the daemon's own derivation — an unreadable
                        // branch degrades to "main" rather than failing.
                        let branch = git.head_branch().unwrap_or_else(|_| "main".into());
                        let remote_ref = format!("origin/{branch}");
                        merge_inputs::load(&client, &git, &remote_ref, &journal, &target)
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
                view.seed_editor(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// (Re)seed the result editor from the current resolution: the editor's
    /// text becomes the assembled result (unresolved conflicts inlined
    /// disk-then-source), and `seed`/`seed_line_starts` record the per-region
    /// line geometry for mapping a free edit back onto regions. Creates the
    /// editor entity on first use; marks the decorator for reinstall.
    fn seed_editor(&mut self, cx: &mut Context<Self>) {
        let Some(loaded) = &self.loaded else {
            self.result_editor = None;
            self.seed = String::new();
            self.seed_line_starts = Vec::new();
            return;
        };
        let texts = document_seed(&loaded.state);
        let mut seed = String::new();
        let mut starts = Vec::with_capacity(texts.len() + 1);
        let mut line = 0usize;
        for text in &texts {
            starts.push(line);
            line += text.split_inclusive('\n').count();
            seed.push_str(text);
        }
        starts.push(line);
        self.seed = seed.clone();
        self.seed_line_starts = starts;
        let editing = self.editing;
        match &self.result_editor {
            Some(editor) => editor.update(cx, |area, _| {
                area.set_read_only(!editing);
                area.set_content(seed);
            }),
            None => {
                let editor = cx.new(|cx| {
                    let mut area = ui::TextArea::read_only(seed, cx);
                    // Text column lines up with the input panes' rows (px_2 +
                    // w_4 gutter + gap_1 = 28px); rail/gutter render inside it.
                    area.set_left_inset(px(28.));
                    area
                });
                self.result_editor = Some(editor);
            }
        }
        self.decor_dirty = true;
    }

    /// Row (in editor line-height units) of region `region`'s first text
    /// line, accounting for the decision-strip block above each changed
    /// region at or before it. Used by Next ↓ to scroll the editor.
    /// Number of decision-strip blocks that sit above text line `line`.
    /// A strip is placed above its region's FIRST line, so a strip counts as
    /// soon as its region starts at or before `line`. Strips only exist in
    /// read-only mode.
    fn strips_above(&self, line: usize) -> usize {
        let Some(loaded) = &self.loaded else {
            return 0;
        };
        let changed: Vec<bool> = (0..loaded.state.doc.regions.len())
            .map(|idx| region_has_strip(&loaded.state, idx))
            .collect();
        strips_above_line(line, &self.seed_line_starts, &changed)
    }

    /// The editor row that text line `line` renders on. `with_strips` is the
    /// read-only geometry (each changed region carries a two-row strip above
    /// its first line); editing has no blocks, so row == line.
    fn row_of_line(&self, line: usize, with_strips: bool) -> usize {
        if with_strips {
            line + self.strips_above(line) * STRIP_ROWS
        } else {
            line
        }
    }

    /// First row of `region` in the CURRENT mode's geometry.
    fn region_row(&self, region: usize) -> usize {
        let line = self.seed_line_starts.get(region).copied().unwrap_or(0);
        self.row_of_line(line, !self.editing)
    }

    /// Apply an in-flight divider drag. Called from the root's mouse-move,
    /// because a drag that leaves the 7px strip must keep tracking.
    fn drag_divider(&mut self, event: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.dragging else {
            return;
        };
        match drag.id {
            Divider::Pane(i) => {
                let delta = drag.delta(event, ui::ResizeAxis::X);
                if let Some(widths) = &mut self.pane_widths {
                    widths[i] = (drag.start + delta).max(MIN_PANE_W);
                }
            }
            Divider::Result => {
                let delta = drag.delta(event, ui::ResizeAxis::Y);
                let max = (self.viewport_h - MIN_RESULT_H).max(MIN_PANES_H);
                self.panes_height = Some((drag.start + delta).clamp(MIN_PANES_H, max));
            }
        }
        cx.notify();
    }

    /// Toggling edit mode adds/removes every strip block, which moves the
    /// text under the viewport. Shift the scroll by exactly the row delta at
    /// `anchor_line` so that line stays put on screen — otherwise clicking
    /// `edit` visibly jumps the document (2026-08-25 report: "the editor is
    /// not at all on the same line as you were before").
    fn rescroll_anchored(&mut self, anchor_line: usize, was_editing: bool) {
        let old_row = self.row_of_line(anchor_line, !was_editing) as f32;
        let new_row = self.row_of_line(anchor_line, !self.editing) as f32;
        let y = f32::from(self.editor_scroll.offset().y) - (new_row - old_row) * PANE_ROW_H;
        self.editor_scroll
            .set_offset(gpui::point(px(0.), px(y.min(0.))));
    }

    /// The single mutation funnel (merge-editor-v2 spec): record the undo
    /// entry, apply the choice, close any revisit, advance the cursor.
    /// Every pick path — click, keybinding, redo — lands here.
    fn apply(&mut self, region: usize, choice: Choice, cx: &mut Context<Self>) {
        self.apply_many(vec![(region, choice)], cx);
    }

    /// Apply several picks as ONE undo step (an editor Apply can touch
    /// many regions; a strip pick is the one-element case).
    fn apply_many(&mut self, picks: Vec<(usize, Choice)>, cx: &mut Context<Self>) {
        let Some(loaded) = &mut self.loaded else {
            return;
        };
        if picks.is_empty() {
            return;
        }
        let prev_cursor = loaded.state.cursor;
        let mut changes = Vec::with_capacity(picks.len());
        for (region, choice) in picks {
            let prev = loaded.state.resolution.get(region).cloned();
            loaded.state.pick(region, choice.clone());
            self.revisiting.remove(&region);
            changes.push((region, prev, choice));
        }
        self.undo.push(UndoEntry {
            changes,
            prev_cursor,
        });
        self.redo.clear();
        self.seed_editor(cx);
        cx.notify();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.undo.pop() else {
            return;
        };
        let Some(loaded) = &mut self.loaded else {
            return;
        };
        for (region, prev, _next) in entry.changes.iter().rev() {
            match prev {
                Some(choice) => loaded.state.resolution.set(*region, choice.clone()),
                None => loaded.state.resolution.unset(*region),
            }
        }
        loaded.state.cursor = entry.prev_cursor;
        self.redo.push(entry);
        self.seed_editor(cx);
        cx.notify();
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.redo.pop() else {
            return;
        };
        let Some(loaded) = &mut self.loaded else {
            return;
        };
        for (region, _prev, next) in &entry.changes {
            loaded.state.pick(*region, next.clone());
        }
        self.undo.push(entry);
        self.seed_editor(cx);
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
        self.decor_dirty = true;
        cx.notify();
    }

    /// Flip the result editor editable, cursor and viewport at `region`
    /// (the strip the user clicked, or the header Edit button's target). The
    /// strips vanish (edit is free-form); Apply/Cancel land in the header.
    pub(super) fn start_edit(&mut self, region: usize, cx: &mut Context<Self>) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        if region >= loaded.state.doc.regions.len() {
            return;
        }
        self.editing = true;
        let cursor_byte = self.seed_line_starts.get(region).map_or(0, |&line| {
            self.seed
                .split_inclusive('\n')
                .take(line)
                .map(|l| l.len())
                .sum()
        });
        if let Some(editor) = &self.result_editor {
            editor.update(cx, |area, _| {
                area.set_read_only(false);
                area.place_cursor(cursor_byte);
            });
        }
        // Keep the clicked region exactly where it is on screen while its
        // strip (and every other) disappears.
        let anchor = self.seed_line_starts.get(region).copied().unwrap_or(0);
        self.rescroll_anchored(anchor, false);
        self.decor_dirty = true;
        cx.notify();
    }

    /// Apply the edit: map the edited document back onto regions (line-diff
    /// against the seed) and land every changed region as `Choice::Edited` —
    /// one document-undo entry — then return to read-only.
    fn commit_edit(&mut self, cx: &mut Context<Self>) {
        if !self.editing {
            return;
        }
        let edited = self
            .result_editor
            .as_ref()
            .map(|e| e.read(cx).text().to_owned())
            .unwrap_or_default();
        let picks = edited_region_picks(&self.seed, &self.seed_line_starts, &edited);
        self.editing = false;
        let anchor = self
            .loaded
            .as_ref()
            .and_then(|l| l.state.cursor)
            .and_then(|r| self.seed_line_starts.get(r).copied())
            .unwrap_or(0);
        self.rescroll_anchored(anchor, true);
        if picks.is_empty() {
            // Nothing changed — same as cancel: re-seed read-only.
            self.seed_editor(cx);
            cx.notify();
            return;
        }
        self.apply_many(picks, cx);
    }

    /// Discard the edit; every region's prior resolution stands.
    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if !self.editing {
            return;
        }
        self.editing = false;
        // The strips come back: anchor on the region the user was working in
        // so the document doesn't jump the other way either.
        let anchor = self
            .loaded
            .as_ref()
            .and_then(|l| l.state.cursor)
            .and_then(|r| self.seed_line_starts.get(r).copied())
            .unwrap_or(0);
        self.rescroll_anchored(anchor, true);
        self.seed_editor(cx);
        cx.notify();
    }

    /// Pick at the cursor (keybindings route here).
    fn pick_at_cursor(&mut self, choice: Choice, cx: &mut Context<Self>) {
        let Some(region) = self.loaded.as_ref().and_then(|l| l.state.cursor) else {
            return;
        };
        self.apply(region, choice, cx);
    }

    /// Region-aligned scroll sync across the read-only input panes: `leader`
    /// is a column (`0..PANE_COLS`, origin included whether or not its pane
    /// is drawn) or [`RESULT_LEADER`]. Maps the leader's viewport-top to
    /// (region, fraction) and positions every other column at ITS version of
    /// that point. Pure offset math over the cached analytic geometry — no
    /// notify, no layout reads: the wheel's own refresh paints the new
    /// offsets in the same frame (2026-08-08: notifying per wheel tick was
    /// part of the lag). The result editor scrolls independently (blocks
    /// make its rows non-uniform with the panes).
    fn sync_from(&mut self, leader: usize) {
        // Perf bisection switch: TOMTE_NOSYNC=1 disables follower updates so
        // frame-rate cost can be attributed (tree vs per-event sync work).
        if std::env::var_os("TOMTE_NOSYNC").is_some() {
            return;
        }
        let Some(loaded) = &self.loaded else {
            return;
        };
        let starts = &loaded.region_line_starts;
        // The result editor's per-region geometry must line up with the
        // panes' or the mapping is meaningless.
        if starts.len() < 2 || self.seed_line_starts.len() != starts.len() {
            return;
        }

        // Where is the leader looking, as (region, fraction through it)?
        let (k, f) = if leader == RESULT_LEADER {
            let row = -f32::from(self.editor_scroll.offset().y) / PANE_ROW_H;
            self.result_region_at_row(row)
        } else {
            let line = -f32::from(self.pane_scrolls[leader].offset().y) / PANE_ROW_H;
            region_at(starts, leader, line)
        };

        for col in 0..PANE_COLS {
            if leader == col {
                continue;
            }
            let y = gpui::px(-(line_at(starts, col, k, f) * PANE_ROW_H));
            let mut offset = self.pane_scrolls[col].offset();
            offset.y = y;
            self.pane_scrolls[col].set_offset(offset);
        }
        if leader != RESULT_LEADER {
            // Follow into the result editor's OWN geometry — its rows carry
            // the decision-strip blocks, so it is not line-for-line with the
            // panes (that mismatch is why sync had to be rebuilt after the
            // 2026-08-22 editor rework).
            let row = self.result_row_for(k, f);
            self.editor_scroll
                .set_offset(gpui::point(px(0.), px(-(row * PANE_ROW_H))));
        }
    }

    /// Which region the result editor's `row` sits in, and how far through
    /// it. A row inside a region's strip block reads as the top of that
    /// region (fraction 0) — the strip belongs to the region below it.
    fn result_region_at_row(&self, row: f32) -> (usize, f32) {
        let regions = self.seed_line_starts.len().saturating_sub(1);
        if regions == 0 {
            return (0, 0.0);
        }
        let with_strips = !self.editing;
        let mut k = regions - 1;
        for r in 0..regions {
            let start = self.row_of_line(self.seed_line_starts[r], with_strips) as f32;
            let len = (self.seed_line_starts[r + 1] - self.seed_line_starts[r]) as f32;
            if row < start + len {
                k = r;
                break;
            }
        }
        let start = self.row_of_line(self.seed_line_starts[k], with_strips) as f32;
        let len = (self.seed_line_starts[k + 1] - self.seed_line_starts[k]) as f32;
        let f = if len > 0.0 {
            ((row - start) / len).clamp(0.0, 1.0)
        } else {
            0.0
        };
        (k, f)
    }

    /// The inverse: the result editor row for `fraction` through region `k`.
    fn result_row_for(&self, k: usize, f: f32) -> f32 {
        let Some(&start_line) = self.seed_line_starts.get(k) else {
            return 0.0;
        };
        let len = self
            .seed_line_starts
            .get(k + 1)
            .map(|&e| (e - start_line) as f32)
            .unwrap_or(0.0);
        self.row_of_line(start_line, !self.editing) as f32 + f * len
    }

    /// `Next ↓`: advance the cursor to the next unresolved conflict — or,
    /// once everything is resolved, to the next changed region, so every
    /// decision point stays reachable for revisiting (2026-08-08 feedback).
    /// Target selection lives in [`next_target`] (pure, tested).
    fn next(&mut self, cx: &mut Context<Self>) {
        let region = {
            let Some(loaded) = &mut self.loaded else {
                return;
            };
            next_target(&mut loaded.state)
        };
        if let Some(region) = region {
            let y = self.region_row(region).saturating_sub(1) as f32 * PANE_ROW_H;
            self.editor_scroll.set_offset(gpui::point(px(0.), px(-y)));
            self.decor_dirty = true;
        }
        cx.notify();
    }

    /// Cancel: back to Review, discarding in-editor choices.
    fn cancel(&self, cx: &mut Context<Self>) {
        if self.repo_reconcile {
            // Bailing out of ONE conflict bails the whole merge — a
            // half-resolved merge state would be worse than none.
            self.shell
                .update(cx, |shell, cx| shell.cancel_reconcile(cx))
                .ok();
            return;
        }
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
        if self.repo_reconcile {
            // Reconcile flow: write the SOURCE file + git add; the Shell
            // advances to the next conflict (or concludes the merge). The
            // Shell hand-off happens OUTSIDE this view's update — advancing
            // re-presents this same MergeView entity, and nesting that
            // panicked gpui ("already being updated", 2026-08-18 crash).
            let source_path = inputs.source_path.clone();
            let shell = self.shell.clone();
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { engine.resolve_repo_conflict(&source_path, &resolved) })
                    .await;
                let advance = this
                    .update(cx, |view, cx| {
                        view.saving = false;
                        let ok = result.is_ok();
                        if let Err(e) = result {
                            view.banner = Some(OutcomeBanner {
                                text: format!("could not record the resolution: {e}").into(),
                                tint: BannerTint::Conflict,
                                undoable: false,
                            });
                        }
                        cx.notify();
                        ok
                    })
                    .unwrap_or(false);
                if advance {
                    let _ = cx.update(|cx| {
                        shell
                            .update(cx, |shell, cx| shell.reconcile_conflict_saved(cx))
                            .ok();
                    });
                }
            })
            .detach();
            return;
        }
        let resolved_target = inputs.target.clone();
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
                        .update(cx, |shell, cx| {
                            shell.state.update(cx, |model, cx| {
                                model.confirm_resolved(&resolved_target);
                                cx.notify();
                            });
                            shell.merge_done(banner, cx);
                        })
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
            bar = bar.child(
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
                                "auto-resolved · {changed} changed region{} — review below",
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
            if changed && !self.editing {
                // Conflict-red while decisions are owed; neutral afterwards
                // (it then walks decided/auto regions for revisiting).
                let tint = if open > 0 {
                    theme.conflict
                } else {
                    theme.text_muted
                };
                bar = bar.child(ui::button(
                    theme,
                    "merge-next",
                    "Next ↓".into(),
                    ui::ButtonVariant::Tonal(tint),
                    ui::ButtonSize::Sm,
                    cx.listener(|view, _ev, _window, cx| view.next(cx)),
                ));
            }
            if self.editing {
                // Editing: apply/cancel the free edit (⌘↵ / esc mirror these).
                bar = bar.child(ui::button(
                    theme,
                    "merge-edit-apply",
                    "Apply edit".into(),
                    ui::ButtonVariant::Tonal(theme.accent),
                    ui::ButtonSize::Sm,
                    cx.listener(|view, _ev, _window, cx| view.commit_edit(cx)),
                ));
                bar = bar.child(ui::button(
                    theme,
                    "merge-edit-cancel",
                    "Discard edit".into(),
                    ui::ButtonVariant::Ghost,
                    ui::ButtonSize::Sm,
                    cx.listener(|view, _ev, _window, cx| view.cancel_edit(cx)),
                ));
            } else {
                // Whole-file editing entry point that doesn't depend on any
                // strip: opens at the focused region (or the top).
                bar = bar.child(ui::button(
                    theme,
                    "merge-edit",
                    "Edit".into(),
                    ui::ButtonVariant::Tonal(theme.text_muted),
                    ui::ButtonSize::Sm,
                    cx.listener(|view, _ev, _window, cx| {
                        let region = view
                            .loaded
                            .as_ref()
                            .and_then(|l| l.state.cursor)
                            .unwrap_or(0);
                        view.start_edit(region, cx);
                    }),
                ));
            }
        }
        bar = bar.child(ui::button(
            theme,
            "merge-cancel",
            "Cancel".into(),
            ui::ButtonVariant::Tonal(theme.text_muted),
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
                let editing = self.editing;
                self.save_button(
                    loaded.state.assembled().is_some() && !editing,
                    unresolved,
                    editing,
                    theme,
                    cx,
                )
                .into_any_element()
            });
        }
        bar
    }

    /// Save is enabled exactly when the document is fully resolved, no save
    /// is in flight, and no edit session is pending (an open editor holds
    /// text the resolution doesn't have yet — saving under it would lie).
    fn save_button(
        &self,
        enabled: bool,
        unresolved: usize,
        editing: bool,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        if enabled {
            ui::button(
                theme,
                "merge-save",
                "Save".into(),
                ui::ButtonVariant::Tonal(theme.accent),
                ui::ButtonSize::Sm,
                cx.listener(|view, _ev, _window, cx| view.save(cx)),
            )
        } else {
            let reason = if editing {
                "apply or cancel the edit first".to_string()
            } else {
                let s = if unresolved == 1 { "" } else { "s" };
                format!("{unresolved} conflict{s} left")
            };
            ui::disabled_button(
                theme,
                "merge-save",
                "Save".into(),
                ui::ButtonSize::Sm,
                Some(reason.into()),
            )
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
        let view = cx.weak_entity();
        let widths = self.pane_widths.unwrap_or([0.; PANE_COLS]);
        let dragging = self.dragging.map(|d| d.id);
        // The vertical handle on column `i`'s trailing edge.
        let pane_divider = |i: usize| {
            let view = view.clone();
            ui::resize_handle(
                ElementId::named_usize("merge-pane-divider", i),
                ui::ResizeAxis::X,
                dragging == Some(Divider::Pane(i)),
                theme,
                move |event, _window, cx| {
                    let event = event.clone();
                    view.update(cx, |merge, cx| {
                        let start = merge.pane_widths.map(|w| w[i]).unwrap_or(0.);
                        merge.dragging = Some(ui::DragState::new(Divider::Pane(i), &event, start));
                        cx.notify();
                    })
                    .ok();
                },
            )
        };
        let editor = |col: usize| loaded.pane_editors.as_ref().and_then(|e| e.get(col));
        let has_origin = loaded.origin_rows.is_some();
        // One shared grid of hairlines (2026-08-08 feedback): the input
        // columns split by vertical dividers, one horizontal rule, then the
        // result flowing as the pane's own content — no floating boxes. Every
        // one of those rules is now a grab handle (2026-08-27).
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(self.panes_height.unwrap_or(0.)))
                    .flex_none()
                    .flex()
                    // Origin leads: remote → this machine's source → last
                    // written → disk reads as one provenance gradient. It is
                    // gated on CONTENT, never on the scan's repo-wide
                    // `origin_differs` hint — a file added locally and never
                    // pushed differs yet HAS no origin version, and an empty
                    // fourth pane would be a lie.
                    .when(has_origin, |el| {
                        el.child(pane_col(
                            "source · origin",
                            theme.remote,
                            editor(ORIGIN_COL),
                            Some(widths[ORIGIN_COL]),
                            ORIGIN_COL,
                            &self.pane_scrolls,
                            &view,
                            theme,
                        ))
                        .child(pane_divider(ORIGIN_COL))
                    })
                    .child(pane_col(
                        // With origin beside it, "source" alone no longer
                        // says WHICH source.
                        if has_origin {
                            "source · this machine"
                        } else {
                            "source (rendered)"
                        },
                        theme.accent,
                        editor(THEIRS_COL),
                        Some(widths[THEIRS_COL]),
                        THEIRS_COL,
                        &self.pane_scrolls,
                        &view,
                        theme,
                    ))
                    .child(pane_divider(THEIRS_COL))
                    .child(if degraded {
                        degraded_base_col(theme, widths[BASE_COL])
                    } else {
                        pane_col(
                            "last written",
                            theme.text_muted,
                            editor(BASE_COL),
                            Some(widths[BASE_COL]),
                            BASE_COL,
                            &self.pane_scrolls,
                            &view,
                            theme,
                        )
                    })
                    .child(pane_divider(BASE_COL))
                    .child(pane_col(
                        "on disk",
                        theme.drift,
                        editor(OURS_COL),
                        None,
                        OURS_COL,
                        &self.pane_scrolls,
                        &view,
                        theme,
                    )),
            )
            .child({
                let view = view.clone();
                ui::resize_handle(
                    "merge-result-divider",
                    ui::ResizeAxis::Y,
                    dragging == Some(Divider::Result),
                    theme,
                    move |event, _window, cx| {
                        let event = event.clone();
                        view.update(cx, |merge, cx| {
                            let start = merge.panes_height.unwrap_or(0.);
                            merge.dragging =
                                Some(ui::DragState::new(Divider::Result, &event, start));
                            cx.notify();
                        })
                        .ok();
                    },
                )
            })
            .child(self.result_pane(theme, &view))
            .into_any_element()
    }

    /// The result pane: the persistent editor (read-only default, editable
    /// while `editing`) inside one scroll container. The decision strips
    /// live INSIDE it as blocks — no separate view, so the text is always
    /// selectable and switching to edit never swaps the surface.
    fn result_pane(&self, theme: Theme, view: &WeakEntity<MergeView>) -> AnyElement {
        let container = div()
            .id("merge-result-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            // Long lines run past the pane; let them be reached.
            .overflow_x_scroll()
            .track_scroll(&self.editor_scroll)
            .py_1()
            // The editor shapes its glyphs from the inherited text style —
            // same Menlo/11px as the input panes' `mono_line` rows, so the
            // result reads as code, not UI text (2026-08-22 regression).
            .font_family("Menlo")
            .text_xs()
            .line_height(px(PANE_ROW_H));
        // The wheel reaches this wrapper AFTER the inner scroll container has
        // applied it, so the leader's fresh offset drives the mapping.
        let container = {
            let view = view.clone();
            container.on_scroll_wheel(move |_event, _window, cx| {
                view.update(cx, |merge, _cx| merge.sync_from(RESULT_LEADER))
                    .ok();
            })
        };
        match &self.result_editor {
            Some(editor) => container.child(editor.clone()).into_any_element(),
            None => container
                .child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child("preparing result…"),
                )
                .into_any_element(),
        }
    }

    /// Test probe for scroll sync: park `leader` at the first line of region
    /// `region`, run the sync, and report where every column landed next to
    /// where it should have. A real wheel event needs a window and layout;
    /// the mapping this drives is the part that breaks.
    #[cfg(test)]
    pub(super) fn sync_probe(&mut self, leader: usize, region: usize) -> SyncProbe {
        let starts = self
            .loaded
            .as_ref()
            .expect("probe needs loaded inputs")
            .region_line_starts
            .clone();
        // The result editor's rows carry the strip blocks, so its own park
        // position is a ROW, not a line index.
        if leader == RESULT_LEADER {
            let row = self.region_row(region) as f32;
            self.editor_scroll
                .set_offset(gpui::point(px(0.), px(-row * PANE_ROW_H)));
        } else {
            let line = starts[region][leader] as f32;
            self.pane_scrolls[leader].set_offset(gpui::point(px(0.), px(-line * PANE_ROW_H)));
        }
        self.sync_from(leader);
        SyncProbe {
            panes: std::array::from_fn(|c| {
                -f32::from(self.pane_scrolls[c].offset().y) / PANE_ROW_H
            }),
            want_panes: std::array::from_fn(|c| starts[region][c] as f32),
            result: -f32::from(self.editor_scroll.offset().y) / PANE_ROW_H,
            want_result: self.region_row(region) as f32,
        }
    }

    /// Build and install the result editor's decorator: per-line provenance
    /// (via [`editor_decors`], which flips edited regions to the ok wash) and
    /// — in read-only mode only — a decision-strip block above every changed
    /// region. Rebuilt on every resolution/mode/appearance change so the
    /// captured strip states stay current.
    fn install_result_decorator(&mut self, theme: Theme, cx: &mut Context<Self>) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let Some(editor) = &self.result_editor else {
            return;
        };
        let n = loaded.state.doc.regions.len();
        let patterns: Vec<Vec<ui::text_area::LineDecor>> = (0..n)
            .map(|idx| region_pattern(&loaded.state, &loaded.protected, theme, idx))
            .collect();
        let has_base = !loaded.state.degraded_base;
        // Strip specs for changed regions (read-only mode only).
        let mut specs: Vec<StripSpec> = Vec::new();
        if !self.editing {
            for idx in 0..n {
                let above = self.seed_line_starts.get(idx).copied().unwrap_or(0);
                match region_display(&loaded.state, idx, &self.revisiting, &loaded.protected) {
                    RegionDisplay::Deciding {
                        current, focused, ..
                    } => specs.push(StripSpec {
                        region: idx,
                        above_line: above,
                        kind: StripKind::Deciding {
                            has_base,
                            current,
                            focused,
                            origin: loaded
                                .origin_texts
                                .get(idx)
                                .cloned()
                                .flatten()
                                .map(SharedString::from),
                        },
                    }),
                    RegionDisplay::Decided {
                        choice, focused, ..
                    } => specs.push(StripSpec {
                        region: idx,
                        above_line: above,
                        kind: StripKind::Decided { choice, focused },
                    }),
                    RegionDisplay::Context { .. } => {}
                }
            }
        }
        let specs = Rc::new(specs);
        let seed = self.seed.clone();
        let starts = self.seed_line_starts.clone();
        let patterns = Rc::new(patterns);
        let editing = self.editing;
        let view = cx.weak_entity();
        editor.update(cx, |area, _| {
            area.set_decorator(Box::new(move |content| {
                let lines = editor_decors(theme, &seed, &starts, &patterns, content);
                let blocks = if editing {
                    Vec::new()
                } else {
                    specs
                        .iter()
                        .map(|spec| strip_block(spec, theme, view.clone()))
                        .collect()
                };
                ui::text_area::Decoration { lines, blocks }
            }));
        });
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
        // Remember the frame the drag handlers will clamp against, and give
        // the panels their first sizes: equal thirds, 40% tall.
        let viewport = window.viewport_size();
        self.viewport_w = f32::from(viewport.width);
        self.viewport_h = f32::from(viewport.height);
        // Equal shares of the row — recomputed when a load changes how many
        // panes there are, so the flexing column is never squeezed out by
        // widths that were divided for a different count.
        let cols = 3 + usize::from(
            self.loaded
                .as_ref()
                .is_some_and(|l| l.origin_rows.is_some()),
        );
        if self.pane_cols != cols {
            let each = ((self.viewport_w - super::SIDEBAR_WIDTH) / cols as f32).max(MIN_PANE_W);
            self.pane_widths = Some([each; PANE_COLS]);
            self.pane_cols = cols;
        }
        if self.panes_height.is_none() {
            self.panes_height = Some((self.viewport_h * 0.4).max(MIN_PANES_H));
        }
        // Poses (gallery/tests) set `loaded` directly without going through
        // load(); seed the editor lazily so it exists before body().
        if self.loaded.is_some() && self.result_editor.is_none() {
            self.seed_editor(cx);
        }
        // Build the input panes' read-only editors once per load (and again
        // on a theme flip): render is where the appearance is known.
        if let Some(loaded) = &mut self.loaded
            && loaded.pane_appearance != Some(window.appearance())
        {
            // Geometry-column order, so index == `*_COL`; origin's editor is
            // built only when origin exists, and carries its own decors (the
            // merge's region kinds say nothing about the remote).
            let mut panes: Vec<PaneContent> = vec![
                (
                    loaded.theirs_rows.clone(),
                    Rc::new(pane_decors(&loaded.theirs_rows, PaneSide::Theirs, theme)),
                ),
                (
                    loaded.base_rows.clone(),
                    Rc::new(pane_decors(&loaded.base_rows, PaneSide::Base, theme)),
                ),
                (
                    loaded.ours_rows.clone(),
                    Rc::new(pane_decors(&loaded.ours_rows, PaneSide::Ours, theme)),
                ),
            ];
            if let Some(rows) = &loaded.origin_rows {
                panes.push((rows.clone(), Rc::new(origin_pane_decors(rows, theme))));
            }
            let editors: Vec<Entity<ui::TextArea>> = panes
                .into_iter()
                .map(|(rows, decors)| {
                    let text: String = rows
                        .iter()
                        .map(|r| r.text.as_ref())
                        .collect::<Vec<_>>()
                        .join("\n");
                    cx.new(|cx| {
                        let mut area = ui::TextArea::read_only(text, cx);
                        area.set_left_inset(px(28.));
                        area.set_decorator(Box::new(move |_| ui::text_area::Decoration {
                            lines: decors.as_ref().clone(),
                            blocks: Vec::new(),
                        }));
                        area
                    })
                })
                .collect();
            loaded.pane_editors = Some(editors);
            loaded.pane_appearance = Some(window.appearance());
        }
        // (Re)install the decorator when the resolution, mode, or appearance
        // changed — render owns it because it needs the window appearance.
        if self.decor_dirty || self.decor_appearance != Some(window.appearance()) {
            self.install_result_decorator(theme, cx);
            self.decor_dirty = false;
            self.decor_appearance = Some(window.appearance());
        }
        // The result editor holds focus whenever loaded: read-only, single
        // keys (1/2/3/b/n) still reach the root's MergeEditor context via
        // the ancestor chain; editable, the root drops that context so the
        // keys type instead.
        if let Some(editor) = &self.result_editor {
            let editor_focus = editor.read(cx).focus_handle();
            if !editor_focus.is_focused(window) {
                editor_focus.focus(window);
            }
        } else if self.loaded.is_some() && !self.focus_handle.is_focused(window) {
            self.focus_handle.focus(window);
        }
        div()
            .track_focus(&self.focus_handle)
            // While editing the root drops the MergeEditor context, so its
            // single-letter picks (1/2/3/b/n) can't shadow typing.
            .key_context(if self.editing {
                "MergeEditorEditing"
            } else {
                "MergeEditor"
            })
            .on_action(
                cx.listener(|this, _: &PickDisk, _w, cx| this.pick_at_cursor(Choice::Ours, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PickSource, _w, cx| this.pick_at_cursor(Choice::Theirs, cx)),
            )
            .on_action(cx.listener(|this, _: &PickBase, _w, cx| {
                if !this.loaded.as_ref().is_some_and(|l| l.state.degraded_base) {
                    this.pick_at_cursor(Choice::Base, cx)
                }
            }))
            .on_action(
                cx.listener(|this, _: &PickBoth, _w, cx| this.pick_at_cursor(Choice::Both, cx)),
            )
            .on_action(cx.listener(|this, _: &NextConflict, _w, cx| this.next(cx)))
            .on_action(cx.listener(|this, _: &UndoChoice, _w, cx| this.undo(cx)))
            .on_action(cx.listener(|this, _: &RedoChoice, _w, cx| this.redo(cx)))
            .on_action(cx.listener(|this, _: &ApplyEdit, _w, cx| this.commit_edit(cx)))
            .on_action(cx.listener(|this, _: &CancelEdit, _w, cx| this.cancel_edit(cx)))
            // Divider drags are tracked HERE, not on the handle: gpui only
            // delivers `on_mouse_move` while the pointer is over the element,
            // and a resize drag routinely runs past a 7px strip.
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _w, cx| {
                this.drag_divider(event, cx)
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    if this.dragging.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    if this.dragging.take().is_some() {
                        cx.notify();
                    }
                }),
            )
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

/// What one [`MergeView::sync_probe`] run observed: where every input column
/// and the result editor ended up, and where the region-aligned mapping says
/// they should be.
#[cfg(test)]
pub(super) struct SyncProbe {
    pub(super) panes: [f32; PANE_COLS],
    pub(super) want_panes: [f32; PANE_COLS],
    pub(super) result: f32,
    pub(super) want_result: f32,
}

/// Merge-local banner: same shape as Review's, minus the Undo button (the
/// banners that stay here — protected span, errors — are never undoable).
fn banner_el(banner: &OutcomeBanner, theme: Theme) -> Div {
    ui::banner(theme, banner.tint, banner.text.clone(), None)
}

/// One read-only pane column: an attached label header (the user should
/// never have to map floating toolbar chips onto panes by position) over a
/// read-only [`ui::TextArea`] over its lines — selectable and horizontally
/// scrollable, decorated with the same washes the result editor uses.
#[allow(clippy::too_many_arguments)]
fn pane_col(
    label: &'static str,
    tint: Rgba,
    editor: Option<&Entity<ui::TextArea>>,
    // Fixed px width, or `None` to take the leftover space.
    width: Option<f32>,
    me: usize,
    scrolls: &[gpui::ScrollHandle; PANE_COLS],
    view: &WeakEntity<MergeView>,
    theme: Theme,
) -> AnyElement {
    let view = view.clone();
    div()
        .id(ElementId::named_usize("merge-pane-wrap", me))
        .map(|el| match width {
            Some(w) => el.w(px(w)).flex_none(),
            None => el.flex_1(),
        })
        .min_w_0()
        .h_full()
        .flex()
        .flex_col()
        .child(ui::pane_label(theme, label, tint))
        .child(
            div()
                .id(ElementId::named_usize("merge-pane-scroll", me))
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .overflow_x_scroll()
                .track_scroll(&scrolls[me])
                .font_family("Menlo")
                .text_xs()
                .line_height(px(PANE_ROW_H))
                .children(editor.cloned()),
        )
        // Scroll sync: this wrapper's wheel handler runs AFTER the inner
        // surface already applied the wheel (children dispatch first), so the
        // leader's fresh position drives the region-aligned mapping.
        .on_scroll_wheel(move |_event, _window, cx| {
            view.update(cx, |merge, _cx| merge.sync_from(me)).ok();
        })
        .into_any_element()
}

/// The base column when no last-written snapshot exists: the merge degraded
/// to 2-way, and the pane says so instead of duplicating theirs (spec §10).
fn degraded_base_col(theme: Theme, width: f32) -> AnyElement {
    div()
        .w(px(width))
        .flex_none()
        .min_w_0()
        .h_full()
        .flex()
        .flex_col()
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

/// Fixed mono row height (`h_5` = 20px) — the unit converting pane scroll
/// offsets to line positions for region-aligned sync.
const PANE_ROW_H: f32 = 20.;

/// Rows a decision-strip block occupies (Zed's FILE_HEADER_HEIGHT).
const STRIP_ROWS: usize = 2;

/// Geometry columns. These indices are STATIC: the three sides of the merge
/// document keep the columns they have always had and origin is appended,
/// so `RESULT_LEADER`, [`region_at`] and [`line_at`] never become variable.
/// Origin renders LEFTMOST all the same (remote → local source → last
/// written → disk reads as one provenance gradient); render order and
/// geometry column are simply two different things.
const THEIRS_COL: usize = 0;
const BASE_COL: usize = 1;
const OURS_COL: usize = 2;
const ORIGIN_COL: usize = 3;
pub(super) const PANE_COLS: usize = 4;

/// `sync_from` leader id for the result editor (0..PANE_COLS are the input
/// panes, whether or not each is currently drawn).
const RESULT_LEADER: usize = PANE_COLS;

/// Which draggable divider a gesture belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Divider {
    /// The handle that resizes input pane column `i` — drawn on its trailing
    /// edge, so the rightmost (flexing) column has none.
    Pane(usize),
    /// Between the input panes and the result.
    Result,
}

/// Panels never collapse to nothing — you have to be able to grab the
/// divider again.
const MIN_PANE_W: f32 = 120.;
const MIN_PANES_H: f32 = 80.;
const MIN_RESULT_H: f32 = 120.;

/// Does region `idx` render a decision strip? Exactly the regions whose
/// [`region_display`] is not `Context` — i.e. anything changed, PLUS an
/// unchanged region the user has since given a choice (a free-text edit can
/// land `Choice::Edited` on context). The row geometry and the decorator
/// MUST agree on this or the two disagree about where every line sits.
fn region_has_strip(state: &MergeState, idx: usize) -> bool {
    state.resolution.get(idx).is_some()
        || state
            .doc
            .regions
            .get(idx)
            .is_some_and(|r| r.kind != RegionKind::Unchanged)
}

/// How many decision strips sit above text line `line`. A strip renders
/// above its region's FIRST line, so it counts as soon as that region starts
/// at or before `line`. Pure so the row geometry is testable without a
/// window (this is what keeps the viewport still across an edit toggle).
fn strips_above_line(line: usize, seed_line_starts: &[usize], changed: &[bool]) -> usize {
    changed
        .iter()
        .enumerate()
        .filter(|&(idx, &is_changed)| {
            is_changed && seed_line_starts.get(idx).is_some_and(|&s| s <= line)
        })
        .count()
}

/// Region-aligned scroll mapping (2026-08-08 feedback: raw offset mirroring
/// is not 1:1 across panes). `starts` has one row per region plus a totals
/// sentinel; `starts[k][col]` is the first line of region k in that column.
/// Returns which region the given line sits in and the fraction through it.
fn region_at(starts: &[[usize; PANE_COLS]], col: usize, line: f32) -> (usize, f32) {
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
fn line_at(starts: &[[usize; PANE_COLS]], col: usize, k: usize, f: f32) -> f32 {
    let len = (starts[k + 1][col] - starts[k][col]) as f32;
    starts[k][col] as f32 + f * len
}

/// A decision-strip block spec, captured when the decorator is (re)built.
#[derive(Clone)]
struct StripSpec {
    region: usize,
    /// Seed line the strip sits above (read-only mode → content == seed).
    above_line: usize,
    kind: StripKind,
}

#[derive(Clone)]
enum StripKind {
    Deciding {
        has_base: bool,
        current: Option<ui::ChoiceKind>,
        focused: bool,
        /// Origin's text for this region, when it has one that differs from
        /// the local source. Carried rather than looked up at click time so
        /// the button and its payload cannot disagree.
        origin: Option<SharedString>,
    },
    Decided {
        choice: ui::ChoiceKind,
        focused: bool,
    },
}

/// Build the block that renders one region's decision strip inside the
/// result editor (Zed's `BlockPlacement::Above`). Buttons route back through
/// the `MergeView` weak handle — pick applies + re-seeds, edit flips the
/// editor editable, revisit reopens a decided strip.
fn strip_block(
    spec: &StripSpec,
    theme: Theme,
    view: WeakEntity<MergeView>,
) -> ui::text_area::Block {
    let region = spec.region;
    let state_kind = spec.kind.clone();
    ui::text_area::Block {
        above_line: spec.above_line,
        // Zed's FILE_HEADER_HEIGHT: room for the inset card without
        // crowding the code above and below.
        height: STRIP_ROWS as u32,
        render: Rc::new(move |_window, _cx| {
            let state = match &state_kind {
                StripKind::Deciding {
                    has_base,
                    current,
                    focused,
                    origin,
                } => ui::StripState::Deciding {
                    has_base: *has_base,
                    current: *current,
                    focused: *focused,
                    origin: origin.clone(),
                },
                StripKind::Decided { choice, focused } => ui::StripState::Decided {
                    choice: *choice,
                    focused: *focused,
                },
            };
            let on_pick = {
                let view = view.clone();
                move |pick: ui::Pick, _ev: &ClickEvent, _w: &mut Window, cx: &mut App| {
                    // Every button the strip renders maps to a choice — the
                    // origin one brings its own text, so no arm can be a
                    // silent no-op.
                    let choice = match pick {
                        ui::Pick::Ours => Choice::Ours,
                        ui::Pick::Theirs => Choice::Theirs,
                        ui::Pick::Base => Choice::Base,
                        ui::Pick::Both => Choice::Both,
                        ui::Pick::Origin(text) => Choice::Origin(text.to_string()),
                    };
                    view.update(cx, |merge, cx| merge.apply(region, choice, cx))
                        .ok();
                }
            };
            let on_edit = {
                let view = view.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    view.update(cx, |merge, cx| merge.start_edit(region, cx))
                        .ok();
                }
            };
            let on_revisit = {
                let view = view.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    view.update(cx, |merge, cx| merge.revisit(region, cx)).ok();
                }
            };
            // Blocks inherit the editor's mono text style; strips are UI
            // chrome, so they reset to the system UI font.
            div()
                .font_family(".SystemUIFont")
                // Pass the block's height through, or the strip's own
                // layout has nothing to centre itself in.
                .h_full()
                .w_full()
                .child(ui::decision_strip(
                    theme, region, state, on_pick, on_edit, on_revisit,
                ))
                .into_any_element()
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use tomte_app::merge_inputs::MergeInputs;
    use tomte_app::merge_state::MergeState;
    use tomte_app::resolve::{ResolveError, ResolveOutcome};
    use tomte_app::theme::Theme;
    use tomte_core::merge::{Choice, RegionKind};
    use tomte_core::template::{anchor::anchor, lexer::lex};

    use super::super::review::BannerTint;
    #[allow(unused_imports)]
    use super::{};
    use super::{
        BASE_COL, LoadedMerge, ORIGIN_COL, OURS_COL, PaneSide, RegionDisplay, STRIP_ROWS,
        THEIRS_COL, document_seed, edited_region_picks, line_at, map_line, merge_banner,
        next_target, origin_projection, pane_lines, protected_line_info, region_at, region_display,
        region_has_strip, region_seed_text, row_bg, strips_above_line,
    };
    use gpui::SharedString;
    use std::collections::HashMap;
    use tomte_ui::components::{ChoiceKind, Side};

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
            origin: None,
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
    fn region_seed_text_reflects_the_current_resolution() {
        let mut state = conflict_state();
        let region = state.conflicts()[0];

        // Undecided conflict: disk then source (the Both order).
        assert_eq!(region_seed_text(&state, region), "v = 2\nv = 3\n");

        state.pick(region, Choice::Theirs);
        assert_eq!(region_seed_text(&state, region), "v = 3\n");

        state.pick(region, Choice::Both);
        assert_eq!(region_seed_text(&state, region), "v = 2\nv = 3\n");

        state.pick(region, Choice::Edited("v = 9\n".into()));
        assert_eq!(region_seed_text(&state, region), "v = 9\n");

        // Auto-resolved region (no explicit pick): the engine default.
        let auto = MergeState::new(&inputs(Some("a\nz\n"), "a\nnew\nz\n", "a\nz\n"));
        let ours_only = auto
            .doc
            .regions
            .iter()
            .position(|r| r.kind == RegionKind::OursOnly)
            .unwrap();
        assert_eq!(region_seed_text(&auto, ours_only), "new\n");

        // Unchanged context seeds ONE copy, never ours+theirs.
        let state = conflict_state();
        let unchanged = state
            .doc
            .regions
            .iter()
            .position(|r| r.kind == RegionKind::Unchanged)
            .unwrap();
        assert_eq!(region_seed_text(&state, unchanged), "a\n");
    }

    /// Build the whole-document seed + per-region line starts the way
    /// `start_edit` does.
    fn seeded(state: &MergeState) -> (String, Vec<usize>) {
        let texts = document_seed(state);
        let mut seed = String::new();
        let mut starts = Vec::with_capacity(texts.len() + 1);
        let mut line = 0usize;
        for text in &texts {
            starts.push(line);
            line += text.split_inclusive('\n').count();
            seed.push_str(text);
        }
        starts.push(line);
        (seed, starts)
    }

    #[test]
    fn document_seed_concatenates_to_the_current_result() {
        let state = conflict_state();
        let (seed, starts) = seeded(&state);
        // Context + inlined conflict (disk then source) + context.
        assert_eq!(seed, "a\nv = 2\nv = 3\nz\n");
        assert_eq!(starts, vec![0, 1, 3, 4]);
    }

    #[test]
    fn map_line_shifts_clamps_and_pins() {
        // One replacement hunk: seed lines 1..2 became edited lines 1..3.
        let hunks = vec![(1..2, 1..3)];
        assert_eq!(map_line(0, &hunks), 0);
        assert_eq!(map_line(1, &hunks), 1, "boundary at hunk start stays");
        assert_eq!(map_line(2, &hunks), 3, "boundary after hunk shifts");
        // A boundary inside a replaced hunk clamps to the hunk start.
        let hunks = vec![(1..4, 1..2)];
        assert_eq!(map_line(2, &hunks), 1);
        assert_eq!(map_line(3, &hunks), 1);
        assert_eq!(map_line(4, &hunks), 2);
        // Insertion exactly AT a boundary belongs to the later region:
        // the boundary itself does not shift.
        let hunks = vec![(2..2, 2..4)];
        assert_eq!(map_line(2, &hunks), 2);
        assert_eq!(map_line(3, &hunks), 5);
    }

    #[test]
    fn edited_picks_land_on_the_touched_regions_only() {
        // Two separated edits (an unchanged line between keeps the hunks
        // distinct): a context rewrite and a conflict resolution.
        let mut state = MergeState::new(&inputs(
            Some("a\nb\nv = 1\nz\n"),
            "a\nb\nv = 2\nz\n",
            "a\nb\nv = 3\nz\n",
        ));
        let (seed, starts) = seeded(&state);
        assert_eq!(seed, "a\nb\nv = 2\nv = 3\nz\n");
        let edited = "a = 0\nb\nv = merged\nz\n";
        let picks = edited_region_picks(&seed, &starts, edited);
        assert_eq!(
            picks,
            vec![
                (0, Choice::Edited("a = 0\nb\n".into())),
                (1, Choice::Edited("v = merged\n".into())),
            ]
        );
        // Applying the picks assembles EXACTLY what the user saw.
        for (region, choice) in picks {
            state.pick(region, choice);
        }
        assert_eq!(state.assembled().as_deref(), Some(edited));
    }

    #[test]
    fn untouched_conflict_stays_undecided_after_an_edit_elsewhere() {
        let state = conflict_state();
        let (seed, starts) = seeded(&state);
        let edited = "a changed\nv = 2\nv = 3\nz\n";
        let picks = edited_region_picks(&seed, &starts, edited);
        assert_eq!(picks, vec![(0, Choice::Edited("a changed\n".into()))]);
        // Region 1 (the conflict) got no pick — Save stays gated on it.
    }

    #[test]
    fn editor_decors_mirror_the_result_pane_until_touched() {
        let theme = Theme::dark();
        let state = conflict_state();
        let (seed, starts) = seeded(&state);
        let patterns: Vec<_> = (0..state.doc.regions.len())
            .map(|idx| super::region_pattern(&state, &HashMap::new(), theme, idx))
            .collect();

        // Untouched: context muted, conflict rows tinted+railed by side
        // with "on disk"/"source" labels on the block's first rows.
        let decors = super::editor_decors(theme, &seed, &starts, &patterns, &seed);
        assert_eq!(decors[0].text_color, Some(theme.text_muted));
        assert_eq!(decors[0].bg, None);
        assert_eq!(decors[1].rail, Some(theme.drift));
        assert_eq!(decors[1].bg, Some(Theme::wash(theme.drift, 0.07)));
        assert_eq!(
            decors[1].label.as_ref().map(|(t, _)| t.as_ref()),
            Some("on disk")
        );
        assert_eq!(decors[2].rail, Some(theme.accent));
        assert_eq!(
            decors[2].label.as_ref().map(|(t, _)| t.as_ref()),
            Some("source")
        );

        // Touch the conflict block: its rows flip to the edited (ok) wash
        // with one "edited" pill; the context rows keep their pattern.
        let edited = "a\nv = merged\nz\n";
        let decors = super::editor_decors(theme, &seed, &starts, &patterns, edited);
        assert_eq!(decors[0].text_color, Some(theme.text_muted));
        assert_eq!(decors[1].bg, Some(Theme::wash(theme.ok, 0.07)));
        assert_eq!(decors[1].rail, None);
        assert_eq!(
            decors[1].label.as_ref().map(|(t, _)| t.as_ref()),
            Some("edited")
        );
        assert_eq!(
            decors[2].text_color,
            Some(theme.text_muted),
            "trailing context intact"
        );
    }

    #[test]
    fn region_pattern_names_decided_and_auto_choices() {
        let theme = Theme::dark();
        let mut state = conflict_state();
        let region = state.conflicts()[0];
        state.pick(region, Choice::Theirs);
        let pattern = super::region_pattern(&state, &HashMap::new(), theme, region);
        assert_eq!(pattern.len(), 1);
        assert_eq!(pattern[0].bg, Some(Theme::wash(theme.accent, 0.07)));
        assert_eq!(
            pattern[0].label.as_ref().map(|(t, _)| t.as_ref()),
            Some("source")
        );

        // Auto-resolved disk-only region: labeled as auto.
        let auto = MergeState::new(&inputs(Some("a\nz\n"), "a\nnew\nz\n", "a\nz\n"));
        let ours_only = auto
            .doc
            .regions
            .iter()
            .position(|r| r.kind == RegionKind::OursOnly)
            .unwrap();
        let pattern = super::region_pattern(&auto, &HashMap::new(), theme, ours_only);
        assert_eq!(
            pattern[0].label.as_ref().map(|(t, _)| t.as_ref()),
            Some("auto · disk")
        );
    }

    #[test]
    fn edited_picks_round_trip_appends_deletions_and_boundary_joins() {
        // Appending at EOF lands in the last region; deleting a whole
        // region empties it; the total always reassembles byte-exact.
        let mut state = conflict_state();
        let (seed, starts) = seeded(&state);
        let edited = "v = merged\nz\nz2\ntail\n";
        let picks = edited_region_picks(&seed, &starts, edited);
        for (region, choice) in picks {
            state.pick(region, choice);
        }
        assert_eq!(state.assembled().as_deref(), Some(edited));
    }

    #[test]
    fn edited_choice_assembles_verbatim_between_regions() {
        let mut state = conflict_state();
        let region = state.conflicts()[0];
        state.pick(region, Choice::Edited("v = merged by hand\n".into()));
        assert_eq!(
            state.assembled().as_deref(),
            Some("a\nv = merged by hand\nz\n")
        );
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
            origin: None,
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
        // Two regions, four columns: region 0 has 4 theirs-lines but 0
        // ours-lines (theirs-only insertion) and 2 origin lines; region 1 has
        // 2 lines everywhere but origin, which has 2 as well.
        let starts = vec![[0usize, 0, 0, 0], [4, 4, 0, 2], [6, 6, 2, 4]];

        // Halfway through region 0 in theirs…
        let (k, f) = region_at(&starts, THEIRS_COL, 2.0);
        assert_eq!(k, 0);
        assert!((f - 0.5).abs() < 1e-6);
        // …maps to ours PINNED at its region-0 boundary (empty side).
        assert_eq!(line_at(&starts, OURS_COL, k, f), 0.0);
        // …and base and origin advance proportionally.
        assert!((line_at(&starts, BASE_COL, k, f) - 2.0).abs() < 1e-6);
        assert!((line_at(&starts, ORIGIN_COL, k, f) - 1.0).abs() < 1e-6);

        // A line inside region 1 of ours maps back to region 1 (the
        // zero-length region 0 is skipped for that column).
        let (k, f) = region_at(&starts, OURS_COL, 1.0);
        assert_eq!(k, 1);
        assert!((f - 0.5).abs() < 1e-6);
        assert!((line_at(&starts, THEIRS_COL, k, f) - 5.0).abs() < 1e-6);

        // Origin leads just like any other column.
        let (k, f) = region_at(&starts, ORIGIN_COL, 3.0);
        assert_eq!(k, 1);
        assert!((f - 0.5).abs() < 1e-6);
        assert!((line_at(&starts, THEIRS_COL, k, f) - 5.0).abs() < 1e-6);

        // Overscroll saturates at the last region, fraction clamped.
        let (k, f) = region_at(&starts, THEIRS_COL, 99.0);
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
                vec![(SharedString::from("v = 3"), Some(Side::Theirs))]
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
                    (SharedString::from("v = 2"), Some(Side::Ours)),
                    (SharedString::from("v = 3"), Some(Side::Theirs)),
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
            } => assert_eq!(lines, vec![(SharedString::from("B"), Some(Side::Ours))]),
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

    #[test]
    fn strips_above_line_counts_each_changed_region_at_or_before_the_line() {
        // 3 regions starting at lines 0, 1, 3; the middle one is changed.
        let starts = [0usize, 1, 3];
        let changed = [false, true, false];
        assert_eq!(
            strips_above_line(0, &starts, &changed),
            0,
            "before the region"
        );
        assert_eq!(
            strips_above_line(1, &starts, &changed),
            1,
            "the strip sits ABOVE the region's first line, so it counts there"
        );
        assert_eq!(strips_above_line(9, &starts, &changed), 1);

        // Two changed regions: both count once the line is past them.
        let changed = [true, true, false];
        assert_eq!(strips_above_line(0, &starts, &changed), 1);
        assert_eq!(strips_above_line(1, &starts, &changed), 2);
        assert_eq!(strips_above_line(3, &starts, &changed), 2);
    }

    /// The anchor arithmetic `rescroll_anchored` performs: entering edit mode
    /// removes every strip, so a line's row drops by 2 per strip above it —
    /// and the scroll offset must move by exactly that much to compensate.
    #[test]
    fn edit_toggle_row_delta_is_the_strips_above_the_anchor() {
        let starts = [0usize, 1, 3];
        let changed = [false, true, false];
        let row_of = |line: usize, with_strips: bool| {
            if with_strips {
                line + strips_above_line(line, &starts, &changed) * STRIP_ROWS
            } else {
                line
            }
        };
        // A line before any strip doesn't move at all.
        assert_eq!(row_of(0, true), row_of(0, false));
        // The changed region's own first line moves up by exactly one strip.
        assert_eq!(row_of(1, true) - row_of(1, false), STRIP_ROWS);
        // …and so does everything after it.
        assert_eq!(row_of(9, true) - row_of(9, false), STRIP_ROWS);
    }

    #[test]
    fn a_context_region_gains_a_strip_once_it_is_edited() {
        use std::collections::HashSet;

        // The row geometry and the decorator must agree on which regions
        // carry a strip: a free-text edit can land `Choice::Edited` on an
        // UNCHANGED region, which then renders one. Counting only
        // `kind != Unchanged` under-counted and skewed every row mapping.
        let mut state = conflict_state();
        let unchanged = state
            .doc
            .regions
            .iter()
            .position(|r| r.kind == RegionKind::Unchanged)
            .unwrap();
        assert!(!region_has_strip(&state, unchanged));
        assert!(
            matches!(
                region_display(&state, unchanged, &HashSet::new(), &HashMap::new()),
                RegionDisplay::Context { .. }
            ),
            "…and the decorator agrees there is no strip"
        );

        state.pick(unchanged, Choice::Edited("edited\n".into()));
        assert!(region_has_strip(&state, unchanged));
        assert!(
            !matches!(
                region_display(&state, unchanged, &HashSet::new(), &HashMap::new()),
                RegionDisplay::Context { .. }
            ),
            "…and now the decorator renders one"
        );
    }

    /// The anchor the view projects origin against: the local source's text
    /// and its per-region line starts — what `LoadedMerge::new` feeds
    /// `origin_region_texts`.
    fn theirs_anchor(state: &MergeState) -> (String, Vec<usize>) {
        let mut starts = Vec::with_capacity(state.doc.regions.len() + 1);
        let mut line = 0usize;
        for region in &state.doc.regions {
            starts.push(line);
            line += region.theirs.len();
        }
        starts.push(line);
        (state.doc.theirs_lines().concat(), starts)
    }

    #[test]
    fn origin_text_projects_onto_merge_regions() {
        // theirs (local source) vs origin differ on one line; that line sits
        // inside exactly one region, and the origin pick must offer only that
        // region's origin text — not the whole file.
        let state = MergeState::new(&inputs(
            Some("a\nv = 1\nz\n"),
            "a\nv = 2\nz\n",
            "a\nv = 3\nz\n",
        ));
        let origin = "a\nv = 9\nz\n";
        let (theirs, starts) = theirs_anchor(&state);
        let per_region = origin_projection(&theirs, &starts, origin).texts;
        let conflict = state.conflicts()[0];
        assert_eq!(per_region[conflict].as_deref(), Some("v = 9\n"));
        // Regions origin agrees on offer nothing — no button, no noise.
        assert_eq!(per_region[0], None);
    }

    /// Inputs whose origin differs from the local source on the middle line.
    fn origin_inputs() -> MergeInputs {
        let mut i = inputs(Some("a\nv = 1\nz\n"), "a\nv = 2\nz\n", "a\nv = 3\nz\n");
        i.origin = Some("a\nv = 9\nz\n".into());
        i
    }

    #[test]
    fn loaded_merge_gives_origin_its_own_column_rows_and_picks() {
        let loaded = LoadedMerge::new(Arc::new(origin_inputs()));
        let conflict = loaded.state.conflicts()[0];

        // The pick offers only the differing region's text.
        assert_eq!(loaded.origin_texts[conflict].as_deref(), Some("v = 9\n"));
        assert_eq!(loaded.origin_texts[0], None);

        // The pane shows the whole origin file…
        let rows = loaded.origin_rows.as_ref().expect("fourth pane rows");
        assert_eq!(
            rows.iter().map(|r| r.text.as_ref()).collect::<Vec<_>>(),
            ["a", "v = 9", "z"]
        );
        // …with the remote wash on exactly the region that differs.
        assert_eq!(
            rows.iter().map(|r| r.differs).collect::<Vec<_>>(),
            [false, true, false]
        );
        let theme = Theme::dark();
        let decors = super::origin_pane_decors(rows, theme);
        assert_eq!(decors[1].bg, Some(Theme::wash(theme.remote, 0.16)));
        assert_eq!(decors[0].bg, None);
        assert_eq!(decors[0].text_color, Some(theme.text_muted));

        // The fourth geometry column is origin's OWN line geometry, so scroll
        // sync maps into it — and the rows a region shows are exactly the
        // text its origin pick applies.
        let starts = &loaded.region_line_starts;
        assert_eq!(
            starts.iter().map(|s| s[ORIGIN_COL]).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        let shown: Vec<&str> = rows[starts[conflict][ORIGIN_COL]..starts[conflict + 1][ORIGIN_COL]]
            .iter()
            .map(|r| r.text.as_ref())
            .collect();
        assert_eq!(shown, ["v = 9"]);
    }

    #[test]
    fn a_boundary_spanning_origin_diff_never_offers_a_deletion() {
        // Origin changed the conflict line AND the line right after it,
        // which belongs to the NEXT region. `line_hunks` returns one hunk
        // spanning both, and `map_line` clamps a boundary interior to a hunk
        // to that hunk's start — so the conflict's projected slice comes out
        // empty. Offering it would delete the region behind a button that
        // reads like any other pick.
        let mut i = inputs(
            Some("v = 1\nkeep\ntail\n"),
            "v = 2\nkeep\ntail\n",
            "v = 3\nkeep\ntail\n",
        );
        let origin = "v = 9\nKEEP\ntail\n";
        i.origin = Some(origin.into());
        let loaded = LoadedMerge::new(Arc::new(i));
        let conflict = loaded.state.conflicts()[0];

        // The premise is real, not hand-built: the projection genuinely
        // collapsed this region to zero origin lines.
        let (theirs, starts) = theirs_anchor(&loaded.state);
        let mapped = super::mapped_boundaries(&theirs, &starts, origin);
        assert_eq!(
            mapped[conflict],
            mapped[conflict + 1],
            "the boundary must clamp for this test to mean anything"
        );
        assert!(
            starts[conflict] < starts[conflict + 1],
            "…while the source DOES have lines there"
        );

        // No pick is offered for it…
        assert_eq!(loaded.origin_texts[conflict], None);
        // …and no offered pick anywhere can empty a region.
        assert!(
            loaded.origin_texts.iter().flatten().all(|t| !t.is_empty()),
            "an origin pick must never be a pure deletion: {:?}",
            loaded.origin_texts
        );
    }

    #[test]
    fn without_origin_the_fourth_column_mirrors_the_disk_column() {
        // Gated on CONTENT: no origin text, no fourth pane — but the column
        // stays well-defined so `region_at`/`line_at` never see a
        // zero-length region.
        let loaded = LoadedMerge::new(Arc::new(inputs(
            Some("a\nv = 1\nz\n"),
            "a\nv = 2\nz\n",
            "a\nv = 3\nz\n",
        )));
        assert!(loaded.origin_rows.is_none());
        assert!(loaded.origin_texts.is_empty());
        for starts in loaded.region_line_starts.iter() {
            assert_eq!(starts[ORIGIN_COL], starts[OURS_COL]);
        }
    }

    #[test]
    fn an_origin_pick_assembles_verbatim_and_reads_as_origin() {
        let loaded = LoadedMerge::new(Arc::new(origin_inputs()));
        let mut state = loaded.state;
        let conflict = state.conflicts()[0];
        let text = loaded.origin_texts[conflict].clone().expect("origin text");
        state.pick(conflict, Choice::Origin(text));
        assert_eq!(state.assembled().as_deref(), Some("a\nv = 9\nz\n"));

        // …and the result rows carry the remote tint, not the edited green.
        let theme = Theme::dark();
        let pattern = super::region_pattern(&state, &HashMap::new(), theme, conflict);
        assert_eq!(pattern[0].bg, Some(Theme::wash(theme.remote, 0.07)));
        assert_eq!(
            pattern[0].label.as_ref().map(|(t, _)| t.as_ref()),
            Some("origin")
        );
    }
}
