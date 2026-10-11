//! Buttons: one chassis, three sizes, four variants (the most-shared
//! component in the app — ~20 consumers across every view).
//!
//! The variant is chosen by THE SURFACE THE BUTTON SITS ON (2026-08-25
//! decision), not by importance:
//!
//! | surface                                   | variant  |
//! |-------------------------------------------|----------|
//! | plain app background                      | `Tonal`  |
//! | inside a container that draws its own border | `Ring` |
//! | on a filled/tinted field (banners)        | `Solid`  |
//! | anywhere, for a tertiary action           | `Ghost`  |
//!
//! Hard 1px full-strength borders are gone: weight comes from a tint field,
//! and where an edge is still wanted (`Ring`) it is a low-alpha wash of the
//! button's own tint so it never competes with the container's border.
//!
//! Neutral actions take `theme.text_muted` as their tint, never `theme.text`
//! — foreground-strength gray reads as an emphasis it doesn't deserve.

use gpui::{
    App, ClickEvent, Div, ElementId, Rgba, SharedString, Stateful, Svg, Window, div, prelude::*,
};

use crate::components::tooltip::text_tooltip;
use crate::theme::Theme;

#[derive(Clone, Copy)]
pub enum ButtonVariant {
    /// Borderless field of the tint at low alpha, tinted label. THE DEFAULT:
    /// use it on the plain app background.
    Tonal(Rgba),
    /// Tonal plus a faint ring in the button's own tint — for buttons inside
    /// a container that already draws a border (decision strips, the floating
    /// toolbar pill, dashboard tiles), so the two edges don't compete.
    Ring(Rgba),
    /// Full-tint fill with a contrasting label — for buttons sitting ON a
    /// filled/tinted field (banners), where a wash would dissolve into it.
    Solid(Rgba),
    /// No fill at rest; materializes on hover. Tertiary actions.
    Ghost,
}

#[derive(Clone, Copy)]
pub enum ButtonSize {
    /// h_6 px_2p5 text_sm — headers, toolbars.
    Md,
    /// h_5p5 px_2 text_xs — pane headers, compact actions.
    Sm,
    /// h_5 px_1p5 text_xs — in-row actions (decision strips).
    Micro,
}

fn chassis(id: impl Into<ElementId>, size: ButtonSize, label: SharedString) -> Stateful<Div> {
    let base = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .whitespace_nowrap();
    match size {
        ButtonSize::Md => base.h_6().px_2p5().text_sm(),
        ButtonSize::Sm => base.h(gpui::px(22.)).px_2().text_xs(),
        ButtonSize::Micro => base.h_5().px_1p5().text_xs(),
    }
    .child(label)
}

pub fn button(
    theme: Theme,
    id: impl Into<ElementId>,
    label: SharedString,
    variant: ButtonVariant,
    size: ButtonSize,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let el = chassis(id, size, label).cursor_pointer().on_click(on_click);
    match variant {
        ButtonVariant::Tonal(tint) => el
            .bg(Theme::wash(tint, 0.14))
            .text_color(tint)
            .hover(move |el| el.bg(Theme::wash(tint, 0.22))),
        ButtonVariant::Ring(tint) => el
            .bg(Theme::wash(tint, 0.12))
            .border_1()
            .border_color(Theme::wash(tint, 0.22))
            .text_color(tint)
            .hover(move |el| {
                el.bg(Theme::wash(tint, 0.2))
                    .border_color(Theme::wash(tint, 0.35))
            }),
        ButtonVariant::Solid(tint) => el
            .bg(tint)
            // The app background always contrasts with a full-strength tint
            // in both themes — that is what makes these tokens tokens.
            .text_color(theme.bg)
            .font_weight(gpui::FontWeight::MEDIUM)
            .hover(move |el| el.bg(Theme::wash(tint, 0.82))),
        ButtonVariant::Ghost => el
            .text_color(theme.text_muted)
            .hover(move |el| el.bg(Theme::wash(theme.text, 0.08)).text_color(theme.text)),
    }
}

/// Disabled twin: a flat neutral field with muted text, no handler, and an
/// optional tooltip explaining why it can't be pressed.
pub fn disabled_button(
    theme: Theme,
    id: impl Into<ElementId>,
    label: SharedString,
    size: ButtonSize,
    tooltip: Option<SharedString>,
) -> Stateful<Div> {
    chassis(id, size, label)
        .bg(Theme::wash(theme.text_muted, 0.08))
        .text_color(Theme::wash(theme.text_muted, 0.65))
        .when_some(tooltip, |el, text| el.tooltip(text_tooltip(text)))
}

/// A 20×20 icon TOGGLE: ghost at rest, the `Tonal(accent)` field and an
/// accent-tinted icon when `active` — a toolbar's Bold, not a segmented
/// control. `icon` arrives already sized (see `icon::folder_tree`); this
/// only tints it and adds the hover wash, the tooltip and the click. The
/// click stops propagation: a toggle that lives on a clickable row (a
/// group header, a list row) must not also fire the row.
pub fn icon_button(
    theme: Theme,
    id: impl Into<ElementId>,
    icon: Svg,
    active: bool,
    tooltip: SharedString,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let accent = theme.accent;
    div()
        .id(id)
        .w_5()
        .h_5()
        .flex_none()
        .rounded_md()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when(active, |el| el.bg(Theme::wash(accent, 0.14)))
        .hover(move |el| {
            if active {
                el.bg(Theme::wash(accent, 0.22))
            } else {
                el.bg(Theme::wash(theme.text, 0.08))
            }
        })
        .child(icon.text_color(if active { accent } else { theme.text_muted }))
        .tooltip(text_tooltip(tooltip))
        .on_click(move |ev, window, cx| {
            cx.stop_propagation();
            on_click(ev, window, cx);
        })
}
