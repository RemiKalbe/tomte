//! Component-isolation previews (`chezmoi-ui --gallery comp:<name>`):
//! captioned variants of each library component, rendered bare in a compact
//! window so pixel-level defects are visible before a screen ships.

use gpui::{AnyElement, App, ClickEvent, Div, Window, div, prelude::*};

use crate::components::*;
use crate::theme::Theme;

/// Registry: `(component name, description)` — the gallery derives its
/// `comp:<name>` states from this list.
pub const COMPONENTS: &[(&str, &str)] = &[
    ("dropdown", "dropdown button, disabled, open menu"),
    ("stepper", "stepper enabled / at-min / loading"),
    ("code-chip", "inline + standalone code chips"),
    ("toolbar", "floating Save/Revert toolbar"),
];

fn noop(_: &ClickEvent, _: &mut Window, _: &mut App) {}

/// A captioned variant inside a preview.
fn variant(theme: Theme, caption: &'static str, el: AnyElement) -> Div {
    div()
        .flex()
        .flex_col()
        .items_start()
        .gap_1p5()
        .child(
            div()
                .font_family("Menlo")
                .text_xs()
                .text_color(theme.text_muted)
                .child(caption),
        )
        .child(el)
}

/// Render one isolated component preview; `None` = unknown name. The list of
/// names lives in `fixtures::STATES` (`comp:` entries).
pub fn render_component(name: &str, theme: Theme) -> Option<AnyElement> {
    let el = match name {
        "dropdown" => div()
            .flex()
            .flex_col()
            .gap_6()
            .child(variant(
                theme,
                "enabled",
                dropdown_button(theme, "prev-dd", "personal".into(), true, noop)
                    .into_any_element(),
            ))
            .child(variant(
                theme,
                "disabled",
                dropdown_button(theme, "prev-dd-off", "personal".into(), false, noop)
                    .into_any_element(),
            ))
            .child(variant(
                theme,
                "menu",
                menu(theme)
                    .child(menu_row(
                        theme,
                        "prev-mi-0",
                        "None (single account)".into(),
                        None,
                        false,
                        noop,
                    ))
                    .child(menu_row(
                        theme,
                        "prev-mi-1",
                        "personal".into(),
                        Some("remi@example.com".into()),
                        true,
                        noop,
                    ))
                    .child(inert_menu_line(
                        theme,
                        "1Password CLI not found or errored",
                        theme.drift,
                    ))
                    .into_any_element(),
            ))
            .into_any_element(),
        "stepper" => div()
            .flex()
            .flex_col()
            .gap_6()
            .child(variant(
                theme,
                "enabled",
                stepper(
                    theme, "prev-s-d1", "prev-s-i1", "15 min".into(), true, true, noop, noop,
                )
                .into_any_element(),
            ))
            .child(variant(
                theme,
                "at minimum",
                stepper(
                    theme, "prev-s-d2", "prev-s-i2", "5 min".into(), false, true, noop, noop,
                )
                .into_any_element(),
            ))
            .child(variant(
                theme,
                "loading",
                stepper(
                    theme, "prev-s-d3", "prev-s-i3", "…".into(), false, false, noop, noop,
                )
                .into_any_element(),
            ))
            .into_any_element(),
        "code-chip" => div()
            .flex()
            .flex_col()
            .gap_6()
            .child(variant(
                theme,
                "inline in a sentence",
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child("Injected as")
                    .child(code_chip(theme, "OP_ACCOUNT").py_0())
                    .child("into every")
                    .child(code_chip(theme, "chezmoi").py_0())
                    .child("subprocess.")
                    .into_any_element(),
            ))
            .child(variant(
                theme,
                "standalone value",
                code_chip(theme, "~/Library/Application Support/ChezmoiUI/daemon.sock")
                    .into_any_element(),
            ))
            .into_any_element(),
        "toolbar" => variant(
            theme,
            "unsaved changes",
            toolbar_pill(theme)
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child("unsaved changes · saving restarts the sync daemon"),
                )
                .child(
                    div()
                        .id("prev-revert")
                        .h_6()
                        .px_2p5()
                        .rounded_md()
                        .flex()
                        .items_center()
                        .text_sm()
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|el| el.bg(Theme::wash(theme.text, 0.08)))
                        .child("Revert"),
                )
                .child(
                    div()
                        .id("prev-save")
                        .h_6()
                        .px_2p5()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.accent)
                        .flex()
                        .items_center()
                        .text_sm()
                        .text_color(theme.accent)
                        .cursor_pointer()
                        .hover(|el| el.bg(Theme::wash(theme.accent, 0.12)))
                        .child("Save"),
                )
                .into_any_element(),
        )
        .into_any_element(),
        _ => return None,
    };
    Some(el)
}
