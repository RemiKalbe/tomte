//! Merge-region components: the decision strip and tinted provenance rows
//! (merge-editor-v2 spec, Zed conflict_view.rs pattern — always-visible
//! labeled buttons above the region; read-only provenance is structural).
//!
//! Tint mapping (established app vocabulary, NOT GitHub's purple):
//! ours/disk = drift amber, theirs/source = accent blue.

use std::collections::HashMap;

use gpui::{App, ClickEvent, Div, ElementId, Rgba, SharedString, Window, div, prelude::*};

use crate::components::button::{ButtonSize, ButtonVariant, button};
use crate::components::mono::{line_gutter, line_text, mono_line};
use crate::components::tooltip::text_tooltip;
use crate::theme::Theme;

/// Which side a provenance block shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Disk (ours) — drift amber.
    Ours,
    /// Source (theirs) — accent blue.
    Theirs,
    /// Last-written base — muted.
    Base,
}

impl Side {
    pub fn tint(self, theme: Theme) -> Rgba {
        match self {
            Self::Ours => theme.drift,
            Self::Theirs => theme.accent,
            Self::Base => theme.text_muted,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ours => "on disk",
            Self::Theirs => "source",
            Self::Base => "last written",
        }
    }
}

/// The choices a strip can display/emit. Mirrors the engine's Choice minus
/// Edited's payload (the strip only names it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceKind {
    Ours,
    Theirs,
    Base,
    Both,
    Edited,
}

impl ChoiceKind {
    /// The provenance tint this choice reads as (matches the result rows).
    pub fn tint(self, theme: Theme) -> Rgba {
        match self {
            Self::Ours => theme.drift,
            Self::Theirs => theme.accent,
            Self::Base => theme.text_muted,
            Self::Both | Self::Edited => theme.ok,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ours => "disk",
            Self::Theirs => "source",
            Self::Base => "last written",
            Self::Both => "both",
            Self::Edited => "edited",
        }
    }
}

/// Strip state drives which buttons show and which reads selected.
pub enum StripState {
    /// Conflict awaiting a decision (or an auto-resolved region being
    /// revisited): full button row. `auto` marks the engine's default
    /// choice for non-conflict regions, rendered as the current selection.
    Deciding {
        has_base: bool,
        current: Option<ChoiceKind>,
        focused: bool,
    },
    /// A decision exists; collapsed affordance names it and offers revisit.
    /// `focused`: the region cursor sits here (Next ↓ walks decided regions
    /// too once every conflict is resolved).
    Decided { choice: ChoiceKind, focused: bool },
}

/// Decision strip rendered above a changed region, styled after Zed's
/// multibuffer buffer header (`editor/src/element/header.rs`): an INSET
/// rounded card — 4px padding all round, hairline border, subtle surface
/// fill, hover highlight — rather than a full-bleed tinted bar. Two rows
/// tall so the controls breathe. A leading dot + label name the region's
/// state; the actions sit on the trailing edge.
///
/// All behavior arrives via `on_pick` / `on_edit` / `on_revisit` so the
/// builder stays pure.
#[allow(clippy::too_many_arguments)]
pub fn decision_strip(
    theme: Theme,
    region_ix: usize,
    state: StripState,
    on_pick: impl Fn(ChoiceKind, &ClickEvent, &mut Window, &mut App) + Clone + 'static,
    on_edit: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_revisit: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let (accent, focused) = match &state {
        StripState::Deciding {
            current, focused, ..
        } => (
            if current.is_none() {
                theme.conflict
            } else {
                theme.text_muted
            },
            *focused,
        ),
        StripState::Decided { choice, focused } => (choice.tint(theme), *focused),
    };
    // Zed's header: `border_focused` when the header owns the cursor,
    // plain `border` otherwise.
    let border = if focused { accent } else { theme.border };
    let card = div()
        .w_full()
        // Explicit padding, NOT `h_full` + `items_center`: an ancestor that
        // doesn't propagate height collapses the card onto the buttons and
        // their edges end up flush against its border (2026-08-25).
        .px_2()
        .py_1()
        .flex()
        .items_center()
        .gap_1p5()
        .text_xs()
        .rounded_md()
        .border_1()
        .border_color(border)
        .bg(theme.surface)
        // State dot: the same vocabulary as the provenance labels.
        .child(div().w_1p5().h_1p5().flex_none().rounded_full().bg(accent));

    let body = match state {
        StripState::Deciding {
            has_base,
            current,
            focused,
        } => {
            let pick_button =
                |id: &'static str, kind: ChoiceKind, label: &'static str, tint: Rgba| {
                    let on_pick = on_pick.clone();
                    let selected = current == Some(kind);
                    button(
                        theme,
                        ElementId::named_usize(id, region_ix),
                        label.into(),
                        ButtonVariant::Ring(tint),
                        ButtonSize::Micro,
                        move |ev, window, cx| on_pick(kind, ev, window, cx),
                    )
                    .when(selected, |el| el.bg(Theme::wash(tint, 0.2)))
                };
            card.child(
                div()
                    .text_color(if focused { accent } else { theme.text_muted })
                    .child(if current.is_none() {
                        "conflict"
                    } else {
                        // "merged" describes the whole-file operation; ONE
                        // region that needed no human decision was resolved.
                        "auto-resolved"
                    }),
            )
            // Push the actions to the trailing edge (Zed's justify_between).
            .child(div().flex_1())
            .child(pick_button(
                "pick-ours",
                ChoiceKind::Ours,
                "disk",
                theme.drift,
            ))
            .child(pick_button(
                "pick-theirs",
                ChoiceKind::Theirs,
                "source",
                theme.accent,
            ))
            .when(has_base, |el| {
                el.child(pick_button(
                    "pick-base",
                    ChoiceKind::Base,
                    "last written",
                    theme.text_muted,
                ))
            })
            .child(pick_button("pick-both", ChoiceKind::Both, "both", theme.ok))
            .child(divider(theme))
            .child(button(
                theme,
                ElementId::named_usize("edit-region", region_ix),
                "edit".into(),
                ButtonVariant::Ghost,
                ButtonSize::Micro,
                on_edit,
            ))
        }
        StripState::Decided { choice, .. } => card
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!("resolved · {}", choice.label()))),
            )
            .child(div().flex_1())
            .child(button(
                theme,
                ElementId::named_usize("revisit-region", region_ix),
                "revisit".into(),
                ButtonVariant::Ring(theme.text_muted),
                ButtonSize::Micro,
                on_revisit,
            ))
            .child(divider(theme))
            .child(button(
                theme,
                ElementId::named_usize("edit-region", region_ix),
                "edit".into(),
                ButtonVariant::Ghost,
                ButtonSize::Micro,
                on_edit,
            )),
    };

    // The 4px inset (Zed's BUFFER_HEADER_PADDING) is what makes the strip
    // read as a card floating in the buffer instead of a band across it.
    div()
        .w_full()
        .h_full()
        .px_1()
        .flex()
        .items_center()
        .child(body)
}

/// Hairline vertical rule separating action groups inside a strip.
fn divider(theme: Theme) -> Div {
    div()
        .w_px()
        .h_3()
        .flex_none()
        .mx_0p5()
        .bg(Theme::wash(theme.text_muted, 0.35))
}

/// Header line naming a provenance block ("on disk", "source"…).
pub fn provenance_label(theme: Theme, side: Side) -> Div {
    let tint = side.tint(theme);
    div()
        .h_5()
        .px_2()
        .flex()
        .items_center()
        .gap_1p5()
        .text_xs()
        .child(div().w_1p5().h_1p5().rounded_full().bg(tint))
        .child(div().text_color(tint).child(side.label()))
}

/// Width of the leading "template" column when a block has protected lines
/// (mirrors the pane gutter: uniform per block, the chip can never clip).
pub fn provenance_rows(
    theme: Theme,
    side: Side,
    lines: &[SharedString],
    protected: &HashMap<usize, SharedString>,
) -> Div {
    let tint = side.tint(theme);
    div()
        .flex()
        .flex_col()
        .border_l_2()
        .border_color(tint)
        .bg(Theme::wash(tint, 0.07))
        .children(lines.iter().enumerate().map(|(ix, line)| {
            let tip = protected.get(&ix).cloned();
            let row = mono_line(theme)
                .when(tip.is_some(), |el| el.bg(Theme::wash(theme.drift, 0.15)))
                .child(line_gutter(
                    theme.drift,
                    if tip.is_some() { "{}" } else { "" },
                ))
                .child(line_text(line.clone()));
            match tip {
                Some(tip) => row
                    .id(ElementId::named_usize("prov-tmpl", ix))
                    .tooltip(text_tooltip(tip))
                    .into_any_element(),
                None => row.into_any_element(),
            }
        }))
}
