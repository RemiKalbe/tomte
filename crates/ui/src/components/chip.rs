//! Chips: inline code, class labels, count badges.

use gpui::{Div, SharedString, div, prelude::*, px};

use crate::theme::Theme;

/// Inline mono code chip (markdown `code` look). 11px Menlo: at equal point
/// size Menlo's x-height reads larger than the UI font, so one point down
/// sits flush inside a 12px sentence.
pub fn code_chip(theme: Theme, text: impl Into<SharedString>) -> Div {
    div()
        .px_1()
        .py_0p5()
        .rounded_sm()
        .bg(Theme::wash(theme.text_muted, 0.15))
        .font_family("Menlo")
        .text_size(px(11.))
        .text_color(theme.text)
        .whitespace_nowrap()
        .child(text.into())
}

