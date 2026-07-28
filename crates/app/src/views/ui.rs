//! Pure component builders: ONE source of styling truth, used by the real
//! views and by the gallery's component-isolation previews
//! (`chezmoi-ui --gallery comp:<name>`). Behavior comes in as plain click
//! handlers, so a component renders identically in place and in isolation —
//! a chevron that's off by two pixels shows up in a 520px window, not after
//! shipping a full screen.

use czui_app::theme::Theme;
use gpui::{
    AnyElement, App, ClickEvent, Div, ElementId, SharedString, Stateful, Svg, Window, div,
    prelude::*, px, svg,
};

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

/// Inline mono code chip (markdown `code` look). 11px Menlo: at equal point
/// size Menlo's x-height reads larger than the UI font, so one point down
/// sits flush inside a 12px sentence.
pub fn code_chip(theme: Theme, text: impl Into<SharedString>) -> Div {
    div()
        .px_1()
        .py_0p5()
        .rounded_sm()
        .bg(Theme::wash(theme.text_muted, 0.15))
        .font_family("Menlo")
        .text_size(px(11.))
        .text_color(theme.text)
        .whitespace_nowrap()
        .child(text.into())
}

/// Filled dropdown trigger: current value + chevron.
pub fn dropdown_button(
    theme: Theme,
    id: impl Into<ElementId>,
    label: SharedString,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .h_6()
        .px_2p5()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .flex()
        .items_center()
        .gap_1p5()
        .text_sm()
        .map(|el| {
            if enabled {
                el.text_color(theme.text)
                    .cursor_pointer()
                    .hover(|el| el.bg(Theme::wash(theme.text, 0.08)))
                    .on_click(on_click)
            } else {
                el.text_color(theme.text_muted)
            }
        })
        .child(label)
        .child(chevron_down(theme))
}

/// Segmented numeric stepper: `− │ value │ +` in one bordered group (Zed's
/// number-field shape).
pub fn stepper(
    theme: Theme,
    dec_id: &'static str,
    inc_id: &'static str,
    value: SharedString,
    dec_enabled: bool,
    inc_enabled: bool,
    on_dec: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_inc: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    div()
        .flex()
        .h_6()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .child(seg_button(dec_id, "−", dec_enabled, theme, on_dec))
        .child(
            div()
                .w_16()
                .border_l_1()
                .border_r_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.text_muted)
                .child(value),
        )
        .child(seg_button(inc_id, "+", inc_enabled, theme, on_inc))
}

/// − / + segment; disabled renders muted with no handler.
fn seg_button(
    id: &'static str,
    label: &'static str,
    enabled: bool,
    theme: Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .w_6()
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(if enabled {
            theme.text
        } else {
            theme.text_muted
        })
        .child(label)
        .when(enabled, |el| {
            el.cursor_pointer()
                .hover(|el| el.bg(Theme::wash(theme.text, 0.08)))
                .on_click(on_click)
        })
}

/// Popover menu container (caller adds `.id()`, dismiss handling, children).
pub fn menu(theme: Theme) -> Div {
    div()
        .min_w(px(260.))
        .max_w(px(380.))
        .p_1()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .shadow_md()
        .flex()
        .flex_col()
}

/// One selectable menu row: ✓ gutter, label, optional muted sublabel.
pub fn menu_row(
    theme: Theme,
    id: impl Into<ElementId>,
    label: SharedString,
    sublabel: Option<SharedString>,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .h_7()
        .px_2()
        .rounded_sm()
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .hover(|el| el.bg(Theme::wash(theme.text, 0.06)))
        .child(
            div()
                .w_4()
                .flex_none()
                .text_sm()
                .text_color(theme.accent)
                .child(if selected { "✓" } else { "" }),
        )
        .child(div().text_sm().text_color(theme.text).child(label))
        .when_some(sublabel, |el, s| {
            el.child(div().text_xs().text_color(theme.text_muted).child(s))
        })
        .on_click(on_click)
}

/// An unselectable status line inside a menu (loading / unavailable).
pub fn inert_menu_line(theme: Theme, text: &'static str, color: gpui::Rgba) -> Div {
    let _ = theme;
    div()
        .h_7()
        .px_2()
        .pl_8()
        .flex()
        .items_center()
        .text_xs()
        .text_color(color)
        .child(text)
}

/// The floating action pill (bottom-anchored toolbars).
pub fn toolbar_pill(theme: Theme) -> Div {
    div()
        .px_3()
        .py_2()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface)
        .shadow_md()
        .flex()
        .items_center()
        .gap_3()
}

// ---------------------------------------------------------------------------
// Component-isolation previews (`--gallery comp:<name>`)
// ---------------------------------------------------------------------------

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
