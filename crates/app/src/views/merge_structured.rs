//! Structure mode of the merge editor (structured-merge spec §6): the strip
//! plan for a [`StructuredState`], the words each strip shows, the lock
//! hover text, the mode toggle, and the small pure rules the view's
//! Structure branches call. Everything here that needs no gpui is a plain
//! function over the state, so the rules are testable without a window;
//! `merge.rs` keeps only thin branches on [`MergeMode`].

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{App, ClickEvent, Context, Div, SharedString, WeakEntity, Window, div, prelude::*};
use tomte_app::structured_state::StructuredState;
use tomte_app::theme::Theme;
use tomte_core::merge::RegionKind;
use tomte_core::structured::{Change, Decision, Pick, Side};
use tomte_core::template::anchor::SpanMap;
use tomte_ui::components as ui;

use super::merge::{MergeMode, MergeView, STRIP_ROWS, span_reason};

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
        return format!("auto-resolved · {changed} changed {noun} — review below");
    }
    let s = if total == 1 { "" } else { "s" };
    format!("{total} conflict{s}, {decided} resolved")
}

/// Input-pane decorations in Structure mode: no region washes (the line
/// regions mean nothing here), only the source pane's `{}` gutter on lines
/// a template generated. `protected[i]` is line `i`'s hover text.
pub(super) fn pane_decors(
    protected: impl Iterator<Item = Option<SharedString>>,
    theme: Theme,
) -> Vec<ui::text_area::LineDecor> {
    protected
        .map(|tip| ui::text_area::LineDecor {
            text_color: Some(theme.text),
            gutter: tip.is_some().then(|| ("{}".into(), theme.drift)),
            tooltip: tip,
            ..Default::default()
        })
        .collect()
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
/// origin chip, Next ↓, and Edit (or, in the Text stage, Discard edit —
/// the line editor's Apply maps regions and has no meaning here).
pub(super) fn header_controls(
    mut bar: Div,
    state: &StructuredState,
    has_origin: bool,
    editing: bool,
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
    if editing {
        return bar.child(ui::button(
            theme,
            "merge-edit-cancel",
            "Discard edit".into(),
            ui::ButtonVariant::Ghost,
            ui::ButtonSize::Sm,
            cx.listener(|view, _ev, _window, cx| view.cancel_edit(cx)),
        ));
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
    use tomte_core::structured::{Pick, StructuredDocument};

    use super::{
        StripKind, detail_words, has_picks, pick_allowed, progress_text, strip_specs, text_row,
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
        let same = state(Some("{\"a\": 1}"), "{ \"a\":1 }", "{\"a\": 1}");
        assert_eq!(
            progress_text(&same),
            "same settings, formatting differs · Save keeps the source layout"
        );
    }
}
