//! Button design lab (`tomte --gallery comp:buttons`).
//!
//! 2026-08-24 feedback: "hard 1px borders is just so dated… explore MANY
//! designs, then put them in fake scenes so I can project myself." Every
//! candidate below is rendered THREE times — as a bare states row, and
//! inside two real scenes it would actually live in (the merge decision
//! strip and the review detail header) — plus the toolbar/banner scenes for
//! the primary-action candidates.
//!
//! OUTCOME (2026-08-25): the winner is not one candidate but a rule keyed on
//! the SURFACE the button sits on — #12 ring tonal inside a container that
//! draws its own border, #11 solid on a filled/tinted field, #3 tonal
//! everywhere else on the plain background, and quiet for tertiary actions.
//! That is what [`crate::components::button`] now implements; this lab stays
//! as the record of what was compared, and is self-contained so it can be
//! deleted whole.

use gpui::{
    App, ClickEvent, Div, ElementId, Rgba, SharedString, Stateful, Window, div, prelude::*, px,
};

use crate::components::button::{ButtonSize, ButtonVariant, button, disabled_button};

use crate::theme::Theme;

fn noop(_: &ClickEvent, _: &mut Window, _: &mut App) {}

/// The candidate designs, in the order they appear in the lab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Candidate {
    /// TODAY: 1px full-strength tinted border, transparent fill.
    Hairline,
    /// The same outline language, dialled down: 35% border + 6% fill.
    SoftHairline,
    /// No border. 14% tint fill, tinted label. (Linear / Vercel / M3 tonal.)
    Tonal,
    /// Tonal, fully rounded.
    TonalPill,
    /// Neutral fill, normal text; the tint appears only as a leading dot.
    NeutralDot,
    /// Neutral surface + a soft drop shadow, no border (macOS control).
    Elevated,
    /// One track, mutually-exclusive items, the selection raised.
    Segmented,
    /// No fill at rest — the button only materializes under the pointer.
    Quiet,
    /// Tonal + a trailing keycap teaching the existing 1/2/3/b shortcut.
    KeyHint,
    /// Text + a 2px tinted underline (tab-like).
    Underline,
    /// Solid tint fill, contrasting label — for THE primary action only.
    Solid,
    /// Tonal fill + tinted 1px inner ring at low alpha (a "lifted" tonal).
    RingTonal,
}

impl Candidate {
    pub const ALL: [Candidate; 12] = [
        Candidate::Hairline,
        Candidate::SoftHairline,
        Candidate::Tonal,
        Candidate::TonalPill,
        Candidate::NeutralDot,
        Candidate::Elevated,
        Candidate::Segmented,
        Candidate::Quiet,
        Candidate::KeyHint,
        Candidate::Underline,
        Candidate::Solid,
        Candidate::RingTonal,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Candidate::Hairline => "1 · hairline (today)",
            Candidate::SoftHairline => "2 · soft hairline",
            Candidate::Tonal => "3 · tonal",
            Candidate::TonalPill => "4 · tonal pill",
            Candidate::NeutralDot => "5 · neutral + dot",
            Candidate::Elevated => "6 · elevated",
            Candidate::Segmented => "7 · segmented",
            Candidate::Quiet => "8 · quiet",
            Candidate::KeyHint => "9 · key hint",
            Candidate::Underline => "10 · underline",
            Candidate::Solid => "11 · solid",
            Candidate::RingTonal => "12 · ring tonal",
        }
    }

    pub fn note(self) -> &'static str {
        match self {
            Candidate::Hairline => "what ships now — every action shouts equally",
            Candidate::SoftHairline => {
                "same shape, 35% border + faint fill: outline without the shout"
            }
            Candidate::Tonal => "borderless color field; weight comes from fill, not stroke",
            Candidate::TonalPill => "tonal, fully rounded — reads softer, eats more width",
            Candidate::NeutralDot => {
                "one neutral chip; the dot carries the side, matching the diff dots"
            }
            Candidate::Elevated => "surface + soft shadow, no stroke at all (macOS control feel)",
            Candidate::Segmented => {
                "the picks are ONE choice — a segmented track says so structurally"
            }
            Candidate::Quiet => "invisible until hovered; strips stay pure content until you reach",
            Candidate::KeyHint => "tonal + keycap: teaches 1/2/3/b while you click",
            Candidate::Underline => "no box at all; a tinted rule under the label",
            Candidate::Solid => "full-tint fill — reserve for the single primary action",
            Candidate::RingTonal => {
                "tonal fill with a faint same-tint ring: edge definition, no hard line"
            }
        }
    }

    /// Candidates that only make sense as a whole mutually-exclusive group.
    pub fn is_group(self) -> bool {
        matches!(self, Candidate::Segmented)
    }
}

/// Visual state a lab button can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtnState {
    Rest,
    /// The current choice for this region (strip semantics).
    Selected,
    Disabled,
}

/// Text that reads on a solid tint fill in both themes.
fn on_solid(theme: Theme) -> Rgba {
    theme.bg
}

/// One candidate button. `key` renders as a trailing keycap where the
/// candidate calls for it.
pub fn lab_button(
    theme: Theme,
    cand: Candidate,
    id: &'static str,
    label: &'static str,
    tint: Rgba,
    state: BtnState,
    key: Option<&'static str>,
) -> Stateful<Div> {
    let selected = state == BtnState::Selected;
    let disabled = state == BtnState::Disabled;
    let base = div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .h_5()
        .px_2()
        .text_xs()
        .whitespace_nowrap()
        .when(!disabled, |el| el.cursor_pointer())
        .when(disabled, |el| el.opacity(0.4));

    let el = match cand {
        Candidate::Hairline => base
            .rounded_sm()
            .border_1()
            .border_color(tint)
            .text_color(tint)
            .when(selected, |el| el.bg(Theme::wash(tint, 0.2)))
            .hover(move |el| el.bg(Theme::wash(tint, 0.12))),
        Candidate::SoftHairline => base
            .rounded_md()
            .border_1()
            .border_color(Theme::wash(tint, 0.35))
            .bg(Theme::wash(tint, 0.06))
            .text_color(tint)
            .when(selected, |el| {
                el.bg(Theme::wash(tint, 0.18)).border_color(tint)
            })
            .hover(move |el| el.bg(Theme::wash(tint, 0.14))),
        Candidate::Tonal => base
            .rounded_md()
            .bg(Theme::wash(tint, 0.14))
            .text_color(tint)
            .when(selected, |el| el.bg(Theme::wash(tint, 0.28)))
            .hover(move |el| el.bg(Theme::wash(tint, 0.22))),
        Candidate::TonalPill => base
            .rounded_full()
            .px_2p5()
            .bg(Theme::wash(tint, 0.14))
            .text_color(tint)
            .when(selected, |el| el.bg(Theme::wash(tint, 0.28)))
            .hover(move |el| el.bg(Theme::wash(tint, 0.22))),
        Candidate::NeutralDot => base
            .rounded_md()
            .bg(Theme::wash(theme.text, 0.07))
            .text_color(theme.text)
            .when(selected, |el| el.bg(Theme::wash(tint, 0.2)))
            .hover(move |el| el.bg(Theme::wash(theme.text, 0.12)))
            .child(div().w_1p5().h_1p5().flex_none().rounded_full().bg(tint)),
        Candidate::Elevated => base
            .rounded_md()
            .bg(theme.surface)
            .text_color(theme.text)
            .shadow_xs()
            .when(selected, |el| {
                el.bg(Theme::wash(tint, 0.2)).text_color(tint)
            })
            .hover(move |el| el.bg(Theme::wash(theme.text, 0.10))),
        // Rendered by `segmented_group`; a lone item is just its resting item.
        Candidate::Segmented => base
            .rounded_md()
            .text_color(if selected {
                theme.text
            } else {
                theme.text_muted
            })
            .when(selected, |el| el.bg(theme.surface).shadow_xs()),
        Candidate::Quiet => base
            .rounded_md()
            .text_color(if selected { tint } else { theme.text_muted })
            .when(selected, |el| el.bg(Theme::wash(tint, 0.16)))
            .hover(move |el| el.bg(Theme::wash(tint, 0.12)).text_color(tint)),
        Candidate::KeyHint => base
            .rounded_md()
            .bg(Theme::wash(tint, 0.14))
            .text_color(tint)
            .when(selected, |el| el.bg(Theme::wash(tint, 0.28)))
            .hover(move |el| el.bg(Theme::wash(tint, 0.22))),
        Candidate::Underline => base
            .px_1()
            .rounded_none()
            .border_b_2()
            .border_color(if selected {
                tint
            } else {
                gpui::transparent_black().into()
            })
            .text_color(if selected { tint } else { theme.text_muted })
            .hover(move |el| el.text_color(tint).border_color(Theme::wash(tint, 0.5))),
        Candidate::Solid => base
            .rounded_md()
            .bg(tint)
            .text_color(on_solid(theme))
            .font_weight(gpui::FontWeight::MEDIUM)
            .hover(move |el| el.bg(Theme::wash(tint, 0.85))),
        Candidate::RingTonal => base
            .rounded_md()
            .bg(Theme::wash(tint, 0.12))
            .border_1()
            .border_color(Theme::wash(tint, 0.22))
            .text_color(tint)
            .when(selected, |el| {
                el.bg(Theme::wash(tint, 0.26))
                    .border_color(Theme::wash(tint, 0.5))
            })
            .hover(move |el| el.bg(Theme::wash(tint, 0.2))),
    }
    .child(label);

    // The keycap rides along only where the candidate asked for it.
    match (cand, key) {
        (Candidate::KeyHint, Some(k)) => el.child(
            div()
                .ml_0p5()
                .px_1()
                .rounded_sm()
                .bg(Theme::wash(tint, 0.22))
                .text_color(tint)
                .text_size(px(10.))
                .child(k),
        ),
        _ => el,
    }
    .on_click(noop)
}

/// The segmented track: mutually-exclusive picks in one container.
fn segmented_group(
    theme: Theme,
    id: &'static str,
    items: &[(&'static str, &'static str, Rgba)],
    selected_ix: Option<usize>,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_0p5()
        .p_0p5()
        .rounded_lg()
        .bg(Theme::wash(theme.text_muted, 0.12))
        .children(
            items
                .iter()
                .enumerate()
                .map(|(ix, (item_id, label, tint))| {
                    let is_sel = selected_ix == Some(ix);
                    div()
                        .id(*item_id)
                        .flex()
                        .items_center()
                        .h_5()
                        .px_2()
                        .text_xs()
                        .rounded_md()
                        .cursor_pointer()
                        .whitespace_nowrap()
                        .when(is_sel, |el| {
                            el.bg(theme.surface).shadow_xs().text_color(*tint)
                        })
                        .when(!is_sel, |el| {
                            el.text_color(theme.text_muted)
                                .hover(|el| el.bg(Theme::wash(theme.text, 0.06)))
                        })
                        .child(*label)
                        .on_click(noop)
                }),
        )
}

/// A hairline vertical rule between action groups.
fn divider(theme: Theme) -> Div {
    div()
        .w_px()
        .h_3()
        .flex_none()
        .mx_1()
        .bg(Theme::wash(theme.text_muted, 0.35))
}

/// SCENE A — the merge decision strip, exactly as it renders in the editor:
/// an inset card above a conflict region.
fn scene_strip(theme: Theme, cand: Candidate, selected: bool) -> Div {
    let picks: [(&'static str, &'static str, Rgba, &'static str); 4] = [
        ("sd", "disk", theme.drift, "1"),
        ("ss", "source", theme.accent, "2"),
        ("sb", "last written", theme.text_muted, "3"),
        ("sw", "both", theme.ok, "b"),
    ];
    let card = div()
        .h_full()
        .w_full()
        .px_2()
        .flex()
        .items_center()
        .gap_1p5()
        .text_xs()
        .rounded_md()
        .border_1()
        .border_color(if selected { theme.accent } else { theme.border })
        .bg(theme.surface)
        .child(
            div()
                .w_1p5()
                .h_1p5()
                .flex_none()
                .rounded_full()
                .bg(if selected {
                    theme.accent
                } else {
                    theme.conflict
                }),
        )
        .child(div().text_color(theme.text_muted).child(if selected {
            "auto-resolved"
        } else {
            "conflict"
        }))
        .child(div().flex_1());

    let card = if cand.is_group() {
        card.child(segmented_group(
            theme,
            "seg-strip",
            &[
                ("g1", "disk", theme.drift),
                ("g2", "source", theme.accent),
                ("g3", "last written", theme.text_muted),
                ("g4", "both", theme.ok),
            ],
            selected.then_some(0),
        ))
        .child(divider(theme))
        .child(lab_button(
            theme,
            Candidate::Quiet,
            "seg-edit",
            "edit",
            theme.text_muted,
            BtnState::Rest,
            None,
        ))
    } else {
        let mut c = card;
        for (ix, (id, label, tint, key)) in picks.into_iter().enumerate() {
            let state = if selected && ix == 0 {
                BtnState::Selected
            } else {
                BtnState::Rest
            };
            c = c.child(lab_button(theme, cand, id, label, tint, state, Some(key)));
        }
        c.child(divider(theme)).child(lab_button(
            theme,
            // `edit` is an escape hatch, not a side: never solid, never dotted.
            if matches!(cand, Candidate::Solid | Candidate::NeutralDot) {
                Candidate::Quiet
            } else {
                cand
            },
            "s-edit",
            "edit",
            theme.text_muted,
            BtnState::Rest,
            None,
        ))
    };

    // The mono code rows around it are what makes the strip readable in situ.
    div()
        .w_full()
        .flex()
        .flex_col()
        .child(code_row(theme, "  \"plugin\": [", theme.text_muted, None))
        .child(div().w_full().h_10().px_1().py_1p5().child(card))
        .child(code_row(
            theme,
            "    editor = \"helix\"",
            theme.text,
            Some(theme.drift),
        ))
        .child(code_row(
            theme,
            "    editor = \"zed --wait\"",
            theme.text,
            Some(theme.accent),
        ))
        .child(code_row(theme, "  ],", theme.text_muted, None))
}

/// A mono line with an optional provenance wash + rail (the editor's rows).
fn code_row(_theme: Theme, text: &'static str, color: Rgba, side: Option<Rgba>) -> Div {
    div()
        .h_5()
        .w_full()
        .flex()
        .items_center()
        .font_family("Menlo")
        .text_xs()
        .text_color(color)
        .when_some(side, |el, tint| el.bg(Theme::wash(tint, 0.07)))
        .child(
            div()
                .w(px(2.))
                .h_full()
                .flex_none()
                .when_some(side, |el, tint| el.bg(tint)),
        )
        .child(div().pl(px(26.)).child(text))
}

/// SCENE B — the review detail header: filename + the quick actions.
fn scene_header(theme: Theme, cand: Candidate) -> Div {
    let primary = if cand == Candidate::Hairline {
        Candidate::Hairline
    } else {
        cand
    };
    div()
        .w_full()
        .h(px(56.))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child("settings.json"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child("~/.config/zed/settings.json"),
                ),
        )
        .child(div().flex_1())
        .child(lab_button(
            theme,
            if primary.is_group() {
                Candidate::Quiet
            } else {
                primary
            },
            "h-disk",
            "Keep disk",
            theme.drift,
            BtnState::Rest,
            None,
        ))
        .child(lab_button(
            theme,
            if primary.is_group() {
                Candidate::Quiet
            } else {
                primary
            },
            "h-src",
            "Keep source",
            theme.accent,
            BtnState::Rest,
            None,
        ))
        .child(lab_button(
            theme,
            if primary.is_group() {
                Candidate::Quiet
            } else {
                primary
            },
            "h-merge",
            "Open merge editor",
            theme.text,
            BtnState::Rest,
            None,
        ))
}

/// SCENE C — the settings toolbar pill (dirty state) and the red dashboard
/// banner: where a PRIMARY action and a destructive-ish action live.
fn scene_toolbar(theme: Theme, cand: Candidate) -> Div {
    let c = if cand.is_group() {
        Candidate::Tonal
    } else {
        cand
    };
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            // Floating Save/Revert pill.
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1p5()
                .rounded_lg()
                .bg(theme.surface)
                .border_1()
                .border_color(theme.border)
                .shadow_xs()
                .child(
                    div()
                        .pl_1()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child("unsaved changes · saving restarts the sync daemon"),
                )
                .child(lab_button(
                    theme,
                    Candidate::Quiet,
                    "t-revert",
                    "Revert",
                    theme.text_muted,
                    BtnState::Rest,
                    None,
                ))
                .child(lab_button(
                    theme,
                    c,
                    "t-save",
                    "Save",
                    theme.accent,
                    BtnState::Rest,
                    None,
                )),
        )
        .child(
            // Diverged-repo banner: the action sits ON a tinted field.
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(Theme::wash(theme.conflict, 0.12))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text)
                        .child("dotfiles repo diverged from origin — 2 local / 1 origin commits"),
                )
                .child(div().flex_1())
                .child(lab_button(
                    theme,
                    c,
                    "b-rec",
                    "Reconcile…",
                    theme.conflict,
                    BtnState::Rest,
                    None,
                )),
        )
}

/// The bare states row: rest / selected / disabled across the app's tints.
fn scene_states(theme: Theme, cand: Candidate) -> Div {
    let row = div().flex().items_center().gap_1p5().flex_wrap();
    if cand.is_group() {
        return row
            .child(segmented_group(
                theme,
                "seg-states",
                &[
                    ("st1", "disk", theme.drift),
                    ("st2", "source", theme.accent),
                    ("st3", "both", theme.ok),
                ],
                Some(1),
            ))
            .child(div().w_2())
            .child(
                div()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child("selection is structural — one track, one winner"),
            );
    }
    row.child(lab_button(
        theme,
        cand,
        "x1",
        "disk",
        theme.drift,
        BtnState::Rest,
        Some("1"),
    ))
    .child(lab_button(
        theme,
        cand,
        "x2",
        "source",
        theme.accent,
        BtnState::Rest,
        Some("2"),
    ))
    .child(lab_button(
        theme,
        cand,
        "x3",
        "both",
        theme.ok,
        BtnState::Rest,
        Some("b"),
    ))
    .child(lab_button(
        theme,
        cand,
        "x4",
        "selected",
        theme.accent,
        BtnState::Selected,
        None,
    ))
    .child(lab_button(
        theme,
        cand,
        "x5",
        "disabled",
        theme.text_muted,
        BtnState::Disabled,
        None,
    ))
    .child(lab_button(
        theme,
        cand,
        "x6",
        "Reconcile…",
        theme.conflict,
        BtnState::Rest,
        None,
    ))
}

/// A scene sits on the app background inside a hairline frame, so each
/// candidate is judged on the surface it will really live on.
fn framed(theme: Theme, inner: Div) -> Div {
    div()
        .w_full()
        .p_1p5()
        .rounded_md()
        .bg(theme.bg)
        .border_1()
        .border_color(Theme::wash(theme.border, 0.7))
        .child(inner)
}

/// A row label: the candidate number/name plus its one-line rationale,
/// in a fixed column so the scenes to their right line up for comparison.
fn row_label(theme: Theme, cand: Candidate) -> Div {
    div()
        .w(px(190.))
        .flex_none()
        .flex()
        .flex_col()
        .child(
            div()
                .font_family("Menlo")
                .text_xs()
                .text_color(theme.accent)
                .child(cand.name()),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(Theme::wash(theme.text_muted, 0.9))
                .child(cand.note()),
        )
}

/// One comparison row: the label column and whatever scene is being compared.
fn compare_row(theme: Theme, cand: Candidate, scene: Div) -> Div {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_3()
        .py_1p5()
        .border_b_1()
        .border_color(Theme::wash(theme.border, 0.5))
        .child(row_label(theme, cand))
        .child(div().flex_1().min_w_0().child(scene))
}

fn section_title(theme: Theme, title: &'static str, sub: &'static str) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_0p5()
        .pt_4()
        .pb_2()
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(title),
        )
        .child(div().text_xs().text_color(theme.text_muted).child(sub))
}

/// The SHIPPED system, on each surface it keys off — living documentation
/// of the rule, rendered with the real [`crate::components::button`].
fn shipped(theme: Theme) -> Div {
    let row = |label: &'static str, inner: Div| {
        div()
            .flex()
            .items_center()
            .gap_3()
            .py_1()
            .child(
                div()
                    .w(px(190.))
                    .flex_none()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child(label),
            )
            .child(inner)
    };
    let trio = |variant: fn(Rgba) -> ButtonVariant, prefix: &'static str| {
        div()
            .flex()
            .items_center()
            .gap_1p5()
            .child(button(
                theme,
                ElementId::from(SharedString::from(format!("{prefix}-disk"))),
                "disk".into(),
                variant(theme.drift),
                ButtonSize::Sm,
                noop,
            ))
            .child(button(
                theme,
                ElementId::from(SharedString::from(format!("{prefix}-src"))),
                "source".into(),
                variant(theme.accent),
                ButtonSize::Sm,
                noop,
            ))
            .child(button(
                theme,
                ElementId::from(SharedString::from(format!("{prefix}-neutral"))),
                "Open merge editor".into(),
                variant(theme.text_muted),
                ButtonSize::Sm,
                noop,
            ))
            .child(button(
                theme,
                ElementId::from(SharedString::from(format!("{prefix}-remote"))),
                "origin".into(),
                variant(theme.remote),
                ButtonSize::Sm,
                noop,
            ))
    };
    div()
        .w_full()
        .flex()
        .flex_col()
        // Plain background → Tonal.
        .child(row(
            "plain background · Tonal",
            trio(ButtonVariant::Tonal, "sh-t"),
        ))
        // Inside a bordered container → Ring.
        .child(row(
            "in a bordered container · Ring",
            div()
                .p_1p5()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .child(trio(ButtonVariant::Ring, "sh-r")),
        ))
        // On a filled field → Solid.
        .child(row(
            "on a filled field · Solid",
            div()
                .px_2()
                .py_1p5()
                .rounded_md()
                .bg(Theme::wash(theme.conflict, 0.12))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(button(
                            theme,
                            "sh-solid-1",
                            "Reconcile…".into(),
                            ButtonVariant::Solid(theme.conflict),
                            ButtonSize::Sm,
                            noop,
                        ))
                        .child(button(
                            theme,
                            "sh-solid-2",
                            "Unlock 1Password".into(),
                            ButtonVariant::Solid(theme.drift),
                            ButtonSize::Sm,
                            noop,
                        )),
                ),
        ))
        .child(row(
            "tertiary · Ghost — and disabled",
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .child(button(
                    theme,
                    "sh-ghost",
                    "Discard edit".into(),
                    ButtonVariant::Ghost,
                    ButtonSize::Sm,
                    noop,
                ))
                .child(disabled_button(
                    theme,
                    "sh-disabled",
                    "Save".into(),
                    ButtonSize::Sm,
                    Some("1 conflict left".into()),
                )),
        ))
}

/// The lab page, grouped BY SCENE (not by candidate) so the twelve
/// variants of the same context stack directly under each other — that is
/// the only way to actually judge them against one another.
pub fn render(theme: Theme) -> Stateful<Div> {
    div()
        .id("button-lab")
        .size_full()
        .overflow_y_scroll()
        .p_4()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child("Button design candidates"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child(SharedString::from(
                            "Twelve designs, each shown in the scenes it would really live \
                             in. Compare down a column, then tell me a number (mixing is \
                             fine — e.g. tonal picks + solid Save).",
                        )),
                ),
        )
        // What actually shipped, rendered with the real component.
        .child(section_title(
            theme,
            "✓ Shipped",
            "the surface rule, as implemented in tomte_ui::button",
        ))
        .child(shipped(theme))
        // AT A GLANCE — every candidate's raw vocabulary, one row each.
        .child(section_title(
            theme,
            "0 · At a glance",
            "rest · selected · disabled, across every tint the app uses",
        ))
        .children(Candidate::ALL.map(|cand| compare_row(theme, cand, scene_states(theme, cand))))
        // SCENE 1 — the dense case that provoked this.
        .child(section_title(
            theme,
            "1 · The merge decision strip",
            "the hardest case: four choices + an escape hatch, on a code surface \
             (the 'current choice' look is in section 0)",
        ))
        .children(
            Candidate::ALL.map(|cand| compare_row(theme, cand, strip_scene(theme, cand, false))),
        )
        // SCENE 2 — the review header's quick actions.
        .child(section_title(
            theme,
            "2 · Review header actions",
            "three side-by-side actions on a plain header — no code underneath",
        ))
        .children(
            Candidate::ALL
                .map(|cand| compare_row(theme, cand, framed(theme, scene_header(theme, cand)))),
        )
        // SCENE 3 — primary actions on chrome and on a tinted field.
        .child(section_title(
            theme,
            "3 · Primary actions",
            "Save in the floating toolbar, and an action sitting ON a red banner",
        ))
        .children(
            Candidate::ALL
                .map(|cand| compare_row(theme, cand, framed(theme, scene_toolbar(theme, cand)))),
        )
}

/// The strip scene, compact: the card plus the code row it sits above.
fn strip_scene(theme: Theme, cand: Candidate, selected: bool) -> Div {
    framed(theme, scene_strip(theme, cand, selected))
}
