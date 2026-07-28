//! Detail-pane header: filename over ~-shortened path, trailing action slot.

use gpui::{AnyElement, Div, FontWeight, SharedString, div, prelude::*};

use crate::theme::Theme;

/// `$HOME/...` → `~/...` — paths read better and truncate less.
pub fn shorten_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

/// `p_3 border_b` header row: name (semibold) over path (muted, clipped —
/// gpui's ellipsis is unreliable in flex columns, so paths are ~-shortened
/// by the caller via [`shorten_home`] and clip only in pathological cases).
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
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .whitespace_nowrap()
                        .child(name),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child(path),
                ),
        )
        .children(trailing)
}
