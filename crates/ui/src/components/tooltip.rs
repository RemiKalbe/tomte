//! The themed tooltip bubble (shared by every `.tooltip(...)` in the app).

use gpui::{AnyView, App, Context, IntoElement, Render, SharedString, Window, div, prelude::*};

use crate::theme::Theme;

/// Minimal themed bubble. Theme is resolved per-render (tooltips paint later,
/// possibly after an appearance change), so it is not a constructor param.
pub struct TextTooltip {
    pub text: SharedString,
}

impl Render for TextTooltip {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme.surface)
            .border_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.text_muted)
            .child(self.text.clone())
    }
}

/// Factory for `.tooltip(...)`: `el.tooltip(text_tooltip("why disabled"))`.
pub fn text_tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text: SharedString = text.into();
    move |_window, cx| {
        let text = text.clone();
        cx.new(|_| TextTooltip { text }).into()
    }
}
