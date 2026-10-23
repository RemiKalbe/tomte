//! Structure mode of the merge editor (structured-merge spec §6): the strip
//! plan for a [`StructuredState`], the words each strip shows, the lock
//! hover text, the mode toggle, and the small pure rules the view's
//! Structure branches call. Everything here that needs no gpui is a plain
//! function over the state, so the rules are testable without a window;
//! `merge.rs` keeps only thin branches on [`MergeMode`].

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{App, ClickEvent, Context, Div, SharedString, WeakEntity, Window, div, prelude::*};
use tomte_app::structured_state::{Stage, StructuredState};
use tomte_app::theme::Theme;
use tomte_core::merge::RegionKind;
use tomte_core::structured::jsonc;
use tomte_core::structured::{Change, Decision, Pick, Side, SideText, StructuredDocument};
use tomte_core::template::anchor::SpanMap;
use tomte_ui::components as ui;

use super::merge::{
    MergeMode, MergeView, PaneSide, STRIP_ROWS, SaveGate, row_bg, save_gate_for, span_reason,
};

/// What one structured strip renders as. Plain data so the plan is testable;
/// [`strip_block`] turns it into a `ui::StripState`.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum StripKind {
    /// Open: an unpicked conflict, an auto-resolved row, or a revisited one.
    Deciding {
        has_base: bool,
        current: Option<ui::ChoiceKind>,
        focused: bool,
        both: bool,
    },
    /// The user picked; the strip names the pick and offers revisit.
    Decided {
        choice: ui::ChoiceKind,
        focused: bool,
    },
    /// A template pins the entry: no buttons, follows source.
    Locked { reason: SharedString, focused: bool },
}

/// One decision's strip in the result editor.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StructuredStrip {
    /// The decision id.
    pub(super) id: usize,
    /// The 0-based result line the strip sits above.
    pub(super) above_line: usize,
    /// The key path (`editor.fontSize`).
    pub(super) label: SharedString,
    /// The change in words ([`detail_words`]).
    pub(super) detail: SharedString,
    pub(super) kind: StripKind,
}

/// The strip display for a structured pick.
pub(super) fn choice_kind(pick: Pick) -> ui::ChoiceKind {
    match pick {
        Pick::Disk => ui::ChoiceKind::Ours,
        Pick::Source => ui::ChoiceKind::Theirs,
        Pick::LastWritten => ui::ChoiceKind::Base,
        Pick::Both => ui::ChoiceKind::Both,
    }
}

/// One strip per decision, in decision order, each above the decision's
/// first line in the current seed.
pub(super) fn strip_specs(
    state: &StructuredState,
    revisiting: &HashSet<usize>,
    template: Option<&str>,
    span_map: Option<&SpanMap>,
) -> Vec<StructuredStrip> {
    let lines = state.result_lines();
    state
        .doc
        .decisions
        .iter()
        .map(|d| {
            let focused = state.cursor == Some(d.id);
            let kind = if d.locked.is_some() {
                StripKind::Locked {
                    reason: lock_reason(d, template, span_map),
                    focused,
                }
            } else {
                match state.resolution.get(d.id) {
                    Some(pick) if !revisiting.contains(&d.id) => StripKind::Decided {
                        choice: choice_kind(pick),
                        focused,
                    },
                    explicit => StripKind::Deciding {
                        has_base: state.doc.has_base(d.id),
                        current: explicit
                            .or_else(|| state.doc.auto_pick(d.id))
                            .map(choice_kind),
                        focused,
                        both: d.both_allowed,
                    },
                }
            };
            StructuredStrip {
                id: d.id,
                above_line: lines.get(d.id).copied().unwrap_or(0),
                label: d.path.clone().into(),
                detail: detail_words(d),
                kind,
            }
        })
        .collect()
}

/// The change in words (spec §6). A duplicated key says so first, because
/// that is what the user cannot see from the value alone; a conflict says
/// "conflict"; otherwise the side that changed and how.
pub(super) fn detail_words(d: &Decision) -> SharedString {
    let words = match (d.duplicated, d.kind, &d.what) {
        (Some(Side::Ours), _, _) => "appears twice on disk · last value used",
        (Some(Side::Theirs), _, _) => "appears twice in source · last value used",
        (_, RegionKind::Conflict, _) => "conflict",
        (_, _, Change::TypeChanged) => "type changed",
        (_, RegionKind::OursOnly, Change::Added) => "added on disk",
        (_, RegionKind::OursOnly, Change::Removed) => "removed on disk",
        (_, RegionKind::OursOnly, _) => "changed on disk",
        (_, RegionKind::TheirsOnly, Change::Added) => "added in source",
        (_, RegionKind::TheirsOnly, Change::Removed) => "removed in source",
        (_, RegionKind::TheirsOnly, _) => "changed in source",
        (_, RegionKind::BothSame | RegionKind::Unchanged, _) => "same change on disk and source",
    };
    words.into()
}

/// The locked strip's hover text: the template line behind the span that
/// pins the decision, in the same words the source pane's `{}` gutter uses.
pub(super) fn lock_reason(
    d: &Decision,
    template: Option<&str>,
    span_map: Option<&SpanMap>,
) -> SharedString {
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

/// The editor row of decision `id`'s first result line: its line plus the
/// strip blocks at or above it (each strip sits above its own line).
pub(super) fn text_row(result_lines: &[usize], id: usize) -> usize {
    let Some(&line) = result_lines.get(id) else {
        return 0;
    };
    let strips = result_lines.iter().filter(|&&l| l <= line).count();
    line + strips * STRIP_ROWS
}

/// The header's progress words over the structured state.
pub(super) fn progress_text(state: &StructuredState) -> String {
    if state.nothing_to_decide() {
        return "same settings, formatting differs · Save keeps the source layout".to_string();
    }
    let (decided, total) = state.progress();
    if total == 0 {
        let changed = state.doc.decisions.len();
        let noun = if changed == 1 { "entry" } else { "entries" };
        return format!("auto-resolved · {changed} changed {noun} · review below");
    }
    let s = if total == 1 { "" } else { "s" };
    format!("{total} conflict{s}, {decided} resolved")
}

/// Input-pane decorations in Structure mode: [`structured_pane_decors`]'s
/// per-decision washes plus the source pane's `{}` gutter on lines a
/// template generated. `protected[i]` is line `i`'s hover text (all `None`
/// on the other panes).
pub(super) fn pane_decors(
    doc: &StructuredDocument,
    side: Side,
    protected: impl Iterator<Item = Option<SharedString>>,
    theme: Theme,
) -> Vec<ui::text_area::LineDecor> {
    let mut decors = structured_pane_decors(doc, side, theme);
    for (decor, tip) in decors.iter_mut().zip(protected) {
        decor.gutter = tip.is_some().then(|| ("{}".into(), theme.drift));
        decor.tooltip = tip;
    }
    decors
}

/// The pane side a structured side shows in.
fn pane_side(side: Side) -> PaneSide {
    match side {
        Side::Base => PaneSide::Base,
        Side::Ours => PaneSide::Ours,
        Side::Theirs => PaneSide::Theirs,
    }
}

/// Decision `d`'s piece of `side`. A degraded base is source standing in,
/// not a last-written state, so it carries no decision of its own.
fn side_piece<'a>(doc: &StructuredDocument, d: &'a Decision, side: Side) -> Option<&'a SideText> {
    match side {
        Side::Base if doc.degraded_base => None,
        Side::Base => d.base.as_ref(),
        Side::Ours => d.ours.as_ref(),
        Side::Theirs => d.theirs.as_ref(),
    }
}

/// The 0-based line of byte `offset` in `text`.
fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

/// One decor per line of `side`'s full text: every line a decision's byte
/// range on that side covers (first line through last) gets the line
/// editor's wash for the decision's kind ([`row_bg`]); an empty range (an
/// insertion point) tints nothing. Where two decisions share a line the
/// first one's wash stays.
pub(super) fn structured_pane_decors(
    doc: &StructuredDocument,
    side: Side,
    theme: Theme,
) -> Vec<ui::text_area::LineDecor> {
    let text = doc.side_text(side);
    let mut decors: Vec<ui::text_area::LineDecor> = text
        .lines()
        .map(|_| ui::text_area::LineDecor {
            text_color: Some(theme.text),
            ..Default::default()
        })
        .collect();
    for d in &doc.decisions {
        let Some(piece) = side_piece(doc, d, side).filter(|p| !p.range.is_empty()) else {
            continue;
        };
        let bg = row_bg(d.kind, pane_side(side), &theme);
        let first = line_of(text, piece.range.start);
        let last = line_of(text, piece.range.end - 1);
        for decor in decors.iter_mut().take(last + 1).skip(first) {
            decor.bg = decor.bg.or(bg);
        }
    }
    decors
}

/// The 0-based first line of decision `id` on each side, in
/// `[Base, Ours, Theirs]` order; `None` where that side has no text for it.
pub(super) fn jump_lines(doc: &StructuredDocument, id: usize) -> [Option<usize>; 3] {
    let Some(d) = doc.decisions.get(id) else {
        return [None; 3];
    };
    [Side::Base, Side::Ours, Side::Theirs]
        .map(|side| side_piece(doc, d, side).map(|p| line_of(doc.side_text(side), p.range.start)))
}

/// The block that renders one structured strip inside the result editor.
/// Buttons route back through the view's weak handle: a pick lands through
/// [`MergeView::apply_pick`], edit enters the text stage, revisit reopens.
pub(super) fn strip_block(
    spec: &StructuredStrip,
    theme: Theme,
    view: WeakEntity<MergeView>,
) -> ui::text_area::Block {
    let id = spec.id;
    let kind = spec.kind.clone();
    let label = spec.label.clone();
    let detail = spec.detail.clone();
    ui::text_area::Block {
        above_line: spec.above_line,
        height: STRIP_ROWS as u32,
        render: Rc::new(move |_window, _cx| {
            let state = match &kind {
                StripKind::Deciding {
                    has_base,
                    current,
                    focused,
                    both,
                } => ui::StripState::Deciding {
                    has_base: *has_base,
                    current: *current,
                    focused: *focused,
                    // Origin is not offered in Structure mode (spec §6).
                    origin: None,
                    both: *both,
                },
                StripKind::Decided { choice, focused } => ui::StripState::Decided {
                    choice: *choice,
                    focused: *focused,
                },
                StripKind::Locked { reason, focused } => ui::StripState::Locked {
                    reason: reason.clone(),
                    focused: *focused,
                },
            };
            let on_pick = {
                let view = view.clone();
                move |pick: ui::Pick, _: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let pick = match pick {
                        ui::Pick::Ours => Pick::Disk,
                        ui::Pick::Theirs => Pick::Source,
                        ui::Pick::Base => Pick::LastWritten,
                        ui::Pick::Both => Pick::Both,
                        ui::Pick::Origin(_) => {
                            // The strip is built with `origin: None`, so this
                            // button does not exist; a click here is a bug.
                            tomte_core::log_error!(
                                "merge",
                                "an origin pick reached a structured strip (decision {id})"
                            );
                            return;
                        }
                    };
                    view.update(cx, |merge, cx| merge.apply_pick(id, pick, cx))
                        .ok();
                }
            };
            let on_edit = {
                let view = view.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    view.update(cx, |merge, cx| merge.start_edit(id, cx)).ok();
                }
            };
            let on_revisit = {
                let view = view.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    view.update(cx, |merge, cx| merge.revisit(id, cx)).ok();
                }
            };
            div()
                .font_family(".SystemUIFont")
                .h_full()
                .w_full()
                .child(ui::decision_strip(
                    theme,
                    id,
                    state,
                    Some(label.clone()),
                    Some(detail.clone()),
                    Some("merge-strip".into()),
                    on_pick,
                    on_edit,
                    on_revisit,
                ))
                .into_any_element()
        }),
    }
}

/// Structure mode's header controls: progress over the decisions, the
/// origin chip, Next ↓, and Edit. In the Text stage (`text` is `Some`):
/// the invalid-JSON chip when the text does not parse, and Back to
/// structure (reading "Discard edits" once armed) — no Apply (the line
/// editor's Apply maps regions and has no meaning here) and no Discard
/// edit (Back to structure is the way out). Save sits after Cancel, drawn
/// by the view.
pub(super) fn header_controls(
    mut bar: Div,
    state: &StructuredState,
    has_origin: bool,
    text: Option<TextStage>,
    theme: Theme,
    cx: &mut Context<MergeView>,
) -> Div {
    let (decided, total) = state.progress();
    let open = total - decided;
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
    bar.child(ui::button(
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
    ))
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
    use std::collections::HashSet;

    use tomte_app::structured_state::StructuredState;
    use tomte_app::theme::Theme;
    use tomte_core::merge::RegionKind;
    use tomte_core::structured::Side;
    use tomte_core::structured::{Pick, StructuredDocument};

    use super::super::merge::{PaneSide, row_bg};
    use super::{
        StripKind, detail_words, has_picks, invalid_json_line, jump_lines, pick_allowed,
        progress_text, strip_specs, structured_pane_decors, text_row,
    };

    fn state(base: Option<&str>, ours: &str, theirs: &str) -> StructuredState {
        StructuredState::new(StructuredDocument::compute(base, ours, theirs, None).expect("json"))
    }

    // MUTATION: swap the OursOnly and TheirsOnly arms, or drop the
    // duplicate arm → the words name the wrong side.
    #[test]
    fn detail_words_name_the_side_and_the_change() {
        let s = state(
            Some("{\"a\": 1, \"b\": 1, \"c\": 1}"),
            "{\"a\": 2, \"b\": 1, \"c\": 1, \"d\": 1}",
            "{\"a\": 1, \"b\": 3}",
        );
        let words: Vec<String> = s
            .doc
            .decisions
            .iter()
            .map(|d| detail_words(d).to_string())
            .collect();
        let by_path = |p: &str| {
            let d = s.doc.decisions.iter().find(|d| d.path == p).expect(p);
            words[d.id].clone()
        };
        assert_eq!(by_path("a"), "changed on disk");
        assert_eq!(by_path("b"), "changed in source");
        assert_eq!(by_path("c"), "removed in source");
        assert_eq!(by_path("d"), "added on disk");

        let dup = state(Some("{\"a\": 1}"), "{\"a\": 1, \"a\": 2}", "{\"a\": 1}");
        assert_eq!(
            detail_words(&dup.doc.decisions[0]),
            "appears twice on disk · last value used"
        );
        let conflict = state(Some("{\"a\": 1}"), "{\"a\": 2}", "{\"a\": 3}");
        assert_eq!(detail_words(&conflict.doc.decisions[0]), "conflict");
    }

    // MUTATION: drop the `revisiting` guard → a picked row stays Decided
    // while revisited; or ignore the resolution → it never reads Decided.
    #[test]
    fn strip_specs_follow_picks_and_revisits() {
        let mut s = state(Some("{\"a\": 1}"), "{\"a\": 2}", "{\"a\": 3}");
        let specs = strip_specs(&s, &HashSet::new(), None, None);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].label.as_ref(), "a");
        assert!(matches!(
            specs[0].kind,
            StripKind::Deciding {
                current: None,
                has_base: true,
                focused: true,
                both: false
            }
        ));
        s.pick(0, Pick::Disk);
        assert!(matches!(
            strip_specs(&s, &HashSet::new(), None, None)[0].kind,
            StripKind::Decided { .. }
        ));
        let revisiting: HashSet<usize> = [0].into();
        assert!(matches!(
            strip_specs(&s, &revisiting, None, None)[0].kind,
            StripKind::Deciding {
                current: Some(_),
                ..
            }
        ));
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

    // MUTATION: tint by decision index instead of byte range (wash line
    // `d.id`) → the wrong lines wash: lines 0 and 1 tint and font_size's
    // line 2 stays bare.
    // MUTATION: tint only the range's first line → the added `vim` object's
    // closing lines stay bare.
    #[test]
    fn pane_decors_tint_exactly_the_lines_a_decision_covers() {
        let theme = Theme::dark();
        let base = "{\n  \"theme\": 1,\n  \"font_size\": 14\n}\n";
        let ours = concat!(
            "{\n",
            "  \"theme\": 1,\n",
            "  \"font_size\": 15,\n",
            "  \"vim\": {\n",
            "    \"on\": true\n",
            "  }\n",
            "}\n",
        );
        let doc = StructuredDocument::compute(Some(base), ours, base, None).expect("json");
        let decors = structured_pane_decors(&doc, Side::Ours, theme);
        assert_eq!(decors.len(), 7, "one decor per line of disk's text");
        let wash = row_bg(RegionKind::OursOnly, PaneSide::Ours, &theme);
        assert!(wash.is_some());
        let bgs: Vec<_> = decors.iter().map(|d| d.bg).collect();
        assert_eq!(
            bgs,
            vec![None, None, wash, wash, wash, wash, None],
            "font_size's line and every line of the added vim object"
        );
        // Source has neither change: no wash there.
        let theirs = structured_pane_decors(&doc, Side::Theirs, theme);
        assert!(theirs.iter().all(|d| d.bg.is_none()));
    }

    // MUTATION: return the range's LAST line, or drop the `None` for a side
    // without the entry (map it to line 0) → the asserted lines differ.
    // MUTATION: ignore `degraded_base` → the two-way base names a line.
    #[test]
    fn jump_lines_name_each_sides_first_line_or_none_when_absent() {
        let base = "{\n  \"a\": 1,\n  \"b\": 1\n}\n";
        // `c` spans three lines on disk: the jump is to its first.
        let ours = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": {\n    \"x\": 1\n  }\n}\n";
        let theirs = "{\n  \"b\": 3,\n  \"a\": 1\n}\n";
        let doc = StructuredDocument::compute(Some(base), ours, theirs, None).expect("json");
        let id = |p: &str| doc.decisions.iter().find(|d| d.path == p).expect(p).id;
        assert_eq!(jump_lines(&doc, id("b")), [Some(2), Some(2), Some(1)]);
        assert_eq!(
            jump_lines(&doc, id("c")),
            [None, Some(3), None],
            "added on disk: only disk has it"
        );
        assert_eq!(jump_lines(&doc, 99), [None; 3], "unknown id");

        let two = StructuredDocument::compute(None, ours, theirs, None).expect("json");
        let b = two.decisions.iter().find(|d| d.path == "b").expect("b").id;
        assert_eq!(jump_lines(&two, b), [None, Some(2), Some(1)]);
    }

    // MUTATION: count strips strictly above the line → the row is one strip short.
    #[test]
    fn text_row_counts_the_strip_above_its_own_line() {
        assert_eq!(text_row(&[1, 3], 0), 1 + super::STRIP_ROWS);
        assert_eq!(text_row(&[1, 3], 1), 3 + 2 * super::STRIP_ROWS);
    }

    // MUTATION: report "N conflicts" with decided and total swapped, or
    // drop the nothing-to-decide copy.
    #[test]
    fn progress_text_uses_the_spec_copy() {
        let mut s = state(
            Some("{\"a\": 1, \"b\": 1}"),
            "{\"a\": 2, \"b\": 2}",
            "{\"a\": 3, \"b\": 3}",
        );
        assert_eq!(progress_text(&s), "2 conflicts, 0 resolved");
        assert!(!has_picks(&s));
        s.pick(0, Pick::Disk);
        assert_eq!(progress_text(&s), "2 conflicts, 1 resolved");
        assert!(has_picks(&s));
        let auto = state(None, "{\"a\": 2}", "{\"a\": 3}");
        assert_eq!(
            progress_text(&auto),
            "auto-resolved · 1 changed entry · review below"
        );
        let same = state(Some("{\"a\": 1}"), "{ \"a\":1 }", "{\"a\": 1}");
        assert_eq!(
            progress_text(&same),
            "same settings, formatting differs · Save keeps the source layout"
        );
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
}
