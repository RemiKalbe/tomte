//! The groups of a folder comparison (a folder in the source against what
//! the disk holds there now): a muted label, then its rows. A row that
//! carries a decision has a checkbox; names that need none are listed
//! inline, wrapped, in the same column as the rows' names.

use gpui::{
    App, ClickEvent, Div, ElementId, FontWeight, SharedString, Stateful, Window, div, prelude::*,
    px,
};

use crate::theme::Theme;

use super::checkbox::{CheckState, checkbox};

/// Checkbox width plus the gap after it: the column every name starts in,
/// with or without a checkbox in front of it.
const NAME_INDENT: f32 = 14. + 8.;
/// A row's name column. Definite, so a long name ends in an ellipsis
/// (`line_clamp` measures against it) instead of pushing the note away.
const NAME_W: f32 = 200.;

/// A group's label: text_xs, semibold, muted. The rows under it are
/// separate elements, so a long group can ride a virtualized list.
pub fn compare_label(theme: Theme, label: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text_muted)
        .child(label.into())
}

/// What a source file is beyond its bytes, marked after its name: Keep
/// disk removes it from the source, and these are the ones a user cannot
/// simply copy back from the disk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NameMarks {
    /// A template: `{}` in the drift tint, the line editor's template mark.
    pub template: bool,
    /// Stored encrypted: the word, muted.
    pub encrypted: bool,
}

/// The marks after a name, or nothing.
fn name_marks(theme: Theme, marks: NameMarks) -> Option<Div> {
    if marks == NameMarks::default() {
        return None;
    }
    Some(
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .text_xs()
            .when(marks.template, |el| {
                el.child(div().text_color(theme.drift).child("{}"))
            })
            .when(marks.encrypted, |el| {
                el.child(div().text_color(theme.text_muted).child("encrypted"))
            }),
    )
}

/// What a [`compare_row`] carries besides its name.
pub struct CompareRowSpec {
    /// The pick and its words ("copy into the link target"), when the row
    /// carries one.
    pub pick: Option<(bool, &'static str)>,
    /// The name opens something (its line diff); `open` marks the one open.
    pub opens: bool,
    pub open: bool,
    pub marks: NameMarks,
}

/// One file row: checkbox (or its empty slot), name, then the pick's words.
/// `on_pick` runs on the checkbox, `on_open` on the name when
/// `spec.opens`.
pub fn compare_row(
    theme: Theme,
    id: impl Into<ElementId>,
    name: SharedString,
    spec: CompareRowSpec,
    on_pick: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_open: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let id = id.into();
    let slot = match spec.pick {
        Some((picked, _)) => checkbox(
            theme,
            ElementId::Name(format!("{id}-pick").into()),
            if picked {
                CheckState::On
            } else {
                CheckState::Off
            },
            on_pick,
        )
        .into_any_element(),
        None => div().w(px(14.)).flex_none().into_any_element(),
    };
    let name_el = div()
        .id(ElementId::Name(format!("{id}-name").into()))
        .w(px(NAME_W))
        .flex_none()
        .px_1()
        .rounded_sm()
        .overflow_hidden()
        .line_clamp(1)
        .text_ellipsis()
        .text_color(if spec.opens { theme.accent } else { theme.text })
        .when(spec.open, |el| {
            el.bg(Theme::wash(theme.accent, 0.12))
                .font_weight(FontWeight::SEMIBOLD)
        })
        .when(spec.opens, |el| {
            el.cursor_pointer()
                .hover(|el| el.bg(Theme::wash(theme.text, 0.08)))
                .on_click(on_open)
        })
        .child(name);
    div()
        .id(id)
        .h_6()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .child(slot)
        .child(name_el)
        .children(name_marks(theme, spec.marks))
        .children(spec.pick.map(|(_, words)| {
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(theme.text_muted)
                .line_clamp(1)
                .child(words)
        }))
}

/// Names that carry no decision, wrapped inline in the rows' name column,
/// each followed by its marks.
pub fn compare_names(theme: Theme, names: Vec<(SharedString, NameMarks)>) -> Div {
    div()
        .pl(px(NAME_INDENT))
        .flex()
        .flex_wrap()
        .gap_x_4()
        .gap_y_0p5()
        .text_sm()
        .text_color(theme.text)
        .children(names.into_iter().map(|(name, marks)| {
            div()
                .px_1()
                .flex()
                .items_center()
                .gap_1()
                .child(name)
                .children(name_marks(theme, marks))
        }))
}
