//! Floating action pill (bottom-anchored toolbars).

use gpui::{Div, div, prelude::*};

use crate::theme::Theme;

/// The floating action pill (bottom-anchored toolbars).
pub fn toolbar_pill(theme: Theme) -> Div {
    // Concentric radii: the Md buttons inside are rounded_md (6px) and sit
    // behind a uniform 6px inset, so the pill is 6 + 6 = 12px (rounded_xl).
    // Text content should add its own lead-in (pl_1p5) — the padding here is
    // sized to the buttons.
    div()
        .p_1p5()
        .rounded_xl()
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .shadow_md()
        .flex()
        .items_center()
        .gap_2()
}
