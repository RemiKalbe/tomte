//! Structure mode's tree rows: one row per key, array element, comment or
//! folded run, with a value chip per side on rows that changed.

use gpui::{
    App, ClickEvent, Div, ElementId, Rgba, SharedString, Stateful, Window, div, prelude::*, px,
    transparent_black,
};

use crate::components::text_tooltip;
use crate::theme::Theme;

/// Height of one tree row; the view's `uniform_list` uses the same value.
pub const TREE_ROW_H: f32 = 20.;
/// Indent per nesting level.
pub const TREE_INDENT_PX: f32 = 12.;
/// Deeper levels keep this indent so a deep tree does not push its keys
/// out of the pane.
pub const TREE_MAX_INDENT_DEPTH: usize = 24;

/// Height of a value chip: four px under [`TREE_ROW_H`].
const CHIP_H: f32 = 16.;
/// Widest a value chip grows before its text ellipsizes.
const CHIP_MAX_W: f32 = 360.;
/// A chip text longer than this many characters gets a tooltip with the
/// full value, since the chip may cut it.
const CHIP_TOOLTIP_CHARS: usize = 48;

/// Left indent of a row at `depth`, capped at [`TREE_MAX_INDENT_DEPTH`].
pub fn indent_px(depth: usize) -> f32 {
    TREE_INDENT_PX * depth.min(TREE_MAX_INDENT_DEPTH) as f32
}

/// How a tree row reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowTone {
    /// A key whose value the row itself shows, in full-strength text.
    Plain,
    /// Identical on every side: dimmed.
    Same,
    /// Changed and decided: a faint wash.
    Changed,
    /// Changed and still to decide: the wash plus a conflict-red left edge.
    Open,
    /// A comment line: dimmed.
    Comment,
    /// A folded run of identical rows: dimmed and italic.
    Fold,
}

/// One clickable tree row: [`inert_tree_row`] plus the pointer, the
/// `list_row` hover wash, and `on_click`. `cursor` takes the list-row
/// selected wash.
pub fn tree_row(
    theme: Theme,
    id: impl Into<ElementId>,
    depth: usize,
    tone: RowTone,
    cursor: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let row = inert_tree_row(theme, id, depth, tone);
    row.when(cursor, |el| el.bg(Theme::wash(theme.text, 0.08)))
        .cursor_pointer()
        .hover(|el| el.bg(Theme::wash(theme.text, 0.06)))
        .on_click(on_click)
}

/// One tree row that nothing happens on (identical rows, comments,
/// previews, closing brackets): Menlo text_xs, [`TREE_ROW_H`] tall,
/// indented by `depth`. Every row carries a 2px left border so the open
/// row's red edge does not shift its text. The tone colour sits on the row;
/// children (key text, chips) inherit it or set their own.
pub fn inert_tree_row(
    theme: Theme,
    id: impl Into<ElementId>,
    depth: usize,
    tone: RowTone,
) -> Stateful<Div> {
    let text_color = match tone {
        RowTone::Plain | RowTone::Changed | RowTone::Open => theme.text,
        RowTone::Same | RowTone::Comment | RowTone::Fold => theme.text_muted,
    };
    let wash =
        matches!(tone, RowTone::Changed | RowTone::Open).then(|| Theme::wash(theme.text, 0.05));
    let row = div()
        .id(id)
        .h(px(TREE_ROW_H))
        .flex()
        .items_center()
        .gap_2()
        .pl(px(indent_px(depth) + 8.))
        .pr_2()
        .overflow_hidden()
        .whitespace_nowrap()
        .font_family("Menlo")
        .text_xs()
        .text_color(text_color)
        .border_l_2()
        .when_some(wash, |el, wash| el.bg(wash));
    match tone {
        RowTone::Open => row.border_color(theme.conflict),
        RowTone::Fold => row.border_color(transparent_black()).italic(),
        _ => row.border_color(transparent_black()),
    }
}

/// One side's value on a changed row: `label` (`"source"`, `"disk"`,
/// `"last written"`, `"both"`, `"source · disk"`) in muted text, then the
/// value clamped to one line. `prefix` (`"▸"` / `"▾"`) marks a container
/// chip. A picked chip is filled with the tint and leads with `●`; an
/// unpicked one has only the tinted border and fills lightly on hover.
#[allow(clippy::too_many_arguments)]
pub fn value_chip(
    theme: Theme,
    id: impl Into<ElementId>,
    tint: Rgba,
    label: &'static str,
    text: SharedString,
    picked: bool,
    prefix: Option<&'static str>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let long = text.chars().count() > CHIP_TOOLTIP_CHARS;
    let mut shown = String::new();
    if picked {
        shown.push_str("● ");
    }
    if let Some(prefix) = prefix {
        shown.push_str(prefix);
        shown.push(' ');
    }
    shown.push_str(&text);
    div()
        .id(id)
        // A fixed height under the row's, so chips on consecutive rows keep
        // a gap between their borders.
        .h(px(CHIP_H))
        .flex()
        .flex_shrink()
        .items_center()
        .gap_1p5()
        .max_w(px(CHIP_MAX_W))
        .min_w_0()
        .px_1p5()
        .rounded_sm()
        .border_1()
        .border_color(tint)
        .cursor_pointer()
        .when(picked, |el| el.bg(Theme::wash(tint, 0.18)))
        .when(!picked, |el| el.hover(|el| el.bg(Theme::wash(tint, 0.08))))
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.text_muted)
                .child(label),
        )
        .child(
            // The row sets nowrap; a clamped line needs normal wrapping to
            // measure against the chip's width and ellipsize.
            div()
                .min_w_0()
                .whitespace_normal()
                .line_clamp(1)
                .text_ellipsis()
                .text_color(theme.text)
                .child(shown),
        )
        .when(long, |el| el.tooltip(text_tooltip(text)))
        .on_click(on_click)
}

/// The lock on a row whose value a template renders: it follows source.
/// `⚿`, the line editor's protected marker, so both editors share one
/// mark; a text glyph, unlike an emoji, takes the muted colour. Returns
/// `Stateful` because a tooltip needs an element id.
pub fn lock_mark(theme: Theme, reason: SharedString) -> Stateful<Div> {
    div()
        .id("lock-mark")
        .flex_none()
        .text_color(theme.text_muted)
        .child("⚿")
        .tooltip(text_tooltip(reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MUTATION: `indent_px` has no cap → 30 * 12 != 24 * 12.
    #[test]
    fn indent_caps_at_depth_24() {
        assert_eq!(indent_px(0), 0.);
        assert_eq!(indent_px(3), 36.);
        assert_eq!(indent_px(30), 288.);
    }
}
