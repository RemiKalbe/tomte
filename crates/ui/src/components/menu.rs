//! Popover menus: container, selectable rows, inert status lines.

use gpui::{
    App, ClickEvent, Div, ElementId, SharedString, Stateful, Window, div, prelude::*, px,
};

use crate::theme::Theme;

/// Popover menu container (caller adds `.id()`, dismiss handling, children).
pub fn menu(theme: Theme) -> Div {
    div()
        .min_w(px(260.))
        .max_w(px(380.))
        .p_1()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .shadow_md()
        .flex()
        .flex_col()
}

/// One selectable menu row: ✓ gutter, label, optional muted sublabel.
pub fn menu_row(
    theme: Theme,
    id: impl Into<ElementId>,
    label: SharedString,
    sublabel: Option<SharedString>,
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
        .hover(|el| el.bg(Theme::wash(theme.text, 0.06)))
        .child(
            div()
                .w_4()
                .flex_none()
                .text_sm()
                .text_color(theme.accent)
                .child(if selected { "✓" } else { "" }),
        )
        .child(div().text_sm().text_color(theme.text).child(label))
        .when_some(sublabel, |el, s| {
            el.child(div().text_xs().text_color(theme.text_muted).child(s))
        })
        .on_click(on_click)
}

/// An unselectable status line inside a menu (loading / unavailable).
pub fn inert_menu_line(theme: Theme, text: &'static str, color: gpui::Rgba) -> Div {
    let _ = theme;
    div()
        .h_7()
        .px_2()
        .pl_8()
        .flex()
        .items_center()
        .text_xs()
        .text_color(color)
        .child(text)
}

