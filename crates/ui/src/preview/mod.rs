//! Component-isolation previews (`tomte --gallery comp:<name>`):
//! captioned variants of each library component, rendered bare in a compact
//! window so pixel-level defects are visible before a screen ships.

use gpui::{AnyElement, App, ClickEvent, Div, Window, div, prelude::*};

pub mod buttons;

use crate::components::*;
use crate::theme::Theme;

/// Registry: `(component name, description)` — the gallery derives its
/// `comp:<name>` states from this list.
pub const COMPONENTS: &[(&str, &str)] = &[
    ("dropdown", "dropdown button, disabled, open menu"),
    ("stepper", "stepper enabled / at-min / loading"),
    ("code-chip", "inline + standalone code chips"),
    ("toolbar", "floating Save/Revert toolbar"),
    ("merge-region", "decision strip states + provenance rows"),
    ("spinner", "indeterminate scan/load spinner"),
    ("buttons", "BUTTON LAB: 12 candidate designs in real scenes"),
    (
        "checkbox",
        "off / partial / on, and forced-visible in a row",
    ),
    ("folder-row", "expanded, collapsed, partial-checked, n of m"),
    (
        "icon-button",
        "folder-tree and fold-identical toggles: off and on",
    ),
    (
        "structure-row",
        "tree rows: same, open conflict, picked, locked, folded",
    ),
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
                dropdown_button(theme, "prev-dd", "personal".into(), true, noop).into_any_element(),
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
                    theme,
                    "prev-s-d1",
                    "prev-s-i1",
                    "15 min".into(),
                    true,
                    true,
                    noop,
                    noop,
                )
                .into_any_element(),
            ))
            .child(variant(
                theme,
                "at minimum",
                stepper(
                    theme,
                    "prev-s-d2",
                    "prev-s-i2",
                    "5 min".into(),
                    false,
                    true,
                    noop,
                    noop,
                )
                .into_any_element(),
            ))
            .child(variant(
                theme,
                "loading",
                stepper(
                    theme,
                    "prev-s-d3",
                    "prev-s-i3",
                    "…".into(),
                    false,
                    false,
                    noop,
                    noop,
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
                code_chip(theme, "~/Library/Application Support/Tomte/daemon.sock")
                    .into_any_element(),
            ))
            .into_any_element(),
        "toolbar" => variant(
            theme,
            "unsaved changes",
            toolbar_pill(theme)
                .child(
                    div()
                        .pl_1p5()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child("unsaved changes · saving restarts the sync daemon"),
                )
                .child(button(
                    theme,
                    "prev-revert",
                    "Revert".into(),
                    ButtonVariant::Ghost,
                    ButtonSize::Md,
                    noop,
                ))
                .child(button(
                    theme,
                    "prev-save",
                    "Save".into(),
                    ButtonVariant::Ring(theme.accent),
                    ButtonSize::Md,
                    noop,
                ))
                .into_any_element(),
        )
        .into_any_element(),
        "merge-region" => {
            let noop_pick = |_: Pick, _: &ClickEvent, _: &mut Window, _: &mut App| {};
            let lines: Vec<gpui::SharedString> = vec![
                "    \"@xberg-io/opencode-xberg\",".into(),
                "    \"@xberg-io/opencode-crawlberg\",".into(),
            ];
            let mut protected = std::collections::HashMap::new();
            protected.insert(
                1usize,
                gpui::SharedString::from("in template:\n    \"{{ .xberg.crawlberg }}\","),
            );
            div()
                .flex()
                .flex_col()
                .gap_6()
                .w(gpui::px(430.))
                .child(variant(
                    theme,
                    "deciding (conflict, focused)",
                    decision_strip(
                        theme,
                        0,
                        StripState::Deciding {
                            has_base: true,
                            current: None,
                            focused: true,
                            origin: None,
                            both: true,
                        },
                        noop_pick,
                        noop,
                        noop,
                    )
                    .into_any_element(),
                ))
                .child(variant(
                    theme,
                    "deciding (auto-resolved, overridable)",
                    decision_strip(
                        theme,
                        1,
                        StripState::Deciding {
                            has_base: false,
                            current: Some(ChoiceKind::Theirs),
                            focused: false,
                            origin: None,
                            both: true,
                        },
                        noop_pick,
                        noop,
                        noop,
                    )
                    .into_any_element(),
                ))
                .child(variant(
                    theme,
                    "deciding (origin has its own version)",
                    decision_strip(
                        theme,
                        3,
                        StripState::Deciding {
                            has_base: true,
                            current: None,
                            focused: false,
                            origin: Some("    \"@xberg-io/opencode-xberg\",\n".into()),
                            both: true,
                        },
                        noop_pick,
                        noop,
                        noop,
                    )
                    .into_any_element(),
                ))
                .child(variant(
                    theme,
                    "deciding (no Both)",
                    decision_strip(
                        theme,
                        4,
                        StripState::Deciding {
                            has_base: true,
                            current: None,
                            focused: false,
                            origin: None,
                            both: false,
                        },
                        noop_pick,
                        noop,
                        noop,
                    )
                    .into_any_element(),
                ))
                .child(variant(
                    theme,
                    "decided",
                    decision_strip(
                        theme,
                        2,
                        StripState::Decided {
                            choice: ChoiceKind::Both,
                            focused: false,
                        },
                        noop_pick,
                        noop,
                        noop,
                    )
                    .into_any_element(),
                ))
                .child(variant(
                    theme,
                    "provenance: disk side",
                    div()
                        .w_full()
                        .child(provenance_label(theme, Side::Ours))
                        // Protected spans anchor to the SOURCE side only.
                        .child(provenance_rows(
                            theme,
                            Side::Ours,
                            &lines,
                            &std::collections::HashMap::new(),
                        ))
                        .into_any_element(),
                ))
                .child(variant(
                    theme,
                    "provenance: source side (protected line)",
                    div()
                        .w_full()
                        .child(provenance_label(theme, Side::Theirs))
                        .child(provenance_rows(theme, Side::Theirs, &lines, &protected))
                        .into_any_element(),
                ))
                .into_any_element()
        }
        "spinner" => variant(
            theme,
            "next to a section header",
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text_muted)
                        .child("ACTIVITY"),
                )
                .child(spinner(theme, "prev-spinner"))
                .into_any_element(),
        )
        .into_any_element(),
        "buttons" => buttons::render(theme).into_any_element(),
        "checkbox" => div()
            .flex()
            .flex_col()
            .gap_6()
            .child(variant(
                theme,
                "off",
                checkbox(theme, "cb-off", CheckState::Off, noop).into_any_element(),
            ))
            .child(variant(
                theme,
                "partial",
                checkbox(theme, "cb-partial", CheckState::Partial, noop).into_any_element(),
            ))
            .child(variant(
                theme,
                "on",
                checkbox(theme, "cb-on", CheckState::On, noop).into_any_element(),
            ))
            .into_any_element(),
        "folder-row" => div()
            .flex()
            .flex_col()
            .gap_2()
            .w(gpui::px(260.))
            .child(folder_row(
                theme,
                "fr-1",
                FolderRow {
                    depth: 0,
                    label: ".config/".into(),
                    collapsed: false,
                    count: "4 of 6".into(),
                    check: CheckState::Partial,
                    show_check: true,
                },
                noop,
                noop,
            ))
            .child(folder_row(
                theme,
                "fr-2",
                FolderRow {
                    depth: 1,
                    label: "nvim/".into(),
                    collapsed: true,
                    count: "2".into(),
                    check: CheckState::On,
                    show_check: true,
                },
                noop,
                noop,
            ))
            .child(folder_row(
                theme,
                "fr-3",
                FolderRow {
                    depth: 1,
                    label: "zed/".into(),
                    collapsed: false,
                    count: "1".into(),
                    check: CheckState::Off,
                    show_check: false,
                },
                noop,
                noop,
            ))
            .into_any_element(),
        "icon-button" => div()
            .flex()
            .flex_col()
            .gap_6()
            .child(variant(
                theme,
                "off",
                icon_button(
                    theme,
                    "prev-ib-off",
                    folder_tree(theme),
                    false,
                    "Folder tree".into(),
                    noop,
                )
                .into_any_element(),
            ))
            .child(variant(
                theme,
                "on",
                icon_button(
                    theme,
                    "prev-ib-on",
                    folder_tree(theme),
                    true,
                    "Folder tree".into(),
                    noop,
                )
                .into_any_element(),
            ))
            .child(variant(
                theme,
                "fold identical, off",
                icon_button(
                    theme,
                    "prev-ib-fold-off",
                    chevrons_down_up(theme),
                    false,
                    "Fold identical".into(),
                    noop,
                )
                .into_any_element(),
            ))
            .child(variant(
                theme,
                "fold identical, on",
                icon_button(
                    theme,
                    "prev-ib-fold-on",
                    chevrons_down_up(theme),
                    true,
                    "Fold identical".into(),
                    noop,
                )
                .into_any_element(),
            ))
            .into_any_element(),
        "structure-row" => {
            let key = |text: &'static str| div().flex_none().child(text);
            let long: gpui::SharedString = "x".repeat(120).into();
            // Identical and comment rows are inert in the view; the rest
            // take the pointer and the hover wash.
            let row = |ix: usize, depth: usize, tone: RowTone| match tone {
                RowTone::Same | RowTone::Comment => {
                    inert_tree_row(theme, ("prev-sr", ix), depth, tone)
                }
                _ => tree_row(theme, ("prev-sr", ix), depth, tone, false, noop),
            };
            let chip = |ix: usize,
                        tint: gpui::Rgba,
                        label: &'static str,
                        text: gpui::SharedString,
                        picked: bool,
                        prefix: Option<&'static str>| {
                value_chip(
                    theme,
                    ("prev-sr-chip", ix),
                    tint,
                    label,
                    text,
                    picked,
                    prefix.map(|glyph| ChipPrefix {
                        glyph,
                        selector: format!("prev-sr-unfold:{ix}"),
                        on_click: Box::new(noop),
                    }),
                    noop,
                )
            };
            list_pane(theme, None)
                .w(gpui::px(520.))
                .py_1()
                .child(row(0, 1, RowTone::Same).child(key("\"editor.formatOnSave\": true,")))
                .child(
                    row(1, 1, RowTone::Open)
                        .child(key("\"theme\":"))
                        .child(chip(
                            0,
                            theme.accent,
                            "source",
                            "One Dark".into(),
                            false,
                            None,
                        ))
                        .child(chip(1, theme.drift, "disk", "Ayu Dark".into(), false, None))
                        .child(chip(
                            2,
                            theme.text_muted,
                            "last written",
                            "Solarized".into(),
                            false,
                            None,
                        )),
                )
                .child(
                    row(2, 1, RowTone::Changed)
                        .child(key("\"font_size\":"))
                        .child(chip(3, theme.accent, "source", "13".into(), false, None))
                        .child(chip(4, theme.drift, "disk", "14".into(), true, None)),
                )
                .child(
                    row(3, 1, RowTone::Changed)
                        .child(key("\"tab_size\":"))
                        .child(chip(
                            5,
                            theme.accent,
                            "source · disk",
                            "4".into(),
                            true,
                            None,
                        )),
                )
                .child(
                    row(4, 1, RowTone::Changed)
                        .child(key("\"account\":"))
                        .child(chip(
                            6,
                            theme.accent,
                            "source",
                            "{{ .onepassword.account }}".into(),
                            true,
                            None,
                        ))
                        .child(lock_mark(
                            theme,
                            "a template renders this value; it follows source".into(),
                        )),
                )
                .child(row(5, 1, RowTone::Fold).child("▸ 4 identical keys"))
                .child(row(6, 1, RowTone::Comment).child("// editor"))
                .child(
                    row(7, 1, RowTone::Changed)
                        .child(key("\"permissions\":"))
                        .child(chip(
                            7,
                            theme.accent,
                            "source",
                            "absent".into(),
                            false,
                            None,
                        ))
                        .child(chip(
                            8,
                            theme.drift,
                            "disk",
                            "{ 3 keys }".into(),
                            true,
                            Some("▾"),
                        )),
                )
                .child(row(8, 2, RowTone::Same).child(key("\"allow\": [\"Bash\"],")))
                .child(
                    row(9, 1, RowTone::Changed)
                        .child(key("\"banner\":"))
                        .child(chip(10, theme.drift, "disk", long, false, None)),
                )
                .into_any_element()
        }
        _ => return None,
    };
    Some(el)
}
