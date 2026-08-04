//! Shell: left sidebar navigation (Zed settings-window shape — fixed-width
//! panel, bordered, compact nav rows) over a routed content pane.

pub mod dashboard;
pub mod fixtures;
pub mod merge;
pub mod review;
pub mod settings;

use std::path::PathBuf;

use gpui::{
    AppContext as _, Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px,
};

use czui_app::model::{SyncModel, time_ago};
use czui_app::theme::Theme;
use czui_ui::components as ui;

use dashboard::DashboardView;
use merge::MergeView;
use review::{OutcomeBanner, ReviewView};
use settings::{SettingsPaths, SettingsView};

/// Zed's settings sidebar is 226px; ours carries shorter labels.
const SIDEBAR_WIDTH: f32 = 200.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Dashboard,
    Review,
    Settings,
    /// Full-window merge editor (plan 7 Task 3). Not a sidebar item — entered
    /// via Review's "Open merge editor", left via Cancel/Save.
    Merge,
    /// Gallery-only: one isolated component preview (`--gallery comp:<name>`),
    /// rendered without the sidebar in a small window. Never reachable from
    /// the app's own navigation.
    Component(&'static str),
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
    /// Lazy like `review`; kept alive so the loaded merge inputs and the
    /// user's per-region choices survive a route switch and back.
    pub merge: Option<Entity<MergeView>>,
    /// Daemon-facing paths the Settings view displays and writes — resolved
    /// once in main.rs so all path policy stays in one place.
    pub paths: SettingsPaths,
    /// Expanded scan-groups in the dashboard timeline, keyed by the group's
    /// newest event timestamp (stable across refreshes).
    pub expanded_scans: std::collections::HashSet<u64>,
    /// A dashboard quick action (keep disk / keep source) is running on the
    /// background executor. Lives here because the dashboard body is rebuilt
    /// from Shell state on every render (unlike the long-lived ReviewView,
    /// which tracks its own in-flight flag).
    pub dashboard_action_in_flight: bool,
    /// The 1Password unlock probe (degraded-banner button) is waiting on the
    /// user's approval.
    pub unlock_in_flight: bool,
}

impl Shell {
    /// Route to Review, optionally selecting a target (dashboard row click).
    pub fn open_review(&mut self, target: Option<PathBuf>, cx: &mut Context<Self>) {
        let review = self.ensure_review(cx);
        if let Some(target) = target {
            review.update(cx, |view, cx| view.select(target, cx));
        }
        self.route = Route::Review;
        cx.notify();
    }

    /// The lazily created Review entity (shared by routing, rendering, and
    /// the merge editor's banner hand-off).
    fn ensure_review(&mut self, cx: &mut Context<Self>) -> Entity<ReviewView> {
        let state = self.state.clone();
        let shell = cx.weak_entity();
        self.review
            .get_or_insert_with(|| cx.new(|cx| ReviewView::new(state, shell, cx)))
            .clone()
    }

    /// Open the full-window merge editor for `target` (plan 7 Task 3):
    /// ensure the lazy entity, kick the background inputs load, route.
    pub fn open_merge(&mut self, target: PathBuf, cx: &mut Context<Self>) {
        let shell = cx.weak_entity();
        let merge = self
            .merge
            .get_or_insert_with(|| cx.new(|cx| MergeView::new(shell, cx)))
            .clone();
        merge.update(cx, |view, cx| view.load(target, cx));
        self.route = Route::Merge;
        cx.notify();
    }

    /// Land a successful merge save: hand the outcome banner to Review (its
    /// banner owns the Undo button), reload its preview so the diff reflects
    /// the converged reality, and route back.
    pub fn merge_done(&mut self, banner: OutcomeBanner, cx: &mut Context<Self>) {
        let review = self.ensure_review(cx);
        review.update(cx, |view, cx| {
            view.last_outcome = Some(banner);
            if let Some(target) = view.selected.clone() {
                view.select(target, cx);
            }
            cx.notify();
        });
        self.route = Route::Review;
        cx.notify();
    }

    fn nav_item(
        &self,
        theme: &Theme,
        label: &'static str,
        route: Route,
        badge: Option<(String, gpui::Rgba)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.route == route;
        div()
            .id(label)
            .h_7()
            .px_2()
            .rounded_sm()
            .cursor_pointer()
            .flex()
            .items_center()
            .justify_between()
            .when(active, |el| el.bg(Theme::wash(theme.text, 0.08)))
            .hover(|el| el.bg(Theme::wash(theme.text, 0.05)))
            .child(
                div()
                    .text_sm()
                    .when(active, |el| el.font_weight(FontWeight::MEDIUM))
                    .text_color(if active { theme.text } else { theme.text_muted })
                    .child(SharedString::from(label)),
            )
            .when_some(badge, |el, (text, color)| {
                el.child(ui::chip(*theme, text, ui::ChipVariant::Wash(color)))
            })
            .on_click(cx.listener(move |shell, _ev, _window, cx| {
                if route == Route::Review {
                    shell.open_review(None, cx);
                } else {
                    shell.route = route;
                    cx.notify();
                }
            }))
    }

    fn sidebar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.state.read(cx);
        let attention = model.needs_attention();
        let drifted = model.drifted.len();
        let review_badge = if attention > 0 {
            Some((attention.to_string(), theme.conflict))
        } else if drifted > 0 {
            Some((drifted.to_string(), theme.drift))
        } else {
            None
        };

        // Footer facts: connection dot + freshness, always honest (spec §10).
        let (dot_tone, status_line): (ui::StatusTone, String) = if !model.connected {
            (ui::StatusTone::Conflict, "daemon not connected".into())
        } else if model.scanning {
            (ui::StatusTone::Drift, "scanning…".into())
        } else if let Some(hint) = &model.degraded {
            (ui::StatusTone::Drift, hint.clone())
        } else if drifted > 0 {
            (ui::StatusTone::Drift, format!("{drifted} drifted"))
        } else {
            (ui::StatusTone::Ok, format!("in sync · {} files", model.in_sync))
        };
        let freshness = match model.last_fetch_ts {
            Some(ts) => format!("origin: fetched {}", time_ago(dashboard::system_now(), ts)),
            None => "origin: never fetched".to_string(),
        };

        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap_px()
            .pt_10() // room for the macOS traffic lights (Zed does the same)
            .px_2()
            .pb_2()
            .bg(theme.surface)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .px_2()
                    .pb_2()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child("CHEZMOI UI"),
            )
            .child(self.nav_item(theme, "Dashboard", Route::Dashboard, None, cx))
            .child(self.nav_item(theme, "Review", Route::Review, review_badge, cx))
            .child(self.nav_item(theme, "Settings", Route::Settings, None, cx))
            .child(div().flex_1())
            .child(
                div()
                    .px_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child(ui::status_dot_line(
                        *theme,
                        dot_tone,
                        status_line.into(),
                        px(170.),
                    ))
                    .child(div().truncate().child(freshness)),
            )
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        if let Route::Component(name) = self.route {
            return div()
                .size_full()
                .bg(theme.bg)
                .text_color(theme.text)
                .flex()
                .items_center()
                .justify_center()
                .p_8()
                .child(
                    czui_ui::preview::render_component(name, theme)
                        .unwrap_or_else(|| div().child("unknown component").into_any_element()),
                )
                .into_any_element();
        }
        div()
            .flex()
            .size_full()
            .bg(theme.bg)
            .text_color(theme.text)
            .child(self.sidebar(&theme, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(match self.route {
                        Route::Dashboard => DashboardView {
                            state: self.state.clone(),
                            now_ts: dashboard::system_now,
                            expanded_scans: self.expanded_scans.clone(),
                            action_in_flight: self.dashboard_action_in_flight,
                            unlock_in_flight: self.unlock_in_flight,
                        }
                        .render(theme, cx)
                        .into_any_element(),
                        Route::Review => self.ensure_review(cx).into_any_element(),
                        Route::Settings => {
                            let paths = self.paths.clone();
                            self.settings
                                .get_or_insert_with(|| cx.new(|cx| SettingsView::new(paths, cx)))
                                .clone()
                                .into_any_element()
                        }
                        Route::Merge => {
                            let shell = cx.weak_entity();
                            self.merge
                                .get_or_insert_with(|| cx.new(|cx| MergeView::new(shell, cx)))
                                .clone()
                                .into_any_element()
                        }
                        // Handled by the early return above.
                        Route::Component(_) => div().into_any_element(),
                    }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod render_smoke {
    //! Render every route in a real (headless) gpui window. Catches the
    //! whole class of render-path panics the pure-logic tests can't see —
    //! e.g. reading an entity from inside its own render ("cannot read …
    //! while it is already being updated"), which shipped and crashed the
    //! app on open.

    use std::collections::HashSet;
    use std::path::PathBuf;

    use czui_app::model::{SyncModel, TimelineRow};
    use gpui::TestAppContext;

    use super::*;

    fn model_with_data() -> SyncModel {
        let mut m = SyncModel {
            connected: true,
            ..Default::default()
        };
        m.hydrate_status(
            vec![czui_proto::DriftSummary {
                target: PathBuf::from("/tmp/smoke/.zshrc"),
                class: "destination_drift".into(),
                since_ts: Some(10),
            }],
            42,
            Some("degraded hint".into()),
            false,
        );
        let info = |ts: u64| TimelineRow {
            ts,
            kind: "fetch".into(),
            target: None,
            machine: "m".into(),
            class: None,
        };
        m.timeline = vec![
            info(30),
            info(29),
            info(28),
            TimelineRow {
                ts: 27,
                kind: "dest_changed".into(),
                target: Some(PathBuf::from("/tmp/smoke/.zshrc")),
                machine: "m".into(),
                class: Some("destination_drift".into()),
            },
        ];
        m
    }

    fn smoke_paths() -> SettingsPaths {
        SettingsPaths {
            socket: PathBuf::from("/tmp/smoke.sock"),
            journal: PathBuf::from("/tmp/smoke-journal.db"),
            settings: PathBuf::from("/tmp/smoke-settings.toml"),
        }
    }

    /// Every gallery state must build and render headlessly — otherwise
    /// `--gallery` (and scripts/shoot.sh) breaks silently the next time a
    /// view struct changes shape.
    #[gpui::test]
    fn every_gallery_state_builds_and_renders(cx: &mut TestAppContext) {
        for (name, _) in fixtures::STATES {
            let (_view, vis) = cx.add_window_view(|_window, cx| {
                fixtures::build(name, smoke_paths(), cx)
                    .unwrap_or_else(|| panic!("fixture missing for listed state {name}"))
            });
            vis.run_until_parked();
        }
    }

    #[gpui::test]
    fn shell_renders_every_route_without_panicking(cx: &mut TestAppContext) {
        for route in [
            Route::Dashboard,
            Route::Review,
            Route::Settings,
            // lazily created with no target: the "open a file from Review"
            // empty state
            Route::Merge,
        ] {
            let (_view, vis) = cx.add_window_view(|_window, cx| {
                let state = cx.new(|_| model_with_data());
                // exercise the expanded scan-group render path too
                let mut expanded_scans = HashSet::new();
                expanded_scans.insert(30u64);
                Shell {
                    route,
                    state,
                    review: None,
                    settings: None,
                    merge: None,
                    paths: smoke_paths(),
                    expanded_scans,
                    dashboard_action_in_flight: false,
                    unlock_in_flight: false,
                }
            });
            vis.run_until_parked();
        }
    }

    /// Plan 6 Task 3 surfaces: the model above already puts the drifted
    /// `.zshrc` in the timeline, so the dashboard renders the inline quick
    /// actions — and with no `EngineSlot` global set (this test app never
    /// connects), they render through the DISABLED path. The review window
    /// gets a posed selection so the header action buttons and the outcome
    /// banner (with its Undo button) render too, plus one in-flight pass for
    /// the "working…" marker.
    #[gpui::test]
    fn resolve_action_ui_renders_without_engine_or_panic(cx: &mut TestAppContext) {
        use review::{BannerTint, OutcomeBanner, ReviewView};
        for (in_flight, banner) in [
            (
                false,
                Some(OutcomeBanner {
                    text: "Kept disk version · committed & pushed".into(),
                    tint: BannerTint::Ok,
                    undoable: true,
                }),
            ),
            (
                false,
                Some(OutcomeBanner {
                    text: "Keep disk failed: daemon gone".into(),
                    tint: BannerTint::Conflict,
                    undoable: false,
                }),
            ),
            (true, None),
        ] {
            let (_view, vis) = cx.add_window_view(|_window, cx| {
                let state = cx.new(|_| model_with_data());
                let shell = cx.weak_entity();
                let review = cx.new(|cx| {
                    let mut view = ReviewView::new(state.clone(), shell, cx);
                    view.selected = Some(PathBuf::from("/tmp/smoke/.zshrc"));
                    view.last_outcome = banner.clone();
                    view.action_in_flight = in_flight;
                    view
                });
                Shell {
                    route: Route::Review,
                    state,
                    review: Some(review),
                    settings: None,
                    merge: None,
                    paths: smoke_paths(),
                    expanded_scans: HashSet::new(),
                    // also exercises the dashboard's "working…" swap when the
                    // shell flag is up (the drifted row is in the timeline)
                    dashboard_action_in_flight: in_flight,
                    unlock_in_flight: false,
                }
            });
            vis.run_until_parked();
        }
        // Dashboard route with the busy flag: quick actions yield to the
        // "working…" marker.
        let (_view, vis) = cx.add_window_view(|_window, cx| {
            let state = cx.new(|_| model_with_data());
            Shell {
                route: Route::Dashboard,
                state,
                review: None,
                settings: None,
                merge: None,
                paths: smoke_paths(),
                expanded_scans: HashSet::new(),
                dashboard_action_in_flight: true,
                unlock_in_flight: false,
            }
        });
        vis.run_until_parked();
    }

    #[gpui::test]
    fn shell_renders_empty_and_scanning_states(cx: &mut TestAppContext) {
        for (connected, scanning) in [(false, false), (true, true)] {
            let (_view, vis) = cx.add_window_view(|_window, cx| {
                let state = cx.new(|_| SyncModel {
                    connected,
                    scanning,
                    ..Default::default()
                });
                Shell {
                    route: Route::Dashboard,
                    state,
                    review: None,
                    settings: None,
                    merge: None,
                    paths: smoke_paths(),
                    expanded_scans: HashSet::new(),
                    dashboard_action_in_flight: false,
                    unlock_in_flight: false,
                }
            });
            vis.run_until_parked();
        }
    }

    /// Plan 7 Task 3: render the merge editor's states in a real (headless)
    /// window — loading, load failure, a plain conflict (placeholder row with
    /// pick buttons incl. `base`), the fully resolved document (Save enabled),
    /// a degraded 2-way templated file (watermark, 🔒 rows, no `base`
    /// button), and a save in flight with a sticky banner. Inputs are
    /// synthetic; no subprocess runs.
    #[gpui::test]
    fn merge_view_renders_all_states_without_panicking(cx: &mut TestAppContext) {
        use czui_app::merge_inputs::MergeInputs;
        use czui_core::merge::Choice;
        use czui_core::template::{anchor::anchor, lexer::lex};
        use merge::{LoadedMerge, MergeView};
        use review::BannerTint;
        use std::sync::Arc;

        fn plain_conflict() -> MergeInputs {
            MergeInputs {
                target: PathBuf::from("/tmp/smoke/.testrc"),
                ours: "a\nv = 2\nz\n".into(),
                theirs: "a\nv = 3\nz\n".into(),
                base: Some("a\nv = 1\nz\n".into()),
                source_path: PathBuf::from("/tmp/smoke/src/dot_testrc"),
                templated: false,
                span_map: None,
            }
        }

        fn templated_degraded() -> MergeInputs {
            let template = "email = {{ .email }}\neditor = hx\n";
            let theirs = "email = a@b.c\neditor = hx\n";
            let span_map = anchor(template, &lex(template).expect("template lexes"), theirs);
            MergeInputs {
                target: PathBuf::from("/tmp/smoke/.testrc"),
                ours: "email = a@b.c\neditor = nvim\n".into(),
                theirs: theirs.into(),
                base: None,
                source_path: PathBuf::from("/tmp/smoke/src/dot_testrc.tmpl"),
                templated: true,
                span_map: Some(span_map),
            }
        }

        type Pose = Box<dyn Fn(&mut MergeView)>;
        let poses: Vec<Pose> = vec![
            Box::new(|view| {
                view.target = Some(PathBuf::from("/tmp/smoke/.testrc"));
                view.loading = true;
            }),
            Box::new(|view| {
                view.target = Some(PathBuf::from("/tmp/smoke/.testrc"));
                view.error =
                    Some("binary content — the merge editor handles UTF-8 text only".into());
            }),
            Box::new(|view| {
                // one unresolved conflict: placeholder + ours/theirs/base
                view.target = Some(PathBuf::from("/tmp/smoke/.testrc"));
                view.loaded = Some(LoadedMerge::new(Arc::new(plain_conflict())));
            }),
            Box::new(|view| {
                // fully resolved: Save enabled, progress in the ok tint
                view.target = Some(PathBuf::from("/tmp/smoke/.testrc"));
                let mut loaded = LoadedMerge::new(Arc::new(plain_conflict()));
                let region = loaded.state.conflicts()[0];
                loaded.state.pick(region, Choice::Ours);
                view.loaded = Some(loaded);
            }),
            Box::new(|view| {
                // degraded 2-way templated: watermark pane, 🔒 rows, no base
                view.target = Some(PathBuf::from("/tmp/smoke/.testrc"));
                view.loaded = Some(LoadedMerge::new(Arc::new(templated_degraded())));
            }),
            Box::new(|view| {
                // save in flight plus a sticky (protected-span) banner
                view.target = Some(PathBuf::from("/tmp/smoke/.testrc"));
                view.loaded = Some(LoadedMerge::new(Arc::new(templated_degraded())));
                view.saving = true;
                view.banner = Some(review::OutcomeBanner {
                    text: "this change touches a templated value — protected span".into(),
                    tint: BannerTint::Drift,
                    undoable: false,
                });
            }),
        ];
        for pose in poses {
            let (_view, vis) = cx.add_window_view(|_window, cx| {
                let state = cx.new(|_| model_with_data());
                let shell = cx.weak_entity();
                let merge = cx.new(|cx| {
                    let mut view = MergeView::new(shell, cx);
                    pose(&mut view);
                    view
                });
                Shell {
                    route: Route::Merge,
                    state,
                    review: None,
                    settings: None,
                    merge: Some(merge),
                    paths: smoke_paths(),
                    expanded_scans: HashSet::new(),
                    dashboard_action_in_flight: false,
                    unlock_in_flight: false,
                }
            });
            vis.run_until_parked();
        }
    }
}
