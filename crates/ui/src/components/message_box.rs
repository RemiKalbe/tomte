//! Error/message box: titled bordered block with optional follow-up hint.

use gpui::{Div, FontWeight, SharedString, div, prelude::*};

use crate::theme::Theme;

pub fn message_box(
    theme: Theme,
    title: impl Into<SharedString>,
    detail: SharedString,
    followup: Option<&'static str>,
) -> Div {
    div()
        .m_3()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(theme.conflict)
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.conflict)
                .child(title.into()),
        )
        .child(div().text_sm().text_color(theme.text).child(detail))
        .when_some(followup, |el, hint| {
            el.child(div().text_xs().text_color(theme.text_muted).child(hint))
        })
}
