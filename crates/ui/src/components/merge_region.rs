//! Merge-region components: the decision strip and tinted provenance rows
//! (merge-editor-v2 spec, Zed conflict_view.rs pattern — always-visible
//! labeled buttons above the region; read-only provenance is structural).
//!
//! Tint mapping (established app vocabulary): ours/disk = drift amber,
//! theirs/source = accent blue, origin = remote purple. Purple is not a
//! merge colour here — it means "the remote", and nothing else uses it.

use std::collections::HashMap;

use gpui::{App, ClickEvent, Div, ElementId, Rgba, SharedString, Window, div, prelude::*, px};

use crate::components::button::{ButtonSize, ButtonVariant, button};
use crate::components::icon::lock;
use crate::components::mono::{line_gutter, line_text, mono_line};
use crate::components::tooltip::text_tooltip;
use crate::theme::Theme;

/// Which side a provenance block shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Disk (ours) — drift amber.
    Ours,
    /// Source (theirs) — accent blue.
    Theirs,
    /// Last-written base — muted.
    Base,
}

impl Side {
    pub fn tint(self, theme: Theme) -> Rgba {
        match self {
            Self::Ours => theme.drift,
            Self::Theirs => theme.accent,
            Self::Base => theme.text_muted,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ours => "on disk",
            Self::Theirs => "source",
            Self::Base => "last written",
        }
    }
}

/// The choices a strip can display/emit. Mirrors the engine's Choice minus
/// Edited's payload (the strip only names it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceKind {
    Ours,
    Theirs,
    Base,
    Both,
    Edited,
    Origin,
}

impl ChoiceKind {
    /// The provenance tint this choice reads as (matches the result rows).
    pub fn tint(self, theme: Theme) -> Rgba {
        match self {
            Self::Ours => theme.drift,
            Self::Theirs => theme.accent,
            Self::Base => theme.text_muted,
            Self::Both | Self::Edited => theme.ok,
            Self::Origin => theme.remote,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ours => "disk",
            Self::Theirs => "source",
            Self::Base => "last written",
            Self::Both => "both",
            Self::Edited => "edited",
            Self::Origin => "origin",
        }
    }
}

/// What a strip button EMITS, as opposed to what a strip displays. The two
/// differ: `Edited` is a state a region can be in but never a button (free
/// text arrives through `on_edit`), and `Origin` is only offerable when
/// origin actually has text for this region — which it carries, so the host
/// has nothing to look up and no case where a click can do nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    Ours,
    Theirs,
    Base,
    Both,
    /// Origin's version of this region, verbatim.
    Origin(SharedString),
}

impl Pick {
    /// The choice this pick lands as — what a strip compares against
    /// `current` to render its button selected, and where its label and tint
    /// come from.
    pub fn kind(&self) -> ChoiceKind {
        match self {
            Self::Ours => ChoiceKind::Ours,
            Self::Theirs => ChoiceKind::Theirs,
            Self::Base => ChoiceKind::Base,
            Self::Both => ChoiceKind::Both,
            Self::Origin(_) => ChoiceKind::Origin,
        }
    }
}

/// Strip state drives which buttons show and which reads selected.
pub enum StripState {
    /// Conflict awaiting a decision (or an auto-resolved region being
    /// revisited): full button row. `auto` marks the engine's default
    /// choice for non-conflict regions, rendered as the current selection.
    Deciding {
        has_base: bool,
        current: Option<ChoiceKind>,
        focused: bool,
        /// Origin's text for this region, when origin HAS one that differs
        /// from the local source. `None` renders no origin button at all —
        /// most files have no origin version, and most regions of the ones
        /// that do match it.
        origin: Option<SharedString>,
        /// Whether the Both button is offered. Line regions always can; a
        /// structured array entry has no meaningful "both".
        both: bool,
    },
    /// A decision exists; collapsed affordance names it and offers revisit.
    /// `focused`: the region cursor sits here (Next ↓ walks decided regions
    /// too once every conflict is resolved).
    Decided { choice: ChoiceKind, focused: bool },
    /// Follows source; no buttons. `reason` is the hover text (the template
    /// line that owns the entry).
    Locked { reason: SharedString, focused: bool },
}

/// Widest a strip's leading path label grows before it ellipsizes.
const LABEL_MAX_W: f32 = 170.;

/// The leading path label (e.g. `editor.fontSize`). `line_clamp(1)` inside a
/// bounded width: `.truncate()` hard-clips mid-glyph in gpui 0.2.2.
fn path_label(theme: Theme, label: SharedString) -> Div {
    div()
        .max_w(px(LABEL_MAX_W))
        .min_w_0()
        .line_clamp(1)
        .text_ellipsis()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text)
        .child(label)
}

/// The words after the label: the caller's `detail` when given, else the
/// engine-state wording the line editor has always shown.
fn state_words(detail: Option<SharedString>, fallback: &'static str) -> SharedString {
    detail.unwrap_or_else(|| fallback.into())
}

/// Decision strip rendered above a changed region, styled after Zed's
/// multibuffer buffer header (`editor/src/element/header.rs`): an INSET
/// rounded card — 4px padding all round, hairline border, subtle surface
/// fill, hover highlight — rather than a full-bleed tinted bar. Two rows
/// tall so the controls breathe. A leading dot + label name the region's
/// state; the actions sit on the trailing edge.
///
/// All behavior arrives via `on_pick` / `on_edit` / `on_revisit` so the
/// builder stays pure.
///
/// `selector_prefix` gives tests a handle on the strip (a no-op outside
/// test-support builds): with `Some("p")` the strip root carries the debug
/// selector `p:<region_ix>`, each pick button `p-pick:<region_ix>:<pick>`
/// (`disk`, `source`, `origin`, `last-written`, `both`), the edit button
/// `p-edit:<region_ix>` and the revisit button `p-revisit:<region_ix>`.
#[allow(clippy::too_many_arguments)]
pub fn decision_strip(
    theme: Theme,
    region_ix: usize,
    state: StripState,
    label: Option<SharedString>,
    detail: Option<SharedString>,
    selector_prefix: Option<SharedString>,
    on_pick: impl Fn(Pick, &ClickEvent, &mut Window, &mut App) + Clone + 'static,
    on_edit: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_revisit: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let (accent, focused) = match &state {
        StripState::Deciding {
            current, focused, ..
        } => (
            if current.is_none() {
                theme.conflict
            } else {
                theme.text_muted
            },
            *focused,
        ),
        StripState::Decided { choice, focused } => (choice.tint(theme), *focused),
        StripState::Locked { focused, .. } => (theme.text_muted, *focused),
    };
    // Zed's header: `border_focused` when the header owns the cursor,
    // plain `border` otherwise.
    let border = if focused { accent } else { theme.border };
    // The leading cell takes the free width (it replaces the old spacer) and
    // may shrink below its content, so a long path label ellipsizes instead
    // of pushing the buttons out of the card.
    let lead = |words: Option<(SharedString, Rgba)>| {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap_1p5()
            .when_some(label.clone(), |el, label| {
                el.child(path_label(theme, label))
            })
            .when_some(words, |el, (words, color)| {
                el.child(div().flex_none().text_color(color).child(words))
            })
    };
    let card = div()
        .w_full()
        // Explicit padding, NOT `h_full` + `items_center`: an ancestor that
        // doesn't propagate height collapses the card onto the buttons and
        // their edges end up flush against its border (2026-08-25).
        .px_2()
        .py_1()
        .flex()
        .items_center()
        .gap_1p5()
        .text_xs()
        .rounded_md()
        .border_1()
        .border_color(border)
        .bg(theme.surface)
        // State dot: the same vocabulary as the provenance labels.
        .child(div().w_1p5().h_1p5().flex_none().rounded_full().bg(accent));

    let body = match state {
        StripState::Deciding {
            has_base,
            current,
            focused,
            origin,
            both,
        } => {
            let pick_button = |id: &'static str, pick: Pick, label: &'static str, tint: Rgba| {
                let on_pick = on_pick.clone();
                let selected = current == Some(pick.kind());
                let selector = pick_selector(&pick);
                button(
                    theme,
                    ElementId::named_usize(id, region_ix),
                    label.into(),
                    ButtonVariant::Ring(tint),
                    ButtonSize::Micro,
                    move |ev, window, cx| on_pick(pick.clone(), ev, window, cx),
                )
                .when(selected, |el| el.bg(Theme::wash(tint, 0.2)))
                .when_some(selector_prefix.clone(), |el, prefix| {
                    el.debug_selector(|| format!("{prefix}-pick:{region_ix}:{selector}"))
                })
            };
            let edit_selector = selector_prefix.clone();
            card.child(lead(Some((
                state_words(
                    detail,
                    if current.is_none() {
                        "conflict"
                    } else {
                        // "merged" describes the whole-file operation; ONE
                        // region that needed no human decision was resolved.
                        "auto-resolved"
                    },
                ),
                if focused { accent } else { theme.text_muted },
            ))))
            .child(pick_button("pick-ours", Pick::Ours, "disk", theme.drift))
            .child(pick_button(
                "pick-theirs",
                Pick::Theirs,
                "source",
                theme.accent,
            ))
            // Next to the source it is a variant of: the remote's copy of
            // the same file, offered only where it has something different.
            .when_some(origin, |el, text| {
                el.child(pick_button(
                    "pick-origin",
                    Pick::Origin(text),
                    "origin",
                    theme.remote,
                ))
            })
            .when(has_base, |el| {
                el.child(pick_button(
                    "pick-base",
                    Pick::Base,
                    "last written",
                    theme.text_muted,
                ))
            })
            .when(both, |el| {
                el.child(pick_button("pick-both", Pick::Both, "both", theme.ok))
            })
            .child(divider(theme))
            .child(
                button(
                    theme,
                    ElementId::named_usize("edit-region", region_ix),
                    "edit".into(),
                    ButtonVariant::Ghost,
                    ButtonSize::Micro,
                    on_edit,
                )
                .when_some(edit_selector, |el, prefix| {
                    el.debug_selector(|| format!("{prefix}-edit:{region_ix}"))
                }),
            )
        }
        StripState::Decided { choice, .. } => card
            .child(lead(Some((
                SharedString::from(format!("resolved · {}", choice.label())),
                theme.text_muted,
            ))))
            .child(
                button(
                    theme,
                    ElementId::named_usize("revisit-region", region_ix),
                    "revisit".into(),
                    ButtonVariant::Ring(theme.text_muted),
                    ButtonSize::Micro,
                    on_revisit,
                )
                .when_some(selector_prefix.clone(), |el, prefix| {
                    el.debug_selector(|| format!("{prefix}-revisit:{region_ix}"))
                }),
            )
            .child(divider(theme))
            .child(
                button(
                    theme,
                    ElementId::named_usize("edit-region", region_ix),
                    "edit".into(),
                    ButtonVariant::Ghost,
                    ButtonSize::Micro,
                    on_edit,
                )
                .when_some(selector_prefix.clone(), |el, prefix| {
                    el.debug_selector(|| format!("{prefix}-edit:{region_ix}"))
                }),
            ),
        StripState::Locked { reason, .. } => card
            .child(lead(detail.map(|d| (d, theme.text_muted))))
            // The hover text names the template line that owns the entry.
            .child(
                div()
                    .id(ElementId::named_usize("locked-region", region_ix))
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_color(theme.text_muted)
                    .child(lock(theme))
                    .child("templated · follows source")
                    .tooltip(text_tooltip(reason)),
            ),
    };

    // The 4px inset (Zed's BUFFER_HEADER_PADDING) is what makes the strip
    // read as a card floating in the buffer instead of a band across it.
    div()
        .w_full()
        .h_full()
        .px_1()
        .flex()
        .items_center()
        .when_some(selector_prefix, |el, prefix| {
            el.debug_selector(|| format!("{prefix}:{region_ix}"))
        })
        .child(body)
}

/// The suffix a pick button's debug selector ends in.
fn pick_selector(pick: &Pick) -> &'static str {
    match pick {
        Pick::Ours => "disk",
        Pick::Theirs => "source",
        Pick::Origin(_) => "origin",
        Pick::Base => "last-written",
        Pick::Both => "both",
    }
}

/// Hairline vertical rule separating action groups inside a strip.
fn divider(theme: Theme) -> Div {
    div()
        .w_px()
        .h_3()
        .flex_none()
        .mx_0p5()
        .bg(Theme::wash(theme.text_muted, 0.35))
}

/// Header line naming a provenance block ("on disk", "source"…).
pub fn provenance_label(theme: Theme, side: Side) -> Div {
    let tint = side.tint(theme);
    div()
        .h_5()
        .px_2()
        .flex()
        .items_center()
        .gap_1p5()
        .text_xs()
        .child(div().w_1p5().h_1p5().rounded_full().bg(tint))
        .child(div().text_color(tint).child(side.label()))
}

/// Width of the leading "template" column when a block has protected lines
/// (mirrors the pane gutter: uniform per block, the chip can never clip).
pub fn provenance_rows(
    theme: Theme,
    side: Side,
    lines: &[SharedString],
    protected: &HashMap<usize, SharedString>,
) -> Div {
    let tint = side.tint(theme);
    div()
        .flex()
        .flex_col()
        .border_l_2()
        .border_color(tint)
        .bg(Theme::wash(tint, 0.07))
        .children(lines.iter().enumerate().map(|(ix, line)| {
            let tip = protected.get(&ix).cloned();
            let row = mono_line(theme)
                .when(tip.is_some(), |el| el.bg(Theme::wash(theme.drift, 0.15)))
                .child(line_gutter(
                    theme.drift,
                    if tip.is_some() { "{}" } else { "" },
                ))
                .child(line_text(line.clone()));
            match tip {
                Some(tip) => row
                    .id(ElementId::named_usize("prov-tmpl", ix))
                    .tooltip(text_tooltip(tip))
                    .into_any_element(),
                None => row.into_any_element(),
            }
        }))
}
