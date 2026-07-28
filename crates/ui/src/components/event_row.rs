//! Timeline/provenance event rows: fixed time gutter, tinted glyph, body,
//! optional chip and trailing slot.

use gpui::{AnyElement, Div, FontWeight, Rgba, SharedString, div, prelude::*};

use crate::components::chip::{ChipVariant, chip};
use crate::theme::Theme;

pub struct EventRowSpec {
    pub time: SharedString,
    pub glyph: (&'static str, Rgba),
    pub title: SharedString,
    pub title_color: Rgba,
    /// Muted truncating detail beside the title (directory, machine…).
    pub detail: Option<SharedString>,
    pub chip: Option<(SharedString, ChipVariant)>,
}

#[derive(Clone, Copy)]
pub enum EventDensity {
    /// text_sm MEDIUM title — clickable timeline rows.
    Interactive,
    /// all text_xs — provenance/history strips.
    Inert,
}

/// Columns only — the caller supplies the row chassis (list_row for
/// clickable timelines, a plain flex row for inert strips) and appends this
/// row's children.
pub fn event_row_children(
    theme: Theme,
    spec: EventRowSpec,
    density: EventDensity,
    trailing: Option<AnyElement>,
) -> Vec<AnyElement> {
    let (glyph, glyph_color) = spec.glyph;
    let mut children: Vec<AnyElement> = vec![
        div()
            .w_12()
            .flex_none()
            .text_xs()
            .text_right()
            .text_color(theme.text_muted)
            .child(spec.time)
            .into_any_element(),
        div()
            .w_4()
            .flex_none()
            .text_center()
            .map(|el| match density {
                EventDensity::Interactive => el.text_sm(),
                EventDensity::Inert => el.text_xs(),
            })
            .text_color(glyph_color)
            .child(glyph)
            .into_any_element(),
    ];
    let mut body = div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_baseline()
        .gap_2()
        .child(
            div()
                .map(|el| match density {
                    EventDensity::Interactive => el.text_sm().font_weight(FontWeight::MEDIUM),
                    EventDensity::Inert => el.text_xs(),
                })
                .text_color(spec.title_color)
                .whitespace_nowrap()
                .child(spec.title),
        );
    if let Some(detail) = spec.detail {
        body = body.child(
            div()
                .min_w_0()
                .text_xs()
                .text_color(theme.text_muted)
                .truncate()
                .child(detail),
        );
    }
    if let Some((label, variant)) = spec.chip {
        body = body.child(chip(theme, label, variant).flex_none());
    }
    children.push(body.into_any_element());
    if let Some(trailing) = trailing {
        children.push(trailing);
    }
    children
}

/// Convenience: an inert event row on a plain flex chassis.
pub fn inert_event_row(theme: Theme, spec: EventRowSpec) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .children(event_row_children(theme, spec, EventDensity::Inert, None))
}
