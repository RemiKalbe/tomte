//! Segmented numeric stepper (Zed's number-field shape).

use gpui::{
    App, ClickEvent, Div, SharedString, Stateful, Window, div, prelude::*,
};

use crate::theme::Theme;

/// Segmented numeric stepper: `− │ value │ +` in one bordered group (Zed's
/// number-field shape).
pub fn stepper(
    theme: Theme,
    dec_id: &'static str,
    inc_id: &'static str,
    value: SharedString,
    dec_enabled: bool,
    inc_enabled: bool,
    on_dec: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_inc: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    div()
        .flex()
        .h_6()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .child(seg_button(dec_id, "−", dec_enabled, theme, on_dec))
        .child(
            div()
                .w_16()
                .border_l_1()
                .border_r_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.text_muted)
                .child(value),
        )
        .child(seg_button(inc_id, "+", inc_enabled, theme, on_inc))
}

/// − / + segment; disabled renders muted with no handler.
fn seg_button(
    id: &'static str,
    label: &'static str,
    enabled: bool,
    theme: Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .w_6()
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(if enabled {
            theme.text
        } else {
            theme.text_muted
        })
        .child(label)
        .when(enabled, |el| {
            el.cursor_pointer()
                .hover(|el| el.bg(Theme::wash(theme.text, 0.08)))
                .on_click(on_click)
        })
}

