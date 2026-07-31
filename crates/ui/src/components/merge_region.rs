//! Merge-region components: the decision strip and tinted provenance rows
//! (merge-editor-v2 spec, Zed conflict_view.rs pattern — always-visible
//! labeled buttons above the region; read-only provenance is structural).
//!
//! Tint mapping (established app vocabulary, NOT GitHub's purple):
//! ours/disk = drift amber, theirs/source = accent blue.

use std::collections::HashSet;

use gpui::{
    App, ClickEvent, Div, ElementId, Rgba, SharedString, Window, div, prelude::*,
};

use crate::components::button::{ButtonSize, ButtonVariant, button};
use crate::components::mono::{line_gutter, line_text, mono_line};
use crate::theme::Theme;

/// Which side a provenance block shows.
#[derive(Clone, Copy, PartialEq, Eq)]
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
    Decided { choice: ChoiceKind },
}

/// One-line decision strip rendered above a changed region (Zed
/// conflict_view.rs:325-445 adapted: interleaved layout, not a block
/// decoration). All behavior arrives via `on_pick` / `on_edit` /
/// `on_revisit` so the builder stays pure.
#[allow(clippy::too_many_arguments)]
pub fn decision_strip(
    theme: Theme,
    region_ix: usize,
    state: StripState,
    on_pick: impl Fn(ChoiceKind, &ClickEvent, &mut Window, &mut App) + Clone + 'static,
    on_edit: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_revisit: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let strip = div()
        .h_6()
        .px_2()
        .flex()
        .items_center()
        .gap_1()
        .text_xs()
        .bg(Theme::wash(theme.text_muted, 0.06));
    match state {
        StripState::Deciding {
            has_base,
            current,
            focused,
        } => {
            let pick_button = |id: &'static str,
                               kind: ChoiceKind,
                               label: &'static str,
                               tint: Rgba| {
                let on_pick = on_pick.clone();
                let selected = current == Some(kind);
                button(
                    theme,
                    ElementId::named_usize(id, region_ix),
                    label.into(),
                    ButtonVariant::Outline(tint),
                    ButtonSize::Micro,
                    move |ev, window, cx| on_pick(kind, ev, window, cx),
                )
                .when(selected, |el| el.bg(Theme::wash(tint, 0.2)))
            };
            strip
                .child(
                    div()
                        .text_color(if focused {
                            theme.conflict
                        } else {
                            theme.text_muted
                        })
                        .child(if current.is_none() {
                            "‹pick›"
                        } else {
                            "‹auto›"
                        }),
                )
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
                .child(pick_button(
                    "pick-both",
                    ChoiceKind::Both,
                    "both",
                    theme.ok,
                ))
                .child(div().w_2())
                .child(button(
                    theme,
                    ElementId::named_usize("edit-region", region_ix),
                    "edit".into(),
                    ButtonVariant::Ghost,
                    ButtonSize::Micro,
                    on_edit,
                ))
        }
        StripState::Decided { choice } => strip
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!("decided: {}", choice.label()))),
            )
            .child(button(
                theme,
                ElementId::named_usize("revisit-region", region_ix),
                "revisit".into(),
                ButtonVariant::Ghost,
                ButtonSize::Micro,
                on_revisit,
            )),
    }
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

/// Read-only tinted mono rows for one side of an undecided region.
/// Structurally read-only: these are divs, there is no edit path.
/// `protected`: line indexes carrying the template-protected glyph.
pub fn provenance_rows(
    theme: Theme,
    side: Side,
    lines: &[SharedString],
    protected: &HashSet<usize>,
) -> Div {
    let tint = side.tint(theme);
    div()
        .flex()
        .flex_col()
        .border_l_2()
        .border_color(tint)
        .bg(Theme::wash(tint, 0.07))
        .children(lines.iter().enumerate().map(|(ix, line)| {
            mono_line(theme)
                .child(line_gutter(
                    theme.drift,
                    if protected.contains(&ix) { "⚿" } else { "" },
                ))
                .child(line_text(line.clone()))
        }))
}
