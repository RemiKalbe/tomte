//! Tri-state checkbox and the folder row of a file tree — pure builders
//! (Theme + data + plain click handlers, no entity coupling), like every
//! component here.

use gpui::{
    App, ClickEvent, Div, ElementId, SharedString, Stateful, Transformation, Window, div,
    prelude::*, px, radians,
};

use crate::theme::Theme;

use super::chip::{ChipVariant, chip};
use super::icon::chevron_down;

/// What a checkbox over a SET shows: none, some, or all of the set ticked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Off,
    Partial,
    On,
}

impl CheckState {
    /// The state of a checkbox standing for `total` items of which
    /// `selected` are ticked. An empty set is Off: there is nothing to be
    /// "all of".
    pub fn of(selected: usize, total: usize) -> Self {
        if total == 0 || selected == 0 {
            Self::Off
        } else if selected == total {
            Self::On
        } else {
            Self::Partial
        }
    }

    /// Where a click takes it: Off and Partial complete the set, On clears
    /// it — the macOS rule, and the one that makes "tick the folder" mean
    /// "everything under it" even when some of it already was.
    pub fn toggled(self) -> Self {
        match self {
            Self::On => Self::Off,
            Self::Off | Self::Partial => Self::On,
        }
    }
}

/// A 14px square. Off: hairline border. On: accent fill with a check.
/// Partial: accent fill with a dash. The handler runs INSTEAD of the row's
/// own click: the click is stopped here so a tick never also selects.
pub fn checkbox(
    theme: Theme,
    id: impl Into<ElementId>,
    state: CheckState,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let filled = state != CheckState::Off;
    div()
        .id(id)
        .w(px(14.))
        .h(px(14.))
        .flex_none()
        .rounded(px(3.))
        .border_1()
        .border_color(if filled { theme.accent } else { theme.border })
        .when(filled, |el| el.bg(theme.accent))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when(state == CheckState::On, |el| {
            el.text_color(theme.bg).text_xs().child("✓")
        })
        .when(state == CheckState::Partial, |el| {
            el.child(div().w(px(6.)).h(px(2.)).rounded(px(1.)).bg(theme.bg))
        })
        .on_click(move |ev, window, cx| {
            cx.stop_propagation();
            on_click(ev, window, cx);
        })
}

/// One folder of a file tree: chevron (rotated when collapsed), the label
/// (ending in `/`), a count chip, and — when the tree is in selection
/// mode — a checkbox for everything under it.
pub struct FolderRow {
    pub depth: usize,
    pub label: SharedString,
    pub collapsed: bool,
    /// "3" or "3 of 5" — what a tick reaches, of what is there.
    pub count: SharedString,
    pub check: CheckState,
    /// Forced visible (a selection exists) rather than hover-revealed.
    pub show_check: bool,
}

/// Indent per tree level, in px.
pub const TREE_INDENT: f32 = 12.;

pub fn folder_row(
    theme: Theme,
    id: impl Into<ElementId>,
    row: FolderRow,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_check: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let id = id.into();
    // `NamedChild` is gpui's own "stable id for a labeled child of an
    // element" (window.rs) — cleaner than hand-rolling one from `Debug`.
    let check_id = ElementId::NamedChild(Box::new(id.clone()), "check".into());
    let chevron = if row.collapsed {
        chevron_down(theme).with_transformation(Transformation::rotate(radians(
            -std::f32::consts::FRAC_PI_2,
        )))
    } else {
        chevron_down(theme)
    };
    div()
        .id(id)
        .group("tree-folder")
        .px_3()
        .py_1()
        .pl(px(12. + TREE_INDENT * row.depth as f32))
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .hover(|el| el.bg(Theme::wash(theme.text, 0.04)))
        .child(
            checkbox(theme, check_id, row.check, on_check)
                .opacity(if row.show_check { 1. } else { 0. })
                .group_hover("tree-folder", |el| el.opacity(1.)),
        )
        .child(chevron)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(theme.text)
                .line_clamp(1)
                .child(row.label),
        )
        .child(chip(theme, row.count, ChipVariant::Wash(theme.text_muted)))
        .on_click(on_toggle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MUTATION: make `of` return `Partial` for `selected == total` → the
    /// second assertion fails; make `toggled` map Partial → Off → the last
    /// fails.
    #[test]
    fn check_state_of_counts_and_toggles_partial_to_on() {
        assert_eq!(CheckState::of(0, 3), CheckState::Off);
        assert_eq!(CheckState::of(3, 3), CheckState::On);
        assert_eq!(CheckState::of(1, 3), CheckState::Partial);
        assert_eq!(
            CheckState::of(0, 0),
            CheckState::Off,
            "nothing to select is Off, not On"
        );
        assert_eq!(CheckState::Off.toggled(), CheckState::On);
        assert_eq!(CheckState::On.toggled(), CheckState::Off);
        assert_eq!(
            CheckState::Partial.toggled(),
            CheckState::On,
            "a partial tick completes the set"
        );
    }
}
