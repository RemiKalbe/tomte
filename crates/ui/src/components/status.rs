//! Status lines: one tinted text_xs line, everywhere ("working…", "Saved ·
//! sync daemon restarting", "2 conflicts left"…).

use gpui::{Div, SharedString, div, prelude::*, px};

use crate::components::tooltip::text_tooltip;
use crate::theme::Theme;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StatusTone {
    Muted,
    Ok,
    Drift,
    Conflict,
}

impl StatusTone {
    pub fn color(self, theme: Theme) -> gpui::Rgba {
        match self {
            Self::Muted => theme.text_muted,
            Self::Ok => theme.ok,
            Self::Drift => theme.drift,
            Self::Conflict => theme.conflict,
        }
    }
}

/// Plain tinted status line; padding belongs to the caller.
pub fn status_text(theme: Theme, text: impl Into<SharedString>, tone: StatusTone) -> Div {
    div()
        .text_xs()
        .text_color(tone.color(theme))
        .child(text.into())
}

/// Dot-led status line for fixed-width chrome (the sidebar footer): ● +
/// one-line text with a REAL ellipsis and the full text in a tooltip.
/// Definite width + line_clamp(1) is deliberate — gpui's `.truncate()`
/// (whitespace_nowrap) memoizes the first unconstrained measure and never
/// ellipsizes; see the czui-ui design notes.
pub fn status_dot_line(
    theme: Theme,
    dot: StatusTone,
    text: SharedString,
    width: gpui::Pixels,
) -> gpui::Stateful<Div> {
    let full = text.clone();
    div()
        .id("status-dot-line")
        .flex()
        .items_center()
        .gap_1p5()
        .text_xs()
        .text_color(theme.text_muted)
        .child(div().text_color(dot.color(theme)).child("●"))
        .child(
            div()
                .w(width - px(20.))
                .flex_none()
                .text_ellipsis()
                .line_clamp(1)
                .child(text),
        )
        .tooltip(text_tooltip(full))
}
