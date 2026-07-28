//! Dropdown trigger button (the popover menu lives in [`super::menu`]).

use gpui::{
    App, ClickEvent, Div, ElementId, SharedString, Stateful, Window, div, prelude::*,
};

use crate::components::icon::chevron_down;
use crate::theme::Theme;

/// Filled dropdown trigger: current value + chevron.
pub fn dropdown_button(
    theme: Theme,
    id: impl Into<ElementId>,
    label: SharedString,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .h_6()
        .px_2p5()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .flex()
        .items_center()
        .gap_1p5()
        .text_sm()
        .map(|el| {
            if enabled {
                el.text_color(theme.text)
                    .cursor_pointer()
                    .hover(|el| el.bg(Theme::wash(theme.text, 0.08)))
                    .on_click(on_click)
            } else {
                el.text_color(theme.text_muted)
            }
        })
        .child(label)
        .child(chevron_down(theme))
}

