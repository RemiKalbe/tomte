//! Shell: left sidebar navigation (Zed settings-window shape — fixed-width
//! panel, bordered, compact nav rows) over a routed content pane.

pub mod changelog;
pub mod dashboard;
pub mod fixtures;
pub mod merge;
pub mod review;
pub mod settings;

use std::path::PathBuf;

use gpui::{
    AppContext as _, Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px,
};

use tomte_app::model::{SyncModel, time_ago};
use tomte_app::resolve::MergeConclusion;
use tomte_app::theme::Theme;
use tomte_ui::components as ui;

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
    /// Gallery-only: the changelog window's body, so it can be screenshot.
    Changelog,
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
    /// A merge with origin is fully resolved but its merge commit did not
    /// happen (typically an unsignable commit). Nothing is left conflicted,
    /// so no review row can re-enter it — the dashboard banner this raises is
    /// the only way back in. Probed on connect and after every decision that
    /// could have stranded one.
    pub pending_merge: bool,
    /// That banner's "Finish merge" is running on the background executor.
    pub merge_finish_in_flight: bool,
    /// Whether the pending-merge probe has already run for this connection.
    /// Cleared on disconnect, so reconnecting (or a daemon restart) asks
    /// again — a merge can be stranded while the app is not even running.
    pub pending_merge_probed: bool,
    /// Gallery-only changelog view, kept so its scroll position survives.
    pub changelog: Option<Entity<changelog::ChangelogView>>,
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

    /// The merge editor recorded one mid-merge source conflict: its Save
    /// wrote the source file and staged it, so git now knows that file is
    /// answered. Conclude the merge if it was the last one open; otherwise
    /// the remaining conflicts stay ordinary review rows, each resolvable on
    /// its own — there is no queue any more, and nothing to present next.
    ///
    /// Called from OUTSIDE the merge view's own update (see the note at its
    /// save site): routing and the banner both re-enter that same entity.
    pub fn repo_conflict_saved(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Review;
        cx.notify();
        let Some(engine) = cx
            .try_global::<crate::EngineSlot>()
            .and_then(|slot| slot.0.clone())
        else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { engine.finish_merge_if_resolved() })
                .await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(MergeConclusion::Committed { pushed, note }) => {
                        shell.origin_merge_finished(pushed, note, cx);
                    }
                    Ok(MergeConclusion::StillConflicted(leftover)) => {
                        shell.merge_done(
                            OutcomeBanner {
                                text: format!(
                                    "Recorded · {} more file(s) still differ from origin",
                                    leftover.len()
                                )
                                .into(),
                                tint: review::BannerTint::Drift,
                                undoable: false,
                            },
                            cx,
                        );
                    }
                    // The editor only saves in this mode while a merge is
                    // running, so this means it ended underneath us.
                    Ok(MergeConclusion::NotMerging) => {
                        tomte_core::log_warn!(
                            "reconcile",
                            "resolution recorded but no merge was in progress"
                        );
                    }
                    Err(e) => {
                        tomte_core::log_error!("reconcile", "finish failed: {e}");
                        crate::notify_osa::notify(
                            &tomte_core::cmd::SystemRunner,
                            "tomte",
                            &format!("merge with origin could not conclude: {e}"),
                        );
                    }
                }
                // A failed conclude is exactly what strands a merge, so ask
                // right here whether one now needs the dashboard banner.
                shell.refresh_pending_merge(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The merge with origin was committed: refresh the model, and say so
    /// out of band — the user may well be somewhere else by now.
    fn origin_merge_finished(
        &mut self,
        pushed: bool,
        note: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.state.update(cx, |_model, cx| {
            cx.notify();
        });
        let body = match (pushed, note) {
            (true, None) => "source repo reconciled with origin & pushed".to_string(),
            (_, Some(note)) => format!("source repo reconciled — {note}"),
            (false, None) => "source repo reconciled (push pending)".to_string(),
        };
        self.merge_done(
            OutcomeBanner {
                text: body.clone().into(),
                tint: review::BannerTint::Ok,
                undoable: false,
            },
            cx,
        );
        crate::notify_osa::notify(&tomte_core::cmd::SystemRunner, "tomte", &body);
    }

    /// Abandon the merge with origin: abort it, restoring the source repo to
    /// its pre-merge state. Cancelling ONE mid-merge conflict bails the
    /// WHOLE merge deliberately — a half-resolved merge state would be worse
    /// than none.
    ///
    /// `rolled_back` is what the editor told the user it would discard, so
    /// the banner afterwards names the same thing the confirmation did. The
    /// rescan matters as much as the banner: the abort rewrites source files
    /// the daemon's last scan saw in their merged form, and a review list
    /// describing a repo state that no longer exists is how the user came to
    /// believe an undone decision had stuck.
    pub fn abort_origin_merge(&mut self, rolled_back: usize, cx: &mut Context<Self>) {
        self.route = Route::Review;
        cx.notify();
        let Some(engine) = cx
            .try_global::<crate::EngineSlot>()
            .and_then(|slot| slot.0.clone())
        else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let rescanned = cx
                .background_executor()
                .spawn(async move {
                    engine.abort_repo_reconcile();
                    engine.request_rescan()
                })
                .await;
            this.update(cx, |shell, cx| {
                let mut text = format!(
                    "Merge with origin rolled back — {rolled_back} source file(s) returned to \
                     their pre-merge content. Destination files and journal entries were not \
                     touched."
                );
                if let Err(e) = rescanned {
                    // The list on screen still describes the merged repo.
                    // Never let that pass silently — it is exactly the
                    // mismatch that makes a rolled-back decision look kept.
                    tomte_core::log_error!("reconcile", "rescan after abort failed: {e}");
                    text.push_str(&format!(
                        " The daemon could not rescan ({e}), so this list may be stale."
                    ));
                }
                shell.merge_done(
                    OutcomeBanner {
                        text: text.into(),
                        tint: review::BannerTint::Drift,
                        undoable: false,
                    },
                    cx,
                );
            })
            .ok();
        })
        .detach();
    }

    /// Probe whether a merge with origin is resolved but uncommitted, and
    /// raise (or clear) the dashboard banner accordingly. Cheap and silent
    /// when there is no merge: one `rev-parse MERGE_HEAD`.
    pub fn refresh_pending_merge(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = cx
            .try_global::<crate::EngineSlot>()
            .and_then(|slot| slot.0.clone())
        else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let pending = cx
                .background_executor()
                .spawn(async move { engine.pending_merge_commit() })
                .await;
            this.update(cx, |shell, cx| {
                match pending {
                    Ok(pending) => shell.pending_merge = pending,
                    Err(e) => {
                        // Leave the flag as it was rather than clearing a
                        // real warning on a failed probe.
                        tomte_core::log_error!("reconcile", "pending-merge probe failed: {e}");
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Commit the merge the banner is reporting. The same
    /// `finish_merge_if_resolved` a last decision would have run — this just
    /// asks it without a decision to carry.
    pub fn finish_pending_merge(&mut self, cx: &mut Context<Self>) {
        if self.merge_finish_in_flight {
            return;
        }
        let Some(engine) = cx
            .try_global::<crate::EngineSlot>()
            .and_then(|slot| slot.0.clone())
        else {
            return;
        };
        self.merge_finish_in_flight = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { engine.finish_merge_if_resolved() })
                .await;
            this.update(cx, |shell, cx| {
                shell.merge_finish_in_flight = false;
                match result {
                    Ok(MergeConclusion::Committed { pushed, note }) => {
                        shell.origin_merge_finished(pushed, note, cx);
                    }
                    // Conflicts reappeared (the user resolved something in a
                    // terminal, say): those are review rows again, so the
                    // banner is simply no longer the right story.
                    Ok(MergeConclusion::StillConflicted(leftover)) => {
                        tomte_core::log_info!(
                            "reconcile",
                            "finish declined: {} conflict(s) reappeared",
                            leftover.len()
                        );
                    }
                    Ok(MergeConclusion::NotMerging) => {}
                    Err(e) => {
                        tomte_core::log_error!("reconcile", "finish failed: {e}");
                        crate::notify_osa::notify(
                            &tomte_core::cmd::SystemRunner,
                            "tomte",
                            &format!("merge with origin could not be committed: {e}"),
                        );
                    }
                }
                shell.refresh_pending_merge(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
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
            (
                ui::StatusTone::Ok,
                format!("in sync · {} files", model.in_sync),
            )
        };
        let update_ready = model
            .update_ready
            .as_ref()
            .map(|u| format!("update ready · {}", u.version));
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
                    .child("TOMTE"),
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
                    .child(div().truncate().child(freshness))
                    .when_some(update_ready, |el, line| {
                        el.child(div().truncate().text_color(theme.accent).child(line))
                    }),
            )
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        // Connect/reconnect probe. A merge left resolved-but-uncommitted has
        // no drifted row to announce it — every file in it is answered — so
        // something has to go and look, and a stranding can well have
        // happened while the app was not running. Latched: once per
        // connection epoch, never per frame.
        if !self.state.read(cx).connected {
            self.pending_merge_probed = false;
        } else if !self.pending_merge_probed
            && cx
                .try_global::<crate::EngineSlot>()
                .is_some_and(|slot| slot.0.is_some())
        {
            self.pending_merge_probed = true;
            self.refresh_pending_merge(cx);
        }
        // The changelog is a popup: no sidebar, no chrome, just the document.
        // Created ONCE and kept — building the view per frame reset the
        // scroll position every frame, so the page could not be scrolled.
        if self.route == Route::Changelog {
            let view = self.changelog.get_or_insert_with(|| {
                cx.new(|_| {
                    changelog::ChangelogView::new("Tomte — changelog", changelog::BUNDLED_CHANGELOG)
                })
            });
            return view.clone().into_any_element();
        }
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
                    tomte_ui::preview::render_component(name, theme)
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
                        Route::Changelog => cx
                            .new(|_| {
                                changelog::ChangelogView::new(
                                    "Tomte — changelog",
                                    changelog::BUNDLED_CHANGELOG,
                                )
                            })
                            .into_any_element(),
                        Route::Dashboard => DashboardView {
                            state: self.state.clone(),
                            now_ts: dashboard::system_now,
                            expanded_scans: self.expanded_scans.clone(),
                            action_in_flight: self.dashboard_action_in_flight,
                            unlock_in_flight: self.unlock_in_flight,
                            pending_merge: self.pending_merge,
                            merge_finish_in_flight: self.merge_finish_in_flight,
                        }
                        .render(theme, cx)
                        .into_any_element(),
                        Route::Review => self.ensure_review(cx).into_any_element(),
                        Route::Settings => {
                            let paths = self.paths.clone();
                            let state = self.state.clone();
                            self.settings
                                .get_or_insert_with(|| {
                                    cx.new(|cx| SettingsView::new(paths, state, cx))
                                })
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

    use gpui::TestAppContext;
    use tomte_app::model::{SyncModel, TimelineRow};

    use super::*;

    fn model_with_data() -> SyncModel {
        let mut m = SyncModel {
            connected: true,
            ..Default::default()
        };
        m.hydrate_status(
            vec![tomte_proto::DriftSummary {
                target: PathBuf::from("/tmp/smoke/.zshrc"),
                class: "destination_drift".into(),
                since_ts: Some(10),
                origin_differs: false,
            }],
            42,
            Some("degraded hint".into()),
            false,
            None,
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

    /// Region-aligned scroll sync must span all FOUR input columns and the
    /// result editor. The origin column was appended to a mapping that had
    /// already been rebuilt once (2026-08-22 editor rework), so this drives
    /// every column as the leader and checks the others land on THEIR first
    /// line of the same region.
    #[gpui::test]
    fn merge_scroll_sync_spans_four_panes_and_the_result(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("merge-origin", smoke_paths(), cx).expect("merge-origin fixture")
        });
        // Render once: the seed and the pane geometry are built there.
        vis.run_until_parked();
        let merge = cx.update(|cx| shell.read(cx).merge.clone().expect("merge view posed"));
        for leader in 0..=merge::PANE_COLS {
            let probe = cx.update(|cx| merge.update(cx, |view, _| view.sync_probe(leader, 1)));
            // Binds the fixture's premise: origin carries a line the others
            // do not, so region 1 starts on a different LINE in each column.
            // With all four equal, `line_at(starts, leader, …)` written where
            // `line_at(starts, col, …)` was meant would pass unnoticed.
            assert!(
                probe.want_panes.iter().any(|&w| w != probe.want_panes[0]),
                "fixture columns must differ, got {:?}",
                probe.want_panes
            );
            assert_eq!(
                probe.panes, probe.want_panes,
                "leader {leader} left a column behind"
            );
            assert_eq!(
                probe.result, probe.want_result,
                "leader {leader} left the result editor behind"
            );
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
                    pending_merge: false,
                    merge_finish_in_flight: false,
                    pending_merge_probed: false,
                    changelog: None,
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
                    pending_merge: false,
                    merge_finish_in_flight: false,
                    pending_merge_probed: false,
                    changelog: None,
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
                pending_merge: false,
                merge_finish_in_flight: false,
                pending_merge_probed: false,
                changelog: None,
            }
        });
        vis.run_until_parked();
    }

    /// The scan flag alone must never split the source action. A file added
    /// locally and never pushed comes back from the scan with
    /// `origin_differs = true` and yet has NO origin blob — offering
    /// "Keep source · origin" for it would offer a resolution to nothing.
    /// The split follows origin's loaded content, which this view has none of.
    #[gpui::test]
    fn origin_flag_without_content_keeps_one_source_action(cx: &mut TestAppContext) {
        use review::{ResolveAction, ReviewView};
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            let mut model = SyncModel {
                connected: true,
                ..Default::default()
            };
            model.hydrate_status(
                vec![tomte_proto::DriftSummary {
                    target: PathBuf::from("/tmp/smoke/.zshrc"),
                    class: "destination_drift".into(),
                    since_ts: Some(10),
                    // flagged by the scan…
                    origin_differs: true,
                }],
                1,
                None,
                false,
                None,
            );
            let state = cx.new(|_| model);
            let shell = cx.weak_entity();
            let review = cx.new(|cx| {
                let mut view = ReviewView::new(state.clone(), shell, cx);
                view.selected = Some(PathBuf::from("/tmp/smoke/.zshrc"));
                // …but no origin content ever loaded for it.
                view.origin_diffs = None;
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
                dashboard_action_in_flight: false,
                unlock_in_flight: false,
                pending_merge: false,
                merge_finish_in_flight: false,
                pending_merge_probed: false,
                changelog: None,
            }
        });
        vis.run_until_parked();
        let actions = cx.update(|cx| {
            let review = shell.read(cx).review.clone().expect("review posed");
            review.read(cx).header_source_actions()
        });
        assert_eq!(actions, vec![ResolveAction::KeepSourceLocal]);
    }

    /// The stranded-merge banner and its in-flight state must render. This
    /// banner is the ONLY surface a resolved-but-uncommitted merge has —
    /// every file in it is answered, so no drifted row exists to carry it —
    /// and a render panic here would take the single way out with it.
    #[gpui::test]
    fn dashboard_renders_the_pending_merge_banner(cx: &mut TestAppContext) {
        for finishing in [false, true] {
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
                    dashboard_action_in_flight: false,
                    unlock_in_flight: false,
                    pending_merge: true,
                    merge_finish_in_flight: finishing,
                    pending_merge_probed: true,
                    changelog: None,
                }
            });
            vis.run_until_parked();
        }
    }

    /// The armed Cancel swaps one button for two — "Keep merge open" and the
    /// destructive "Discard merge" — above a conflict-tinted warning. Posed
    /// here so the confirmation bar cannot silently stop building.
    #[gpui::test]
    fn merge_view_renders_the_armed_cancel_confirmation(cx: &mut TestAppContext) {
        use merge::{LoadedMerge, MergeView};
        use std::sync::Arc;
        use tomte_app::merge_inputs::MergeInputs;

        let (shell, vis) = cx.add_window_view(|_window, cx| {
            let state = cx.new(|_| model_with_data());
            let shell = cx.weak_entity();
            let merge = cx.new(|cx| {
                let mut view = MergeView::new(shell, cx);
                view.target = Some(PathBuf::from("/tmp/smoke/.testrc"));
                view.repo_reconcile = true;
                view.loaded = Some(LoadedMerge::new(Arc::new(MergeInputs {
                    target: PathBuf::from("/tmp/smoke/.testrc"),
                    ours: "a\nv = 2\nz\n".into(),
                    theirs: "a\nv = 3\nz\n".into(),
                    base: Some("a\nv = 1\nz\n".into()),
                    source_path: PathBuf::from("/tmp/smoke/src/dot_testrc"),
                    templated: false,
                    template: None,
                    span_map: None,
                    origin: None,
                })));
                view.cancel_armed = Some(2);
                view.banner = Some(review::OutcomeBanner {
                    text: merge::cancel_warning(2, None).into(),
                    tint: review::BannerTint::Conflict,
                    undoable: false,
                });
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
                pending_merge: false,
                merge_finish_in_flight: false,
                pending_merge_probed: true,
                changelog: None,
            }
        });
        vis.run_until_parked();
        // Still armed after a render: the bar is a state, not a one-frame
        // flash the next repaint would clear.
        let armed = cx.update(|cx| {
            shell
                .read(cx)
                .merge
                .clone()
                .expect("merge view posed")
                .read(cx)
                .cancel_armed
        });
        assert_eq!(armed, Some(2));
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
                    pending_merge: false,
                    merge_finish_in_flight: false,
                    pending_merge_probed: false,
                    changelog: None,
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
        use merge::{LoadedMerge, MergeView};
        use review::BannerTint;
        use std::sync::Arc;
        use tomte_app::merge_inputs::MergeInputs;
        use tomte_core::merge::Choice;
        use tomte_core::template::{anchor::anchor, lexer::lex};

        fn plain_conflict() -> MergeInputs {
            MergeInputs {
                target: PathBuf::from("/tmp/smoke/.testrc"),
                ours: "a\nv = 2\nz\n".into(),
                theirs: "a\nv = 3\nz\n".into(),
                base: Some("a\nv = 1\nz\n".into()),
                source_path: PathBuf::from("/tmp/smoke/src/dot_testrc"),
                templated: false,
                template: None,
                span_map: None,
                origin: None,
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
                template: Some(template.into()),
                span_map: Some(span_map),
                origin: None,
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
                    pending_merge: false,
                    merge_finish_in_flight: false,
                    pending_merge_probed: false,
                    changelog: None,
                }
            });
            vis.run_until_parked();
        }
    }
}
