//! SVG icons (embedded via [`crate::assets::Assets`]).

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, ElementId, Svg, Transformation, percentage, prelude::*, px, svg,
};

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

/// Indeterminate spinner: a rotating arc, muted. Use anywhere work is in
/// flight without progress info (scans, loads). One per id.
pub fn spinner(theme: Theme, id: impl Into<ElementId>) -> impl IntoElement {
    svg()
        .path("icons/spinner.svg")
        .w(px(12.))
        .h(px(12.))
        .flex_none()
        .text_color(theme.text_muted)
        .with_animation(
            id,
            Animation::new(Duration::from_millis(900)).repeat(),
            |el, delta| el.with_transformation(Transformation::rotate(percentage(delta))),
        )
}
