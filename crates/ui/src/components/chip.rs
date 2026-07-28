//! Chips: class labels, count badges, inline code.

use gpui::{Div, Rgba, SharedString, div, prelude::*, px};

use crate::theme::Theme;

#[derive(Clone, Copy)]
pub enum ChipVariant {
    /// Tinted text on wash(tint, 0.15) — class chips, count badges.
    Wash(Rgba),
    /// Tinted text and 1px tinted border — provenance class chips.
    Outline(Rgba),
    /// Menlo 11px on wash(text_muted, 0.15) — paths, env vars, commands.
    /// (One point below the 12px UI text: Menlo's x-height reads larger at
    /// equal size.) Add `.py_0()` for inline-in-sentence use.
    Code,
}

pub fn chip(theme: Theme, label: impl Into<SharedString>, variant: ChipVariant) -> Div {
    let base = div().rounded_sm().whitespace_nowrap().child(label.into());
    match variant {
        ChipVariant::Wash(tint) => base
            .px_1p5()
            .bg(Theme::wash(tint, 0.15))
            .text_xs()
            .text_color(tint),
        ChipVariant::Outline(tint) => base
            .px_1p5()
            .border_1()
            .border_color(tint)
            .text_xs()
            .text_color(tint),
        ChipVariant::Code => base
            .px_1()
            .py_0p5()
            .bg(Theme::wash(theme.text_muted, 0.15))
            .font_family("Menlo")
            .text_size(px(11.))
            .text_color(theme.text),
    }
}

/// Convenience for the most common code chip call.
pub fn code_chip(theme: Theme, text: impl Into<SharedString>) -> Div {
    chip(theme, text, ChipVariant::Code)
}
