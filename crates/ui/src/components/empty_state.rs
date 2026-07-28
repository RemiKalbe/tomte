//! Empty and note states — never a lone sentence floating in a void.

use gpui::{Div, Rgba, SharedString, div, prelude::*};

use crate::theme::Theme;

/// Structured empty block: tinted glyph + fact + muted context, top-aligned
/// where the content would start.
pub fn empty_state(
    theme: Theme,
    glyph: &'static str,
    tint: Rgba,
    primary: impl Into<SharedString>,
    secondary: impl Into<SharedString>,
) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .pt_16()
        .child(div().text_xl().text_color(tint).child(glyph))
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(primary.into()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.text_muted)
                .child(secondary.into()),
        )
}

/// Dead-centered single-line note (loading, "select a file to review", …).
pub fn centered_note(theme: Theme, text: SharedString, color: Rgba) -> Div {
    let _ = theme;
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(color)
        .child(text)
}
