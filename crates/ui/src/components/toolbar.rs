//! Floating action pill (bottom-anchored toolbars).

use gpui::{Div, div, prelude::*};

use crate::theme::Theme;

/// The floating action pill (bottom-anchored toolbars).
pub fn toolbar_pill(theme: Theme) -> Div {
    div()
        .px_3()
        .py_2()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .shadow_md()
        .flex()
        .items_center()
        .gap_3()
}

