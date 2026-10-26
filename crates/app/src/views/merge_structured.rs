//! Structure mode of the merge editor (spec 2026-10-25): the tree of the
//! file with a chip per side, the lock hover text, the header controls, the
//! mode toggle, and the small pure rules the view's Structure branches
//! call. The rows themselves are planned by [`tomte_app::structured_tree`];
//! this module draws them as given. `merge.rs` keeps only thin branches on
//! [`MergeMode`].

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;

use gpui::{
    AnyElement, Context, Div, Rgba, SharedString, Stateful, WeakEntity, div, prelude::*, px,
    uniform_list,
};
use tomte_app::merge_inputs::MergeInputs;
use tomte_app::structured_state::{Stage, StructuredState};
use tomte_app::structured_tree::{Bracket, Chip, ChipSide, FoldKey, FoldWhat, NodeKey, TreeRow};
use tomte_app::theme::Theme;
use tomte_core::structured::jsonc;
use tomte_core::structured::{Decision, NotStructured, Pick, looked_like_json};
use tomte_core::template::anchor::SpanMap;
use tomte_core::template::writeback::{WriteBackError, write_back};
use tomte_ui::components as ui;

use super::merge::{LoadedMerge, MergeMode, MergeView, SaveGate, save_gate_for, span_reason};

/// A chip's tint: the colour the line editor gives that side.
pub(super) fn chip_tint(theme: Theme, side: ChipSide) -> Rgba {
    match side {
        ChipSide::Disk => theme.drift,
        ChipSide::Source => theme.accent,
        ChipSide::LastWritten => theme.text_muted,
        ChipSide::Both => theme.ok,
    }
}

/// The tree list's top and bottom padding. gpui places row `i` at
/// `TREE_PAD_Y + i * TREE_ROW_H` (`uniform_list.rs`), so the view
/// subtracts it when it reads the top row from the scroll offset.
pub(super) const TREE_PAD_Y: f32 = 4.;

/// What every row of one frame's tree shares: the list's closure owns it.
struct TreeFrame {
    theme: Theme,
    /// The structured cursor, drawn with the selected wash.
    cursor: Option<usize>,
    /// Each locked decision's hover text ([`lock_reason`]).
    locks: std::rc::Rc<HashMap<usize, SharedString>>,
    view: WeakEntity<MergeView>,
    #[cfg(test)]
    drawn: std::rc::Rc<std::cell::Cell<usize>>,
}

/// Structure mode's body: the planned rows in one `uniform_list`, one
/// [`ui::TREE_ROW_H`] row per [`TreeRow`]. The closure only reads the rows;
/// every click upgrades the view's weak handle in a fresh update.
pub(super) fn render_tree(
    view: &MergeView,
    loaded: &LoadedMerge,
    theme: Theme,
    cx: &mut Context<MergeView>,
) -> AnyElement {
    let rows = loaded.tree_rows.clone();
    let state = loaded.structured.as_ref();
    let frame = TreeFrame {
        theme,
        cursor: state.and_then(|s| s.cursor),
        locks: loaded.lock_reasons.clone(),
        view: cx.weak_entity(),
        #[cfg(test)]
        drawn: view.tree_rows_drawn.clone(),
    };
    uniform_list("merge-tree", rows.len(), move |range, _window, _cx| {
        tree_rows(&rows, range, &frame)
    })
    .track_scroll(view.tree_scroll.clone())
    .flex_1()
    .min_h_0()
    .py(px(TREE_PAD_Y))
    .font_family("Menlo")
    .text_xs()
    .into_any_element()
}

/// The elements for `rows[range]`, the part of the tree in view.
fn tree_rows(rows: &[TreeRow], range: Range<usize>, frame: &TreeFrame) -> Vec<AnyElement> {
    #[cfg(test)]
    frame.drawn.set(frame.drawn.get() + range.len());
    range.map(|ix| tree_row(&rows[ix], ix, frame)).collect()
}

/// One planned row as drawn. Decision rows are keyed by decision id
/// (`tree-row:<id>`), every other row by its index (`tree-line:<ix>`).
fn tree_row(row: &TreeRow, ix: usize, frame: &TreeFrame) -> AnyElement {
    let theme = frame.theme;
    let line = |depth: usize, tone: ui::RowTone| {
        ui::tree_row(theme, ("tree-line", ix), depth, tone, false, |_, _, _| {})
            .w_full()
            .debug_selector(move || format!("tree-line:{ix}"))
    };
    match row {
        TreeRow::Decision {
            depth,
            id,
            key,
            chips,
            open,
            locked,
        } => {
            let id = *id;
            let tone = if *open {
                ui::RowTone::Open
            } else {
                ui::RowTone::Changed
            };
            let view = frame.view.clone();
            // Outside the chips, a click moves the cursor without picking.
            let row = ui::tree_row(
                theme,
                ("tree-row", id),
                *depth,
                tone,
                frame.cursor == Some(id),
                move |_, _, cx| {
                    view.update(cx, |merge, cx| merge.select_decision(id, cx))
                        .ok();
                },
            )
            .w_full()
            .cursor_pointer()
            .debug_selector(move || format!("tree-row:{id}"))
            .when_some(key.as_ref(), |el, key| {
                el.child(div().flex_none().child(format!("{key}:")))
            })
            .children(chips.iter().map(|chip| value_chip(chip, id, frame)));
            if !*locked {
                return row.into_any_element();
            }
            let reason = frame.locks.get(&id).cloned().unwrap_or_else(|| {
                tomte_core::log_error!("merge", "locked decision {id} has no lock reason");
                "generated by template".into()
            });
            row.child(ui::lock_mark(theme, reason).debug_selector(move || format!("lock:{id}")))
                .into_any_element()
        }
        TreeRow::Open {
            depth,
            key,
            bracket,
            folded,
            summary,
            same,
            node,
            comma,
        } => {
            let (open, close) = match bracket {
                Bracket::Object => ("{", "}"),
                Bracket::Array => ("[", "]"),
            };
            let mut text = String::from(if *folded { "▸ " } else { "▾ " });
            if let Some(key) = key {
                text.push_str(key);
                text.push_str(": ");
            }
            text.push_str(open);
            // Folded, the row stands for the whole container (the planner
            // emits no `Close` for it): `▸ "key": { 2 keys },`.
            let tail = folded.then(|| {
                let comma = if *comma { "," } else { "" };
                (summary.clone(), format!("{close}{comma}"))
            });
            let tone = if *same {
                ui::RowTone::Same
            } else {
                ui::RowTone::Plain
            };
            let node = *node;
            let view = frame.view.clone();
            ui::tree_row(
                theme,
                ("tree-line", ix),
                *depth,
                tone,
                false,
                move |_, _, cx| {
                    view.update(cx, |merge, cx| merge.toggle_container(node, cx))
                        .ok();
                },
            )
            .w_full()
            .cursor_pointer()
            .debug_selector(move || format!("tree-line:{ix}"))
            .child(text)
            .when_some(tail, |el, (summary, close)| {
                el.child(
                    div()
                        .flex()
                        .flex_none()
                        .gap_2()
                        .child(div().text_color(theme.text_muted).child(summary))
                        .child(close),
                )
            })
            .into_any_element()
        }
        TreeRow::Close {
            depth,
            bracket,
            comma,
        } => {
            let close = match bracket {
                Bracket::Object => "}",
                Bracket::Array => "]",
            };
            line(*depth, ui::RowTone::Plain)
                .child(if *comma {
                    format!("{close},")
                } else {
                    close.to_string()
                })
                .into_any_element()
        }
        TreeRow::Comment { depth, text } => line(*depth, ui::RowTone::Comment)
            .child(text.clone())
            .into_any_element(),
        TreeRow::Same { depth, text } | TreeRow::Preview { depth, text } => {
            line(*depth, ui::RowTone::Same)
                .child(text.clone())
                .into_any_element()
        }
        TreeRow::Fold {
            depth,
            count,
            what,
            key,
        } => {
            let noun = match (what, *count == 1) {
                (FoldWhat::Keys, true) => "key",
                (FoldWhat::Keys, false) => "keys",
                (FoldWhat::Elements, true) => "element",
                (FoldWhat::Elements, false) => "elements",
            };
            let key = *key;
            let view = frame.view.clone();
            ui::tree_row(
                theme,
                ("tree-line", ix),
                *depth,
                ui::RowTone::Fold,
                false,
                move |_, _, cx| {
                    view.update(cx, |merge, cx| merge.expand_fold(key, cx)).ok();
                },
            )
            .w_full()
            .cursor_pointer()
            .debug_selector(move || format!("tree-line:{ix}"))
            .child(format!("▸ {count} identical {noun}"))
            .into_any_element()
        }
    }
}

/// One side's chip on decision `id`'s row. A container chip unfolds or
/// folds its preview; every other chip picks its side.
fn value_chip(chip: &Chip, id: usize, frame: &TreeFrame) -> Stateful<Div> {
    let side = chip.side;
    let label = if chip.agreed {
        "source · disk"
    } else {
        side.label()
    };
    let prefix = if chip.unfolded {
        Some("▾")
    } else if chip.container {
        Some("▸")
    } else {
        None
    };
    let container = chip.container;
    let view = frame.view.clone();
    ui::value_chip(
        frame.theme,
        (SharedString::from(format!("chip-{}", side.selector())), id),
        chip_tint(frame.theme, side),
        label,
        chip.text.clone().into(),
        chip.picked,
        prefix,
        move |_, _, cx| {
            // The row's own click (cursor only) must not follow: the pick
            // below moves the cursor on, and the row would pull it back.
            cx.stop_propagation();
            view.update(cx, |merge, cx| {
                if container {
                    merge.toggle_chip(id, side, cx);
                } else {
                    // A pick lands where the cursor is, as a pick key does:
                    // the cursor comes to this row, then advances from it.
                    merge.select_decision(id, cx);
                    merge.apply_pick(id, side.pick(), cx);
                }
            })
            .ok();
        },
    )
    .debug_selector(move || format!("chip:{id}:{}", side.selector()))
}

/// Every locked decision's hover text ([`lock_reason`]), by decision id.
/// Each one lexes the template, so the view words them once per load.
pub(super) fn lock_reasons(
    state: &StructuredState,
    inputs: &MergeInputs,
) -> HashMap<usize, SharedString> {
    state
        .doc
        .decisions
        .iter()
        .filter(|d| d.locked.is_some())
        .map(|d| {
            let reason = lock_reason(d, inputs.template.as_deref(), inputs.span_map.as_ref());
            (d.id, reason)
        })
        .collect()
}

#[cfg(test)]
thread_local! {
    /// Test seam: how many lock reasons this thread has worded.
    static LOCK_REASONS_WORDED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many lock reasons this test's thread has worded so far.
#[cfg(test)]
pub(super) fn lock_reasons_worded() -> usize {
    LOCK_REASONS_WORDED.with(|n| n.get())
}

/// A locked row's hover text: the template line behind the span that
/// pins the decision, in the same words the source pane's `{}` gutter uses.
pub(super) fn lock_reason(
    d: &Decision,
    template: Option<&str>,
    span_map: Option<&SpanMap>,
) -> SharedString {
    #[cfg(test)]
    LOCK_REASONS_WORDED.with(|n| n.set(n.get() + 1));
    let span = d
        .locked
        .zip(span_map)
        .and_then(|(lock, map)| map.spans.get(lock.span));
    let reason = match span {
        Some(span) => {
            let segments = template.and_then(|t| tomte_core::template::lexer::lex(t).ok());
            span_reason(&span.origin, template, segments.as_deref())
        }
        None => "template expression".to_string(),
    };
    format!("generated by template:\n{reason}").into()
}

/// Whether `pick` may land on decision `id`: never on a locked decision,
/// last written only where the base had the entry, both only on an array
/// conflict run.
pub(super) fn pick_allowed(state: &StructuredState, id: usize, pick: Pick) -> bool {
    let Some(d) = state.doc.decisions.get(id) else {
        return false;
    };
    if d.locked.is_some() {
        return false;
    }
    match pick {
        Pick::LastWritten => state.doc.has_base(id),
        Pick::Both => d.both_allowed,
        Pick::Disk | Pick::Source => true,
    }
}

/// Whether the user has made any structure pick (what a mode switch would
/// discard).
pub(super) fn has_picks(state: &StructuredState) -> bool {
    (0..state.doc.decisions.len()).any(|id| state.resolution.get(id).is_some())
}

/// Next ↓ in Structure mode: unresolved conflicts first, then every
/// unlocked decision, mirroring the line editor's `next_target`.
pub(super) fn next_target(state: &mut StructuredState) -> Option<usize> {
    if state.unresolved().is_empty() {
        state.next_changed()
    } else {
        state.next_unresolved()
    }
}

/// The header's progress words over the structured state: "4 of 5 picked
/// · 1 to decide", counting every unlocked decision (a row whose effective
/// pick is automatic counts as picked), and "5 of 5 picked" once nothing is
/// open.
pub(super) fn progress_text(state: &StructuredState) -> String {
    if state.nothing_to_decide() {
        return "same settings, formatting differs · Save keeps the source layout".to_string();
    }
    let changed = state.changed().len();
    let open = state.unresolved().len();
    let picked = changed.saturating_sub(open);
    if open == 0 {
        format!("{picked} of {changed} picked")
    } else {
        format!("{picked} of {changed} picked · {open} to decide")
    }
}

/// A row "Fold identical" can keep in view: one with an identity that
/// survives a replan (spec §3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Anchor {
    Decision(usize),
    Open(NodeKey),
    Fold(FoldKey),
    /// No identified row at or above the viewport's top.
    Top,
}

/// `row`'s identity, `None` for rows a replan may renumber or absorb.
pub(super) fn row_anchor(row: &TreeRow) -> Option<Anchor> {
    match row {
        TreeRow::Decision { id, .. } => Some(Anchor::Decision(*id)),
        TreeRow::Open { node, .. } => Some(Anchor::Open(*node)),
        TreeRow::Fold { key, .. } => Some(Anchor::Fold(*key)),
        TreeRow::Close { .. }
        | TreeRow::Comment { .. }
        | TreeRow::Same { .. }
        | TreeRow::Preview { .. } => None,
    }
}

fn row_depth(row: &TreeRow) -> usize {
    match row {
        TreeRow::Open { depth, .. }
        | TreeRow::Close { depth, .. }
        | TreeRow::Comment { depth, .. }
        | TreeRow::Same { depth, .. }
        | TreeRow::Decision { depth, .. }
        | TreeRow::Preview { depth, .. }
        | TreeRow::Fold { depth, .. } => *depth,
    }
}

/// The nearest row at or before `top` that has an identity, `Top` when
/// none does.
pub(super) fn fold_anchor(rows: &[TreeRow], top: usize) -> Anchor {
    rows.iter()
        .take(top.saturating_add(1))
        .rev()
        .find_map(row_anchor)
        .unwrap_or(Anchor::Top)
}

/// Where `anchor` sits in the replanned `rows`. When the old top row was
/// not the anchor itself (`was_top == false`: it had no identity, so a fold
/// may have absorbed it), the first `Fold` row after the anchor, before
/// the next identified row and inside the anchor's container, stands in
/// for it. `None` when the anchor is gone from `rows`.
pub(super) fn anchor_row(rows: &[TreeRow], anchor: &Anchor, was_top: bool) -> Option<usize> {
    let (at, start, depth) = match anchor {
        Anchor::Top => {
            if rows.is_empty() {
                return None;
            }
            (0, 0, 0)
        }
        _ => {
            let at = rows
                .iter()
                .position(|row| row_anchor(row).as_ref() == Some(anchor))?;
            (at, at + 1, row_depth(&rows[at]))
        }
    };
    if was_top {
        return Some(at);
    }
    for (ix, row) in rows.iter().enumerate().skip(start) {
        match row {
            TreeRow::Fold { .. } => return Some(ix),
            TreeRow::Decision { .. } | TreeRow::Open { .. } => break,
            TreeRow::Close { depth: d, .. } if *d < depth => break,
            _ => {}
        }
    }
    Some(at)
}

/// The row of `new` to put at the viewport's top after a "Fold identical"
/// replan, given the `old` rows and the old top row (spec §3.6).
/// - A row with an identity keeps it: the same row in `new`.
/// - A `Close` of a container that is not identical keeps its container:
///   that container's `Close` in `new`.
/// - A row a fold can absorb ([`foldable`]) goes to the `Fold` that took
///   it, the first one after the identified row above it ([`anchor_row`]).
/// - Anything else (a `Fold` the replan expanded, a `Preview` line, a row
///   no fold took) is found by walking from the nearest row above it that
///   `new` still has ([`walk_to`]).
pub(super) fn kept_top(old: &[TreeRow], new: &[TreeRow], top: usize) -> Option<usize> {
    let top = top.min(old.len().checked_sub(1)?);
    let last = new.len().checked_sub(1)?;
    let row = &old[top];
    if let Some(anchor) = row_anchor(row)
        && let Some(ix) = position_of(new, &anchor)
    {
        return Some(ix);
    }
    if let TreeRow::Close { .. } = row
        && let Some(open) = matching_open(old, top)
        && let TreeRow::Open { same: false, .. } = &old[open]
        && let Some(ix) = kept_in_new(old, new, top)
    {
        return Some(ix);
    }
    if foldable(old, top)
        && let Some((anchor, at)) = surviving_anchor(old, new, top)
        // `anchor_row` answers the anchor's own row when no fold follows it.
        && let Some(ix) = anchor_row(new, &anchor, false)
        && ix != at
    {
        return Some(ix);
    }
    Some(walk_to(old, new, top).min(last))
}

/// Where old row `k` sits in `new` when the replan kept it: an identified
/// row by its identity, a `Close` through its container's `Open` (when
/// that container is open in `new`).
fn kept_in_new(old: &[TreeRow], new: &[TreeRow], k: usize) -> Option<usize> {
    match &old[k] {
        TreeRow::Close { .. } => {
            let TreeRow::Open { node, .. } = &old[matching_open(old, k)?] else {
                return None;
            };
            let open = position_of(new, &Anchor::Open(*node))?;
            match &new[open] {
                TreeRow::Open { folded: false, .. } => matching_close(new, open),
                _ => None,
            }
        }
        row => position_of(new, &row_anchor(row)?),
    }
}

/// Old row `top`'s place in `new`: from the nearest row above it that
/// `new` kept, step down the old rows one by one, counting in `new` the
/// rows each one became. A row the replan kept is one row; a `Fold` it
/// expanded is the `count` children it stood for ([`skip_children`]).
/// Every row counted is a row of `new`, so a removed fold between the
/// anchor and `top` cannot shift the answer.
fn walk_to(old: &[TreeRow], new: &[TreeRow], top: usize) -> usize {
    let (mut k, mut j) = (0..top)
        .rev()
        .find_map(|k| kept_in_new(old, new, k).map(|j| (k, j)))
        .map_or((0, 0), |(k, j)| (k + 1, j + 1));
    while k < top {
        j = match &old[k] {
            TreeRow::Fold {
                depth, count, key, ..
            } if position_of(new, &Anchor::Fold(*key)).is_none() => {
                skip_children(new, j, *depth, *count)
            }
            _ => j + 1,
        };
        k += 1;
    }
    j
}

/// The index just past `count` children at `depth` starting at `rows[j]`:
/// each child is its leading `Comment` rows, then one `Same` row, or an
/// `Open` row with, when it is open, its children and its `Close`.
fn skip_children(rows: &[TreeRow], mut j: usize, depth: usize, count: usize) -> usize {
    for _ in 0..count {
        while matches!(rows.get(j), Some(TreeRow::Comment { depth: d, .. }) if *d == depth) {
            j += 1;
        }
        j = match rows.get(j) {
            Some(TreeRow::Open {
                depth: d,
                folded: false,
                ..
            }) if *d == depth => matching_close(rows, j).map_or(j + 1, |close| close + 1),
            Some(_) => j + 1,
            None => return j,
        };
    }
    j
}

/// Whether a fold can have absorbed `rows[ix]`: a `Same` or `Comment` row,
/// or the `Close` of an identical container (`same: true`), which folds
/// with its run.
fn foldable(rows: &[TreeRow], ix: usize) -> bool {
    match &rows[ix] {
        TreeRow::Same { .. } | TreeRow::Comment { .. } => true,
        TreeRow::Close { .. } => matching_open(rows, ix)
            .is_some_and(|open| matches!(rows[open], TreeRow::Open { same: true, .. })),
        TreeRow::Open { .. }
        | TreeRow::Decision { .. }
        | TreeRow::Preview { .. }
        | TreeRow::Fold { .. } => false,
    }
}

/// The nearest identified row at or above `top` in `old` that `new` still
/// has, with its index in `new`: an identical container's `Open` folds
/// away with its run, so the row above it stands in.
fn surviving_anchor(old: &[TreeRow], new: &[TreeRow], top: usize) -> Option<(Anchor, usize)> {
    let mut top = top;
    loop {
        let anchor = fold_anchor(old, top);
        if anchor == Anchor::Top {
            return None;
        }
        if let Some(at) = position_of(new, &anchor) {
            return Some((anchor, at));
        }
        top = position_of(old, &anchor)?.checked_sub(1)?;
    }
}

/// The index of `anchor`'s row in `rows`.
fn position_of(rows: &[TreeRow], anchor: &Anchor) -> Option<usize> {
    rows.iter()
        .position(|row| row_anchor(row).as_ref() == Some(anchor))
}

/// The `Open` row of the `Close` at `close`: the nearest `Open` above it
/// at its depth (deeper rows belong to its children).
fn matching_open(rows: &[TreeRow], close: usize) -> Option<usize> {
    let depth = row_depth(&rows[close]);
    rows[..close]
        .iter()
        .rposition(|row| matches!(row, TreeRow::Open { depth: d, .. } if *d == depth))
}

/// The `Close` row of the unfolded `Open` at `open`: the first `Close`
/// below it at its depth.
fn matching_close(rows: &[TreeRow], open: usize) -> Option<usize> {
    let depth = row_depth(&rows[open]);
    rows.iter()
        .enumerate()
        .skip(open + 1)
        .find(|(_, row)| matches!(row, TreeRow::Close { depth: d, .. } if *d == depth))
        .map(|(ix, _)| ix)
}

/// Structure mode's header controls: progress over the decisions, the
/// origin chip, Fold identical, Next ↓, and Edit. In the Text stage (`text` is `Some`):
/// the invalid-JSON chip when the text does not parse, and Back to
/// structure (reading "Discard edits" once armed) — no Apply (the line
/// editor's Apply maps regions and has no meaning here) and no Discard
/// edit (Back to structure is the way out). Save sits after Cancel, drawn
/// by the view.
pub(super) fn header_controls(
    mut bar: Div,
    state: &StructuredState,
    has_origin: bool,
    fold_identical: bool,
    text: Option<TextStage>,
    theme: Theme,
    cx: &mut Context<MergeView>,
) -> Div {
    let open = state.unresolved().len();
    bar = bar.child(
        div()
            .text_xs()
            .whitespace_nowrap()
            .text_color(if open == 0 { theme.ok } else { theme.conflict })
            .child(progress_text(state)),
    );
    if has_origin {
        bar = bar.child(ui::chip(
            theme,
            "origin differs · see Lines",
            ui::ChipVariant::Wash(theme.remote),
        ));
    }
    if let Some(text) = text {
        if let Some(line) = text.invalid_line {
            bar = bar.child(
                div()
                    .id("merge-json-chip")
                    .debug_selector(|| "merge-json-chip".into())
                    .child(ui::chip(
                        theme,
                        json_chip_text(line),
                        ui::ChipVariant::Wash(theme.conflict),
                    )),
            );
        }
        let (label, variant) = if text.discard_armed {
            ("Discard edits", ui::ButtonVariant::Solid(theme.conflict))
        } else {
            ("Back to structure", ui::ButtonVariant::Ghost)
        };
        return bar.child(
            ui::button(
                theme,
                "merge-back-to-structure",
                label.into(),
                variant,
                ui::ButtonSize::Sm,
                cx.listener(|view, _ev, _window, cx| view.back_to_structure(cx)),
            )
            .debug_selector(|| "merge-back-to-structure".into()),
        );
    }
    // An icon toggle, the Review sidebar's List/Tree shape: the words
    // live in its tooltip, so the header keeps one row.
    bar = bar.child(
        ui::icon_button(
            theme,
            "merge-fold-identical",
            ui::chevrons_down_up(theme),
            fold_identical,
            "Fold identical".into(),
            cx.listener(|view, _ev, _window, cx| view.toggle_fold_identical(cx)),
        )
        .debug_selector(|| "merge-fold-identical".into()),
    );
    if !state.doc.decisions.is_empty() {
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
    bar.child(
        ui::button(
            theme,
            "merge-edit",
            "Edit".into(),
            ui::ButtonVariant::Tonal(theme.text_muted),
            ui::ButtonSize::Sm,
            cx.listener(|view, _ev, _window, cx| {
                let id = view
                    .loaded
                    .as_ref()
                    .and_then(|l| l.structured.as_ref())
                    .and_then(|s| s.cursor)
                    .unwrap_or(0);
                view.start_edit(id, cx);
            }),
        )
        .debug_selector(|| "merge-edit".into()),
    )
}

/// Structure mode's Save gate. The Structure stage: enabled once no
/// conflict is open (an assembly that then fails to re-parse must reach
/// `save`, which shows it, spec §7). The Text stage: always enabled — Save
/// takes the editor text verbatim, open conflicts and invalid JSON included
/// (the user may save an invalid file on purpose, spec §6).
pub(super) fn save_gate(state: &StructuredState, editing: bool) -> SaveGate {
    if state.stage == Stage::Text {
        return SaveGate::Enabled;
    }
    let unresolved = state.unresolved().len();
    save_gate_for(unresolved == 0, unresolved, editing)
}

/// The 1-based line of the first JSON(C) parse error in `text`, or `None`
/// when it parses. Drives the Text stage's "not valid JSON · line N" chip.
pub(super) fn invalid_json_line(text: &str) -> Option<usize> {
    jsonc::parse(text).err().map(|e| e.line)
}

/// The line editor's header note for a file that does not merge
/// structurally (spec §7): "disk is not valid JSON (line 12): line merge"
/// or "disk and source are different JSON shapes: line merge". A scalar
/// root gets the shapes wording too (spec §7 names both under one note);
/// [`NotStructured`]'s own text for it stays more specific. Shown only when
/// the file looked like JSON (spec §4), so a shell file says nothing; two
/// empty sides say nothing either.
pub(super) fn not_structured_note(
    target: &Path,
    ours: &str,
    theirs: &str,
    why: Option<&NotStructured>,
) -> Option<String> {
    let why = match why? {
        NotStructured::BothEmpty => return None,
        NotStructured::ScalarRoot => &NotStructured::ShapeMismatch,
        other => other,
    };
    looked_like_json(target, ours, theirs).then(|| format!("{why}: line merge"))
}

/// The write-back's refusal of a Structure-stage save on a templated file,
/// checked before anything is queued (ruling R27). Each unlocked decision
/// writes back on its own; this catches what the per-decision lock rule
/// cannot see, such as two free picks on adjacent lines of different
/// template literals that the write-back reads as one edit. `None` for a
/// plain file, or when the result writes back.
pub(super) fn template_refusal(inputs: &MergeInputs, assembled: &str) -> Option<WriteBackError> {
    let (Some(template), Some(map)) = (inputs.template.as_deref(), inputs.span_map.as_ref()) else {
        return None;
    };
    write_back(template, map, &inputs.theirs, assembled).err()
}

/// The chip's words for an invalid line.
pub(super) fn json_chip_text(line: usize) -> String {
    format!("not valid JSON · line {line}")
}

/// What the Text stage's header shows beside Save: the chip when the text
/// does not parse, and whether Back to structure is armed.
pub(super) struct TextStage {
    pub(super) invalid_line: Option<usize>,
    pub(super) discard_armed: bool,
}

/// The armed Back to structure's banner text.
pub(super) const TEXT_DISCARD_WARNING: &str =
    "Discard the text edits and return to the structure picks?";

/// The "Structure | Lines" control: the selected mode tonal, the other
/// ghost. A press goes through [`MergeView::set_mode`], which arms a
/// confirmation first when the current mode has picks.
pub(super) fn mode_toggle(theme: Theme, mode: MergeMode, cx: &mut Context<MergeView>) -> Div {
    let variant = |this: MergeMode| {
        if this == mode {
            ui::ButtonVariant::Tonal(theme.accent)
        } else {
            ui::ButtonVariant::Ghost
        }
    };
    div()
        .flex()
        .items_center()
        .gap_0p5()
        .child(
            ui::button(
                theme,
                "merge-mode-structure",
                "Structure".into(),
                variant(MergeMode::Structure),
                ui::ButtonSize::Sm,
                cx.listener(|view, _ev, _window, cx| view.set_mode(MergeMode::Structure, cx)),
            )
            .debug_selector(|| "merge-mode-structure".into()),
        )
        .child(
            ui::button(
                theme,
                "merge-mode-lines",
                "Lines".into(),
                variant(MergeMode::Lines),
                ui::ButtonSize::Sm,
                cx.listener(|view, _ev, _window, cx| view.set_mode(MergeMode::Lines, cx)),
            )
            .debug_selector(|| "merge-mode-lines".into()),
        )
}

/// The armed mode switch's banner text.
pub(super) fn switch_warning(to: MergeMode) -> &'static str {
    match to {
        MergeMode::Lines => {
            "Switching to Lines discards the structure picks · click again to confirm"
        }
        MergeMode::Structure => {
            "Switching to Structure discards the line picks · click again to confirm"
        }
    }
}

#[cfg(test)]
mod tests {
    use tomte_app::structured_state::StructuredState;
    use tomte_core::structured::Side;
    use tomte_core::structured::{Pick, StructuredDocument};

    use tomte_app::structured_tree::{Bracket, FoldKey, FoldWhat, NodeKey, TreeRow};

    use super::{
        Anchor, anchor_row, fold_anchor, has_picks, invalid_json_line, kept_top,
        not_structured_note, pick_allowed, progress_text,
    };

    fn state(base: Option<&str>, ours: &str, theirs: &str) -> StructuredState {
        StructuredState::new(StructuredDocument::compute(base, ours, theirs, None).expect("json"))
    }

    // MUTATION: allow LastWritten in two-way, or Both on a key row.
    #[test]
    fn picks_the_row_cannot_take_are_refused() {
        let three = state(Some("{\"a\": 1}"), "{\"a\": 2}", "{\"a\": 3}");
        assert!(pick_allowed(&three, 0, Pick::LastWritten));
        assert!(
            !pick_allowed(&three, 0, Pick::Both),
            "a key row has no both"
        );
        let two = state(None, "{\"a\": 2}", "{\"a\": 3}");
        assert!(
            !pick_allowed(&two, 0, Pick::LastWritten),
            "no base in two-way"
        );
        assert!(!pick_allowed(&two, 7, Pick::Disk), "unknown id");
    }

    /// MUTATION: m counts conflicts only → "0 of 1 picked" on json_inputs.
    #[test]
    fn progress_text_counts_every_unlocked_decision() {
        let inputs = super::super::fixtures::json_inputs();
        let mut s = StructuredState::new(
            StructuredDocument::compute(inputs.base.as_deref(), &inputs.ours, &inputs.theirs, None)
                .expect("json"),
        );
        assert_eq!(progress_text(&s), "4 of 5 picked · 1 to decide");
        assert!(!has_picks(&s));
        let open = s.unresolved()[0];
        s.pick(open, Pick::Disk);
        assert_eq!(progress_text(&s), "5 of 5 picked");
        assert!(has_picks(&s));
        let same = state(Some("{\"a\": 1}"), "{ \"a\":1 }", "{\"a\": 1}");
        assert_eq!(
            progress_text(&same),
            "same settings, formatting differs · Save keeps the source layout"
        );
    }

    fn node(n: usize) -> NodeKey {
        NodeKey {
            side: Side::Theirs,
            node: n,
        }
    }

    fn open_row(depth: usize, n: usize) -> TreeRow {
        TreeRow::Open {
            depth,
            key: None,
            bracket: Bracket::Object,
            folded: false,
            summary: "3 keys".into(),
            same: false,
            node: node(n),
            comma: false,
        }
    }

    fn decision_row(depth: usize, id: usize) -> TreeRow {
        TreeRow::Decision {
            depth,
            id,
            key: None,
            chips: Vec::new(),
            open: false,
            locked: false,
        }
    }

    fn same_row(depth: usize) -> TreeRow {
        TreeRow::Same {
            depth,
            text: "\"x\": 1,".into(),
        }
    }

    fn fold_row(depth: usize, first: usize) -> TreeRow {
        TreeRow::Fold {
            depth,
            count: 3,
            what: FoldWhat::Keys,
            key: FoldKey {
                container: node(0),
                first,
            },
        }
    }

    fn close_row(depth: usize) -> TreeRow {
        TreeRow::Close {
            depth,
            bracket: Bracket::Object,
            comma: false,
        }
    }

    /// MUTATION: `fold_anchor` always returns `Top` → the decision, open
    /// and fold anchors all read `Top`.
    #[test]
    fn fold_anchor_prefers_the_nearest_identified_row() {
        let rows = vec![
            open_row(0, 0),
            same_row(1),
            decision_row(1, 4),
            TreeRow::Comment {
                depth: 1,
                text: "// c".into(),
            },
            same_row(1),
            fold_row(1, 5),
            same_row(1),
            close_row(0),
        ];
        assert_eq!(fold_anchor(&rows, 0), Anchor::Open(node(0)));
        assert_eq!(fold_anchor(&rows, 1), Anchor::Open(node(0)));
        assert_eq!(fold_anchor(&rows, 2), Anchor::Decision(4));
        assert_eq!(
            fold_anchor(&rows, 4),
            Anchor::Decision(4),
            "past the comment"
        );
        assert_eq!(
            fold_anchor(&rows, 6),
            Anchor::Fold(FoldKey {
                container: node(0),
                first: 5
            })
        );
        assert_eq!(fold_anchor(&rows, 99), fold_anchor(&rows, 7), "clamped");
        assert_eq!(fold_anchor(&[same_row(0)], 0), Anchor::Top);
        assert_eq!(fold_anchor(&[], 0), Anchor::Top);
    }

    /// MUTATION: `anchor_row` returns the anchor's index even when
    /// `was_top` is false → the absorbed top row maps to decision 0's row
    /// (1), not the fold (2).
    #[test]
    fn anchor_row_lands_on_the_fold_that_absorbed_the_top_row() {
        // Folded: the three `Same` rows after decision 0 became one fold.
        let folded = vec![
            open_row(0, 0),
            decision_row(1, 0),
            fold_row(1, 1),
            decision_row(1, 1),
            close_row(0),
        ];
        assert_eq!(anchor_row(&folded, &Anchor::Decision(0), false), Some(2));
        assert_eq!(anchor_row(&folded, &Anchor::Decision(0), true), Some(1));
        assert_eq!(anchor_row(&folded, &Anchor::Decision(1), false), Some(3));
        assert_eq!(anchor_row(&folded, &Anchor::Decision(9), false), None);
        // A decision before the fold: the top row was not in that run.
        let later = vec![
            open_row(0, 0),
            decision_row(1, 0),
            decision_row(1, 1),
            fold_row(1, 2),
            close_row(0),
        ];
        assert_eq!(anchor_row(&later, &Anchor::Decision(0), false), Some(1));
        // The whole pipeline: old top on the second identical row.
        let old = vec![
            open_row(0, 0),
            decision_row(1, 0),
            same_row(1),
            same_row(1),
            same_row(1),
            decision_row(1, 1),
            close_row(0),
        ];
        assert_eq!(kept_top(&old, &folded, 3), Some(2));
        assert_eq!(kept_top(&old, &folded, 5), Some(3), "a decision stays put");
        assert_eq!(kept_top(&[], &old, 0), None);
    }

    fn preview_row(depth: usize) -> TreeRow {
        TreeRow::Preview {
            depth,
            text: "\"x\": 1".into(),
        }
    }

    fn container_row(depth: usize, n: usize, same: bool) -> TreeRow {
        match open_row(depth, n) {
            TreeRow::Open {
                depth,
                key,
                bracket,
                folded,
                summary,
                node,
                comma,
                ..
            } => TreeRow::Open {
                depth,
                key,
                bracket,
                folded,
                summary,
                same,
                node,
                comma,
            },
            _ => unreachable!(),
        }
    }

    /// Folding off with a `Fold` row at the top: the first row of the
    /// restored run goes to the top.
    ///
    /// MUTATION: a removed fold answers its predecessor's own row → Some(1).
    #[test]
    fn kept_top_puts_an_expanded_fold_s_first_row_at_the_top() {
        let folded = vec![
            open_row(0, 0),
            decision_row(1, 0),
            fold_row(1, 1),
            decision_row(1, 1),
            close_row(0),
        ];
        let unfolded = vec![
            open_row(0, 0),
            decision_row(1, 0),
            same_row(1),
            same_row(1),
            same_row(1),
            decision_row(1, 1),
            close_row(0),
        ];
        assert_eq!(kept_top(&folded, &unfolded, 2), Some(2));
    }

    /// A chip's preview line at the top stays beside its decision; a fold
    /// further down did not absorb it.
    ///
    /// MUTATION: treat a `Preview` row as foldable → the fold below, Some(4).
    #[test]
    fn kept_top_keeps_a_preview_line_beside_its_decision() {
        let old = vec![
            open_row(0, 0),
            decision_row(1, 0),
            preview_row(2),
            preview_row(2),
            same_row(1),
            same_row(1),
            decision_row(1, 1),
            close_row(0),
        ];
        let new = vec![
            open_row(0, 0),
            decision_row(1, 0),
            preview_row(2),
            preview_row(2),
            fold_row(1, 1),
            decision_row(1, 1),
            close_row(0),
        ];
        assert_eq!(kept_top(&old, &new, 3), Some(3));
    }

    /// The `Close` of a container that differs keeps its container even
    /// when its identical children fold; the `Close` of an identical
    /// container folds with its run.
    ///
    /// MUTATION: drop the `Close` branch of `kept_top` → the offset from
    /// decision 0 lands on the sibling fold, Some(5).
    /// MUTATION: `foldable` says false for every `Close` → the identical
    /// container's `Close` keeps its offset instead of joining the fold,
    /// Some(3).
    #[test]
    fn kept_top_keeps_a_close_row_with_its_container() {
        let old = vec![
            open_row(0, 0),
            container_row(1, 1, false),
            decision_row(2, 0),
            same_row(2),
            same_row(2),
            close_row(1),
            same_row(1),
            close_row(0),
        ];
        let new = vec![
            open_row(0, 0),
            container_row(1, 1, false),
            decision_row(2, 0),
            fold_row(2, 1),
            close_row(1),
            fold_row(1, 2),
            close_row(0),
        ];
        assert_eq!(kept_top(&old, &new, 5), Some(4));
        // An identical container after decision 0: its rows fold into one.
        let old = vec![
            open_row(0, 0),
            decision_row(1, 0),
            container_row(1, 2, true),
            same_row(2),
            close_row(1),
            same_row(1),
            close_row(0),
        ];
        let new = vec![
            open_row(0, 0),
            decision_row(1, 0),
            fold_row(1, 1),
            close_row(0),
        ];
        // The `Close` at 4 sits inside the run that became the fold at 2.
        assert_eq!(kept_top(&old, &new, 4), Some(2));
    }

    fn fold_of(depth: usize, count: usize, container: usize, first: usize) -> TreeRow {
        TreeRow::Fold {
            depth,
            count,
            what: FoldWhat::Keys,
            key: FoldKey {
                container: node(container),
                first,
            },
        }
    }

    /// Folding off with a `Fold` at the top that follows an object holding
    /// a fold of its own (one change, then identical siblings): the view
    /// lands on the first restored row after the object's `}`, not inside
    /// the object's restored run.
    ///
    /// MUTATION: measure the old-row distance from the nearest identified
    /// row (the round-1 rule) → decision 0 at 2 plus 3, Some(5).
    #[test]
    fn kept_top_counts_a_restored_run_above_the_top_fold() {
        let old = vec![
            open_row(0, 0),
            container_row(1, 1, false),
            decision_row(2, 0),
            fold_of(2, 3, 1, 1),
            close_row(1),
            fold_of(1, 2, 0, 1),
            close_row(0),
        ];
        let new = vec![
            open_row(0, 0),
            container_row(1, 1, false),
            decision_row(2, 0),
            same_row(2),
            same_row(2),
            same_row(2),
            close_row(1),
            same_row(1),
            same_row(1),
            close_row(0),
        ];
        assert_eq!(kept_top(&old, &new, 5), Some(7));
    }

    /// Folding off with a `Same` row of a run the user expanded at the top,
    /// after an object that held a fold: the row keeps its place.
    ///
    /// MUTATION: measure the old-row distance from the nearest identified
    /// row (the round-1 rule) → decision 0 at 2 plus 4, Some(6).
    #[test]
    fn kept_top_keeps_an_expanded_row_after_a_restored_run() {
        let old = vec![
            open_row(0, 0),
            container_row(1, 1, false),
            decision_row(2, 0),
            fold_of(2, 3, 1, 1),
            close_row(1),
            same_row(1),
            same_row(1),
            close_row(0),
        ];
        let new = vec![
            open_row(0, 0),
            container_row(1, 1, false),
            decision_row(2, 0),
            same_row(2),
            same_row(2),
            same_row(2),
            close_row(1),
            same_row(1),
            same_row(1),
            close_row(0),
        ];
        assert_eq!(kept_top(&old, &new, 6), Some(8));
    }

    /// Folding off with the next entry's comment at the top, right under a
    /// fold: no kept row sits between the fold and the comment, so only
    /// counting the fold's restored rows finds it.
    ///
    /// MUTATION: `walk_to` counts a removed `Fold` as one row → Some(3).
    #[test]
    fn kept_top_counts_the_rows_a_fold_restores() {
        let old = vec![
            open_row(0, 0),
            decision_row(1, 0),
            fold_of(1, 3, 0, 1),
            TreeRow::Comment {
                depth: 1,
                text: "// next".into(),
            },
            decision_row(1, 1),
            close_row(0),
        ];
        // The fold's three children: a Same, a commented Same, and an open
        // identical container.
        let new = vec![
            open_row(0, 0),
            decision_row(1, 0),
            same_row(1),
            TreeRow::Comment {
                depth: 1,
                text: "// a".into(),
            },
            same_row(1),
            // The third child: an identical container the user opened.
            container_row(1, 7, true),
            same_row(2),
            close_row(1),
            TreeRow::Comment {
                depth: 1,
                text: "// next".into(),
            },
            decision_row(1, 1),
            close_row(0),
        ];
        assert_eq!(kept_top(&old, &new, 3), Some(8));
    }

    /// Folding off with a `Same` row of an expanded run at the top: the
    /// row keeps its place, no fold in sight.
    ///
    /// MUTATION: return `anchor_row`'s answer when no fold follows the
    /// anchor → the anchor's own row, Some(1).
    #[test]
    fn kept_top_keeps_a_row_no_fold_took() {
        let old = vec![
            open_row(0, 0),
            decision_row(1, 0),
            same_row(1),
            same_row(1),
            decision_row(1, 1),
            fold_row(1, 4),
            close_row(0),
        ];
        let new = vec![
            open_row(0, 0),
            decision_row(1, 0),
            same_row(1),
            same_row(1),
            decision_row(1, 1),
            same_row(1),
            same_row(1),
            close_row(0),
        ];
        assert_eq!(kept_top(&old, &new, 3), Some(3));
    }

    // MUTATION: return the error's byte offset (or a 0-based line) instead
    // of its 1-based line → the "line 3" asserts fail.
    // MUTATION: `Some` on a valid text → the JSONC-with-comments case fails.
    #[test]
    fn invalid_json_line_names_the_parse_errors_line() {
        assert_eq!(invalid_json_line("{\n  \"a\": \n}"), Some(3));
        assert_eq!(invalid_json_line("{\n  \"a\": 1,\n  \"b\" 2\n}\n"), Some(3));
        assert_eq!(invalid_json_line("{\n  // c\n  \"a\": 1\n}\n"), None);
        assert_eq!(invalid_json_line("{}"), None);
    }

    // MUTATION: drop the `looked_like_json` check → the .zshrc parse error
    // gets a note. MUTATION: drop the BothEmpty arm → the both-empty case
    // gets one. MUTATION: treat ScalarRoot like BothEmpty → the scalar root
    // has no note. MUTATION: name the side wrongly in the Display text →
    // the source line reads "disk".
    #[test]
    fn the_note_names_parse_errors_and_shapes_on_json_looking_files_only() {
        use std::path::Path;
        use tomte_core::structured::NotStructured;
        let json = Path::new("settings.json");
        let parse = |side| NotStructured::Parse { side, line: 12 };
        assert_eq!(
            not_structured_note(json, "{", "{}", Some(&parse(Side::Ours))).as_deref(),
            Some("disk is not valid JSON (line 12): line merge")
        );
        assert_eq!(
            not_structured_note(json, "{}", "{", Some(&parse(Side::Theirs))).as_deref(),
            Some("source is not valid JSON (line 12): line merge")
        );
        assert_eq!(
            not_structured_note(json, "{}", "[]", Some(&NotStructured::ShapeMismatch)).as_deref(),
            Some("disk and source are different JSON shapes: line merge")
        );
        assert_eq!(
            not_structured_note(json, "1", "2", Some(&NotStructured::ScalarRoot)).as_deref(),
            Some("disk and source are different JSON shapes: line merge")
        );
        assert_eq!(
            not_structured_note(json, "", "", Some(&NotStructured::BothEmpty)),
            None
        );
        assert_eq!(
            not_structured_note(Path::new(".zshrc"), "x", "y", Some(&parse(Side::Ours))),
            None
        );
        assert_eq!(not_structured_note(json, "{}", "{}", None), None);
        // MUTATION: treat TooDeep like BothEmpty → no note.
        assert_eq!(
            not_structured_note(
                json,
                "[]",
                "[]",
                Some(&NotStructured::TooDeep { depth: 300 })
            )
            .as_deref(),
            Some("the JSON nests deeper than 256 levels: line merge")
        );
    }

    // Ruling R32. MUTATION: use the repeated-literal wording for an
    // unrepeated literal in `span_reason` → the hover reads "repeated
    // literal: …".
    #[test]
    fn a_two_spans_lock_names_the_directive_beside_it() {
        use super::lock_reason;
        use tomte_core::template::{anchor::anchor, lexer::lex};
        let template = "{\n  \"editor\": \"hx\",\n  {{- if .work }}\n  \"proxy\": \"corp\",\n  {{- end }}\n  \"tabs\": 2\n}\n";
        let theirs = "{\n  \"editor\": \"hx\",\n  \"proxy\": \"corp\",\n  \"tabs\": 2\n}\n";
        let ours = "{\n  \"editor\": \"vi\",\n  \"proxy\": \"home\",\n  \"tabs\": 2\n}\n";
        let map = anchor(template, &lex(template).unwrap(), theirs);
        let doc = StructuredDocument::compute(None, ours, theirs, Some(&map)).expect("json");
        let reason = |path: &str| {
            let d = doc.decisions.iter().find(|d| d.path == path).expect(path);
            lock_reason(d, Some(template), Some(&map)).to_string()
        };
        assert_eq!(
            reason("editor"),
            "generated by template:\nbeside a template directive: {{- if .work }}"
        );
        assert_eq!(
            reason("proxy"),
            "generated by template:\nbeside a template directive: {{- end }}"
        );
    }

    // MUTATION: `template_refusal` returns None unconditionally → the
    // adjacent-literals save is not refused.
    #[test]
    fn template_refusal_catches_free_picks_the_write_back_reads_as_one_edit() {
        use super::super::fixtures;
        use super::template_refusal;
        use tomte_core::template::writeback::WriteBackError;
        let inputs = fixtures::json_adjacent_literals_inputs();
        let doc = StructuredDocument::compute(
            inputs.base.as_deref(),
            &inputs.ours,
            &inputs.theirs,
            inputs.span_map.as_ref(),
        )
        .expect("json");
        assert!(
            doc.decisions.iter().all(|d| d.locked.is_none()),
            "each line sits in one literal"
        );
        let mut s = StructuredState::new(doc);
        let both = s.assembled().expect("valid").expect("no conflicts");
        assert!(matches!(
            template_refusal(&inputs, &both),
            Some(WriteBackError::EditOutsideLiteral { .. })
        ));
        // One of the two alone writes back.
        let proxy = s
            .doc
            .decisions
            .iter()
            .find(|d| d.path == "proxy")
            .unwrap()
            .id;
        s.pick(proxy, Pick::Source);
        let one = s.assembled().expect("valid").expect("no conflicts");
        assert!(template_refusal(&inputs, &one).is_none());
        // A plain file has no template to refuse.
        let plain = fixtures::json_inputs();
        assert!(template_refusal(&plain, &plain.ours).is_none());
    }
}
