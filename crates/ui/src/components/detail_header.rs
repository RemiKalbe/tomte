//! Detail-pane header: filename over ~-shortened path, trailing action slot.

use gpui::{AnyElement, Div, FontWeight, SharedString, div, prelude::*, px};

use crate::theme::Theme;

/// `$HOME/...` → `~/...` — paths read better and truncate less.
pub fn shorten_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

/// Name line height: text_sm at gpui's default 1.618 line height.
const NAME_LINE_H: f32 = 23.;
/// Path line height: text_xs at gpui's default 1.618 line height.
const PATH_LINE_H: f32 = 19.;

/// One line of the title column that ends in an ellipsis when it does not
/// fit. gpui 0.2.2 ellipsizes only text measured against a definite width
/// (`elements/text.rs`): a `w_full` line inside the flex column is measured
/// narrow while the column's width is still unresolved, and the cached
/// layout then shows a few wrapped characters with no ellipsis. An
/// absolutely positioned line pinned to both sides of a fixed-height box
/// gets the box's final width, so its measure is definite. `.truncate()`
/// sets nowrap and hard-clips mid-glyph instead.
fn ellipsized_line(text: SharedString, line_h: f32) -> Div {
    div().relative().w_full().h(px(line_h)).child(
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .overflow_hidden()
            .line_height(px(line_h))
            .line_clamp(1)
            .text_ellipsis()
            .child(text),
    )
}

/// `p_3 border_b` header row: name (semibold) over path (muted), then the
/// trailing actions. The title column takes the width the actions leave,
/// and each of its lines ends in an ellipsis when it does not fit
/// ([`ellipsized_line`]). Callers still ~-shorten paths via
/// [`shorten_home`].
pub fn detail_header(
    theme: Theme,
    name: SharedString,
    path: SharedString,
    trailing: Vec<AnyElement>,
) -> Div {
    div()
        .p_3()
        .border_b_1()
        .border_color(theme.border)
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    ellipsized_line(name, NAME_LINE_H)
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(
                    ellipsized_line(path, PATH_LINE_H)
                        .text_xs()
                        .text_color(theme.text_muted),
                ),
        )
        .children(trailing)
}
