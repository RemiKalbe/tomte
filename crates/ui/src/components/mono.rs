//! Code-line primitives. Deliberately NOT one configurable line component:
//! a chassis (geometry + typography), a gutter cell, and a text cell —
//! tints, background washes, word-diff highlight spans, and inline controls
//! are composition at the call site.

use gpui::{Div, IntoElement, Rgba, SharedString, div, prelude::*};

use crate::theme::Theme;

/// One code-line chassis: h_5, px_2, gap_1, Menlo text_xs, nowrap. Callers
/// refine color/background and append [`line_gutter`] / [`line_text`] cells.
pub fn mono_line(theme: Theme) -> Div {
    div()
        .h_5()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .font_family("Menlo")
        .text_xs()
        .text_color(theme.text)
        .whitespace_nowrap()
}

/// Fixed marker cell (diff `+`/`−`, protected `⚿`, …): w_4, non-shrinking.
pub fn line_gutter(tint: Rgba, glyph: impl Into<SharedString>) -> Div {
    div()
        .w_4()
        .flex_shrink_0()
        .text_color(tint)
        .child(glyph.into())
}

/// The flexing content cell of a mono line; clips at the pane edge.
pub fn line_text(content: impl IntoElement) -> Div {
    div().flex_1().min_w_0().overflow_hidden().child(content)
}
