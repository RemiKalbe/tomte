//! SVG icons (embedded via [`crate::assets::Assets`]).

use gpui::{Svg, prelude::*, px, svg};

use crate::theme::Theme;

/// Rounded chevron (Lucide-style SVG, embedded via `crate::assets::Assets`),
/// geometrically centered — no baseline nudges.
pub fn chevron_down(theme: Theme) -> Svg {
    svg()
        .path("icons/chevron_down.svg")
        .w(px(11.))
        .h(px(11.))
        .flex_none()
        .text_color(theme.text_muted)
}

