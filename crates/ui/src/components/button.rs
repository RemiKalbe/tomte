//! Buttons: one chassis, three sizes, three variants (catalog: 15 consumers
//! across every view — the most-shared component in the app).

use gpui::{
    App, ClickEvent, Div, ElementId, Rgba, SharedString, Stateful, Window, div, prelude::*,
};

use crate::components::tooltip::text_tooltip;
use crate::theme::Theme;

#[derive(Clone, Copy)]
pub enum ButtonVariant {
    /// 1px tinted border, tinted text; hover = wash(tint, 0.12).
    Outline(Rgba),
    /// No border; muted text; hover = wash(text, 0.08).
    Ghost,
    /// Borderless tinted pill on wash(tint, 0.15); hover deepens to 0.25.
    Wash(Rgba),
}

#[derive(Clone, Copy)]
pub enum ButtonSize {
    /// h_6 px_2p5 text_sm rounded_md — headers, toolbars.
    Md,
    /// px_2 py_0p5 text_xs rounded_md — pane headers, compact actions.
    Sm,
    /// px_1p5 text_xs rounded_sm, no vertical padding — in-row actions.
    Micro,
}

fn chassis(id: impl Into<ElementId>, size: ButtonSize, label: SharedString) -> Stateful<Div> {
    let base = div().id(id).flex().items_center().whitespace_nowrap();
    match size {
        ButtonSize::Md => base.h_6().px_2p5().text_sm().rounded_md(),
        ButtonSize::Sm => base.px_2().py_0p5().text_xs().rounded_md(),
        ButtonSize::Micro => base.px_1p5().text_xs().rounded_sm(),
    }
    .child(label)
}

pub fn button(
    theme: Theme,
    id: impl Into<ElementId>,
    label: SharedString,
    variant: ButtonVariant,
    size: ButtonSize,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let el = chassis(id, size, label).cursor_pointer().on_click(on_click);
    match variant {
        ButtonVariant::Outline(tint) => el
            .border_1()
            .border_color(tint)
            .text_color(tint)
            .hover(move |el| el.bg(Theme::wash(tint, 0.12))),
        ButtonVariant::Ghost => el
            .text_color(theme.text_muted)
            .hover(move |el| el.bg(Theme::wash(theme.text, 0.08))),
        ButtonVariant::Wash(tint) => el
            .bg(Theme::wash(tint, 0.15))
            .text_color(tint)
            .hover(move |el| el.bg(Theme::wash(tint, 0.25))),
    }
}

/// Disabled twin: muted border and text, no handler, optional tooltip
/// explaining why.
pub fn disabled_button(
    theme: Theme,
    id: impl Into<ElementId>,
    label: SharedString,
    size: ButtonSize,
    tooltip: Option<SharedString>,
) -> Stateful<Div> {
    chassis(id, size, label)
        .border_1()
        .border_color(theme.border)
        .text_color(theme.text_muted)
        .when_some(tooltip, |el, text| el.tooltip(text_tooltip(text)))
}
