//! List chassis: interactive rows and the bordered list pane.

use gpui::{App, ClickEvent, Div, ElementId, Rgba, Stateful, Window, div, prelude::*};

use crate::theme::Theme;

/// The row chassis every clickable list in the app rides: h_7, px_2,
/// rounded_sm, wash hover, deeper wash when selected. Callers append columns.
pub fn list_row(
    theme: Theme,
    id: impl Into<ElementId>,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .h_7()
        .px_2()
        .rounded_sm()
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .when(selected, |el| el.bg(Theme::wash(theme.text, 0.08)))
        .hover(|el| el.bg(Theme::wash(theme.text, 0.06)))
        .on_click(on_click)
}

/// Same chassis without identity or interactivity (inert display rows).
pub fn inert_list_row(theme: Theme) -> Div {
    let _ = theme;
    div()
        .h_7()
        .px_2()
        .rounded_sm()
        .flex()
        .items_center()
        .gap_2()
}

/// The label strip a pane carries on its top edge: tint dot + muted label.
pub fn pane_label(theme: Theme, text: &'static str, tint: Rgba) -> Div {
    div()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(theme.border)
        .flex()
        .items_center()
        .gap_1p5()
        .child(div().w_1p5().h_1p5().rounded_full().bg(tint))
        .child(div().text_xs().text_color(theme.text_muted).child(text))
}

/// Bordered rounded pane that hosts a `uniform_list` (or a centered note).
/// `label`: the merge-editor pane strip — tint dot + muted label attached to
/// the pane itself, never floating in a toolbar.
pub fn list_pane(theme: Theme, label: Option<(&'static str, Rgba)>) -> Div {
    div()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .flex()
        .flex_col()
        .when_some(label, |el, (text, tint)| {
            el.child(pane_label(theme, text, tint))
        })
}
