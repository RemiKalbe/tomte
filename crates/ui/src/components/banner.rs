//! Outcome banners: slim tinted wash strip with an optional action slot.

use gpui::{AnyElement, Div, Rgba, SharedString, div, prelude::*};

use crate::theme::Theme;

/// Theme token a banner is tinted with (symbolic so mappings stay pure and
/// testable; resolved to a color at render).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerTint {
    Ok,
    Drift,
    Conflict,
}

impl BannerTint {
    pub fn color(self, theme: Theme) -> Rgba {
        match self {
            Self::Ok => theme.ok,
            Self::Drift => theme.drift,
            Self::Conflict => theme.conflict,
        }
    }
}

/// Margins are the under-header default (mx_3 mt_2); callers with different
/// placement override via the returned builder.
pub fn banner(
    theme: Theme,
    tint: BannerTint,
    text: SharedString,
    action: Option<AnyElement>,
) -> Div {
    let color = tint.color(theme);
    div()
        .mx_3()
        .mt_2()
        .px_2()
        .py_1()
        .rounded_sm()
        .bg(Theme::wash(color, 0.12))
        .flex()
        .items_center()
        .gap_2()
        .text_xs()
        .text_color(color)
        .child(div().flex_1().min_w_0().truncate().child(text))
        .when_some(action, |el, action| {
            el.child(div().flex_none().child(action))
        })
}
