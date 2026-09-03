//! Shell: left sidebar navigation (Zed settings-window shape — fixed-width
//! panel, bordered, compact nav rows) over a routed content pane.

pub mod changelog;
pub mod dashboard;
pub mod fixtures;
pub mod merge;
pub mod review;
pub mod settings;

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;

use gpui::{
    AppContext as _, Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px,
};

use tomte_app::action_queue::{ActionQueue, Decision, QueuedAction};
use tomte_app::model::{SyncModel, time_ago};
use tomte_app::resolve::{BatchItem, MergeConclusion, ResolveEngine, ResolveError, ResolveOutcome};
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

/// A repo-level operation the user asked for; runs when the latch is free.
///
/// None of these is about ONE file, so none can live in the action queue:
/// the first two answer the whole merge with origin that the queue's
/// decisions are being staged into, and the third undoes whichever session
/// happened to finish last. They still take their turn behind the same
/// latch, because they write the same files a draining batch is reading.
#[derive(Debug)]
pub(super) enum PendingRepoOp {
    /// Commit the merge the dashboard banner is reporting.
    FinishMerge,
    /// Abandon the merge with origin, rolling the source repo back. Carries
    /// what the editor told the user it would discard, so the banner
    /// afterwards names the same thing the confirmation did.
    AbortMerge(merge::Rollback),
    /// Restore the destination files of the last finished journal session.
    /// Not per-target — "the last thing that landed" is a property of the
    /// journal, not of the row the button sits under.
    UndoLast,
}

/// Which repo-level operation, without its payload.
///
/// The payload ([`merge::Rollback`]) is neither `Copy` nor renderable, and
/// the surfaces that have to REACT to a pending or running op — the
/// dashboard's Finish button — only ever ask which one it is. So they get
/// this, copied into their snapshot, rather than a borrow of the op itself.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum RepoOpKind {
    Finish,
    Abort,
    Undo,
}

/// Where an outcome banner goes: to the user, or to where the user will be.
///
/// THE ONE DIFFERENCE between a repo-level answer the user is standing there
/// waiting for and the same answer arriving out of the queue. Task 5's ruling:
/// a landed queued decision never changes the route.
#[derive(Clone, Copy, PartialEq, Debug)]
enum BannerDelivery {
    /// The user pressed the button that produced this and is waiting for it:
    /// show it and take them to Review ([`Shell::merge_done`]).
    Route,
    /// It came out of the queue, possibly minutes later: leave it where they
    /// will find it and do not move them ([`Shell::show_outcome`]).
    InPlace,
}

impl PendingRepoOp {
    pub(super) fn kind(&self) -> RepoOpKind {
        match self {
            Self::FinishMerge => RepoOpKind::Finish,
            Self::AbortMerge(_) => RepoOpKind::Abort,
            Self::UndoLast => RepoOpKind::Undo,
        }
    }
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
    /// Every decision the user has made that is not finished yet, plus the
    /// failures nobody has acknowledged. Lives on the Shell because it
    /// outlives any one view: a decision made in the merge editor is still
    /// owed work after the editor has been left, and Review's rows have to
    /// render as "processing" for decisions the dashboard queued.
    pub queue: ActionQueue,
    /// TRUE WHILE ANY GIT MUTATION [`Self::pump`] STARTED IS RUNNING: a
    /// queue batch, that batch's staged-merge conclusion, a merge finish, or
    /// a merge abort.
    ///
    /// The latch that keeps all of that SERIAL — one mutation in flight,
    /// because every one of them writes the same git repository (index,
    /// merge state, chezmoi's own autoCommit/autoPush) — and the reason
    /// [`Self::pump`] is safe to call from anywhere.
    ///
    /// The rule it exists to enforce is "everything that mutates the repo
    /// enters through [`Self::pump`]", and as of Task 5 that rule is WHOLE:
    /// no surface in this app spawns git work of its own any more.
    ///
    /// Task 3b closed the Shell's own two doors (a finish and an abort now
    /// take their turn behind it instead of spawning a commit+push, or a
    /// merge abort, beside a draining batch). Task 4 closed Review's three:
    /// its quick actions and "Keep both" are [`Decision`]s on the queue, and
    /// its Undo is a [`PendingRepoOp`] — `undo_last` writes managed
    /// DESTINATION files back from the journal, which is the same set of
    /// files a batch's `chezmoi re-add` is reading. Task 5 closed the last
    /// three: the dashboard's quick actions and `MergeView::save` are
    /// [`Decision`]s too, and the merge conclusion a staged repo conflict
    /// owes (a merge commit and a push) is awaited inside the batch's own
    /// in-flight window, before this clears — it used to be spawned beside
    /// the drain by a `Shell::repo_conflict_saved` that no longer exists.
    ///
    /// Anything added later that writes the source repo, the index, or a
    /// managed destination file belongs behind this latch too: as a
    /// [`Decision`] if it is about one file, as a [`PendingRepoOp`] if it is
    /// about the repository.
    pub queue_pumping: bool,
    /// A finish, an abort, or an undo the user pressed that has not run yet,
    /// because the latch was held or no engine existed. [`Self::pump`] runs
    /// it BEFORE it takes the next batch.
    ///
    /// One slot, not a queue: pressing "Finish merge" twice means one
    /// finish, and the answers here are about a single merge, so the last
    /// answer is normally the one the user means.
    ///
    /// NORMALLY — the precedence is deliberately ASYMMETRIC, and
    /// [`Self::request_repo_op`] is where it is written down: an abort may
    /// replace a pending finish (the later, CONFIRMED answer), a finish may
    /// never replace a pending abort (one unconfirmed click must not commit
    /// a merge the user confirmed throwing away), and an undo replaces
    /// neither (it is about a journal session, not about this merge, and
    /// running it ahead of either answer would restore files on top of a
    /// repo state that is still being decided).
    pub(super) deferred_repo_op: Option<PendingRepoOp>,
    /// Which repo op [`Self::pump`] has actually STARTED, if any. Set where
    /// the op is picked up, cleared where its completion clears
    /// [`Self::queue_pumping`] — the two are the same window.
    ///
    /// [`Self::deferred_repo_op`] alone cannot tell the dashboard's Finish
    /// button what to say: an abort that is still waiting and an abort that
    /// is halfway through rewriting the source tree are both states in which
    /// a finish must not be offered, and `deferred_repo_op` is `None` for the
    /// second one.
    pub(super) running_repo_op: Option<RepoOpKind>,
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

    /// Report one `finish_merge_if_resolved` result — the merge a staged
    /// repo conflict belongs to, concluded (or not).
    ///
    /// Reached from the QUEUE only, since Task 5 deleted the detached
    /// `repo_conflict_saved` that was the other caller: a merge-editor Save
    /// on a mid-merge conflict is a [`Decision`] now, and the conclusion it
    /// owes is awaited inside the batch's own in-flight window. So every
    /// banner here is delivered [`BannerDelivery::InPlace`] — the user was
    /// routed at Save time and may be three files further on by now.
    ///
    /// Does NOT probe for a stranded merge: [`Self::batch_finished`] owns
    /// that call on every path, and probing here as well would double it.
    fn conclude_staged_merge(
        &mut self,
        result: Result<MergeConclusion, ResolveError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(MergeConclusion::Committed { pushed, note }) => {
                self.origin_merge_finished(pushed, note, BannerDelivery::InPlace, cx);
            }
            Ok(MergeConclusion::StillConflicted(leftover)) => {
                self.deliver(
                    OutcomeBanner {
                        text: format!(
                            "Recorded · {} more file(s) still differ from origin",
                            leftover.len()
                        )
                        .into(),
                        tint: review::BannerTint::Drift,
                        undoable: false,
                    },
                    BannerDelivery::InPlace,
                    cx,
                );
            }
            // The editor only saves in this mode while a merge is running,
            // so this means it ended underneath us.
            Ok(MergeConclusion::NotMerging) => {
                tomte_core::log_warn!(
                    "reconcile",
                    "resolution recorded but no merge was in progress"
                );
            }
            Err(e) => {
                tomte_core::log_error!("reconcile", "finish failed: {e}");
                self.notify_in_background(format!("merge with origin could not conclude: {e}"), cx);
            }
        }
    }

    /// The merge with origin was committed: refresh the model, and say so
    /// out of band — the user may well be somewhere else by now.
    ///
    /// `delivery` is the one thing its two callers disagree about. A deferred
    /// `FinishMerge` is a button the user pressed and is waiting on, so its
    /// answer routes to Review as it always has; the same conclusion reached
    /// through the QUEUE lands minutes after a Save, and yanking the user out
    /// of whatever they moved on to would be the very thing the queue exists
    /// to stop.
    fn origin_merge_finished(
        &mut self,
        pushed: bool,
        note: Option<String>,
        delivery: BannerDelivery,
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
        self.deliver(
            OutcomeBanner {
                text: body.clone().into(),
                tint: review::BannerTint::Ok,
                undoable: false,
            },
            delivery,
            cx,
        );
        self.notify_in_background(body, cx);
    }

    /// Post one desktop notification without blocking the window.
    ///
    /// [`crate::notify_osa::notify`] runs `osascript` and BLOCKS on it, so
    /// firing it inside a Shell update freezes the UI for the length of a
    /// subprocess — every caller here is inside one. Suppressed under
    /// `cfg(test)`: a test run must not post real notifications to whoever is
    /// running it, and the body is built by pure functions the tests assert
    /// on directly.
    fn notify_in_background(&self, body: String, cx: &mut Context<Self>) {
        if cfg!(test) {
            tomte_core::log_info!("resolve", "notification (suppressed in tests): {body}");
            return;
        }
        cx.background_executor()
            .spawn(async move {
                crate::notify_osa::notify(&tomte_core::cmd::SystemRunner, "tomte", &body);
            })
            .detach();
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
    ///
    /// The abort itself waits for the latch ([`Self::queue_pumping`]): it is
    /// a merge abort on the very repository a draining batch is writing, and
    /// the two overlapping is how a decision comes to look kept when it was
    /// rolled back. The user leaves the editor NOW either way — decisions
    /// they make in Review meanwhile queue BEHIND the rollback and run
    /// against the rolled-back repo, which is what "abort" means.
    pub fn abort_origin_merge(&mut self, rolled_back: merge::Rollback, cx: &mut Context<Self>) {
        self.route = Route::Review;
        self.request_repo_op(PendingRepoOp::AbortMerge(rolled_back), cx);
    }

    /// The abort, running with the latch already taken. Only [`Self::pump`]
    /// calls this.
    fn run_abort(
        &mut self,
        engine: std::sync::Arc<ResolveEngine>,
        rolled_back: merge::Rollback,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    abort_outcome(AssertUnwindSafe(|| {
                        engine.abort_repo_reconcile();
                        AbortOutcome::RolledBack(engine.request_rescan())
                    }))
                })
                .await;
            this.update(cx, |shell, cx| {
                shell.queue_pumping = false;
                shell.running_repo_op = None;
                shell.merge_done(abort_banner(outcome, &rolled_back), cx);
                // On BOTH arms. A rollback that completed ended the merge the
                // dashboard banner was reporting, and one that panicked left
                // whether a merge is in progress unknown — either way what is
                // on screen is a guess until it is re-probed.
                shell.refresh_pending_merge(cx);
                cx.notify();
                // Whatever queued behind the rollback, now against the
                // repo the rollback restored.
                shell.pump(cx);
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
    ///
    /// A REQUEST, not the work: the commit and push it runs land on the
    /// repository a draining batch is already writing, so it takes its turn
    /// behind [`Self::queue_pumping`] like everything else that mutates the
    /// repo. No `in_flight` guard any more — the latch is the guard.
    pub fn finish_pending_merge(&mut self, cx: &mut Context<Self>) {
        self.request_repo_op(PendingRepoOp::FinishMerge, cx);
    }

    /// Undo the last finished journal session: restore its destination files
    /// from their snapshots. Pressed on Review's outcome banner.
    ///
    /// A REQUEST, not the work. `undo_last` writes the very destination
    /// files a draining batch's `chezmoi re-add` is reading, so it takes its
    /// turn behind [`Self::queue_pumping`] like everything else that mutates
    /// what the repo tracks. Not a per-file [`Decision`] either: "the last
    /// thing that landed" is a property of the journal, and a queue whose
    /// entries are keyed by target has nowhere to put it.
    pub fn undo_last(&mut self, cx: &mut Context<Self>) {
        self.request_repo_op(PendingRepoOp::UndoLast, cx);
    }

    /// Is ANY git work owed — running, queued, or waiting for the latch?
    ///
    /// The gate for Undo, and only for Undo. Every other button in the app
    /// stays live while work drains (that is the whole point of the queue),
    /// but "undo the last thing that landed" is unambiguous only when
    /// nothing is landing: with a batch in flight the session it would undo
    /// is whichever one happens to finish first.
    pub fn is_busy(&self) -> bool {
        self.queue_pumping || self.queue.depth() > 0 || self.deferred_repo_op.is_some()
    }

    /// The undo, running with the latch already taken. Only [`Self::pump`]
    /// calls this.
    fn run_undo(&mut self, engine: std::sync::Arc<ResolveEngine>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    catching_panics(
                        "an undo",
                        internal_error,
                        AssertUnwindSafe(|| engine.undo_last()),
                    )
                })
                .await;
            this.update(cx, |shell, cx| {
                shell.queue_pumping = false;
                shell.running_repo_op = None;
                // Reached from a FRESH Shell update, so nesting the review
                // view's update is safe here — unlike the synchronous path
                // in `request_repo_op`, which runs inside that same view.
                // `merge_done` re-selects the current row from inside it, so
                // the diff reflects the restored file.
                shell.merge_done(review::undo_banner(&result), cx);
                cx.notify();
                // Whatever queued while the undo ran.
                shell.pump(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Remember a repo-level operation and try to run it right away.
    ///
    /// Returns immediately, always: if the latch is held (or nothing is
    /// connected yet) the op waits in [`Self::deferred_repo_op`] and
    /// [`Self::pump`] picks it up — the click is remembered, never
    /// discarded.
    ///
    /// THE DIRECTIONS ARE NOT SYMMETRIC. An abort may replace a pending
    /// finish: it is the user's later choice, and a CONFIRMED one — the
    /// merge editor arms Cancel and makes them press "Discard merge" a
    /// second time (`merge.rs`'s `discard_merge`). A finish must NEVER
    /// replace a pending abort. It is one unconfirmed click on the dashboard
    /// banner, and while an abort waits — parked, or behind a batch that
    /// takes seconds — that click would otherwise COMMIT the merge the user
    /// just confirmed throwing away. So it is dropped instead, logged, and
    /// said out loud: a destructive answer already given is not overridable
    /// by a stray click, and a click that does nothing must not do it
    /// silently.
    ///
    /// An UNDO displaces neither of them. It is about the last journal
    /// session, not about this merge, and running it ahead of a finish or an
    /// abort would restore destination files on top of a repo state that is
    /// still being decided. It is dropped and logged rather than banner-ed:
    /// the only caller is the Review banner's Undo button, which is pressed
    /// from inside that view's own update, and `merge_done` reaches back
    /// into that same view — the nesting gpui refuses (2026-08-18 crash).
    /// The button is disabled whenever [`Self::is_busy`], so reaching this
    /// arm at all takes a click landing in the same frame as the press that
    /// deferred the other op.
    fn request_repo_op(&mut self, op: PendingRepoOp, cx: &mut Context<Self>) {
        if matches!(op, PendingRepoOp::UndoLast) && self.deferred_repo_op.is_some() {
            tomte_core::log_info!(
                "resolve",
                "undo ignored: {:?} is already queued for this repo",
                self.deferred_repo_op
            );
            return;
        }
        if matches!(op, PendingRepoOp::FinishMerge)
            && matches!(self.deferred_repo_op, Some(PendingRepoOp::AbortMerge(_)))
        {
            tomte_core::log_info!(
                "reconcile",
                "finish ignored: a confirmed roll-back of this merge is already queued"
            );
            self.merge_done(
                OutcomeBanner {
                    text: "Finish ignored — this merge is being rolled back, as you \
                           confirmed in the merge editor. Nothing was committed."
                        .into(),
                    tint: review::BannerTint::Drift,
                    undoable: false,
                },
                cx,
            );
            return;
        }
        // An abort rolls the source repo back, so every merge-editor save
        // still waiting to be staged INTO that merge is orphaned: there will
        // be no merge left to stage it into. Fail those NOW, on their own
        // rows, in words that say what to do — running them afterwards would
        // either fail obscurely or quietly open a SECOND merge with origin.
        //
        // Only the repo-conflict saves. `Merged`, `KeepDisk`,
        // `KeepSourceLocal` and `KeepSourceOrigin` do not depend on the merge
        // and run correctly against the rolled-back repo, which is exactly
        // what "abort, then apply my other decisions" should mean.
        //
        // Pure queue mutation, deliberately: this runs inside the merge
        // view's own update (the Discard button), so it must not reach back
        // into any view.
        if matches!(op, PendingRepoOp::AbortMerge(_)) {
            let orphaned = self.queue.fail_pending_where(
                |a| matches!(a.decision, Decision::RepoConflict { .. }),
                "merge rolled back before this save ran — save again",
            );
            if !orphaned.is_empty() {
                tomte_core::log_info!(
                    "reconcile",
                    "roll-back orphaned {} queued save(s): {}",
                    orphaned.len(),
                    orphaned
                        .iter()
                        .map(|t| t.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        if let Some(prev) = self.deferred_repo_op.replace(op) {
            tomte_core::log_info!("reconcile", "{prev:?} superseded before it ran");
        }
        cx.notify();
        self.pump(cx);
        // Still here after the pump: it was deferred, not run. Say which and
        // why, because the run can be seconds away and the log is the only
        // place the press and its run can be tied together.
        if let Some(waiting) = &self.deferred_repo_op {
            let why = if self.queue_pumping {
                "the latch is held by work already running"
            } else {
                "no engine is connected yet"
            };
            tomte_core::log_info!("reconcile", "{waiting:?} deferred: {why}");
        }
    }

    /// The finish, running with the latch already taken. Only [`Self::pump`]
    /// calls this.
    fn run_finish(&mut self, engine: std::sync::Arc<ResolveEngine>, cx: &mut Context<Self>) {
        self.merge_finish_in_flight = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    catching_panics(
                        "a merge finish",
                        internal_error,
                        AssertUnwindSafe(|| engine.finish_merge_if_resolved()),
                    )
                })
                .await;
            this.update(cx, |shell, cx| {
                shell.merge_finish_in_flight = false;
                shell.queue_pumping = false;
                shell.running_repo_op = None;
                match result {
                    Ok(MergeConclusion::Committed { pushed, note }) => {
                        // The user pressed Finish and is waiting on it: this
                        // answer routes, as it always has.
                        shell.origin_merge_finished(pushed, note, BannerDelivery::Route, cx);
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
                        shell.notify_in_background(
                            format!("merge with origin could not be committed: {e}"),
                            cx,
                        );
                    }
                }
                shell.refresh_pending_merge(cx);
                cx.notify();
                // Whatever queued while the finish ran.
                shell.pump(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Land a successful merge save: hand the outcome banner to Review (its
    /// banner owns the Undo button), reload its preview so the diff reflects
    /// the converged reality, and route back.
    pub fn merge_done(&mut self, banner: OutcomeBanner, cx: &mut Context<Self>) {
        self.show_outcome(banner, cx);
        self.route = Route::Review;
        cx.notify();
    }

    /// Put an outcome banner on the Review view WITHOUT moving the user.
    ///
    /// The other half of [`Self::merge_done`], and the one the QUEUE uses.
    /// A decision the user made minutes ago must not yank them out of
    /// whatever they are doing when it lands: they were routed at the moment
    /// they pressed the button, and they may have opened another file, the
    /// merge editor, or Settings since. The banner is waiting for them in
    /// Review either way.
    pub fn show_outcome(&mut self, banner: OutcomeBanner, cx: &mut Context<Self>) {
        let review = self.ensure_review(cx);
        review.update(cx, |view, cx| {
            view.last_outcome = Some(banner);
            if let Some(target) = view.selected.clone() {
                view.select(target, cx);
            }
            cx.notify();
        });
        cx.notify();
    }

    /// Deliver one banner the way its origin calls for.
    fn deliver(&mut self, banner: OutcomeBanner, delivery: BannerDelivery, cx: &mut Context<Self>) {
        match delivery {
            BannerDelivery::Route => self.merge_done(banner, cx),
            BannerDelivery::InPlace => self.show_outcome(banner, cx),
        }
    }

    /// Record a decision and start draining if nothing is in flight.
    ///
    /// Returns immediately, always. This is the whole difference from the
    /// `if in_flight { return }` guards it replaces: the interface's job
    /// ends the moment the choice is remembered, and the row it came from
    /// goes to "processing" instead of the click being discarded.
    pub fn enqueue(&mut self, action: QueuedAction, cx: &mut Context<Self>) {
        // Every press that owes git work says so in the log, the same as a
        // repo op's request does: the run can be seconds (a batch) or
        // minutes (a queue) away, and the log is the only place a press and
        // its outcome can be tied back together.
        tomte_core::log_info!(
            "resolve",
            "queued {} for {}",
            action.decision.label(),
            action.target.display()
        );
        self.queue.push(action);
        cx.notify();
        self.pump(cx);
    }

    /// Watch the shared model, and prune what a model change invalidates.
    ///
    /// Registered once, where the Shell is built. The re-render half is why
    /// this observer has always existed (views read the model in `render`);
    /// the pruning half is [`Self::model_changed`], and it lives in here
    /// rather than at the call site so there is exactly one copy of the
    /// wiring for the app and the tests to share.
    pub fn observe_model(&mut self, cx: &mut Context<Self>) {
        let state = self.state.clone();
        cx.observe(&state, |shell, _, cx| {
            shell.model_changed(cx);
            cx.notify();
        })
        .detach();
    }

    /// The model changed: forget failures for targets that are no longer
    /// drifted.
    ///
    /// A failure outlives its action deliberately (spec §2.5 — the queue
    /// moves on in milliseconds and a transient banner would go unseen), but
    /// it must not outlive the DRIFT. A file that failed, then went back in
    /// sync (the user fixed it in a terminal, or a later scan disagreed),
    /// and then drifts again for an unrelated reason would otherwise come
    /// back with the old message still attached to it.
    pub fn model_changed(&mut self, cx: &mut Context<Self>) {
        let drifted: std::collections::HashSet<PathBuf> = self
            .state
            .read(cx)
            .drifted
            .iter()
            .map(|d| d.target.clone())
            .collect();
        self.queue
            .retain_failures(|target| drifted.contains(target));
    }

    /// Start the next piece of git work if none is running: a deferred
    /// finish/abort first, then the next batch. Idempotent and cheap, so it
    /// is safe to call from `enqueue`, from `request_repo_op`, from the end
    /// of every batch and every repo op, and from the connect latch in
    /// `render`.
    ///
    /// The one door for the SHELL's git work: nothing on `Shell` spawns a
    /// mutation except through here, which is what makes
    /// [`Self::queue_pumping`] a real serial latch rather than a flag only
    /// the queue respects. The views' own action paths do not come through
    /// here yet — [`Self::queue_pumping`] lists them and the tasks that
    /// close them.
    fn pump(&mut self, cx: &mut Context<Self>) {
        if self.queue_pumping {
            return;
        }
        let Some(engine) = cx
            .try_global::<crate::EngineSlot>()
            .and_then(|slot| slot.0.clone())
        else {
            // No engine to run with: PARK. Everything stays queued, every
            // row stays "processing", and `deferred_repo_op` stays SET —
            // these are the user's decisions, and dropping them here would
            // be exactly the discarded click the queue exists to stop.
            //
            // `EngineSlot` is REPUBLISHED ON EVERY (RE)CONNECT (`main.rs`'s
            // reconnect loop), and a reconnect whose `build_engine` fails
            // publishes `EngineSlot(None)`. So this arm is reached before
            // the first connect AND after a reconnect that could not build
            // an engine; the queue then waits, rows at "processing" and its
            // deferred op intact, until a later connect publishes an engine
            // — at which point the connect latch in `render` pumps. A daemon
            // that goes away WITHOUT a reconnect attempt leaves the last
            // engine in place: the batch then runs and fails honestly at
            // `SessionStart` rather than parking.
            return;
        };
        // A finish or an abort the user pressed goes FIRST. It is a
        // repo-level answer about the very merge the queued decisions would
        // otherwise be staged into, so it has to land before the next batch
        // starts writing: a finish presses the merge the batch was going to
        // extend, an abort throws it away.
        if let Some(op) = self.deferred_repo_op.take() {
            // The other half of the trace `request_repo_op` opens: this is
            // the line that says the press finally got its turn.
            tomte_core::log_info!("reconcile", "running deferred {op:?}");
            self.queue_pumping = true;
            // Same window as the latch, and the only way a surface can tell
            // "an abort is waiting" from "an abort is under way".
            self.running_repo_op = Some(op.kind());
            cx.notify();
            match op {
                PendingRepoOp::FinishMerge => self.run_finish(engine, cx),
                PendingRepoOp::AbortMerge(rolled_back) => self.run_abort(engine, rolled_back, cx),
                PendingRepoOp::UndoLast => self.run_undo(engine, cx),
            }
            return;
        }
        let batch = self.queue.take_batch();
        if batch.is_empty() {
            return;
        }
        self.queue_pumping = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let runner = engine.clone();
            let results = cx
                .background_executor()
                .spawn(async move { run_batch(&runner, batch) })
                .await;
            // A staged repo conflict is only half of that decision's work:
            // the merge it belongs to still has to be concluded, and that is
            // a merge commit and a push on the very repository the NEXT
            // batch would start writing to. So it is awaited HERE, inside
            // the in-flight window and before the latch clears, rather than
            // spawned off to race the drain.
            let conclusion = if results
                .iter()
                .any(|(_, _, r)| matches!(r, Ok(BatchResult::RepoConflictStaged)))
            {
                // NO ROUTING HERE. The Save that staged this routed the user
                // to Review at the moment they pressed it; this runs however
                // long afterwards the queue took, and by then they may be in
                // the merge editor on another file. Moving them then is the
                // yank the queue exists to remove.
                Some(
                    cx.background_executor()
                        .spawn(async move {
                            catching_panics(
                                "a staged merge's conclusion",
                                internal_error,
                                AssertUnwindSafe(|| engine.finish_merge_if_resolved()),
                            )
                        })
                        .await,
                )
            } else {
                None
            };
            this.update(cx, |shell, cx| {
                shell.batch_finished(results, conclusion, cx)
            })
            .ok();
        })
        .detach();
    }

    /// A batch came back: record every result, then take the next one.
    ///
    /// EVERY report lands somewhere. A success drops its row from the model
    /// at once (the daemon's next scan would agree, seconds later);
    /// everything else becomes a failure string on the queue, against the
    /// target it belongs to. A decision whose result went nowhere is a
    /// decision the user believes was applied.
    ///
    /// `conclusion` is present exactly when the batch staged a repo
    /// conflict: it is that merge's ALREADY-AWAITED outcome, so the latch
    /// below clears only once every git mutation the batch owed is done —
    /// never while a merge commit and push are still running.
    ///
    /// Reached from a fresh Shell update, so it may touch `self.state` and
    /// nest an update of the review view (through `show_outcome`). What it
    /// must never do is nest an update of a view that is itself mid-update,
    /// which is why nothing here reaches back into the merge editor.
    fn batch_finished(
        &mut self,
        results: Vec<DecisionReport>,
        conclusion: Option<Result<MergeConclusion, ResolveError>>,
        cx: &mut Context<Self>,
    ) {
        self.queue_pumping = false;
        // The decisions this batch is finishing, read BEFORE `finish_all`
        // clears them: what each one was, and whether its surface asked to be
        // told out of band when it landed. Cloned rather than borrowed —
        // everything below needs `&mut self`.
        let ran: Vec<QueuedAction> = self.queue.running().to_vec();
        let mut finished: Vec<(PathBuf, Option<SharedString>)> = Vec::with_capacity(results.len());
        let mut landed: Vec<OutcomeBanner> = Vec::new();
        let mut announce: Vec<String> = Vec::new();
        for (target, label, result) in results {
            // `run_batch` reports exactly the batch it was handed, so this
            // normally finds it. A report for something else is not something
            // to guess about: it gets no banner and no notification, only its
            // failure or its success, which is what the row is about.
            let decision = ran
                .iter()
                .find(|a| a.target == target)
                .map(|a| (&a.decision, a.notify_on_done));
            if let Some((decision, true)) = decision {
                announce.push(dashboard::quick_action_body(decision, &target, &result));
            }
            if let Some((decision, _)) = decision
                && let Some(banner) = review::queued_outcome_banner(decision, &result)
            {
                landed.push(banner);
            }
            let failure: Option<SharedString> = match result {
                Ok(BatchResult::Resolved(ResolveOutcome::Done { .. })) => {
                    self.state.update(cx, |model, cx| {
                        model.confirm_resolved(&target);
                        cx.notify();
                    });
                    None
                }
                Ok(BatchResult::Resolved(ResolveOutcome::NeedsMergeEditor)) => {
                    Some("templated · use the merge editor".into())
                }
                Ok(BatchResult::Resolved(ResolveOutcome::ProtectedSpan { detail })) => {
                    Some(format!("touches a templated value: {detail}").into())
                }
                // Not a resolve session: the answer went into the merge's
                // index, so there is no session id and nothing for
                // `confirm_resolved` to retire. The merge's own conclusion
                // (`conclusion`, below) and the rescan behind it is what
                // drops the row.
                Ok(BatchResult::RepoConflictStaged) => None,
                // The label is the user's own words for the choice, so a
                // failure minutes later still names the button they pressed.
                Err(e) => Some(format!("{label} failed: {e}").into()),
            };
            finished.push((target, failure));
        }
        self.queue.finish_all(&finished);
        // A merge-editor Save reports itself, WITHOUT moving the user:
        // `MergeView::save` left "Save queued · <file>" on screen and that
        // claim has to be settled now, whichever way the save went.
        // `queued_outcome_banner` decides which decisions those are — no
        // other kind comes back with a banner, so a thirty-file sweep cannot
        // quietly overwrite what the banner was saying.
        //
        // At most one in practice: `take_batch` hands out either a run of
        // batchable one-click decisions (never banner-ed) or exactly one
        // decision that runs alone, which is what a Save is. The `pop` is the
        // last-wins rule for the case where that stops being true.
        if let Some(banner) = landed.pop() {
            self.show_outcome(banner, cx);
        }
        for body in announce {
            self.notify_in_background(body, cx);
        }
        if let Some(conclusion) = conclusion {
            self.conclude_staged_merge(conclusion, cx);
        }
        // Once, on every path: any decision above can have started a merge
        // with origin and then failed to commit it, leaving nothing
        // conflicted and so no row to try again from.
        self.refresh_pending_merge(cx);
        cx.notify();
        self.pump(cx);
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

/// One decision's fate, as [`run_batch`] reports it: the target it was
/// about, the label of the choice the user pressed (kept so a failure
/// minutes later still names the button and not just the file), and what
/// running it produced.
pub(crate) type DecisionReport = (PathBuf, &'static str, Result<BatchResult, ResolveError>);

/// What running one decision produced.
///
/// A staged repo conflict is deliberately NOT dressed up as a
/// [`ResolveOutcome`]: `resolve_repo_conflict` writes the source file and
/// stages it inside an open merge without opening a journal session at all,
/// so there is no session id to report and nothing for `confirm_resolved` to
/// retire. A fabricated `Done { session: 0 }` would travel straight into the
/// journal-backed undo UI as a lie about a session that never existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BatchResult {
    /// A full resolve ran; the outcome is exactly what the engine returned.
    Resolved(ResolveOutcome),
    /// One mid-merge source conflict is answered and staged in the index.
    RepoConflictStaged,
}

/// Run one batch to completion, panic and all. Called on the background
/// executor and touches no gpui state, so `pub(crate)` for the headless
/// driver too.
pub(crate) fn run_batch(engine: &ResolveEngine, batch: Vec<QueuedAction>) -> Vec<DecisionReport> {
    let owed: Vec<(PathBuf, &'static str)> = batch
        .iter()
        .map(|a| (a.target.clone(), a.decision.label()))
        .collect();
    let what = format!("a queued batch of {} file(s)", owed.len());
    catching_panics(
        &what,
        |_detail| panicked_batch(owed),
        AssertUnwindSafe(|| run_batch_inner(engine, batch)),
    )
}

/// One failure report per target a panicking batch was owed. `finish_all`
/// records these against the rows they belong to, so every row is
/// answerable again instead of frozen at "processing".
fn panicked_batch(owed: Vec<(PathBuf, &'static str)>) -> Vec<DecisionReport> {
    owed.into_iter()
        .map(|(target, label)| {
            (
                target,
                label,
                Err(ResolveError::Failed(
                    "internal error — see the log".to_string(),
                )),
            )
        })
        .collect()
}

/// What [`Shell::run_abort`]'s background closure came back with.
///
/// Deliberately NOT a `Result`: a panic and a failed rescan mean opposite
/// things about the one fact the banner asserts. A rescan failure happens
/// AFTER `merge_abort` ran, so the rollback did happen and only the list on
/// screen is suspect. A panic can land before or inside `merge_abort`, so
/// nothing is known about the merge at all.
#[derive(Debug)]
enum AbortOutcome {
    /// `merge_abort` ran; the rescan behind it succeeded, or failed like this.
    RolledBack(Result<(), ResolveError>),
    /// The closure panicked partway, with this detail.
    Panicked(String),
}

/// Run the abort's work under [`catching_panics`] with the ONE fallback that
/// claims nothing.
///
/// A seam, not a wrapper for its own sake: `abort_banner` can be tested
/// against every outcome, but that proves nothing about which outcome the
/// abort HANDS it, and a fallback of the wrong shape ([`internal_error`],
/// say, re-wrapped) turns every panic back into "the roll-back ran, the
/// rescan failed". There is no way to make `abort_repo_reconcile` panic, so
/// the choice of fallback lives here where a test can drive it with a
/// closure that does.
fn abort_outcome(work: impl FnOnce() -> AbortOutcome + std::panic::UnwindSafe) -> AbortOutcome {
    catching_panics(
        "a merge abort",
        // NOT `internal_error`: a panic must not come back as "the rollback
        // ran and the rescan failed".
        AbortOutcome::Panicked,
        work,
    )
}

/// The banner one abort outcome earns.
///
/// Split out of the completion so the panic arm is testable: it must never
/// inherit the rolled-back arm's sentence. Claiming "Merge with origin
/// rolled back" after a panic would be a confident statement about the exact
/// thing the panic took away.
fn abort_banner(outcome: AbortOutcome, rolled_back: &merge::Rollback) -> OutcomeBanner {
    match outcome {
        AbortOutcome::RolledBack(rescanned) => {
            let mut text = format!(
                "Merge with origin rolled back — {} returned to their pre-merge content. \
                 Destination files and journal entries were not touched.",
                rolled_back.phrase()
            );
            if let Err(e) = rescanned {
                // The list on screen still describes the merged repo. Never
                // let that pass silently — it is exactly the mismatch that
                // makes a rolled-back decision look kept.
                tomte_core::log_error!("reconcile", "rescan after abort failed: {e}");
                text.push_str(&format!(
                    " The daemon could not rescan ({e}), so this list may be stale."
                ));
            }
            OutcomeBanner {
                text: text.into(),
                tint: review::BannerTint::Drift,
                undoable: false,
            }
        }
        AbortOutcome::Panicked(detail) => {
            tomte_core::log_error!(
                "reconcile",
                "the roll-back did not complete: {detail} — the merge may still be in progress"
            );
            OutcomeBanner {
                text: "The roll-back did not complete (internal error — see the log). \
                       The merge with origin may still be in progress, and this list \
                       may describe the repo either before or after it."
                    .into(),
                tint: review::BannerTint::Conflict,
                undoable: false,
            }
        }
    }
}

/// The `on_panic` fallback for a background git closure that reports a
/// `Result`: an ordinary failure, which its completion already knows how to
/// report. So the completion still runs, the latch still clears, and the
/// user still hears about it.
fn internal_error<T>(detail: String) -> Result<T, ResolveError> {
    Err(ResolveError::Failed(format!(
        "internal error — see the log: {detail}"
    )))
}

/// Run `work`, turning a panic inside it into `on_panic`'s value.
///
/// EVERY git closure [`Shell::pump`] starts goes through here, because the
/// latch has to be UN-WEDGEABLE. A panic escaping one would leave `queue_pumping` set
/// — and, for a batch, its targets stuck in `running` — forever: every later
/// decision would pile up behind it, every row would sit at "processing",
/// and nothing would ever clear it, not even the connect latch, since `pump`
/// returns early on that latch. So the panic becomes an ordinary value the
/// completion already knows how to report: per-target failures for a batch
/// ([`panicked_batch`]); an `Err` for a finish, a conclusion, or an undo
/// ([`internal_error`]); and, for the abort, its own
/// [`AbortOutcome::Panicked`] — the one caller whose fallback is NOT an
/// ordinary failure, because "the rollback ran and the rescan failed" is a
/// claim a panic cannot support (see [`abort_banner`]).
fn catching_panics<T>(
    what: &str,
    on_panic: impl FnOnce(String) -> T,
    work: impl FnOnce() -> T + std::panic::UnwindSafe,
) -> T {
    match std::panic::catch_unwind(work) {
        Ok(value) => value,
        Err(payload) => {
            let detail = payload
                .downcast_ref::<&'static str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            tomte_core::log_error!("resolve", "{what} panicked: {detail}");
            on_panic(detail)
        }
    }
}

/// The batch itself, free to panic.
///
/// A run of batchable decisions goes through
/// [`ResolveEngine::resolve_batch`]: one `re-add`/`apply`, one commit and one
/// push for the whole run instead of that tail per file. Every other
/// decision needs the repo to itself and goes through its own single-file
/// engine method.
fn run_batch_inner(engine: &ResolveEngine, batch: Vec<QueuedAction>) -> Vec<DecisionReport> {
    // Every arm below can reach `ensure_merge_started` and so leave a merge
    // with origin resolved but uncommitted. It cannot probe for one from
    // here — the background executor has no `Context` — so the completion
    // this feeds, `Shell::batch_finished`, is where `refresh_pending_merge`
    // runs, on every path, batch or single.
    if batch.iter().all(|a| a.decision.batchable()) {
        let labels: Vec<&'static str> = batch.iter().map(|a| a.decision.label()).collect();
        let items: Vec<BatchItem> = batch
            .into_iter()
            .map(|a| BatchItem {
                target: a.target,
                decision: a
                    .decision
                    .as_batch()
                    .expect("`batchable` just said every decision here has a batch form"),
            })
            .collect();
        return engine
            .resolve_batch(items)
            .per_target
            .into_iter()
            .zip(labels)
            .map(|((target, result), label)| (target, label, result.map(BatchResult::Resolved)))
            .collect();
    }
    // `take_batch` hands out exactly ONE non-batchable decision, so this
    // loop normally runs once. It still reports every item it was given
    // rather than only the first: a caller that passes more must not have
    // the rest silently disappear.
    let mut reports = Vec::with_capacity(batch.len());
    for QueuedAction {
        target, decision, ..
    } in batch
    {
        let label = decision.label();
        let result = match decision {
            Decision::KeepDisk => engine.keep_disk(&target).map(BatchResult::Resolved),
            Decision::KeepSourceLocal => engine.keep_source(&target).map(BatchResult::Resolved),
            Decision::KeepSourceOrigin { seen } => engine
                .keep_source_origin(&target, &seen)
                .map(BatchResult::Resolved),
            // "Keep both" is the merge editor's write with the auto-merge as
            // its resolution — the same engine call, deliberately, so the two
            // cannot drift apart. Only the words differ (`Decision::label`).
            Decision::Merged { resolved, inputs } | Decision::KeepBoth { resolved, inputs } => {
                engine
                    .resolve_merged(&inputs, &resolved)
                    .map(BatchResult::Resolved)
            }
            Decision::RepoConflict {
                source_path,
                resolved,
            } => engine
                .resolve_repo_conflict(&source_path, &resolved)
                .map(|()| BatchResult::RepoConflictStaged),
        };
        reports.push((target, label, result));
    }
    reports
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
            // Resume anything parked because no engine existed yet — a
            // queued decision, or a Finish/Discard the user pressed while
            // the app was still connecting. This arm is re-armed by every
            // disconnect, and `EngineSlot` is republished by every
            // (re)connect, so a connection that could not build an engine
            // (`EngineSlot(None)`) leaves the work parked and the NEXT
            // successful connect releases it. `pump` is idempotent and this
            // fires once per connection epoch, so it cannot loop.
            self.pump(cx);
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
                            unlock_in_flight: self.unlock_in_flight,
                            pending_merge: self.pending_merge,
                            merge_finish_in_flight: self.merge_finish_in_flight,
                            pending_repo_op: self.deferred_repo_op.as_ref().map(|op| op.kind()),
                            running_repo_op: self.running_repo_op,
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
                    unlock_in_flight: false,
                    pending_merge: false,
                    merge_finish_in_flight: false,
                    pending_merge_probed: false,
                    changelog: None,
                    queue: Default::default(),
                    queue_pumping: false,
                    deferred_repo_op: None,
                    running_repo_op: None,
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
    /// banner (with its Undo button) render too.
    ///
    /// The queue poses came with Task 4 and each is a whole render path of
    /// its own: the PROCESSING group and its muted rows, a row carrying a
    /// failure message under its name, and the Undo button in its disabled
    /// form. The review view reads the SHELL for all three, from inside its
    /// own render — the one shape that would panic if the Shell were still
    /// leased at that point.
    #[gpui::test]
    fn resolve_action_ui_renders_without_engine_or_panic(cx: &mut TestAppContext) {
        use review::{BannerTint, OutcomeBanner, ReviewView};

        let target = PathBuf::from("/tmp/smoke/.zshrc");
        let landed = || {
            Some(OutcomeBanner {
                text: "Kept disk version · committed & pushed".into(),
                tint: BannerTint::Ok,
                undoable: true,
            })
        };
        type Pose = Box<dyn Fn(&mut ActionQueue)>;
        let idle: Pose = Box::new(|_| {});
        let running: Pose = Box::new(|q: &mut ActionQueue| {
            q.push(QueuedAction {
                target: PathBuf::from("/tmp/smoke/.zshrc"),
                decision: Decision::KeepDisk,
                notify_on_done: false,
            });
            q.take_batch();
            // A second target, queued behind it and still waiting.
            q.push(QueuedAction {
                target: PathBuf::from("/tmp/smoke/other.toml"),
                decision: Decision::KeepSourceLocal,
                notify_on_done: false,
            });
        });
        let failed: Pose = Box::new(|q: &mut ActionQueue| {
            q.push(QueuedAction {
                target: PathBuf::from("/tmp/smoke/.zshrc"),
                decision: Decision::KeepDisk,
                notify_on_done: false,
            });
            q.take_batch();
            q.finish_all(&[(
                PathBuf::from("/tmp/smoke/.zshrc"),
                Some("Keep disk failed: op timed out".into()),
            )]);
        });
        for (banner, pose, pumping) in [
            (landed(), &idle, false),
            (
                Some(OutcomeBanner {
                    text: "Keep disk failed: daemon gone".into(),
                    tint: BannerTint::Conflict,
                    undoable: false,
                }),
                &idle,
                false,
            ),
            // Work in flight: the selected file's buttons are inert, the
            // PROCESSING group renders, and Undo is disabled rather than gone.
            (landed(), &running, true),
            // A failure on the selected row, with nothing running.
            (landed(), &failed, false),
        ] {
            let target = target.clone();
            let (_view, vis) = cx.add_window_view(|_window, cx| {
                let state = cx.new(|_| model_with_data());
                let shell = cx.weak_entity();
                let review = cx.new(|cx| {
                    let mut view = ReviewView::new(state.clone(), shell, cx);
                    view.selected = Some(target);
                    view.last_outcome = banner.clone();
                    view
                });
                let mut queue = ActionQueue::default();
                pose(&mut queue);
                Shell {
                    route: Route::Review,
                    state,
                    review: Some(review),
                    settings: None,
                    merge: None,
                    paths: smoke_paths(),
                    expanded_scans: HashSet::new(),
                    unlock_in_flight: false,
                    pending_merge: false,
                    merge_finish_in_flight: false,
                    pending_merge_probed: false,
                    changelog: None,
                    queue,
                    queue_pumping: pumping,
                    deferred_repo_op: None,
                    running_repo_op: None,
                }
            });
            vis.run_until_parked();
        }
        // The dashboard route on the same model: its inline quick actions
        // render (the drifted row is in the timeline) and stay LIVE — a
        // decision draining elsewhere no longer makes them a no-op.
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
                unlock_in_flight: false,
                pending_merge: false,
                merge_finish_in_flight: false,
                pending_merge_probed: false,
                changelog: None,
                queue: Default::default(),
                queue_pumping: false,
                deferred_repo_op: None,
                running_repo_op: None,
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
                unlock_in_flight: false,
                pending_merge: false,
                merge_finish_in_flight: false,
                pending_merge_probed: false,
                changelog: None,
                queue: Default::default(),
                queue_pumping: false,
                deferred_repo_op: None,
                running_repo_op: None,
            }
        });
        vis.run_until_parked();
        let actions = cx.update(|cx| {
            let review = shell.read(cx).review.clone().expect("review posed");
            review.read(cx).header_source_actions()
        });
        assert_eq!(actions, vec![ResolveAction::KeepSourceLocal]);
    }

    /// Every surface that can start a merge must also be able to report one
    /// it stranded. Twice now an action path reached `ensure_merge_started`
    /// with no matching probe, and the second time it was the DASHBOARD —
    /// the very surface the failure note tells the user to go to.
    ///
    /// So this walks the view sources rather than trusting a memory: any
    /// function that runs a resolve action must also mention
    /// `refresh_pending_merge`. A crude split on `fn` boundaries, not a
    /// parser — it exists to fail loudly when a new action path is added
    /// without its probe, and the fix is always the same one line.
    #[test]
    fn every_resolve_action_path_also_probes_for_a_stranded_merge() {
        // Reaching any of these reaches `ensure_merge_started`: `keep_disk`
        // and `keep_source_origin` are the one-click writes, `resolve_merged`
        // is the merge editor's and "Keep both"'s.
        const STARTS_A_MERGE: [&str; 4] = [
            "action.run(",
            ".resolve_merged(",
            ".keep_disk(",
            ".keep_source_origin(",
        ];
        const PROBE: &str = "refresh_pending_merge";
        let sources = [
            ("views/mod.rs", include_str!("mod.rs")),
            ("views/review.rs", include_str!("review.rs")),
            ("views/merge.rs", include_str!("merge.rs")),
            ("views/dashboard.rs", include_str!("dashboard.rs")),
        ];
        let mut checked = 0;
        for (name, src) in sources {
            // Function boundaries: `fn f(`, `pub fn f(`, `pub(super) fn f(`,
            // at top level or one indent in.
            let mut chunks: Vec<&str> = Vec::new();
            let mut start = 0;
            for (idx, _) in src.match_indices("fn ") {
                let head = src[..idx].rsplit('\n').next().unwrap_or("");
                let head = head.trim_start_matches(' ');
                if head.is_empty() || head.starts_with("pub ") || head.starts_with("pub(") {
                    if idx > start {
                        chunks.push(&src[start..idx]);
                    }
                    start = idx;
                }
            }
            chunks.push(&src[start..]);
            for chunk in chunks {
                // The test's own body names these strings; skip it.
                if chunk.contains("STARTS_A_MERGE") {
                    continue;
                }
                if STARTS_A_MERGE.iter().any(|needle| chunk.contains(needle)) {
                    checked += 1;
                    let head = chunk.lines().next().unwrap_or("").trim();
                    assert!(
                        chunk.contains(PROBE),
                        "{name}: `{head}` starts a merge but never calls `{PROBE}` — \
                         a merge it strands would have no surface to recover from"
                    );
                }
            }
        }
        // Guards the guard: if the call sites are renamed out from under the
        // needles, this finds nothing and would pass vacuously.
        //
        // ONE, down from the four this started at, and that is the POINT of
        // this task rather than a weakening of the test. Task 4 moved
        // Review's two action paths onto the queue and Task 5 moved the
        // dashboard's and the merge editor's, so exactly one function in
        // these four files still calls an engine method that can start a
        // merge: `run_batch_inner`, the queue's drain.
        //
        // It satisfies the needle by DELEGATION, not by probing: it runs on
        // the background executor with no `Context` to probe from, and its
        // body names `batch_finished` as where `refresh_pending_merge`
        // actually runs, on every path. That naming is what this crude scan
        // reads. Whoever rewrites that comment must keep the reference, or
        // move the probe.
        //
        // A NEW door — any view spawning an engine call of its own again —
        // adds a chunk here and has to carry its own probe, which is the
        // whole reason this scan outlives the doors it was written for.
        assert_eq!(
            checked, 1,
            "expected exactly the queue's drain, found {checked} — either a \
             view started spawning resolves again, or `resolve_merged` / \
             `keep_disk` have been renamed"
        );
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
                    unlock_in_flight: false,
                    pending_merge: true,
                    merge_finish_in_flight: finishing,
                    pending_merge_probed: true,
                    changelog: None,
                    queue: Default::default(),
                    queue_pumping: false,
                    deferred_repo_op: None,
                    running_repo_op: None,
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
                view.cancel_armed = Some(merge::Rollback::Files(2));
                view.banner = Some(review::OutcomeBanner {
                    text: merge::cancel_warning(&merge::Rollback::Files(2)).into(),
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
                unlock_in_flight: false,
                pending_merge: false,
                merge_finish_in_flight: false,
                pending_merge_probed: true,
                changelog: None,
                queue: Default::default(),
                queue_pumping: false,
                deferred_repo_op: None,
                running_repo_op: None,
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
                .clone()
        });
        assert_eq!(armed, Some(merge::Rollback::Files(2)));
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
                    unlock_in_flight: false,
                    pending_merge: false,
                    merge_finish_in_flight: false,
                    pending_merge_probed: false,
                    changelog: None,
                    queue: Default::default(),
                    queue_pumping: false,
                    deferred_repo_op: None,
                    running_repo_op: None,
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
                // A sticky banner that keeps the editor open. No "saving in
                // flight" pose any more: Save records the decision and leaves
                // in the same frame, so the editor has no in-flight state.
                view.target = Some(PathBuf::from("/tmp/smoke/.testrc"));
                view.loaded = Some(LoadedMerge::new(Arc::new(templated_degraded())));
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
                    unlock_in_flight: false,
                    pending_merge: false,
                    merge_finish_in_flight: false,
                    pending_merge_probed: false,
                    changelog: None,
                    queue: Default::default(),
                    queue_pumping: false,
                    deferred_repo_op: None,
                    running_repo_op: None,
                }
            });
            vis.run_until_parked();
        }
    }

    /// The point of the whole queue: a second decision is accepted while the
    /// first is still owed work, so no surface ever has to answer a click
    /// with "busy". This test app publishes no `EngineSlot` (it never
    /// connects), which is also the disconnected case — `pump` must then
    /// PARK the queue: both decisions stay remembered, both rows stay
    /// "processing", and nothing panics. A decision dropped here is the very
    /// discarded click the queue exists to stop.
    #[gpui::test]
    fn enqueue_never_blocks_and_drains_in_order(cx: &mut TestAppContext) {
        use std::path::Path;

        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review", smoke_paths(), cx).expect("review fixture")
        });
        vis.run_until_parked();

        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                s.enqueue(
                    QueuedAction {
                        target: PathBuf::from("/a"),
                        decision: Decision::KeepDisk,
                        notify_on_done: false,
                    },
                    cx,
                );
                s.enqueue(
                    QueuedAction {
                        target: PathBuf::from("/b"),
                        decision: Decision::KeepDisk,
                        notify_on_done: false,
                    },
                    cx,
                );
                assert_eq!(s.queue.depth(), 2, "both remembered");
                assert!(
                    s.queue.processing().contains(&PathBuf::from("/b")),
                    "the second was not discarded"
                );
            });
        });
        // Render and let anything spawned settle: nothing may run, and
        // nothing may vanish.
        vis.run_until_parked();
        vis.update(|_window, cx| {
            let shell = shell.read(cx);
            assert_eq!(
                shell.queue.depth(),
                2,
                "with no engine the queue parks — a parked decision is not a dropped one"
            );
            // `depth()` alone cannot tell parked from taken-and-dropped: it
            // counts `pending + running`, so a `pump` that took the batch
            // and then lost it would satisfy the assertion above while
            // wedging the queue for good. Both decisions must still be
            // PENDING.
            assert!(
                shell.queue.running().is_empty(),
                "nothing was taken out of the queue — the batch is still pending, not lost"
            );
            assert!(
                !shell.queue_pumping,
                "nothing is in flight, so a reconnect is free to pump"
            );
            assert!(
                shell.queue.failure(Path::new("/a")).is_none(),
                "parking is not a failure to report"
            );
        });
    }
    /// A panic inside a batch must come back as ordinary per-target
    /// failures. If it escaped instead, `queue_pumping` would stay set and
    /// the batch would stay in `running` forever — every later decision
    /// piling up behind rows frozen at "processing", with nothing able to
    /// clear it, because `pump` returns early on that very latch.
    ///
    /// Driven through `catching_panics` rather than a fake engine:
    /// `run_batch_inner` takes a concrete `&ResolveEngine` built from
    /// concrete chezmoi/git clients, so there is no seam to inject a
    /// panicking one. The catch itself is the whole mechanism, and this
    /// drives it directly.
    #[test]
    fn a_panicking_batch_reports_a_failure_for_every_target_it_owed() {
        use std::panic::AssertUnwindSafe;
        use std::path::Path;

        let owed = vec![
            (PathBuf::from("/a"), "Keep disk"),
            (PathBuf::from("/b"), "Keep source · this machine"),
        ];
        let reports = catching_panics(
            "a test batch",
            |_detail| panicked_batch(owed),
            AssertUnwindSafe(|| panic!("index out of bounds")),
        );

        assert_eq!(
            reports
                .iter()
                .map(|(t, _, _)| t.as_path())
                .collect::<Vec<_>>(),
            [Path::new("/a"), Path::new("/b")],
            "every target the batch was owed is reported, not just the first"
        );
        for (target, _, result) in &reports {
            let Err(e) = result else {
                panic!("{} was reported as a success", target.display());
            };
            assert!(
                e.to_string().contains("internal error"),
                "{}: {e}",
                target.display()
            );
        }

        // And the reports are exactly what un-wedges the queue: `finish_all`
        // takes them, clears `running`, and leaves each row answerable.
        let mut q = ActionQueue::default();
        q.push(QueuedAction {
            target: PathBuf::from("/a"),
            decision: Decision::KeepDisk,
            notify_on_done: false,
        });
        q.push(QueuedAction {
            target: PathBuf::from("/b"),
            decision: Decision::KeepSourceLocal,
            notify_on_done: false,
        });
        q.take_batch();
        let finished: Vec<(PathBuf, Option<SharedString>)> = reports
            .into_iter()
            .map(|(target, label, result)| {
                let err = result
                    .err()
                    .map(|e| SharedString::from(format!("{label} failed: {e}")));
                (target, err)
            })
            .collect();
        q.finish_all(&finished);
        assert_eq!(q.depth(), 0, "the queue is drained, not wedged");
        assert!(
            q.failure(Path::new("/a")).is_some() && q.failure(Path::new("/b")).is_some(),
            "and each row shows why it failed rather than sitting at \"processing\""
        );
    }

    fn now_ts() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Scratch home + bare origin + file-backed journal + in-process daemon +
    /// connected `ResolveEngine`, PUBLISHED as the app's `EngineSlot` so
    /// `pump` runs for real.
    ///
    /// Copied from `tests/resolve_e2e.rs`'s `DriftLab` rather than shared
    /// with it: views are bin-only (`lib.rs`'s header), so an integration
    /// test cannot reach `Shell` at all. `tomte-daemon` and `tomte-core`'s
    /// `test-support` are dev-dependencies of the BIN, which is what lets
    /// the whole stack be built here instead.
    pub(crate) struct EngineLab {
        pub s: tomte_core::testsupport::Scratch,
        /// These tests drive the engine through the published `EngineSlot`;
        /// the handle is kept for the stories that call it directly.
        #[allow(dead_code)]
        pub engine: std::sync::Arc<ResolveEngine>,
        /// Kept for journal assertions (the daemon is the single writer, so
        /// asserting means opening this path read-only).
        #[allow(dead_code)]
        pub journal_path: PathBuf,
    }

    impl EngineLab {
        pub(crate) fn publish(cx: &mut TestAppContext) -> Self {
            use std::sync::{Arc, Mutex};
            use tomte_app::ipc::IpcClient;
            use tomte_core::cmd::SystemRunner;
            use tomte_core::git::GitClient;
            use tomte_core::testsupport::Scratch;
            use tomte_daemon::core::DaemonCore;
            use tomte_daemon::server::{ServeCtx, serve};
            use tomte_journal::Journal;

            // `Scratch::new` writes the repo-LOCAL identity and
            // `commit.gpgsign = false` the production `GitClient` needs: it
            // runs plain `git`, without testsupport's per-invocation `-c`
            // overrides, so without that it would inherit the developer's
            // global signing config.
            let s = Scratch::new();
            let journal_path = s.root.path().join("journal.db");
            let journal = Journal::open(&journal_path, "shell-test").expect("journal opens");
            let core = Arc::new(Mutex::new(
                DaemonCore::new(
                    s.chezmoi(),
                    GitClient::new(Arc::new(SystemRunner), s.source.clone()),
                    journal,
                    "origin/main".into(),
                )
                .expect("daemon core builds over the scratch home"),
            ));
            let sock = s.root.path().join("d.sock");
            let listener =
                std::os::unix::net::UnixListener::bind(&sock).expect("scratch socket binds");
            std::thread::spawn(move || {
                serve(listener, ServeCtx::ready(core, now_ts, Arc::new(|| {})))
            });
            let ipc = Arc::new(IpcClient::connect(&sock).expect("the daemon accepts the app"));
            let engine = Arc::new(ResolveEngine {
                // The SCRATCH clients — mutations must hit the scratch home.
                chezmoi: s.chezmoi(),
                git: GitClient::new(Arc::new(SystemRunner), s.source.clone()),
                ipc,
                journal_path: journal_path.clone(),
            });
            // One drifted managed file, so a `KeepDisk` on `target()` has
            // real work to do: the scratch source holds `dot_testrc` =
            // "a=1\n" and the destination now says something else.
            std::fs::write(s.home.join(".testrc"), b"a=local\n").expect("drift the target");
            cx.set_global(crate::EngineSlot(Some(engine.clone())));
            Self {
                s,
                engine,
                journal_path,
            }
        }

        /// The managed destination file, `~/.testrc` in the scratch home.
        pub(crate) fn target(&self) -> PathBuf {
            self.s.home.join(".testrc")
        }

        /// Its source: `<source>/dot_testrc`.
        pub(crate) fn source_file(&self) -> PathBuf {
            self.s.source.join("dot_testrc")
        }
    }

    /// A Shell with an EMPTY model, so the latch tests are about the latch
    /// and not about rows. Safe because `SyncModel::confirm_resolved` is a
    /// no-op on a target the model never heard of — it retains a list that
    /// never contained it, and only counts a move when that list actually
    /// shrank — and `batch_finished` is the only thing here that calls it.
    /// The model is also NOT connected, so `render`'s connect latch never
    /// pumps behind these tests' backs.
    fn latch_shell(route: Route, cx: &mut Context<Shell>) -> Shell {
        Shell {
            route,
            state: cx.new(|_| SyncModel::default()),
            review: None,
            settings: None,
            merge: None,
            paths: smoke_paths(),
            expanded_scans: HashSet::new(),
            unlock_in_flight: false,
            pending_merge: false,
            merge_finish_in_flight: false,
            pending_merge_probed: false,
            changelog: None,
            queue: Default::default(),
            queue_pumping: false,
            deferred_repo_op: None,
            running_repo_op: None,
        }
    }

    /// The queue's REAL path, end to end: `enqueue` → `pump` → `run_batch`
    /// against a live engine, and the decision actually lands on disk.
    ///
    /// Finding 4 of the Task 3 re-review: no test published an `EngineSlot`,
    /// so `pump` always parked and nothing below it ever ran — deleting the
    /// staged-merge conclusion await, or `catching_panics` itself, left all
    /// 335 tests green.
    ///
    /// MUTATION: make `pump` return before it spawns the batch → this fails
    /// (the queue never drains, and the source file keeps "a=1\n").
    #[gpui::test]
    fn a_batch_really_drains_through_pump(cx: &mut TestAppContext) {
        let lab = EngineLab::publish(cx);
        let target = lab.target();
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Review, cx));

        let queued = target.clone();
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                s.enqueue(
                    QueuedAction {
                        target: queued,
                        decision: Decision::KeepDisk,
                        notify_on_done: false,
                    },
                    cx,
                );
                assert!(s.queue_pumping, "the batch took the latch straight away");
            });
        });
        vis.run_until_parked();
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert!(
                s.queue.failure(&target).is_none(),
                "{:?}",
                s.queue.failure(&target)
            );
            assert_eq!(s.queue.depth(), 0, "the batch drained");
            assert!(!s.queue_pumping, "and the latch is free again");
        });
        assert_eq!(
            std::fs::read(lab.source_file()).expect("source file readable"),
            b"a=local\n",
            "keep-disk really re-added the drifted content into the source"
        );
    }

    /// A finish pressed while a batch drains runs AFTER that batch lands and
    /// BEFORE the next batch starts — the merge commit and the next
    /// decision's writes never overlap on the one repository.
    ///
    /// MUTATION: in `pump`, take the batch before checking
    /// `deferred_repo_op` → the probe below sees `running()` non-empty and
    /// `merge_finish_in_flight` false.
    #[gpui::test]
    fn a_finish_pressed_mid_drain_runs_before_the_next_batch(cx: &mut TestAppContext) {
        let lab = EngineLab::publish(cx);
        let target = lab.target();
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Dashboard, cx));

        let queued = target.clone();
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                // Stand in for a batch that is already draining.
                s.queue_pumping = true;
                s.queue.push(QueuedAction {
                    target: queued,
                    decision: Decision::KeepDisk,
                    notify_on_done: false,
                });
                s.finish_pending_merge(cx); // pressed while draining
                assert!(matches!(
                    s.deferred_repo_op,
                    Some(PendingRepoOp::FinishMerge)
                ));
                assert!(
                    !s.merge_finish_in_flight,
                    "nothing may run while the latch is held"
                );
                assert!(
                    s.queue.running().is_empty(),
                    "and no batch may start either"
                );
            });
        });
        // The stand-in batch lands with nothing to report.
        vis.update(|_window, cx| shell.update(cx, |s, cx| s.batch_finished(Vec::new(), None, cx)));
        // PROBE before anything is polled: the finish took the latch; the
        // pending batch did NOT.
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert!(
                s.deferred_repo_op.is_none(),
                "the finish was taken, not left"
            );
            assert!(s.merge_finish_in_flight, "and it is what started");
            assert!(s.queue_pumping);
            assert!(
                s.queue.running().is_empty(),
                "the batch is still waiting its turn"
            );
            assert_eq!(s.queue.depth(), 1);
        });
        vis.run_until_parked(); // the finish (NotMerging here), then the batch
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert!(!s.merge_finish_in_flight);
            assert!(!s.queue_pumping);
            assert_eq!(s.queue.depth(), 0, "and the batch ran after it");
            assert!(
                s.queue.failure(&target).is_none(),
                "{:?}",
                s.queue.failure(&target)
            );
        });
        assert_eq!(
            std::fs::read(lab.source_file()).expect("source file readable"),
            b"a=local\n"
        );
    }

    /// The abort is the case the ordering exists for: the user leaves the
    /// editor NOW, but the rollback waits for the latch, and the decisions
    /// queued behind it run against the ROLLED-BACK repo instead of being
    /// staged into a merge that has just been discarded.
    ///
    /// MUTATION: in `pump`, take the batch before checking
    /// `deferred_repo_op` → the probe below sees `running()` non-empty.
    /// MUTATION: drop the `self.route = Route::Review` from
    /// `abort_origin_merge` → the first assertion fails.
    #[gpui::test]
    fn an_abort_pressed_mid_drain_routes_now_and_runs_later(cx: &mut TestAppContext) {
        let lab = EngineLab::publish(cx);
        let target = lab.target();
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Merge, cx));

        let queued = target.clone();
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                s.queue_pumping = true;
                s.queue.push(QueuedAction {
                    target: queued,
                    decision: Decision::KeepDisk,
                    notify_on_done: false,
                });
                // Exactly what `MergeView::discard_merge` hands over.
                s.abort_origin_merge(merge::Rollback::Files(2), cx);
                assert_eq!(s.route, Route::Review, "the editor is left immediately");
                assert!(matches!(
                    s.deferred_repo_op,
                    Some(PendingRepoOp::AbortMerge(_))
                ));
                assert!(
                    s.queue.running().is_empty(),
                    "nothing started under the latch"
                );
            });
        });
        vis.update(|_window, cx| shell.update(cx, |s, cx| s.batch_finished(Vec::new(), None, cx)));
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert!(s.deferred_repo_op.is_none(), "the abort was taken …");
            assert!(s.queue_pumping, "… and holds the latch");
            assert!(
                s.queue.running().is_empty(),
                "the queued decision waits behind the rollback"
            );
            assert_eq!(s.queue.depth(), 1);
        });
        vis.run_until_parked(); // the abort, then the decision behind it
        let review = vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert!(!s.queue_pumping);
            // `depth() == 0` alone would also hold if the decision behind
            // the rollback had FAILED — `finish_all` clears `running` for a
            // failure too — so the row and the file both have to say it
            // landed.
            assert!(
                s.queue.failure(&target).is_none(),
                "{:?}",
                s.queue.failure(&target)
            );
            assert_eq!(
                s.queue.depth(),
                0,
                "the decision ran against the rolled-back repo"
            );
            s.review.clone()
        });
        assert_eq!(
            std::fs::read(lab.source_file()).expect("source file readable"),
            b"a=local\n",
            "and it really landed: keep-disk re-added the drifted content"
        );
        let review = review.expect("the abort's banner made a review view");
        let banner = vis.update(|_window, cx| review.read(cx).last_outcome.clone());
        let banner = banner.expect("the rollback is reported, never silent");
        // Not just the format string's fixed prefix: the banner has to carry
        // the size the CONFIRMATION named, and the roll-back tint rather
        // than the panic arm's.
        assert!(
            banner.text.contains("2 source file(s)"),
            "the banner names what the confirmation named: {}",
            banner.text
        );
        assert_eq!(banner.tint, review::BannerTint::Drift);
    }

    /// A press made while the app is still connecting is not a discarded
    /// click either. No `EngineSlot` is published here, so `pump` PARKS —
    /// and a parked queue keeps its deferred op until something runs it.
    ///
    /// MUTATION: have `pump` take `deferred_repo_op` before the engine
    /// lookup → the second assertion fails.
    #[gpui::test]
    fn a_deferred_op_survives_a_parked_queue(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Dashboard, cx));
        vis.update(|_window, cx| shell.update(cx, |s, cx| s.finish_pending_merge(cx)));
        vis.run_until_parked();
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert!(
                !s.queue_pumping,
                "nothing ran, so a reconnect is free to pump"
            );
            assert!(matches!(
                s.deferred_repo_op,
                Some(PendingRepoOp::FinishMerge)
            ));
            assert!(
                !s.merge_finish_in_flight,
                "and the banner button does not claim to be working"
            );
        });
    }

    /// `catching_panics` now guards four different closures with four
    /// different return types, so its fallback must travel with the caller
    /// rather than being baked into the catch.
    ///
    /// MUTATION: have `catching_panics` re-raise (drop the `catch_unwind`)
    /// → the first case panics instead of returning.
    #[test]
    fn catching_panics_hands_back_the_fallback_for_any_type() {
        let v: Result<u8, String> = catching_panics("t", Err, || panic!("boom"));
        assert!(matches!(v, Err(ref d) if d.contains("boom")), "{v:?}");
        let v: Result<u8, String> = catching_panics("t", Err, || Ok(7));
        assert_eq!(v, Ok(7), "and a closure that returns is left alone");
    }

    /// A confirmed destructive choice outranks an unconfirmed click. Discard
    /// is armed in the merge editor and pressed twice; Finish is one press on
    /// the dashboard banner. With the abort deferred — parked, or behind a
    /// batch that takes seconds — letting the click replace it would COMMIT
    /// the merge the user just confirmed throwing away, and the only trace
    /// would be a log line.
    ///
    /// MUTATION: drop the precedence check from `request_repo_op` (replace
    /// unconditionally) → the first half fails, the slot holding FinishMerge.
    #[gpui::test]
    fn a_finish_never_displaces_a_confirmed_abort(cx: &mut TestAppContext) {
        // No engine and no lab needed: the latch is held throughout, so
        // `pump` returns before it ever looks for one, and the slot is the
        // only thing deciding.
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Dashboard, cx));

        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                s.queue_pumping = true;
                // Confirmed in the merge editor …
                s.abort_origin_merge(merge::Rollback::Files(2), cx);
                // … then one unconfirmed click on the dashboard banner.
                s.finish_pending_merge(cx);
                assert!(
                    matches!(s.deferred_repo_op, Some(PendingRepoOp::AbortMerge(_))),
                    "a finish must never commit a merge the user confirmed discarding"
                );
            });
        });
        // And the dropped click is not a silent one.
        let review = vis
            .update(|_window, cx| shell.read(cx).review.clone())
            .expect("the explanation made a review view");
        let banner = vis
            .update(|_window, cx| review.read(cx).last_outcome.clone())
            .expect("an ignored click is reported, never silently swallowed");
        assert!(
            banner.text.contains("Finish ignored"),
            "the banner says why the click did nothing: {}",
            banner.text
        );

        // The other direction: the user's LATER, confirmed choice does
        // replace a pending finish.
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                s.deferred_repo_op = None;
                s.finish_pending_merge(cx);
                assert!(matches!(
                    s.deferred_repo_op,
                    Some(PendingRepoOp::FinishMerge)
                ));
                s.abort_origin_merge(merge::Rollback::Files(3), cx);
                assert!(
                    matches!(s.deferred_repo_op, Some(PendingRepoOp::AbortMerge(_))),
                    "an abort supersedes a pending finish — it is the later, confirmed answer"
                );
            });
        });
    }

    /// The user's own acceptance bar, end to end: confirming a file records
    /// the decision and moves to the next one, with nothing to wait for.
    ///
    /// The fixture's selection is `.config/zed/settings.json`, third in
    /// display order (`env.nu`, `secrets.yaml`, then the two one-click
    /// rows), so the row after it is `.gitconfig`. No `EngineSlot` is
    /// published, so the decision PARKS on the queue — which is the point:
    /// the interface is done with it either way.
    ///
    /// MUTATION: drop the `advance_from` call from `run_action` → the
    /// selection stays on the file just answered. MUTATION: have
    /// `ReviewView::enqueue` skip the `shell.update` → the queue is empty
    /// and the click was discarded, the exact defect this plan removes.
    #[gpui::test]
    fn a_review_decision_queues_and_advances_the_selection(cx: &mut TestAppContext) {
        use review::ResolveAction;

        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Users/gallery"));
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review", smoke_paths(), cx).expect("review fixture")
        });
        let review = vis
            .update(|_window, cx| shell.read(cx).review.clone())
            .expect("the fixture poses a review view");
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selected.clone()),
            Some(home.join(".config/zed/settings.json"))
        );

        vis.update(|_window, cx| {
            review.update(cx, |view, cx| view.run_action(ResolveAction::KeepDisk, cx))
        });

        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert!(
                s.queue
                    .processing()
                    .contains(&home.join(".config/zed/settings.json")),
                "the decision is recorded on the Shell's queue, not discarded"
            );
            assert_eq!(s.queue.depth(), 1);
        });
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selected.clone()),
            Some(home.join(".gitconfig")),
            "and the selection advanced to the next row that still needs a decision"
        );
    }

    /// A failure has to outlive its action (spec §2.5) but NOT the drift.
    /// Only a retry cleared one before this, so a file that failed, went
    /// back in sync, and drifted again months later for an unrelated reason
    /// came back with the old message still on its row — and `push` is the
    /// one thing that clears failures, which a fresh drift does not call.
    ///
    /// Drives the real wiring: `observe_model` is what the app registers, so
    /// the model's own notify is what has to do the pruning.
    ///
    /// MUTATION: drop `model_changed` from `observe_model`'s closure (or
    /// `retain_failures`' body) → `/a` keeps its message.
    #[gpui::test]
    fn a_failure_is_forgotten_once_its_target_stops_being_drifted(cx: &mut TestAppContext) {
        use std::path::Path;

        let drift = |target: &str| tomte_proto::DriftSummary {
            target: PathBuf::from(target),
            class: "destination_drift".into(),
            since_ts: Some(1),
            origin_differs: false,
        };
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            let mut shell = latch_shell(Route::Review, cx);
            shell.observe_model(cx);
            shell
        });
        let state = vis.update(|_window, cx| shell.read(cx).state.clone());

        // Both files drifted, both with a failed decision on them.
        vis.update(|_window, cx| {
            state.update(cx, |model, cx| {
                model.hydrate_status(vec![drift("/a"), drift("/b")], 1, None, false, None);
                cx.notify();
            });
            shell.update(cx, |s, _cx| {
                s.queue.finish_all(&[
                    (
                        PathBuf::from("/a"),
                        Some("Keep disk failed: op timed out".into()),
                    ),
                    (
                        PathBuf::from("/b"),
                        Some("Keep disk failed: op timed out".into()),
                    ),
                ]);
            });
        });
        vis.run_until_parked();
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert!(
                s.queue.failure(Path::new("/a")).is_some()
                    && s.queue.failure(Path::new("/b")).is_some(),
                "both rows show why they failed while both are still drifted"
            );
        });

        // `/a` goes back in sync; `/b` is still drifted.
        vis.update(|_window, cx| {
            state.update(cx, |model, cx| {
                model.hydrate_status(vec![drift("/b")], 2, None, false, None);
                cx.notify();
            });
        });
        vis.run_until_parked();
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert!(
                s.queue.failure(Path::new("/a")).is_none(),
                "the message went with the drift — it must not wait to ambush the \
                 file's next one"
            );
            assert!(
                s.queue.failure(Path::new("/b")).is_some(),
                "and a row that is still drifted keeps its failure: nobody has \
                 retried it"
            );
        });
    }

    /// Undo is a repo op like the other two: it takes the latch, runs
    /// behind it, reports through Review's banner, and lets go. Before this
    /// it spawned `undo_last` beside a draining batch — and `undo_last`
    /// rewrites the very destination files that batch's `chezmoi re-add` is
    /// reading.
    ///
    /// The lab's journal has no finished session, so the honest answer is
    /// "nothing to undo" — which still has to REACH the user.
    ///
    /// MUTATION: drop the `PendingRepoOp::UndoLast` arm's `run_undo` from
    /// `pump` (leave the op taken) → the latch never clears and no banner
    /// lands.
    #[gpui::test]
    fn an_undo_takes_the_latch_runs_and_reports(cx: &mut TestAppContext) {
        let _lab = EngineLab::publish(cx);
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Review, cx));

        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                assert!(!s.is_busy(), "idle: the Undo button is live");
                s.undo_last(cx);
                assert!(s.queue_pumping, "the undo took the latch straight away");
                assert!(s.deferred_repo_op.is_none(), "it ran, it did not wait");
                assert!(s.is_busy(), "and the button is inert while it runs");
            });
        });
        vis.run_until_parked();
        let review = vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert!(!s.queue_pumping, "the latch is free again");
            assert!(!s.is_busy());
            s.review.clone()
        });
        let review = review.expect("the undo's banner made a review view");
        let banner = vis
            .update(|_window, cx| review.read(cx).last_outcome.clone())
            .expect("an undo is reported, never silent");
        assert_eq!(banner.text.as_ref(), "nothing to undo");
        assert!(!banner.undoable, "undoing an undo restores nothing");
    }

    /// An undo displaces neither answer about the merge, and — unlike the
    /// finish-vs-abort drop — it must not say so through `merge_done`.
    /// `undo_last` is called from inside the review view's own update, and
    /// `merge_done` reaches back into that same view: the nesting gpui
    /// refuses. The log line is the whole report from this path.
    ///
    /// MUTATION: drop the `UndoLast` guard from `request_repo_op` → the
    /// finish is superseded and the first assertion fails. MUTATION: report
    /// the drop through `merge_done` → the second fails.
    #[gpui::test]
    fn an_undo_never_displaces_a_pending_merge_answer(cx: &mut TestAppContext) {
        // No engine: `pump` parks, so the deferred op stays put and the slot
        // is the only thing deciding.
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Review, cx));

        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                s.finish_pending_merge(cx);
                s.undo_last(cx);
                assert!(
                    matches!(s.deferred_repo_op, Some(PendingRepoOp::FinishMerge)),
                    "the answer about the merge survives an undo pressed after it"
                );
            });
        });
        // The route rendered the review view; the drop must not have put
        // anything in it. Banner-ing this drop would nest an update of the
        // very view Undo is pressed from — the crash of 2026-08-18.
        let review = vis
            .update(|_window, cx| shell.read(cx).review.clone())
            .expect("the Review route built its view");
        assert!(
            vis.update(|_window, cx| review.read(cx).last_outcome.clone())
                .is_none(),
            "the dropped undo is reported through the log only"
        );
        // The same holds for the abort, the other repo-level answer.
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                s.deferred_repo_op = Some(PendingRepoOp::AbortMerge(merge::Rollback::Files(1)));
                s.undo_last(cx);
                assert!(matches!(
                    s.deferred_repo_op,
                    Some(PendingRepoOp::AbortMerge(_))
                ));
            });
        });
    }

    /// The abort re-probes for a stranded merge on BOTH arms, and the
    /// dashboard banner it raises has to come DOWN when the merge it was
    /// reporting is gone. Nothing else clears that flag: every file in such
    /// a merge is answered, so no drifted row will ever contradict it.
    ///
    /// MUTATION: drop `refresh_pending_merge` from `run_abort`'s completion
    /// → the banner stays up over a merge that no longer exists.
    #[gpui::test]
    fn an_abort_reprobes_whether_a_merge_is_still_pending(cx: &mut TestAppContext) {
        let _lab = EngineLab::publish(cx);
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Merge, cx));
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                // The dashboard banner is up, over a merge the scratch repo
                // does not actually have.
                s.pending_merge = true;
                s.abort_origin_merge(merge::Rollback::Files(1), cx);
            });
        });
        vis.run_until_parked();
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert!(
                !s.pending_merge,
                "the probe ran and took the banner down with the merge"
            );
            assert!(!s.queue_pumping, "and the latch is free");
        });
    }

    /// The abort's panic fallback must not borrow the rolled-back arm's
    /// sentence. A failed rescan happens AFTER `merge_abort` ran, so the
    /// rollback did happen; a panic can land before or inside it, so nothing
    /// is known about the merge — and the banner is the only thing the user
    /// has to go on.
    ///
    /// MUTATION: route `AbortOutcome::Panicked` through the `RolledBack`
    /// arm (i.e. hand `catching_panics` `internal_error` again) → the third
    /// case claims a roll-back that may never have happened.
    #[test]
    fn a_panicked_abort_never_claims_the_roll_back_happened() {
        let rolled_back = merge::Rollback::Files(2);

        let done = abort_banner(AbortOutcome::RolledBack(Ok(())), &rolled_back);
        assert!(
            done.text.contains("rolled back") && done.text.contains("2 source file(s)"),
            "{}",
            done.text
        );
        assert_eq!(done.tint, review::BannerTint::Drift);

        let stale = abort_banner(
            AbortOutcome::RolledBack(Err(ResolveError::Failed("daemon gone".into()))),
            &rolled_back,
        );
        assert!(
            stale.text.contains("rolled back") && stale.text.contains("may be stale"),
            "a rescan failure still means the rollback happened: {}",
            stale.text
        );

        let boom = abort_banner(
            AbortOutcome::Panicked("index out of bounds".into()),
            &rolled_back,
        );
        assert!(
            !boom.text.contains("rolled back"),
            "a panic must never assert the roll-back: {}",
            boom.text
        );
        assert!(
            boom.text.contains("may still be in progress"),
            "and must say the merge's state is unknown: {}",
            boom.text
        );
        assert_eq!(
            boom.tint,
            review::BannerTint::Conflict,
            "an unknown repo state is not a routine drift outcome"
        );
    }

    /// `a_panicked_abort_never_claims_the_roll_back_happened` proves
    /// `abort_banner` maps the outcomes correctly. It cannot prove that the
    /// abort HANDS it the right one, and that is where the bug lived: a
    /// fallback of the wrong shape turns every panic back into "the
    /// roll-back ran, the rescan failed".
    ///
    /// `abort_repo_reconcile` cannot be made to panic, so `run_abort`'s
    /// choice of fallback lives in [`abort_outcome`] — a real seam, driven
    /// here with a closure that panics. (The predecessor of this test split
    /// this file's own source text on a literal, which passed or failed on
    /// formatting rather than on behaviour.)
    ///
    /// MUTATION: give `abort_outcome` the fallback
    /// `|d| AbortOutcome::RolledBack(internal_error(d))` → the first
    /// assertion fails.
    #[test]
    fn the_abort_hands_catching_panics_a_fallback_that_claims_nothing() {
        let boom = abort_outcome(|| panic!("index out of bounds"));
        assert!(
            matches!(boom, AbortOutcome::Panicked(ref d) if d.contains("index out of bounds")),
            "a panicking abort must come back as `Panicked` — anything that reports a \
             completed roll-back would have the banner assert a rollback that may never \
             have run. Got: {boom:?}"
        );
        // And an abort that returns is left exactly as it is.
        let done = abort_outcome(|| AbortOutcome::RolledBack(Ok(())));
        assert!(matches!(done, AbortOutcome::RolledBack(Ok(()))), "{done:?}");
    }

    /// The merge editor's Save is a QUEUE entry now, and the whole reason
    /// that matters: a save no longer blocks the next review decision. Before
    /// this, `MergeView::save` spawned `resolve_merged` itself behind its own
    /// `saving` boolean — beside whatever the queue was draining, on the same
    /// git repository — and the editor sat on "saving…" until it came back.
    ///
    /// No `EngineSlot` is published, so both decisions PARK. That is the
    /// point twice over: the interface is done with each of them either way,
    /// and a disconnected daemon no longer answers a Save with "cannot save"
    /// (the press is remembered, and runs when something reconnects).
    ///
    /// MUTATION: reinstate a `saving`-style early return in `save` (or its
    /// engine lookup, which with no engine returns before enqueueing) → the
    /// merge decision never reaches the queue and `depth()` is 1.
    /// MUTATION: have `save` spawn `resolve_merged` again instead of
    /// enqueueing → same.
    #[gpui::test]
    fn a_merge_save_queues_and_never_blocks_the_next_decision(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("merge-resolved", smoke_paths(), cx).expect("merge-resolved fixture")
        });
        let merge = vis
            .update(|_window, cx| shell.read(cx).merge.clone())
            .expect("the fixture poses a resolved merge");
        let target = vis
            .update(|_window, cx| merge.read(cx).target.clone())
            .expect("posed with a target");

        vis.update(|_window, cx| merge.update(cx, |view, cx| view.save(cx)));
        // The hand-off is deliberately OUTSIDE the view's own update, so it
        // lands on the next turn of the executor.
        vis.run_until_parked();

        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert_eq!(
                s.queue.decision_label(&target),
                Some("Save"),
                "the resolution is on the queue, not in flight inside the editor"
            );
            assert_eq!(s.route, Route::Review, "and the editor is left immediately");
        });

        // A review decision on ANOTHER file, made while the save is still
        // owed work. This is the user's own complaint: it must be accepted.
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                s.enqueue(
                    QueuedAction {
                        target: PathBuf::from("/b"),
                        decision: Decision::KeepDisk,
                        notify_on_done: false,
                    },
                    cx,
                );
            });
        });
        vis.run_until_parked();
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert_eq!(s.queue.depth(), 2, "both decisions are remembered");
            let processing = s.queue.processing();
            assert!(
                processing.contains(&target) && processing.contains(&PathBuf::from("/b")),
                "and both rows are processing: {processing:?}"
            );
        });

        // The Save's own banner is the one place it routes the user, and it
        // says only what is true so far.
        let review = vis
            .update(|_window, cx| shell.read(cx).review.clone())
            .expect("the save's hand-off made a review view");
        let banner = vis
            .update(|_window, cx| review.read(cx).last_outcome.clone())
            .expect("a save is never silent");
        assert!(
            banner.text.starts_with("Save queued · "),
            "the banner claims only that it was recorded: {}",
            banner.text
        );
    }

    /// An abort rolls the source repo back, so a merge-editor save still
    /// waiting to be staged INTO that merge has nowhere to go. It fails
    /// FAST, on its own row, in words that say what to do — running it
    /// afterwards would either fail obscurely or quietly open a second merge.
    ///
    /// And only that one: a keep-disk queued behind the abort does not depend
    /// on the merge and runs correctly against the rolled-back repo, which is
    /// what "abort, then apply my other decisions" has to mean.
    ///
    /// MUTATION: remove the `fail_pending_where` call from `request_repo_op`
    /// → the orphaned save is still pending and its row shows no failure.
    /// MUTATION: widen the predicate to every decision → the keep-disk is
    /// thrown away too.
    #[gpui::test]
    fn an_abort_fails_the_saves_it_would_orphan_and_keeps_the_rest(cx: &mut TestAppContext) {
        use std::path::Path;

        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Merge, cx));
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                // A batch is draining, so nothing below can start.
                s.queue_pumping = true;
                s.queue.push(QueuedAction {
                    target: PathBuf::from("/staged"),
                    decision: Decision::RepoConflict {
                        source_path: PathBuf::from("/src/staged"),
                        resolved: "x".into(),
                    },
                    notify_on_done: false,
                });
                s.queue.push(QueuedAction {
                    target: PathBuf::from("/plain"),
                    decision: Decision::KeepDisk,
                    notify_on_done: false,
                });

                s.abort_origin_merge(merge::Rollback::Files(2), cx);

                assert_eq!(
                    s.queue.failure(Path::new("/staged")).map(|m| m.as_ref()),
                    Some("merge rolled back before this save ran — save again"),
                    "the orphaned save says so on its row, and says what to do"
                );
                assert_eq!(
                    s.queue.decision_label(Path::new("/plain")),
                    Some("Keep disk"),
                    "a decision that does not depend on the merge stays queued"
                );
                assert_eq!(s.queue.depth(), 1);
                assert!(
                    matches!(s.deferred_repo_op, Some(PendingRepoOp::AbortMerge(_))),
                    "and the roll-back itself still waits for the latch"
                );
            });
        });
    }

    /// `running_repo_op` exists so a surface can tell "an answer is waiting"
    /// from "an answer is under way" — `deferred_repo_op` is `None` for the
    /// second, and the dashboard's Finish button has to say something
    /// different in each. It is set where the op is picked up and cleared
    /// where the latch is, so the two windows are the same window.
    ///
    /// MUTATION: drop `running_repo_op = Some(op.kind())` from `pump` → the
    /// middle probe fails. MUTATION: drop the clear from `run_finish`'s
    /// completion → the last one does, and the Finish button would read
    /// "Finishing…" forever.
    #[gpui::test]
    fn running_repo_op_spans_exactly_the_op_it_is_running(cx: &mut TestAppContext) {
        let _lab = EngineLab::publish(cx);
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Dashboard, cx));

        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                s.queue_pumping = true; // a batch holds the latch
                s.finish_pending_merge(cx);
                assert_eq!(
                    s.running_repo_op, None,
                    "pressed but not started: the button says \"queued\", not \"finishing\""
                );
                assert_eq!(
                    s.deferred_repo_op.as_ref().map(|op| op.kind()),
                    Some(RepoOpKind::Finish)
                );
            });
        });
        // The stand-in batch lands; `pump` picks the finish up.
        vis.update(|_window, cx| shell.update(cx, |s, cx| s.batch_finished(Vec::new(), None, cx)));
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert_eq!(
                s.running_repo_op,
                Some(RepoOpKind::Finish),
                "taken and started"
            );
            assert!(s.deferred_repo_op.is_none(), "it is no longer waiting");
            assert!(s.queue_pumping, "the same window as the latch");
        });
        vis.run_until_parked();
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert_eq!(s.running_repo_op, None, "and cleared when it finished");
            assert!(!s.queue_pumping);
        });
    }

    /// THE YANK RULE. A decision the user made minutes ago must never move
    /// them when it lands: they were routed at the moment they pressed the
    /// button, and may have opened another file, the merge editor, or
    /// Settings since. `show_outcome` puts the banner where they will find
    /// it; `merge_done` is for the answers they are standing there waiting
    /// for.
    ///
    /// MUTATION: have `show_outcome` set `route = Route::Review` (i.e. make
    /// it `merge_done` again) → the first assertion fails.
    #[gpui::test]
    fn a_landed_outcome_reports_without_moving_the_user(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Settings, cx));
        let banner = || OutcomeBanner {
            text: "Merged · committed & pushed".into(),
            tint: review::BannerTint::Ok,
            undoable: true,
        };

        vis.update(|_window, cx| shell.update(cx, |s, cx| s.show_outcome(banner(), cx)));
        let review = vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert_eq!(
                s.route,
                Route::Settings,
                "a queued decision landing must not drag the user out of where they are"
            );
            s.review.clone()
        });
        let review = review.expect("the banner still made a review view to land in");
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).last_outcome.clone())
                .map(|b| b.text),
            Some(banner().text),
            "and it is waiting for them there"
        );

        // The other half: an answer the user pressed and is waiting on DOES
        // take them to it.
        vis.update(|_window, cx| shell.update(cx, |s, cx| s.merge_done(banner(), cx)));
        vis.update(|_window, cx| assert_eq!(shell.read(cx).route, Route::Review));
    }
}
