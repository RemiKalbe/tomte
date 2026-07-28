//! Section headers: in-pane caps groupings and page-level mono ruled labels.

use gpui::{AnyElement, Div, FontWeight, div, prelude::*};

use crate::theme::Theme;

pub enum SectionHeaderStyle {
    /// text_xs SEMIBOLD muted, all-caps label — in-pane list grouping
    /// (ACTIVITY, NEEDS A DECISION…). Caller supplies padding.
    Caps,
    /// Menlo text_xs muted over a solid hairline — page sections (settings).
    /// `spaced` adds mt_8 between sections.
    MonoRuled { spaced: bool },
}

pub fn section_header(
    theme: Theme,
    label: &'static str,
    style: SectionHeaderStyle,
    trailing: Option<AnyElement>,
) -> Div {
    match style {
        SectionHeaderStyle::Caps => div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(label),
            )
            .when_some(trailing, |el, t| el.child(t)),
        SectionHeaderStyle::MonoRuled { spaced } => div()
            .when(spaced, |el| el.mt_8())
            .mb_1()
            .flex()
            .flex_col()
            .gap_1p5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .font_family("Menlo")
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child(label),
                    )
                    .when_some(trailing, |el, t| el.child(t)),
            )
            .child(hairline(theme)),
    }
}

/// Solid section rule.
pub fn hairline(theme: Theme) -> Div {
    div().h_px().w_full().bg(theme.border)
}
