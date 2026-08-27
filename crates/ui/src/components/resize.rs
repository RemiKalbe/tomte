//! Draggable panel dividers.
//!
//! Every structural border in the app doubles as a grab handle: the hairline
//! stays exactly where it was, but a wider invisible strip around it takes
//! the pointer, shows a resize cursor, and tints the line while you drag.
//!
//! The handle only reports the drag START — gpui's `on_mouse_move` fires only
//! while the pointer is over the element, so tracking a drag that runs past
//! the handle has to happen on the view's ROOT (whose hitbox spans the
//! window). [`DragState`] is the small bit of bookkeeping that makes that
//! ergonomic; hosts keep one and call [`DragState::delta`] on move.

use gpui::{
    App, CursorStyle, Div, ElementId, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels,
    Stateful, Window, div, prelude::*, px,
};

use crate::theme::Theme;

/// Which way a divider moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeAxis {
    /// A vertical rule dragged left/right — it resizes widths.
    X,
    /// A horizontal rule dragged up/down — it resizes heights.
    Y,
}

impl ResizeAxis {
    fn cursor(self) -> CursorStyle {
        match self {
            ResizeAxis::X => CursorStyle::ResizeLeftRight,
            ResizeAxis::Y => CursorStyle::ResizeUpDown,
        }
    }
}

/// Total thickness of the grab strip. The visible rule stays 1px; the rest
/// is forgiveness — a 1px target is unusable with a trackpad.
const GRAB: f32 = 7.;

/// An in-flight divider drag: which divider, where the pointer went down,
/// and the sizes at that moment. Hosts store `Option<DragState<Id>>`.
#[derive(Debug, Clone, Copy)]
pub struct DragState<Id> {
    pub id: Id,
    origin: gpui::Point<Pixels>,
    /// Whatever the host was measuring when the drag began (a width, a
    /// height, a pair) — returned untouched so the host can add the delta to
    /// the value it started from rather than accumulating per-frame error.
    pub start: f32,
}

impl<Id: Copy> DragState<Id> {
    pub fn new(id: Id, event: &MouseDownEvent, start: f32) -> Self {
        Self {
            id,
            origin: event.position,
            start,
        }
    }

    /// Pixels moved along `axis` since the drag began.
    pub fn delta(&self, event: &MouseMoveEvent, axis: ResizeAxis) -> f32 {
        let d = event.position - self.origin;
        match axis {
            ResizeAxis::X => f32::from(d.x),
            ResizeAxis::Y => f32::from(d.y),
        }
    }
}

/// A divider that can be dragged. Renders the same hairline the border it
/// replaces did, centred inside a `GRAB`-wide transparent strip.
///
/// `active` tints the rule while this divider is the one being dragged, so
/// the feedback survives the pointer leaving the strip.
pub fn resize_handle(
    id: impl Into<ElementId>,
    axis: ResizeAxis,
    active: bool,
    theme: Theme,
    on_start: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let line_color = if active { theme.accent } else { theme.border };
    let rule = div().flex_none().map(|el| match axis {
        ResizeAxis::X => el.w_px().h_full(),
        ResizeAxis::Y => el.h_px().w_full(),
    });
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor(axis.cursor())
        .map(|el| match axis {
            ResizeAxis::X => el.w(px(GRAB)).h_full().mx(px(-(GRAB - 1.) / 2.)),
            ResizeAxis::Y => el.h(px(GRAB)).w_full().my(px(-(GRAB - 1.) / 2.)),
        })
        // The strip is wider than the rule it carries, so it must not eat
        // layout space the panels were using: negative margins pull it back
        // to the 1px the border occupied.
        .child(
            rule.bg(line_color)
                .when(!active, |el| el.hover(|el| el.bg(theme.accent))),
        )
        .on_mouse_down(MouseButton::Left, on_start)
}
