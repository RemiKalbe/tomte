pub mod dashboard;
pub mod review;
pub mod settings;

use gpui::{AppContext as _, Context, Entity, SharedString, Window, div, prelude::*};

use czui_app::model::SyncModel;
use czui_app::theme::Theme;

use dashboard::DashboardView;
use review::ReviewView;
use settings::{SettingsPaths, SettingsView};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Dashboard,
    Review,
    Settings,
}

pub struct Shell {
    pub route: Route,
    /// The shared sync model; the window observes it (wired in `open_shell`)
    /// so every entity notify re-renders the shell.
    pub state: Entity<SyncModel>,
    /// Created on the first visit to Review and kept for the window's
    /// lifetime, so target selection and the loaded preview survive route
    /// switches (unlike the stateless dashboard body).
    pub review: Option<Entity<ReviewView>>,
    /// Same lifetime rationale as `review`: stepper/picker edits and the
    /// background loads survive route switches.
    pub settings: Option<Entity<SettingsView>>,
    /// Daemon-facing paths the Settings view displays and writes — resolved
    /// once in main.rs so all path policy stays in one place.
    pub paths: SettingsPaths,
}

impl Shell {
    fn nav_button(
        &self,
        theme: &Theme,
        label: &'static str,
        route: Route,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.route == route;
        div()
            .id(label)
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .when(active, |el| el.bg(theme.surface))
            .text_color(if active { theme.text } else { theme.text_muted })
            .child(SharedString::from(label))
            .on_click(cx.listener(move |shell, _ev, _window, cx| {
                shell.route = route;
                cx.notify();
            }))
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg)
            .text_color(theme.text)
            .child(
                div()
                    .flex()
                    .gap_1()
                    .p_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(self.nav_button(&theme, "Dashboard", Route::Dashboard, cx))
                    .child(self.nav_button(&theme, "Review", Route::Review, cx))
                    .child(self.nav_button(&theme, "Settings", Route::Settings, cx)),
            )
            .child(match self.route {
                Route::Dashboard => DashboardView {
                    state: self.state.clone(),
                    now_ts: dashboard::system_now,
                }
                .render(theme, cx)
                .into_any_element(),
                Route::Review => {
                    let state = self.state.clone();
                    self.review
                        .get_or_insert_with(|| cx.new(|cx| ReviewView::new(state, cx)))
                        .clone()
                        .into_any_element()
                }
                Route::Settings => {
                    let paths = self.paths.clone();
                    self.settings
                        .get_or_insert_with(|| cx.new(|cx| SettingsView::new(paths, cx)))
                        .clone()
                        .into_any_element()
                }
            })
    }
}
