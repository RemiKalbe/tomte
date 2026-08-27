//! The changelog window: release notes rendered as markdown, in a window of
//! their own.
//!
//! 2026-08-27: the Updates row used to inline the first fourteen lines of a
//! release body with the `**` stripped out and "… full notes on the GitHub
//! release" underneath. Now nothing shows by default, and a button opens
//! THIS — the whole document, properly rendered, offline.
//!
//! Two sources feed it: the CHANGELOG.md compiled into the binary (this
//! build's own history) and, when an update is staged, that release's notes
//! — which by definition are newer than anything the running binary shipped
//! with.

use gpui::{
    App, Context, IntoElement, Render, SharedString, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, size,
};
use tomte_app::theme::Theme;
use tomte_ui::components as ui;

/// This build's own changelog, compiled in so the window works with no
/// network and no repo checkout.
pub const BUNDLED_CHANGELOG: &str = include_str!("../../../../CHANGELOG.md");

pub struct ChangelogView {
    title: SharedString,
    /// Parsed once at construction — markdown does not change under us.
    blocks: Vec<ui::markdown::Block>,
}

impl ChangelogView {
    pub fn new(title: impl Into<SharedString>, source: &str) -> Self {
        Self {
            title: title.into(),
            blocks: ui::markdown::parse(source),
        }
    }
}

impl ChangelogView {
    /// Does the document already open with its own top-level title? Then the
    /// window must not print one above it (2026-08-27: "Tomte — changelog"
    /// sat directly on top of the `# Changelog` heading). Release notes
    /// usually start at `##` or a bullet, and those DO want a title.
    fn has_own_title(&self) -> bool {
        matches!(
            self.blocks.first(),
            Some(ui::markdown::Block::Heading { level: 1, .. })
        )
    }
}

impl Render for ChangelogView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        let own_title = self.has_own_title();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .when(!own_title, |el| {
                el.child(
                    div()
                        .flex_none()
                        .px_5()
                        .pt_10()
                        .pb_3()
                        .border_b_1()
                        .border_color(theme.border)
                        .text_lg()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(self.title.clone()),
                )
            })
            .child(
                div()
                    .id("changelog-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_5()
                    .pb_4()
                    // With no header bar the content clears the floating
                    // traffic lights itself.
                    .map(|el| if own_title { el.pt_10() } else { el.pt_4() })
                    .child(ui::markdown::render(&self.blocks, theme)),
            )
    }
}

/// Open (or re-open) the changelog in its own window.
pub fn open_changelog(title: impl Into<SharedString>, source: String, cx: &mut App) {
    let title = title.into();
    let window_title = title.clone();
    let bounds = gpui::Bounds::centered(None, size(px(720.), px(760.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some(window_title),
                appears_transparent: true,
                traffic_light_position: Some(gpui::point(px(12.), px(12.))),
            }),
            window_min_size: Some(size(px(420.), px(320.))),
            ..Default::default()
        },
        |_window, cx| cx.new(|_| ChangelogView::new(title, &source)),
    )
    .ok();
}

#[cfg(test)]
mod tests {
    use super::BUNDLED_CHANGELOG;
    use tomte_ui::components::markdown;

    /// The bundled changelog must actually parse into the structure the
    /// window renders — a silently-empty window would be worse than the old
    /// truncated block.
    #[test]
    fn the_bundled_changelog_parses_into_headings_and_bullets() {
        let blocks = markdown::parse(BUNDLED_CHANGELOG);
        let headings = blocks
            .iter()
            .filter(|b| matches!(b, markdown::Block::Heading { .. }))
            .count();
        let bullets = blocks
            .iter()
            .filter(|b| matches!(b, markdown::Block::ListItem { .. }))
            .count();
        assert!(
            headings >= 5,
            "every released version contributes a heading, got {headings}"
        );
        assert!(bullets >= 10, "release entries are bullets, got {bullets}");
        // And the version headings survive with their text intact.
        assert!(blocks.iter().any(|b| matches!(
            b,
            markdown::Block::Heading { level: 2, content } if content.text.starts_with("0.1.")
        )));
    }
}
