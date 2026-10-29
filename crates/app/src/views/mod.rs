//! Shell: left sidebar navigation (Zed settings-window shape — fixed-width
//! panel, bordered, compact nav rows) over a routed content pane.

pub mod changelog;
pub mod dashboard;
pub mod fixtures;
pub mod merge;
mod merge_structured;
pub mod review;
mod review_selection;
mod review_tree;
pub mod settings;

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

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
    /// Abandon the merge with origin, rolling the source repo back.
    AbortMerge {
        /// What the editor told the user it would discard, so the banner
        /// afterwards names the same thing the confirmation did.
        rolled_back: merge::Rollback,
        /// Queued merge-editor saves this roll-back orphaned, filled in by
        /// [`Shell::request_repo_op`] when the op is accepted.
        ///
        /// They travel WITH the op because their rows cannot be relied on to
        /// carry the news: `fail_pending_where` puts the message on each row,
        /// but a file that was a review row only BECAUSE the merge left it
        /// conflicted stops being drifted the moment the merge is rolled
        /// back, and `model_changed` then prunes the message with the row.
        /// The banner is the one surface that outlives both.
        dropped: Vec<PathBuf>,
    },
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
            Self::AbortMerge { .. } => RepoOpKind::Abort,
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
    /// enters through [`Self::pump`]", and as of Task 5 it holds for every
    /// VIEW surface: Review's quick actions and "Keep both", the merge
    /// editor's Save, the dashboard's quick actions, and a finish, a
    /// roll-back or an Undo all come through here.
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
    /// ONE door is still open, and it predates this work: the tray's "Sync
    /// all" (`main.rs`'s `run_sync_all`) spawns `engine.sync_all()` —
    /// `chezmoi update`, which is a git pull plus an apply — without
    /// consulting this latch. It is reachable beside a pending finish,
    /// because Sync all is gated on nothing being drifted, which is exactly
    /// the stranded-merge state where the dashboard's Finish is live.
    /// Routing it as a [`PendingRepoOp`] is a follow-up rather than part of
    /// this change: the tray handler holds `&mut App` with no `Shell` handle
    /// to pump.
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
        self.request_repo_op(
            PendingRepoOp::AbortMerge {
                rolled_back,
                // `request_repo_op` fills this: it is the one place that
                // knows which queued saves the roll-back just took away.
                dropped: Vec::new(),
            },
            cx,
        );
    }

    /// The abort, running with the latch already taken. Only [`Self::pump`]
    /// calls this.
    fn run_abort(
        &mut self,
        engine: std::sync::Arc<ResolveEngine>,
        rolled_back: merge::Rollback,
        dropped: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    abort_outcome(AssertUnwindSafe(|| {
                        abort_outcome_of(engine.abort_repo_reconcile(), || engine.request_rescan())
                    }))
                })
                .await;
            this.update(cx, |shell, cx| {
                shell.queue_pumping = false;
                shell.running_repo_op = None;
                // ROUTES, like the finish arm at `run_finish`: `merge_done`
                // sends the banner to Review, so a roll-back that lands
                // minutes later can pull the user out of Settings. That is
                // the intent. The user pressed Discard and is owed the
                // answer wherever they went — and above all the `AbortFailed`
                // arm MUST be seen: it says the merge is still sitting there
                // with their answers staged in it, which is not a thing to
                // learn later from a banner nobody routed.
                shell.merge_done(abort_banner(outcome, &rolled_back, &dropped), cx);
                // On EVERY arm. A rollback that completed ended the merge the
                // dashboard banner was reporting; one that FAILED left it
                // open, which is exactly when that banner has to be up; and
                // one that panicked left whether a merge is in progress
                // unknown. In all three what is on screen is a guess until it
                // is re-probed — and this probe is one `rev-parse
                // MERGE_HEAD`, not the daemon rescan the failed arm skips.
                shell.refresh_pending_merge(cx);
                cx.notify();
                // Whatever queued behind the rollback, now against the repo
                // the rollback restored — or, if it failed, did not.
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
    /// PENDING **OR** RUNNING. An abort that is halfway through rewriting the
    /// source tree is `running_repo_op`, not `deferred_repo_op`, and dropping
    /// only the pending case left that window open: the finish would sit in
    /// the slot, `pump` would take it the moment the abort's completion let
    /// go of the latch, and it would run against whatever the abort left. It
    /// was harmless only by accident — `finish_merge_if_resolved` would find
    /// `merge_in_progress() == false` after a roll-back that WORKED. Now that
    /// an abort can honestly fail ([`AbortOutcome::AbortFailed`]) the merge
    /// can still be there, and that accident becomes the exact commit the
    /// user confirmed throwing away. A finish never outranks an abort.
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
        let abort_queued = matches!(
            self.deferred_repo_op,
            Some(PendingRepoOp::AbortMerge { .. })
        );
        let abort_running = self.running_repo_op == Some(RepoOpKind::Abort);
        if matches!(op, PendingRepoOp::FinishMerge) && (abort_queued || abort_running) {
            tomte_core::log_info!(
                "reconcile",
                "finish ignored: a confirmed roll-back of this merge is already {}",
                if abort_queued { "queued" } else { "running" }
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
        //
        // The targets travel ON the op, because their rows cannot be trusted
        // to carry the news: a file that was a review row only because the
        // merge left it conflicted stops being drifted the moment the
        // roll-back lands, and `model_changed` then prunes its message along
        // with its row. The roll-back's own banner is what outlives both.
        //
        // THE MESSAGE IS ABOUT THE REQUEST, NOT THE ROLL-BACK, and that is
        // Ruling 30 again. These rows are failed HERE, when Discard is
        // pressed — minutes before the abort runs, and whatever it then does.
        // On the [`AbortOutcome::AbortFailed`] arm the merge is NOT rolled
        // back and these rows SURVIVE (their files are still conflicted), so
        // a message saying "merge rolled back" would have every row assert
        // exactly what the banner had just stopped asserting. "A roll-back
        // was requested" is true on all three arms, and it is the fact that
        // actually explains why the save is gone.
        let mut op = op;
        if let PendingRepoOp::AbortMerge { dropped, .. } = &mut op {
            let mut orphaned = self.queue.fail_pending_where(
                |a| matches!(a.decision, Decision::RepoConflict { .. }),
                "dropped: a roll-back was requested before this save ran — save again",
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
            // A roll-back ALREADY waiting took its own orphans out of
            // `pending` when it was requested, so the call above finds
            // nothing for them — and this one is about to replace it. Carry
            // them across, or the second Discard's banner would report zero
            // while the first one's saves are just as gone. Reachable
            // without contrivance: Discard, latch held, back to Review, open
            // the editor on another file, Discard again.
            if let Some(PendingRepoOp::AbortMerge {
                dropped: earlier, ..
            }) = &self.deferred_repo_op
            {
                let carried: Vec<PathBuf> = earlier
                    .iter()
                    .filter(|t| !orphaned.contains(t))
                    .cloned()
                    .collect();
                if !carried.is_empty() {
                    tomte_core::log_info!(
                        "reconcile",
                        "carrying {} save(s) dropped by the roll-back this one supersedes",
                        carried.len()
                    );
                    orphaned.extend(carried);
                }
            }
            *dropped = orphaned;
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
    ///
    /// "A finish is running" is [`Self::running_repo_op`] `== Some(Finish)`,
    /// which `pump` set immediately above the call to this. There used to be
    /// a `merge_finish_in_flight` bool set here as well, over exactly the
    /// same window — two sources of truth for one fact, and the dashboard
    /// read both (Ruling 41).
    fn run_finish(&mut self, engine: std::sync::Arc<ResolveEngine>, cx: &mut Context<Self>) {
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
        self.note_outcome(banner, cx);
        self.reload_selected(cx);
    }

    /// The banner half of [`Self::show_outcome`]: set it, move nothing,
    /// reload nothing.
    ///
    /// Separate because [`Self::batch_finished`] emits one banner PER LANDED
    /// TARGET (the last wins, and it is the one Undo would revert), while the
    /// preview reload below is a `chezmoi cat` plus a journal read. Doing that
    /// once per file would fire thirty subprocess round trips at the end of a
    /// thirty-file sweep to render one diff — so the banners go through here
    /// and the reload happens once.
    fn note_outcome(&mut self, banner: OutcomeBanner, cx: &mut Context<Self>) {
        let review = self.ensure_review(cx);
        review.update(cx, |view, cx| {
            view.last_outcome = Some(banner);
            cx.notify();
        });
        cx.notify();
    }

    /// Reload the review view's preview so its diff reflects the converged
    /// reality after something landed.
    fn reload_selected(&mut self, cx: &mut Context<Self>) {
        let review = self.ensure_review(cx);
        review.update(cx, |view, cx| {
            if let Some(target) = view.selected.clone() {
                view.select(target, cx);
            }
        });
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
        self.enqueue_many(vec![action], cx);
    }

    /// Several decisions made in ONE gesture — a bulk press over a
    /// selection. Every one is pushed before the queue is pumped, so they
    /// drain as one batch: one `re-add`/`apply`, one commit, one push
    /// (queue spec §2.2). `enqueue` per file would pump on the first push
    /// and drain it alone. Each press still logs its own line: the run can
    /// be minutes away and the log is where a press meets its outcome.
    pub fn enqueue_many(&mut self, actions: Vec<QueuedAction>, cx: &mut Context<Self>) {
        if actions.is_empty() {
            return;
        }
        for action in actions {
            tomte_core::log_info!(
                "resolve",
                "queued {} for {}",
                action.decision.label(),
                action.target.display()
            );
            self.queue.push(action);
        }
        cx.notify();
        self.pump(cx);
    }

    /// Take back a queued decision (spec 2026-10-11 §2.4). No latch and no
    /// repo: only the queue changes, so this may be reached from inside the
    /// review view's own update — it never updates that view back.
    pub fn cancel_queued(&mut self, target: &Path, cx: &mut Context<Self>) {
        let label = self.queue.decision_label(target).unwrap_or("a decision");
        if self.queue.cancel(target) {
            tomte_core::log_info!(
                "resolve",
                "removed the queued {label} for {}",
                target.display()
            );
            cx.notify();
        }
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
    /// The one door for the app's git work: nothing on `Shell` spawns a
    /// mutation except through here, which is what makes
    /// [`Self::queue_pumping`] a real serial latch rather than a flag only
    /// the queue respects. The views' own action paths all come through here
    /// too — Review, the merge editor's Save, the dashboard's quick actions,
    /// and finish / roll-back / Undo. The one surface that does not is the
    /// tray's "Sync all"; [`Self::queue_pumping`] says why it is still
    /// outside.
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
                PendingRepoOp::AbortMerge {
                    rolled_back,
                    dropped,
                } => self.run_abort(engine, rolled_back, dropped, cx),
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
                && let Some(banner) = review::queued_outcome_banner(decision, &target, &result)
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
        // EVERY landed target updates the banner, in result order, so the
        // LAST one wins — WITHOUT moving the user (the yank rule).
        //
        // Last-wins is what keeps Undo honest rather than a matter of taste.
        // Undo is per file (spec §2.3) and `undo_last` reverts the last
        // FINISHED journal session, which for a batch is its last item —
        // `resolve_batch` ends one session per file in batch order and
        // `per_target` comes back slot-indexed. So the banner above the Undo
        // button has to name that same last item. Naming an earlier one (or
        // leaving a merge save's banner standing over a later sweep) offers
        // an Undo that reverts a different file than the sentence describes.
        let landed_any = !landed.is_empty();
        for banner in landed {
            self.note_outcome(banner, cx);
        }
        // Once, not per banner: this is a `chezmoi cat` and a journal read.
        if landed_any {
            self.reload_selected(cx);
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

    /// This Shell as the dashboard renders it, copied out once per render.
    ///
    /// A method rather than a struct literal inside `render` so the WIRING is
    /// testable. `running` in particular joins the queue to the quick
    /// actions' row lock, and that join is invisible everywhere else: the
    /// buttons are `opacity(0.)` until the row is hovered, so no screenshot
    /// can show it, and replacing this field with `Vec::new()` used to leave
    /// every test green while the lock silently stopped working.
    pub(super) fn dashboard_snapshot(&self) -> DashboardView {
        DashboardView {
            state: self.state.clone(),
            now_ts: dashboard::system_now,
            expanded_scans: self.expanded_scans.clone(),
            unlock_in_flight: self.unlock_in_flight,
            pending_merge: self.pending_merge,
            pending_repo_op: self.deferred_repo_op.as_ref().map(|op| op.kind()),
            running_repo_op: self.running_repo_op,
            running: self
                .queue
                .running()
                .iter()
                .map(|a| a.target.clone())
                .collect(),
        }
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
/// Deliberately NOT a `Result`: the three ways this can go say three
/// different things about the one fact the banner asserts.
///
/// A rescan failure happens AFTER the abort ran, so the rollback did happen
/// and only the list on screen is suspect. An abort that FAILED means the
/// merge is still open with the user's answers still staged in it — the
/// opposite fact. And a panic can land before or inside the abort, so nothing
/// is known about the merge at all.
#[derive(Debug)]
enum AbortOutcome {
    /// The abort ran; the rescan behind it succeeded, or failed like this.
    RolledBack(Result<(), ResolveError>),
    /// `abort_repo_reconcile` came back `Err`: git would not roll the merge
    /// back (an `index.lock` held past its retries, most often). Nothing was
    /// undone and the merge is still in progress.
    ///
    /// Reportable at all only since Task 6: `GitClient::merge_abort` used to
    /// discard its own result, so this arm's reality was delivered as
    /// [`Self::RolledBack`] — "Merge with origin rolled back" over a merge
    /// that was still sitting there.
    AbortFailed(ResolveError),
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

/// Which [`AbortOutcome`] an abort's own `Result` earns, and whether the
/// daemon is asked to rescan behind it.
///
/// A SEAM, for the same reason [`abort_outcome`] is one and not for a
/// different one. `abort_banner` can be tested against every outcome, and
/// that proves the banner mapping — but nothing about which outcome the abort
/// HANDS it, which is exactly where Ruling 30's bug lived: routing an `Err`
/// through `RolledBack(Err(..))` (the pre-Task-6 shape) makes a roll-back
/// that git REFUSED report as one that ran with only a failed rescan behind
/// it. `run_abort`'s closure is a background `cx.spawn` over a live engine,
/// so nothing there is assertable; this is.
///
/// `rescan` is a closure, not a `Result`, because WHETHER IT RUNS is the
/// second half of the decision. It belongs on the `Ok` arm only: a rescan is
/// what makes the list on screen describe the RESTORED repo, and after a
/// failed abort nothing was restored — asking the daemon to re-walk the tree
/// would cost ~17 s of held core lock to confirm the very repo its last scan
/// already described. Passing it as a closure is what lets a test prove it
/// was never called.
fn abort_outcome_of(
    abort: Result<(), ResolveError>,
    rescan: impl FnOnce() -> Result<(), ResolveError>,
) -> AbortOutcome {
    match abort {
        Ok(()) => AbortOutcome::RolledBack(rescan()),
        Err(e) => AbortOutcome::AbortFailed(e),
    }
}

/// The banner one abort outcome earns.
///
/// Split out of the completion so the panic arm is testable: it must never
/// inherit the rolled-back arm's sentence. Claiming "Merge with origin
/// rolled back" after a panic would be a confident statement about the exact
/// thing the panic took away.
fn abort_banner(
    outcome: AbortOutcome,
    rolled_back: &merge::Rollback,
    dropped: &[PathBuf],
) -> OutcomeBanner {
    match outcome {
        AbortOutcome::RolledBack(rescanned) => {
            let mut text = format!(
                "Merge with origin rolled back — {} returned to their pre-merge content. \
                 Destination files and journal entries were not touched.",
                rolled_back.phrase()
            );
            // Said HERE, not only on the rows: a file that was a review row
            // only because this merge left it conflicted stops being drifted
            // the moment the roll-back lands, taking its row — and the
            // failure message on it — with it. Without this line the user's
            // save would have disappeared with nothing but a log entry.
            if !dropped.is_empty() {
                text.push_str(&format!(
                    " {} queued save(s) were dropped with it — save again.",
                    dropped.len()
                ));
            }
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
        AbortOutcome::AbortFailed(e) => {
            tomte_core::log_error!("reconcile", "the roll-back failed: {e}");
            // NOT the word "rolled back", in any arrangement. This is the one
            // outcome where the merge is definitely STILL THERE, and the
            // sentence has to be readable by someone who only skims it.
            // The remedy names where Cancel actually IS. This completion
            // leaves `route == Review`, and Cancel lives in the merge
            // editor's header — telling the user to "try Cancel again" from
            // a screen that has no Cancel is the same class of defect as the
            // sentence above it.
            let mut text = format!(
                "The roll-back failed ({e}). The merge with origin is still in progress and \
                 nothing was undone — the answers you had already given are still staged in \
                 it. Open one of its files in the merge editor and press Cancel there to try \
                 again, or finish the merge from the dashboard banner."
            );
            // Dropped when the roll-back was REQUESTED, so they are gone
            // whether or not it ran — and here the user is being told the
            // rest did not happen, which makes the silence worse.
            if !dropped.is_empty() {
                text.push_str(&format!(
                    " {} queued save(s) were dropped when it was requested — save again.",
                    dropped.len()
                ));
            }
            OutcomeBanner {
                text: text.into(),
                tint: review::BannerTint::Conflict,
                undoable: false,
            }
        }
        AbortOutcome::Panicked(detail) => {
            tomte_core::log_error!(
                "reconcile",
                "the roll-back did not complete: {detail} — the merge may still be in progress"
            );
            let mut text = "The roll-back did not complete (internal error — see the log). \
                            The merge with origin may still be in progress, and this list \
                            may describe the repo either before or after it."
                .to_string();
            // The saves were dropped when the roll-back was REQUESTED, so
            // they are gone whether or not it completed. Silence here would
            // lose them exactly when the user has least to go on.
            if !dropped.is_empty() {
                text.push_str(&format!(
                    " {} queued save(s) were dropped when it was requested — save again.",
                    dropped.len()
                ));
            }

            OutcomeBanner {
                text: text.into(),
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
            .map(|a| {
                a.into_batch_item()
                    .expect("`batchable` just said every decision here has a batch form")
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
            Decision::TypeChange {
                keep_disk: true,
                copies,
                ..
            } => engine
                .keep_disk_with(&target, &copies)
                .map(BatchResult::Resolved),
            Decision::TypeChange {
                keep_disk: false, ..
            } => engine.keep_source(&target).map(BatchResult::Resolved),
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
                        Route::Dashboard => self
                            .dashboard_snapshot()
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
    use std::rc::Rc;

    use gpui::{TestAppContext, VisualTestContext};
    use tomte_app::model::{SyncModel, TimelineRow};

    use super::*;

    fn model_with_data() -> SyncModel {
        let mut m = SyncModel {
            connected: true,
            ..Default::default()
        };
        m.hydrate_status(
            vec![tomte_proto::DriftSummary {
                type_change: None,
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
            label: None,
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
                label: None,
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
        // The dashboard route on the same model, whose timeline carries the
        // drifted `.zshrc`, so its inline quick actions render. Twice: with
        // nothing owed they stay LIVE (a decision draining on another file no
        // longer makes them a no-op — that flag was defect §1.1), and with
        // THAT file's decision running they render through the disabled path
        // instead, so a second press cannot duplicate it.
        for running_here in [false, true] {
            let (_view, vis) = cx.add_window_view(|_window, cx| {
                let state = cx.new(|_| model_with_data());
                let mut queue = ActionQueue::default();
                if running_here {
                    queue.push(QueuedAction {
                        target: PathBuf::from("/tmp/smoke/.zshrc"),
                        decision: Decision::KeepDisk,
                        notify_on_done: false,
                    });
                    queue.take_batch();
                }
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
                    pending_merge_probed: false,
                    changelog: None,
                    queue,
                    queue_pumping: running_here,
                    deferred_repo_op: None,
                    running_repo_op: None,
                }
            });
            vis.run_until_parked();
        }
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
                    type_change: None,
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
        const STARTS_A_MERGE: [&str; 5] = [
            "action.run(",
            ".resolve_merged(",
            ".keep_disk(",
            ".keep_disk_with(",
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
                    pending_merge_probed: true,
                    changelog: None,
                    queue: Default::default(),
                    queue_pumping: finishing,
                    deferred_repo_op: None,
                    // The in-flight state the banner has to render is now
                    // exactly this — `merge_finish_in_flight` was a second
                    // bool over the same window (Ruling 41).
                    running_repo_op: finishing.then_some(RepoOpKind::Finish),
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

        // The Structure-mode gallery poses build, render, and show what
        // their descriptions promise: Structure mode on both JSON poses, and
        // the Text stage with the not-valid-JSON chip on the third.
        for name in [
            "merge-structured",
            "merge-structured-folded",
            "merge-structured-locked",
            "merge-structured-text",
        ] {
            let (shell, vis) = cx.add_window_view(|_window, cx| {
                fixtures::build(name, smoke_paths(), cx).unwrap_or_else(|| panic!("{name} builds"))
            });
            vis.run_until_parked();
            let merge = vis
                .update(|_w, cx| shell.read(cx).merge.clone())
                .unwrap_or_else(|| panic!("{name} poses a merge"));
            let (mode, locked, chip) = vis.update(|_w, cx| {
                let view = merge.read(cx);
                let loaded = view.loaded.as_ref().expect("loaded");
                let locked = loaded
                    .structured
                    .as_ref()
                    .is_some_and(|s| s.doc.decisions.iter().any(|d| d.locked.is_some()));
                (loaded.mode, locked, view.json_chip(cx))
            });
            assert_eq!(mode, merge::MergeMode::Structure, "{name}");
            assert_eq!(locked, name == "merge-structured-locked", "{name}");
            assert_eq!(chip.is_some(), name == "merge-structured-text", "{name}");
            // MUTATION: the folded pose leaves `fold_identical` off → no
            // `Fold` row in its plan.
            let folds = vis.update(|_w, cx| {
                merge
                    .read(cx)
                    .tree_rows_for_test()
                    .iter()
                    .any(|row| matches!(row, tomte_app::structured_tree::TreeRow::Fold { .. }))
            });
            assert_eq!(folds, name == "merge-structured-folded", "{name}");
        }

        // The disk-equals-source pose opens in Lines, where the note shows.
        // MUTATION: the pose builds from `json_tree_inputs` → Structure.
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("merge-structured-same", smoke_paths(), cx)
                .expect("merge-structured-same builds")
        });
        vis.run_until_parked();
        let merge = vis
            .update(|_w, cx| shell.read(cx).merge.clone())
            .expect("merge-structured-same poses a merge");
        let mode = vis.update(|_w, cx| merge.read(cx).loaded.as_ref().expect("loaded").mode);
        assert_eq!(mode, merge::MergeMode::Lines);

        // The not-structured pose opens the line editor with the note.
        // MUTATION: `not_structured_note` returns None → no note.
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("merge-not-structured", smoke_paths(), cx)
                .expect("merge-not-structured builds")
        });
        vis.run_until_parked();
        let merge = vis
            .update(|_w, cx| shell.read(cx).merge.clone())
            .expect("merge-not-structured poses a merge");
        let (mode, note) = vis.update(|_w, cx| {
            let view = merge.read(cx);
            (view.mode(), view.not_structured_note())
        });
        assert_eq!(mode, merge::MergeMode::Lines);
        assert_eq!(
            note.as_deref(),
            Some("disk is not valid JSON (line 5): line merge")
        );
        assert!(vis.debug_bounds("merge-not-structured").is_some());
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

    /// Every argv handed to one `GitClient`, in order — the shared log a
    /// [`RecordingRunner`] writes into and [`EngineLab::git_invocations`]
    /// reads.
    type GitCallLog = std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>;

    /// A `CommandRunner` that writes down each argv and then runs it for
    /// real through `SystemRunner`.
    ///
    /// Why a recorder and not a count of commits at origin: a `git push`
    /// costs ~1.5 s per INVOCATION whatever it carries, and a push with
    /// nothing new to send leaves no trace at origin at all. So counting
    /// commits cannot see a tail that pushed once per file — which is the
    /// half of "one push" the user is actually waiting on. Only the argv log
    /// can.
    struct RecordingRunner {
        inner: tomte_core::cmd::SystemRunner,
        calls: GitCallLog,
    }

    impl tomte_core::cmd::CommandRunner for RecordingRunner {
        fn run(
            &self,
            req: tomte_core::cmd::CommandRequest,
        ) -> Result<tomte_core::cmd::CommandOutput, tomte_core::cmd::CommandError> {
            self.calls
                .lock()
                .expect("the git call log is not poisoned")
                .push(req.args.clone());
            self.inner.run(req)
        }
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
        /// Kept for journal assertions (the daemon is the single writer, so
        /// asserting means opening this path read-only).
        pub journal_path: PathBuf,
        /// The ENGINE's git calls only. The daemon keeps a plain
        /// `SystemRunner`, so its own scans and probes never land in here to
        /// be mistaken for work a decision did.
        engine_git_calls: GitCallLog,
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
            // The engine's git goes through the recorder so a story can count
            // INVOCATIONS, not just their effects. Behaviour is unchanged:
            // `RecordingRunner` delegates every call to the same
            // `SystemRunner` the daemon above uses.
            let engine_git_calls: GitCallLog = Arc::new(Mutex::new(Vec::new()));
            let engine = Arc::new(ResolveEngine {
                // The SCRATCH clients — mutations must hit the scratch home.
                chezmoi: s.chezmoi(),
                git: GitClient::new(
                    Arc::new(RecordingRunner {
                        inner: SystemRunner,
                        calls: engine_git_calls.clone(),
                    }),
                    s.source.clone(),
                ),
                ipc,
                journal_path: journal_path.clone(),
                daemon_patience: tomte_app::resolve::DAEMON_PATIENCE,
            });
            // One drifted managed file, so a `KeepDisk` on `target()` has
            // real work to do: the scratch source holds `dot_testrc` =
            // "a=1\n" and the destination now says something else.
            std::fs::write(s.home.join(".testrc"), b"a=local\n").expect("drift the target");
            // The published global is the only handle these tests need —
            // they drive the engine through `pump` — and it is what keeps
            // this `Arc` alive for the length of the test.
            cx.set_global(crate::EngineSlot(Some(engine)));
            Self {
                s,
                journal_path,
                engine_git_calls,
            }
        }

        /// Every argv the ENGINE's `GitClient` has run so far, in order.
        ///
        /// Not the daemon's, and not chezmoi's: chezmoi runs its OWN git from
        /// inside `re-add` when `autoCommit`/`autoPush` are on, as a
        /// subprocess of the chezmoi subprocess, and nothing here can see it.
        /// This is the git Tomte itself decided to run — which is exactly the
        /// set whose size Tomte controls.
        pub(crate) fn git_invocations(&self) -> Vec<Vec<String>> {
            self.engine_git_calls
                .lock()
                .expect("the git call log is not poisoned")
                .clone()
        }

        /// How many `git push` invocations the engine has made.
        ///
        /// `GitClient::push` is the only method that pushes and it always
        /// leads with `push` (`git.rs:307` — `["push", "--quiet", remote]`),
        /// so the first argument is the whole test.
        pub(crate) fn engine_pushes(&self) -> usize {
            self.git_invocations()
                .iter()
                .filter(|argv| argv.first().map(String::as_str) == Some("push"))
                .count()
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
            pending_merge_probed: false,
            changelog: None,
            queue: Default::default(),
            queue_pumping: false,
            deferred_repo_op: None,
            running_repo_op: None,
        }
    }

    /// The `ReviewView` a posed Shell built, for tests that drive it
    /// directly — a bulk press, a ⌘/⇧-click — rather than only observing
    /// its render (the way `a_review_decision_queues_and_advances_the_
    /// selection` already does inline). Pulled out so Task 8's key-binding
    /// smoke test can share it. Panics if the pose has no Review entity:
    /// pose a `review*` state first.
    fn review_of(shell: &Entity<Shell>, vis: &mut VisualTestContext) -> Entity<ReviewView> {
        vis.update(|_window, cx| shell.read(cx).review.clone())
            .expect("pose has a Review entity")
    }

    /// The sidebar plan is rebuilt when its inputs change, not when the
    /// window redraws (spec 2026-10-11 §2.3): the same `Rc` comes back
    /// until a gesture or the model changes it.
    ///
    /// MUTATION: return `Rc::new(sidebar_rows(..))` on every call (drop the
    /// cache lookup) → the first `ptr_eq` fails. MUTATION: drop
    /// `view.plan_dirty = true` from the model observer → the last
    /// assertion sees the old plan after the model changed. MUTATION: set
    /// `plan_dirty = true` at the top of `render_inner` (rebuild every frame)
    /// → the forced redraw hands back a new Rc and the first `ptr_eq` fails.
    #[gpui::test]
    fn the_sidebar_plan_is_rebuilt_only_when_its_inputs_change(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-multiselect", smoke_paths(), cx)
                .expect("review-multiselect fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        let first = vis.update(|_w, cx| review.update(cx, |view, cx| view.plan(cx)));
        // Force a real frame: without a notify nothing draws between the reads.
        vis.update(|_w, cx| review.update(cx, |_, cx| cx.notify()));
        vis.run_until_parked();
        let second = vis.update(|_w, cx| review.update(cx, |view, cx| view.plan(cx)));
        assert!(
            Rc::ptr_eq(&first, &second),
            "nothing changed: the cached plan is handed back"
        );

        vis.update(|_w, cx| review.update(cx, |view, cx| view.toggle_mode(cx)));
        let third = vis.update(|_w, cx| review.update(cx, |view, cx| view.plan(cx)));
        assert!(!Rc::ptr_eq(&second, &third), "a gesture is an input");

        let state = vis.update(|_w, cx| shell.read(cx).state.clone());
        vis.update(|_w, cx| {
            state.update(cx, |model, cx| {
                model.drifted.remove(0);
                cx.notify();
            })
        });
        vis.run_until_parked();
        let fourth = vis.update(|_w, cx| review.update(cx, |view, cx| view.plan(cx)));
        assert!(
            !Rc::ptr_eq(&third, &fourth),
            "the model observer marks the plan dirty"
        );
        assert_eq!(fourth.rows.len(), third.rows.len() - 1);
    }

    /// The cross on a waiting row goes `ReviewView::cancel_queued` →
    /// `Shell::cancel_queued` → `ActionQueue::cancel`, and the file comes
    /// back under its group with no cross while the running row is
    /// untouched.
    ///
    /// MUTATION: return from `Shell::cancel_queued` before
    /// `self.queue.cancel` → the depth stays 2 and the waiting row stays
    /// under PROCESSING. MUTATION: make `ReviewView::cancel_queued` a no-op
    /// → the same assertions fail.
    #[gpui::test]
    fn the_x_on_a_waiting_row_removes_its_decision_from_the_queue(cx: &mut TestAppContext) {
        use review::SidebarRow;
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-processing", smoke_paths(), cx)
                .expect("review-processing fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        let waiting = fixtures::home(".gitconfig");
        let running = fixtures::home(".config/zed/settings.json");
        let before = vis.update(|_w, cx| review.update(cx, |view, cx| view.plan(cx)));
        assert!(before.rows.iter().any(|r| matches!(r,
            SidebarRow::Target { entry, cancellable: true, .. } if entry.target == waiting)));
        vis.update(|_w, cx| review.update(cx, |view, cx| view.cancel_queued(&waiting, cx)));
        vis.run_until_parked();
        vis.update(|_w, cx| {
            let s = shell.read(cx);
            assert_eq!(s.queue.depth(), 1);
            assert_eq!(s.queue.running().len(), 1);
            assert_eq!(s.queue.running()[0].target, running);
            assert!(s.queue.failure(&waiting).is_none());
        });
        let after = vis.update(|_w, cx| review.update(cx, |view, cx| view.plan(cx)));
        let group_of = |rows: &[SidebarRow], target: &Path| -> &'static str {
            let mut current = "";
            for r in rows {
                match r {
                    SidebarRow::Group { label, .. } => current = label,
                    SidebarRow::Target { entry, .. } if entry.target == target => return current,
                    _ => {}
                }
            }
            "absent"
        };
        assert_eq!(group_of(&after.rows, &waiting), "SAFE TO RESOLVE");
        assert_eq!(group_of(&after.rows, &running), "PROCESSING");
    }

    /// The ✕ really renders on the waiting row ONLY, and its click takes
    /// the decision back without selecting the row.
    ///
    /// MUTATION: in `target_row`, `if queued.is_some()` instead of
    /// `if cancellable` → the running row also gets a ✕ and the `is_none`
    /// assertion fails. MUTATION: drop the `cx.stop_propagation()` from the
    /// ✕ listener → the row's own click fires and `selected` moves to
    /// `.gitconfig`.
    #[gpui::test]
    fn a_real_click_on_the_x_removes_the_queued_decision_without_selecting_the_row(
        cx: &mut TestAppContext,
    ) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-processing", smoke_paths(), cx)
                .expect("review-processing fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        let waiting = fixtures::home(".gitconfig");
        let running = fixtures::home(".config/zed/settings.json");
        let selected_before = vis.update(|_w, cx| review.read(cx).selected.clone());
        assert_eq!(
            selected_before,
            Some(running.clone()),
            "the pose selects the running file"
        );
        // `debug_bounds` wants a `&'static str`; leaking two short strings in
        // a test is the price of keying the selector by file.
        let running_x: &'static str =
            Box::leak(format!("review-cancel:{}", running.display()).into_boxed_str());
        let waiting_x: &'static str =
            Box::leak(format!("review-cancel:{}", waiting.display()).into_boxed_str());
        assert!(
            vis.debug_bounds(running_x).is_none(),
            "a running row has no ✕"
        );
        let x = vis
            .debug_bounds(waiting_x)
            .expect("the waiting row renders its ✕");
        vis.simulate_click(x.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        vis.update(|_w, cx| {
            let s = shell.read(cx);
            assert_eq!(s.queue.depth(), 1, "the waiting decision is gone");
            assert_eq!(s.queue.running()[0].target, running);
            assert!(s.queue.failure(&waiting).is_none());
        });
        assert_eq!(
            vis.update(|_w, cx| review.read(cx).selected.clone()),
            selected_before,
            "the ✕ did not select its row"
        );
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
    /// `running_repo_op` `None`.
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
                assert_eq!(
                    s.running_repo_op, None,
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
            assert_eq!(
                s.running_repo_op,
                Some(RepoOpKind::Finish),
                "and it is what started"
            );
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
            assert_eq!(s.running_repo_op, None);
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

    /// The roll-back reports itself, and names what the CONFIRMATION named.
    ///
    /// Nothing is queued behind it here, deliberately: this test is about the
    /// roll-back's own banner surviving to be read. Its sibling below covers
    /// the case where a decision lands after it and legitimately replaces it.
    ///
    /// It also pins `run_abort`'s USE of [`abort_outcome_of`], which the unit
    /// test of that seam cannot: the `Drift` tint and "2 source file(s)"
    /// belong to the `RolledBack` arm alone, so a completion that stopped
    /// delegating and hardcoded an arm lands here.
    ///
    /// MUTATION: drop the `self.route = Route::Review` from
    /// `abort_origin_merge` → the first assertion fails. MUTATION: hand
    /// `abort_banner` `rolled_back` from anywhere but the confirmation → the
    /// "2 source file(s)" assertion fails. MUTATION: have `run_abort` build
    /// an `AbortOutcome` itself instead of calling `abort_outcome_of` → the
    /// tint assertion fails.
    #[gpui::test]
    fn an_abort_reports_the_roll_back_it_confirmed(cx: &mut TestAppContext) {
        let _lab = EngineLab::publish(cx);
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Merge, cx));

        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                // Exactly what `MergeView::discard_merge` hands over.
                s.abort_origin_merge(merge::Rollback::Files(2), cx);
                assert_eq!(s.route, Route::Review, "the editor is left immediately");
            });
        });
        vis.run_until_parked();
        let review = vis
            .update(|_window, cx| shell.read(cx).review.clone())
            .expect("the abort's banner made a review view");
        let banner = vis
            .update(|_window, cx| review.read(cx).last_outcome.clone())
            .expect("the rollback is reported, never silent");
        // Not just the format string's fixed prefix: the banner has to carry
        // the size the CONFIRMATION named, and the roll-back tint rather
        // than the panic arm's.
        assert!(
            banner.text.contains("2 source file(s)"),
            "the banner names what the confirmation named: {}",
            banner.text
        );
        assert_eq!(banner.tint, review::BannerTint::Drift);
        assert!(
            !banner.text.contains("dropped"),
            "nothing was queued, so nothing was dropped with it: {}",
            banner.text
        );
    }

    /// The abort is the case the ordering exists for: the user leaves the
    /// editor NOW, but the rollback waits for the latch, and the decisions
    /// queued behind it run against the ROLLED-BACK repo instead of being
    /// staged into a merge that has just been discarded.
    ///
    /// The final banner is the DECISION's, not the roll-back's, and that is
    /// correct: it landed after, and it is the one Undo would revert. The
    /// roll-back's own banner is covered by the test above, with nothing
    /// queued behind it.
    ///
    /// MUTATION: in `pump`, take the batch before checking
    /// `deferred_repo_op` → the probe below sees `running()` non-empty.
    /// MUTATION: stop banner-ing landed one-click decisions → the last
    /// assertion fails and the banner still names the roll-back while Undo
    /// would revert the keep-disk.
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
                s.abort_origin_merge(merge::Rollback::Files(2), cx);
                assert_eq!(s.route, Route::Review, "the editor is left immediately");
                assert!(matches!(
                    s.deferred_repo_op,
                    Some(PendingRepoOp::AbortMerge { .. })
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
        let banner = vis
            .update(|_window, cx| review.read(cx).last_outcome.clone())
            .expect("the landing is reported, never silent");
        assert_eq!(
            banner.text.as_ref(),
            "Kept disk version · .testrc · committed & pushed",
            "the LAST thing to land owns the banner, and NAMES the file it \
             reports — it is what Undo reverts"
        );
        assert!(banner.undoable);
    }

    /// Pristine content of the extra managed files a sweep story adds.
    const SWEEP_BASE: &[u8] = b"x=1\n";

    /// Turn on the chezmoi git automation the way the real user's is.
    ///
    /// `Scratch::config_path` writes an EMPTY `chezmoi.toml`, so without this
    /// a fixture is blind to what chezmoi does on a machine that has
    /// `autoCommit`/`autoPush` on: it runs its OWN git from inside `re-add`,
    /// committing and pushing without asking Tomte. That blindness is how the
    /// 0.1.15 field bug shipped green — and here it would make the push
    /// counts below measure nothing at all.
    ///
    /// Copied from `tests/resolve_e2e.rs` rather than shared with it: views
    /// are bin-only (`lib.rs`'s header), so an integration test cannot reach
    /// `Shell` and nothing can live in one place for both.
    fn chezmoi_commits_and_pushes_for_itself(s: &tomte_core::testsupport::Scratch) {
        std::fs::write(
            s.config_path(),
            "[git]\nautoCommit = true\nautoPush = true\n",
        )
        .expect("the scratch chezmoi config is writable");
    }

    /// Extra managed files beside `.testrc`: `dot_<name>` = [`SWEEP_BASE`],
    /// committed, pushed and applied — so `~/.<name>` exists and is in sync,
    /// ready to be drifted.
    ///
    /// Applied in ONE pass, and BEFORE any drift is seeded: `apply(None)` is
    /// repo-wide and `--force`, so applying between drifts would wash the
    /// earlier ones straight back out — including the `.testrc` drift
    /// [`EngineLab::publish`] leaves behind, which is why both stories below
    /// re-drift every file after calling this.
    fn more_managed_files(lab: &EngineLab, names: &[&str]) {
        use tomte_core::testsupport::git;

        for name in names {
            std::fs::write(lab.s.source.join(format!("dot_{name}")), SWEEP_BASE)
                .expect("the scratch source is writable");
        }
        git(&lab.s.source, &["add", "."]);
        git(&lab.s.source, &["commit", "-m", "more managed files"]);
        git(&lab.s.source, &["push"]);
        lab.s
            .chezmoi()
            .apply(None)
            .expect("apply the newly managed files");
    }

    /// Commits on the bare origin's `main` — how many pushes the fixture has
    /// actually seen.
    ///
    /// A plain subprocess, the way `resolve_e2e.rs`'s `commit_count` does it:
    /// counting commits is an assertion about the fixture, not a capability
    /// the production `GitClient` needs.
    fn origin_commit_count(lab: &EngineLab) -> usize {
        let out = std::process::Command::new("git")
            .args(["rev-list", "--count", "main"])
            .current_dir(&lab.s.bare)
            .output()
            .expect("git rev-list runs");
        assert!(
            out.status.success(),
            "rev-list in {}: {}",
            lab.s.bare.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("rev-list --count prints a number")
    }

    /// Is the source repo sitting in a merge nobody concluded?
    ///
    /// The shared batch tail stages the whole index, so a sweep that left a
    /// merge open would be a sweep that could commit conflicts the user never
    /// answered. `resolve_e2e.rs`'s batch stories assert the same thing the
    /// same way.
    fn mid_merge(lab: &EngineLab) -> bool {
        use std::sync::Arc;
        use tomte_core::cmd::SystemRunner;
        use tomte_core::git::GitClient;

        GitClient::new(Arc::new(SystemRunner), lab.s.source.clone()).merge_in_progress()
    }

    /// The journal's FINISHED sessions, as `(id, summary)`, newest first.
    ///
    /// `Journal::last_finished_session` — what `resolve_e2e.rs` reaches for —
    /// reports only the newest, and its batch story counts sessions from the
    /// `BatchOutcome` instead. Through the Shell there is no `BatchOutcome`
    /// to read: `batch_finished` receives targets and messages, no session
    /// ids. So this counts the daemon's own record instead. The server emits
    /// exactly one `session_end` event per `end_session` that SUCCEEDED, with
    /// the id and summary in its `meta`, which is the same "finished" the
    /// undo path means.
    fn finished_sessions(lab: &EngineLab) -> Vec<(i64, String)> {
        use tomte_journal::Journal;

        let journal = Journal::open_read_only(&lab.journal_path, "assert")
            .expect("the daemon's journal opens read-only");
        journal
            .timeline(200, None)
            .expect("the timeline reads")
            .into_iter()
            .filter(|row| row.kind == "session_end")
            .map(|row| {
                let meta = row.meta.expect("a session_end event carries its meta");
                (
                    meta["session"].as_i64().expect("meta.session is an id"),
                    meta["summary"]
                        .as_str()
                        .expect("meta.summary is a string")
                        .to_string(),
                )
            })
            .collect()
    }

    /// The three files a sweep story decides, as
    /// `(destination, source, content the user left on disk)`.
    ///
    /// `.testrc` is the lab's own managed file; `.a` and `.b` come from
    /// [`more_managed_files`]. Distinct bodies per file so an assertion
    /// cannot pass on the wrong one's content.
    fn sweep_targets(lab: &EngineLab) -> [(PathBuf, PathBuf, &'static [u8]); 3] {
        [
            (lab.target(), lab.source_file(), b"a=from-the-disk\n"),
            (
                lab.s.home.join(".a"),
                lab.s.source.join("dot_a"),
                b"a=disk\n",
            ),
            (
                lab.s.home.join(".b"),
                lab.s.source.join("dot_b"),
                b"b=disk\n",
            ),
        ]
    }

    /// Three managed files, all drifted on disk, with chezmoi's own git
    /// automation on. Returns what [`sweep_targets`] describes.
    fn a_sweep_of_three_drifted_files(lab: &EngineLab) -> [(PathBuf, PathBuf, &'static [u8]); 3] {
        chezmoi_commits_and_pushes_for_itself(&lab.s);
        more_managed_files(lab, &["a", "b"]);
        let files = sweep_targets(lab);
        // Every drift AFTER the apply inside `more_managed_files`, which is
        // repo-wide and `--force`.
        for (target, _, body) in &files {
            std::fs::write(target, body).expect("drift the target");
        }
        files
    }

    /// SPEC §2.2, the sweep the spec is about: three one-click decisions that
    /// were all made before the drain started cost ONE re-add, ONE commit and
    /// ONE push, and still leave three separate Undos.
    ///
    /// Driven through the queue and `pump` rather than
    /// `ResolveEngine::resolve_batch` (`resolve_e2e.rs`'s batch stories cover
    /// that): only the Shell's own path knows where batch boundaries fall,
    /// and where they fall is the whole claim.
    ///
    /// `push` then ONE `pump`, not three `enqueue`s, and that is not a
    /// shortcut around the real entry point — it is the state `enqueue`
    /// reaches from its SECOND click onwards, because `enqueue` pumps
    /// synchronously. Its sibling below drives `enqueue` itself and measures
    /// what the real sequence costs.
    ///
    /// MUTATION: make `take_batch` hand out one item at a time → the
    /// `running().len() == 3` assertion fails, and the commit count comes
    /// back `before + 3`. MUTATION: open one shared journal session for a
    /// batch instead of one per file → `finished_sessions` finds one, not
    /// three. MUTATION: have `resolve_batch` call `commit_batch` once per
    /// target (`resolve.rs`'s phase 2) → the commit count stays `before + 1`
    /// and only the push-INVOCATION assertion fails, which is the finding
    /// that assertion exists for.
    #[gpui::test]
    fn three_decisions_queued_before_the_drain_are_one_push(cx: &mut TestAppContext) {
        let lab = EngineLab::publish(cx);
        let files = a_sweep_of_three_drifted_files(&lab);
        let before = origin_commit_count(&lab);

        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Review, cx));
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                for (target, _, _) in &files {
                    s.queue.push(QueuedAction {
                        target: target.clone(),
                        decision: Decision::KeepDisk,
                        notify_on_done: false,
                    });
                }
                s.pump(cx);
                assert_eq!(
                    s.queue.running().len(),
                    3,
                    "one batch took all three, not one of them"
                );
            });
        });
        vis.run_until_parked();
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert_eq!(s.queue.depth(), 0, "the sweep drained");
            assert!(!s.queue_pumping, "and the latch is free again");
            for (target, _, _) in &files {
                assert!(
                    s.queue.failure(target).is_none(),
                    "{}: {:?}",
                    target.display(),
                    s.queue.failure(target)
                );
            }
        });

        // Filesystem truth, not just an empty queue: each source carries what
        // its own destination held.
        for (target, source, body) in &files {
            assert_eq!(
                std::fs::read(source).expect("source file readable"),
                *body,
                "{} must carry the disk content the sweep chose",
                target.display()
            );
        }
        assert_eq!(
            origin_commit_count(&lab),
            before + 1,
            "three decisions, ONE commit reaching origin"
        );
        // And ONE push INVOCATION, which is the half of the claim the commit
        // count above cannot see. With chezmoi's `autoCommit` on it is
        // chezmoi's own push that carries the commit, so Tomte's tail pushes
        // a repo with nothing new — free at origin, ~1.5 s on the wire, and
        // invisible to `origin_commit_count`. A tail that ran once per file
        // would therefore cost three of those and still add one commit.
        assert_eq!(
            lab.engine_pushes(),
            1,
            "three decisions, ONE `git push` invocation: {:?}",
            lab.git_invocations()
        );
        assert!(
            !mid_merge(&lab),
            "a sweep must not leave a merge for the next one to commit blind"
        );

        // Undo stays per file (§2.3): three DISTINCT finished sessions, one
        // naming each file — never one session the batch shared, which would
        // make Undo revert the whole sweep.
        let sessions = finished_sessions(&lab);
        let ids: std::collections::BTreeSet<i64> = sessions.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids.len(),
            3,
            "three files, three sessions — undo is per file: {sessions:?}"
        );
        for name in [".testrc", ".a", ".b"] {
            assert!(
                sessions
                    .iter()
                    .any(|(_, summary)| summary == &format!("keep_disk {name}")),
                "no finished session for {name}: {sessions:?}"
            );
        }
    }

    /// A bulk press is ONE batch: `enqueue_many` pushes every decision and
    /// pumps once, so three files are one re-add, one commit, one push.
    /// `enqueue` per file would pump on the first push and drain it alone
    /// (queue spec §2.2's "first click drains alone") — and indeed `enqueue`
    /// is defined as `enqueue_many(vec![action], cx)`, so a mutation cannot
    /// swap one for the other without recursing; the mutation below instead
    /// breaks `enqueue_many`'s OWN one-batch promise.
    /// MUTATION: move the push inside the loop so `enqueue_many` pushes and
    /// pumps PER ITEM (`self.queue.push(action); cx.notify(); self.pump(cx);`
    /// inside the `for`, instead of pushing all three first) → the first
    /// push drains alone, so `running()` is 1 (not 3) right after the call,
    /// and the other two land in a second batch behind it: `engine_pushes()`
    /// is 2, not the ONE push this test is named for.
    #[gpui::test]
    fn a_bulk_press_is_one_batch_and_one_push(cx: &mut TestAppContext) {
        let lab = EngineLab::publish(cx);
        let files = a_sweep_of_three_drifted_files(&lab);
        let before = origin_commit_count(&lab);

        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Review, cx));
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                let actions = files
                    .iter()
                    .map(|(target, _, _)| QueuedAction {
                        target: target.clone(),
                        decision: Decision::KeepDisk,
                        notify_on_done: false,
                    })
                    .collect();
                s.enqueue_many(actions, cx);
                assert_eq!(s.queue.running().len(), 3, "one batch took all three");
            });
        });
        vis.run_until_parked();
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert_eq!(s.queue.depth(), 0);
            for (target, _, _) in &files {
                assert!(
                    s.queue.failure(target).is_none(),
                    "{:?}",
                    s.queue.failure(target)
                );
            }
        });
        assert_eq!(
            origin_commit_count(&lab),
            before + 1,
            "ONE commit reaching origin"
        );
        assert_eq!(
            lab.engine_pushes(),
            1,
            "ONE push invocation: {:?}",
            lab.git_invocations()
        );
    }

    /// The same sweep through the button the user actually presses — and the
    /// honest count that comes out of it.
    ///
    /// `enqueue` pumps synchronously, so the FIRST click of a sweep always
    /// drains ALONE. The clicks made while it runs join one batch behind it:
    /// three clicks, TWO pushes, never three. This is why the changelog says
    /// "decisions made while one is running share a single re-add, commit and
    /// push" and does not promise one push per sweep.
    ///
    /// The second batch is taken from inside `batch_finished` (`pump` at its
    /// end), which runs in a foreground update spawned by the first batch's
    /// completion — so ONE `run_until_parked` drains both, exactly as
    /// `a_finish_pressed_mid_drain_runs_before_the_next_batch` relies on.
    ///
    /// MUTATION: drop the `self.pump(cx)` from `enqueue` → the first
    /// assertion fails (nothing started on the first click). MUTATION: make
    /// `take_batch` hand out one item at a time → three commits and three
    /// push invocations where two of each were owed.
    #[gpui::test]
    fn clicks_during_a_drain_join_the_next_batch(cx: &mut TestAppContext) {
        use std::path::Path;

        let lab = EngineLab::publish(cx);
        let files = a_sweep_of_three_drifted_files(&lab);
        let before = origin_commit_count(&lab);

        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Review, cx));
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                fn click(target: &Path, s: &mut Shell, cx: &mut Context<Shell>) {
                    s.enqueue(
                        QueuedAction {
                            target: target.to_path_buf(),
                            decision: Decision::KeepDisk,
                            notify_on_done: false,
                        },
                        cx,
                    );
                }
                click(&files[0].0, s, cx);
                assert_eq!(
                    s.queue
                        .running()
                        .iter()
                        .map(|a| a.target.clone())
                        .collect::<Vec<_>>(),
                    vec![files[0].0.clone()],
                    "the first click drains alone — `enqueue` pumps"
                );
                click(&files[1].0, s, cx);
                click(&files[2].0, s, cx);
                assert_eq!(s.queue.depth(), 3, "nothing was discarded");
                assert_eq!(
                    s.queue.running().len(),
                    1,
                    "the other two wait for the first click's batch"
                );
            });
        });
        vis.run_until_parked();
        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert_eq!(s.queue.depth(), 0, "both batches drained");
            assert!(!s.queue_pumping);
            for (target, _, _) in &files {
                assert!(
                    s.queue.failure(target).is_none(),
                    "{}: {:?}",
                    target.display(),
                    s.queue.failure(target)
                );
            }
        });

        for (target, source, body) in &files {
            assert_eq!(
                std::fs::read(source).expect("source file readable"),
                *body,
                "{} must carry the disk content the sweep chose",
                target.display()
            );
        }
        assert_eq!(
            origin_commit_count(&lab),
            before + 2,
            "one commit for the first click, one for the two behind it"
        );
        assert_eq!(
            lab.engine_pushes(),
            2,
            "two batches, TWO `git push` invocations — not one per file: {:?}",
            lab.git_invocations()
        );
        assert!(!mid_merge(&lab));
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
            assert_eq!(
                s.running_repo_op, None,
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
                    matches!(s.deferred_repo_op, Some(PendingRepoOp::AbortMerge { .. })),
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
                    matches!(s.deferred_repo_op, Some(PendingRepoOp::AbortMerge { .. })),
                    "an abort supersedes a pending finish — it is the later, confirmed answer"
                );
            });
        });
    }

    /// PENDING **OR** RUNNING. `a_finish_never_displaces_a_confirmed_abort`
    /// covers the abort that is still waiting; this covers the one that is
    /// halfway through rewriting the source tree, where `deferred_repo_op` is
    /// `None` and only `running_repo_op` knows.
    ///
    /// Without the running half the click is not dropped but DEFERRED: it
    /// sits in the slot, `pump` takes it the instant the abort's completion
    /// releases the latch, and it runs against whatever the abort left. That
    /// was harmless only by accident — `finish_merge_if_resolved` finds no
    /// merge after a roll-back that WORKED — and Task 6 removes the accident:
    /// an abort can now honestly fail with the merge still open, and this
    /// press would then commit the very merge the user confirmed discarding.
    ///
    /// MUTATION: drop the `running_repo_op == Some(Abort)` half of the guard
    /// in `request_repo_op` → `deferred_repo_op` becomes `Some(FinishMerge)`
    /// and the first assertion fails.
    #[gpui::test]
    fn a_finish_never_displaces_an_abort_that_is_already_running(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Dashboard, cx));

        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                // The state `pump` leaves behind while `run_abort`'s
                // background work is in flight: latch held, slot empty, the
                // running kind the only witness.
                s.queue_pumping = true;
                s.running_repo_op = Some(RepoOpKind::Abort);
                assert!(s.deferred_repo_op.is_none(), "nothing is merely waiting");

                s.finish_pending_merge(cx);

                assert!(
                    s.deferred_repo_op.is_none(),
                    "the click is DROPPED, not parked to run the moment the roll-back \
                     lets go of the latch: {:?}",
                    s.deferred_repo_op
                );
            });
        });
        // And dropped out loud, exactly as the pending case is.
        let review = vis
            .update(|_window, cx| shell.read(cx).review.clone())
            .expect("the explanation made a review view");
        let banner = vis
            .update(|_window, cx| review.read(cx).last_outcome.clone())
            .expect("an ignored click is reported, never silently swallowed");
        assert!(
            banner.text.contains("Finish ignored") && banner.text.contains("Nothing was committed"),
            "the banner says why the click did nothing: {}",
            banner.text
        );
    }

    /// The type-change pane rides a gpui `list`: rendering the posed folder
    /// fills the list with the planned items, one list item each.
    ///
    /// MUTATION: drop `self.sync_compare_list(cx)` from `render_inner` →
    /// the list holds no items and the keys stay empty.
    #[gpui::test]
    fn the_type_change_pane_fills_its_list_from_the_plan(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-type-folder-link", smoke_paths(), cx)
                .expect("type-change fixture")
        });
        vis.run_until_parked();
        let review = vis
            .update(|_window, cx| shell.read(cx).review.clone())
            .expect("the fixture poses a review view");
        vis.update(|_window, cx| {
            let view = review.read(cx);
            // Three labels, WARP.md, two name runs, the footer.
            assert_eq!(view.compare_keys.len(), 7);
            assert_eq!(view.compare_list.item_count(), 7);
            assert_eq!(view.compare_keys.last(), Some(&review::CompareItem::Footer));
        });
    }

    /// A press on a type-change row queues under the words on its button,
    /// carrying the picks, and the picks are gone from the view once the
    /// decision has them. The model holds the one row, so the selection has
    /// nowhere to advance to and stays on it.
    ///
    /// MUTATION: drop `self.copy_picks.clear()` from `run_action` → WARP.md
    /// is still ticked after the press. MUTATION: pass `None` for the type
    /// change in `run_action`'s match → the queue says "Keep disk".
    #[gpui::test]
    fn a_type_change_press_queues_its_button_words_and_clears_the_picks(cx: &mut TestAppContext) {
        use review::ResolveAction;

        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-type-folder-link-picked", smoke_paths(), cx)
                .expect("type-change fixture")
        });
        let review = vis
            .update(|_window, cx| shell.read(cx).review.clone())
            .expect("the fixture poses a review view");
        let target = vis
            .update(|_window, cx| review.read(cx).selected.clone())
            .expect("the fixture selects the folder");
        vis.update(|_window, cx| {
            let state = shell.read(cx).state.clone();
            state.update(cx, |m, _| {
                let only: Vec<_> = m
                    .drifted
                    .iter()
                    .filter(|d| d.target == target)
                    .cloned()
                    .collect();
                m.hydrate_status(only, m.in_sync, None, false, None);
            });
            assert!(!review.read(cx).copy_picks.is_empty(), "posed with a pick");
        });

        vis.update(|_window, cx| {
            review.update(cx, |view, cx| view.run_action(ResolveAction::KeepDisk, cx))
        });

        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert_eq!(s.queue.decision_label(&target), Some("Keep the link"));
            let view = review.read(cx);
            assert_eq!(view.selected.as_ref(), Some(&target));
            assert!(view.copy_picks.is_empty(), "the decision took the picks");
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

    /// A bulk press clears the selection and advances `selected` past the
    /// last member (spec §2.4) — the VIEW half of the queue's headline
    /// behaviour. Task 4's `a_bulk_press_is_one_batch_and_one_push` drives
    /// `Shell::enqueue_many` directly and never touches `ReviewView`, so
    /// this is the only coverage of `run_bulk` itself.
    ///
    /// The `review-multiselect` fixture pre-selects `.gitconfig`,
    /// `starship.toml` and `.zshrc` — the last three of the five SAFE rows
    /// in display order (`settings.json`, `.gitconfig`, `starship.toml`,
    /// `.zshrc`, `init.lua`). This test UNTICKS `starship.toml` first, so
    /// the pressed set is `.gitconfig` and `.zshrc` with a GAP between
    /// them — the shape that actually distinguishes "advance from the last
    /// member" from "advance from the first member" (a contiguous
    /// selection does not: `next_after` re-filters every candidate against
    /// the real queue regardless of which already-processing member is
    /// handed to it as `resolved`, so walking forward from either end of a
    /// contiguous block skips the same processing rows and lands on the
    /// same next candidate — this is why the fixture's original, untouched
    /// three-in-a-row selection could not tell the two apart). With the
    /// gap, `starship.toml` itself is never queued and stays answerable:
    /// advancing from the LAST member (`.zshrc`) walks past it to
    /// `init.lua`; advancing from the FIRST member (`.gitconfig`) stops at
    /// `starship.toml`, the untouched row sitting right after it.
    ///
    /// `EngineLab::publish` is what makes `running().len()` mean anything:
    /// with no engine, `pump` PARKS and never calls `take_batch`, so
    /// nothing is ever "running" (see `pump`'s own comment). This never
    /// calls `run_until_parked`, so the spawned batch is never driven past
    /// that synchronous point and the fixture's symbolic paths are never
    /// actually handed to chezmoi/git.
    ///
    /// MUTATION: drop `self.selection.clear()` from `run_bulk` → the
    /// `selection.is_empty()` assertion fails. MUTATION: advance from
    /// `members.first()` instead of the last (visible) member → `selected`
    /// becomes `starship.toml` instead of `init.lua`.
    ///
    /// All assertions after the press run INSIDE the same `review.update`
    /// as `run_bulk` itself, not through a later, separate read: a render
    /// between the call and the read would prune the selection down to
    /// empty on its own (both pressed members just became PROCESSING, so
    /// neither is in the next plan's `safe_order` either way), silently
    /// satisfying `selection.is_empty()` even with the mutation applied
    /// and making the assertion prove nothing.
    #[gpui::test]
    fn a_bulk_press_clears_the_selection_and_advances_past_the_last_member(
        cx: &mut TestAppContext,
    ) {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Users/gallery"));
        let safe_order = vec![
            home.join(".config/zed/settings.json"),
            home.join(".gitconfig"),
            home.join(".config/starship.toml"),
            home.join(".zshrc"),
            home.join(".config/nvim/init.lua"),
        ];
        let _lab = EngineLab::publish(cx);
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-multiselect", smoke_paths(), cx)
                .expect("review-multiselect fixture")
        });
        let review = review_of(&shell, vis);

        vis.update(|_window, cx| {
            review.update(cx, |view, cx| {
                // Untick starship.toml: members become .gitconfig, .zshrc —
                // a gap, not the fixture's original contiguous block.
                view.selection
                    .toggle(&home.join(".config/starship.toml"), &safe_order);
                assert_eq!(
                    view.selection.members().to_vec(),
                    vec![home.join(".gitconfig"), home.join(".zshrc"),],
                    "starship.toml is unticked, leaving a gap between the two remaining members"
                );

                view.run_bulk(review::ResolveAction::KeepDisk, cx);
                assert!(
                    view.selection.is_empty(),
                    "the selection cleared after the press"
                );
                assert_eq!(
                    view.selected,
                    Some(home.join(".config/nvim/init.lua")),
                    "advanced past the last (visible) member, past the untouched \
                     starship.toml in the gap, to the next row still needing a decision"
                );
            });
            assert_eq!(
                shell.read(cx).queue.running().len(),
                2,
                "one batch took both pressed members — starship.toml was never selected"
            );
        });
    }

    /// The ⌘/⇧-click gesture switch (spec §2.3), dispatched through
    /// `row_click` directly rather than a synthetic mouse event. Four
    /// gestures in sequence, on the `review-multiselect` pose (`selected`
    /// starts on `settings.json`, `selection` starts with `.gitconfig`,
    /// `starship.toml`, `.zshrc`):
    ///
    /// 1. A plain click on `init.lua` selects it and clears the members.
    /// 2. A ⌘-click on `settings.json` (now unselected) toggles it in, sets
    ///    the anchor, and — the rule with no coverage before this — never
    ///    moves `selected`.
    /// 3. A ⇧-click on `.zshrc` extends the range from that anchor
    ///    (`settings.json`) to `.zshrc`, covering `.gitconfig` and
    ///    `starship.toml` along the way; `selected` still does not move.
    /// 4. A ⌘-click on `env.nu` (a NEEDS A DECISION row, `selectable =
    ///    false`) changes nothing at all — not the members, not the anchor,
    ///    not `selected` — instead of falling through to the plain-click arm
    ///    and wiping out the selection steps 2–3 just built (final review,
    ///    Minor 3).
    ///
    /// MUTATION: swap the ⌘ and ⇧ arms → step 2's membership assertion
    /// fails (a ⇧-style range lands instead of a single toggle). MUTATION:
    /// the plain-click arm no longer clears → step 1's
    /// `selection.is_empty()` assertion fails (the fixture's original three
    /// members are still there). MUTATION: drop the `!selectable &&
    /// (mods.platform || mods.shift)` early return in `row_click` → step 4's
    /// membership assertion fails (the ⌘-click on env.nu clears the
    /// selection instead of leaving it alone).
    #[gpui::test]
    fn row_click_dispatches_plain_platform_and_shift_gestures(cx: &mut TestAppContext) {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Users/gallery"));
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-multiselect", smoke_paths(), cx)
                .expect("review-multiselect fixture")
        });
        let review = review_of(&shell, vis);

        // 1. Plain click.
        vis.update(|_window, cx| {
            review.update(cx, |view, cx| {
                view.row_click(
                    &home.join(".config/nvim/init.lua"),
                    gpui::Modifiers::default(),
                    true,
                    cx,
                )
            });
        });
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selected.clone()),
            Some(home.join(".config/nvim/init.lua")),
            "a plain click selects the row"
        );
        assert!(
            vis.update(|_window, cx| review.read(cx).selection.is_empty()),
            "a plain click clears the members"
        );

        // 2. ⌘-click.
        let platform = gpui::Modifiers {
            platform: true,
            ..Default::default()
        };
        vis.update(|_window, cx| {
            review.update(cx, |view, cx| {
                view.row_click(&home.join(".config/zed/settings.json"), platform, true, cx)
            });
        });
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selection.members().to_vec()),
            vec![home.join(".config/zed/settings.json")],
            "⌘-click toggles the row into the selection"
        );
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selection.anchor.clone()),
            Some(home.join(".config/zed/settings.json")),
            "⌘-click sets the anchor"
        );
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selected.clone()),
            Some(home.join(".config/nvim/init.lua")),
            "⌘-click never moves `selected`"
        );

        // 3. ⇧-click.
        let shift = gpui::Modifiers {
            shift: true,
            ..Default::default()
        };
        vis.update(|_window, cx| {
            review.update(cx, |view, cx| {
                view.row_click(&home.join(".zshrc"), shift, true, cx)
            });
        });
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selection.members().to_vec()),
            vec![
                home.join(".config/zed/settings.json"),
                home.join(".gitconfig"),
                home.join(".config/starship.toml"),
                home.join(".zshrc"),
            ],
            "⇧-click extends the range from the anchor (settings.json) to the target (.zshrc)"
        );
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selection.anchor.clone()),
            Some(home.join(".config/zed/settings.json")),
            "the anchor stays put across a ⇧-click"
        );
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selected.clone()),
            Some(home.join(".config/nvim/init.lua")),
            "⇧-click never moves `selected`"
        );

        // 4. ⌘-click on a NEEDS A DECISION row (not selectable).
        vis.update(|_window, cx| {
            review.update(cx, |view, cx| {
                view.row_click(&home.join(".config/nushell/env.nu"), platform, false, cx)
            });
        });
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selection.members().to_vec()),
            vec![
                home.join(".config/zed/settings.json"),
                home.join(".gitconfig"),
                home.join(".config/starship.toml"),
                home.join(".zshrc"),
            ],
            "⌘-click on env.nu is ignored: the selection from steps 2–3 is untouched"
        );
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selection.anchor.clone()),
            Some(home.join(".config/zed/settings.json")),
            "the anchor is untouched too"
        );
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selected.clone()),
            Some(home.join(".config/nvim/init.lua")),
            "and `selected` still does not move"
        );
    }

    /// Spec §2.2's whole guarantee — "nothing is ever selected that the plan
    /// does not list as safe" — comes down to one call in `render_inner`:
    /// `self.selection.prune(&plan.safe_order)`. `Selection::prune` itself is
    /// well unit-tested, but nothing before this pinned the CALL: every other
    /// test either asserts inside the same `review.update` as the mutating
    /// call (deliberately, so a render in between cannot prune first) or
    /// asserts a count pruning would not change anyway.
    ///
    /// Pose `review-multiselect` (members `.gitconfig`, `starship.toml`,
    /// `.zshrc`), then reclassify `.gitconfig` as a conflict — the shape a
    /// rescan produces when a file a decision touched turns out to need a
    /// human after all: it stays drifted, just no longer safe. A render then
    /// has to prune it out of the LIVE selection on its own, with no gesture
    /// at all; the other two members keep their identity and order.
    ///
    /// MUTATION: remove `self.selection.prune(&plan.safe_order)` from
    /// `render_inner` → `.gitconfig` stays a member after the render.
    #[gpui::test]
    fn a_render_prunes_a_member_that_stopped_being_safe(cx: &mut TestAppContext) {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Users/gallery"));
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-multiselect", smoke_paths(), cx)
                .expect("review-multiselect fixture")
        });
        let review = review_of(&shell, vis);
        let state = vis.update(|_window, cx| shell.read(cx).state.clone());

        vis.update(|_window, cx| {
            review.update(cx, |view, _cx| {
                assert_eq!(
                    view.selection.members().to_vec(),
                    vec![
                        home.join(".gitconfig"),
                        home.join(".config/starship.toml"),
                        home.join(".zshrc"),
                    ],
                    "the pose starts with these three ticked"
                );
            });
        });

        // .gitconfig turns into a conflict — still drifted, no longer safe —
        // the same shape a rescan produces after a decision landed elsewhere.
        vis.update(|_window, cx| {
            state.update(cx, |model, cx| {
                let mut drifted = model.drifted.clone();
                let entry = drifted
                    .iter_mut()
                    .find(|d| d.target == home.join(".gitconfig"))
                    .expect(".gitconfig is in the pose's drifted set");
                entry.class = "conflict".into();
                let in_sync = model.in_sync;
                model.hydrate_status(drifted, in_sync, None, false, None);
                cx.notify();
            });
        });
        vis.run_until_parked();

        vis.update(|_window, cx| {
            review.update(cx, |view, _cx| {
                assert_eq!(
                    view.selection.members().to_vec(),
                    vec![home.join(".config/starship.toml"), home.join(".zshrc")],
                    "the render pruned .gitconfig out on its own — no gesture drove it"
                );
            });
        });
    }

    /// The folder row's click gesture (spec §2.3/§2.5), dispatched through
    /// `folder_click` — mirrors `row_click_dispatches_plain_platform_and_
    /// shift_gestures`. Drives the `review-tree` pose (`.config/nvim/`
    /// collapsed, only `starship.toml` ticked) through seven steps in a row:
    ///
    /// 1. ⌘-click on `.config/nvim` (collapsed) ticks its one file —
    ///    `init.lua` — even though it is hidden, and `check_state` over
    ///    nvim's descendants reads On.
    /// 2. A plain click on `.config/zed` collapses it; membership does not
    ///    change.
    /// 3. Expanding `.config/nvim` again (`toggle_collapse`) leaves
    ///    membership untouched too — collapsing/expanding never edits
    ///    `Selection`.
    /// 4. `toggle_mode` flipped twice (Tree → Flat → Tree) keeps every
    ///    member and lands back on Tree.
    /// 5. `set_mode(Tree)` while already on Tree is a no-op: neither `mode`
    ///    nor membership moves (Minor 11 — the segmented control's own
    ///    "re-pressing the active segment does nothing" guard, pulled out of
    ///    the listener and now directly testable).
    /// 6. ⌘-click on `.config` (Partial — 2 of its 3 safe descendants
    ///    ticked) completes the set: every safe file under `.config`
    ///    becomes a member.
    /// 7. The same ⌘-click again — now On — clears the whole set.
    ///
    /// All seven steps and their assertions run inside the SAME
    /// `review.update` as the calls that produce them, for the reason
    /// `a_bulk_press_clears_the_selection_and_advances_past_the_last_member`'s
    /// own comment gives: a render in between would prune the selection
    /// against the live plan's `safe_order`, silently hiding a mutation
    /// that leaves a stray member outside it.
    ///
    /// MUTATION: `toggle_folder` ticks `plan.visible_safe` filtered to the
    /// folder instead of `safe_descendants(&plan.safe_order, ..)` → step 1's
    /// membership assertion fails (`init.lua`, hidden inside the collapsed
    /// `nvim/`, is never reached).
    /// MUTATION: `toggle_collapse` also drops the folder's descendants from
    /// the selection → step 2's membership assertion fails.
    /// MUTATION: `toggle_mode` calls `self.selection.clear()` after flipping
    /// → step 4's membership assertion fails.
    /// MUTATION: `set_mode` always flips instead of guarding on the current
    /// mode → step 5's mode assertion fails (Tree flips to Flat).
    /// MUTATION: swap `folder_click`'s plain/`platform` arms (plain ticks,
    /// ⌘ collapses) → step 1 never ticks anything and step 2 never
    /// collapses `zed/`, so both the membership and the collapse
    /// assertions fail.
    #[gpui::test]
    fn folder_click_dispatches_plain_and_platform_and_reaches_collapsed_files(
        cx: &mut TestAppContext,
    ) {
        use review::ReviewMode;
        use tomte_ui::components::CheckState;

        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Users/gallery"));
        let nvim = home.join(".config/nvim");
        let zed = home.join(".config/zed");
        let config = home.join(".config");
        let init_lua = home.join(".config/nvim/init.lua");
        let settings_json = home.join(".config/zed/settings.json");
        let starship = home.join(".config/starship.toml");

        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-tree", smoke_paths(), cx).expect("review-tree fixture")
        });
        let review = review_of(&shell, vis);

        let platform = gpui::Modifiers {
            platform: true,
            ..Default::default()
        };

        vis.update(|_window, cx| {
            review.update(cx, |view, cx| {
                assert_eq!(
                    view.selection.members().to_vec(),
                    vec![starship.clone()],
                    "the pose starts with only starship.toml ticked"
                );

                // 1. ⌘-click a COLLAPSED folder still reaches its file.
                view.folder_click(&nvim, platform, cx);
                assert_eq!(
                    view.selection.members().to_vec(),
                    vec![init_lua.clone(), starship.clone()],
                    "⌘-click on nvim/ ticks init.lua even though it is hidden"
                );
                assert_eq!(
                    view.selection.check_state(std::slice::from_ref(&init_lua)),
                    CheckState::On,
                    "every safe file under nvim/ is now ticked"
                );

                // 2. A plain click collapses zed/ without touching membership.
                assert!(!view.collapsed.contains(&zed), "zed/ starts open");
                view.folder_click(&zed, gpui::Modifiers::default(), cx);
                assert!(
                    view.collapsed.contains(&zed),
                    "the plain click collapsed zed/"
                );
                assert_eq!(
                    view.selection.members().to_vec(),
                    vec![init_lua.clone(), starship.clone()],
                    "collapsing a folder never edits the selection"
                );

                // 3. Expanding nvim/ again is the same: membership untouched.
                view.toggle_collapse(&nvim, cx);
                assert!(!view.collapsed.contains(&nvim), "nvim/ is open again");
                assert_eq!(
                    view.selection.members().to_vec(),
                    vec![init_lua.clone(), starship.clone()],
                    "expanding a folder never edits the selection either"
                );

                // 4. A mode round-trip keeps every member.
                view.toggle_mode(cx);
                view.toggle_mode(cx);
                assert_eq!(view.mode, ReviewMode::Tree, "back to Tree after two flips");
                assert_eq!(
                    view.selection.members().to_vec(),
                    vec![init_lua.clone(), starship.clone()],
                    "members keep their identity across a mode round-trip"
                );

                // 5. `set_mode` on the already-active mode is a no-op.
                view.set_mode(ReviewMode::Tree, cx);
                assert_eq!(
                    view.mode,
                    ReviewMode::Tree,
                    "re-pressing the active Tree segment stays on Tree"
                );
                assert_eq!(
                    view.selection.members().to_vec(),
                    vec![init_lua.clone(), starship.clone()],
                    "and does not touch membership either"
                );

                // 6. ⌘-click on the Partial `.config/` completes its safe set.
                assert_eq!(
                    view.selection.check_state(&[
                        init_lua.clone(),
                        settings_json.clone(),
                        starship.clone()
                    ]),
                    CheckState::Partial,
                    "2 of .config/'s 3 safe descendants are ticked before the click"
                );
                view.folder_click(&config, platform, cx);
                assert_eq!(
                    view.selection.members().to_vec(),
                    vec![init_lua.clone(), settings_json.clone(), starship.clone()],
                    "the recursive tick completes .config/'s whole safe set"
                );

                // 7. The same ⌘-click again, now On, clears it.
                view.folder_click(&config, platform, cx);
                assert!(
                    view.selection.is_empty(),
                    "a second ⌘-click on the now-fully-ticked folder clears it"
                );
            });
        });
    }

    /// Esc / ⌘A (spec §2.3, Task 8) dispatched through REAL keystrokes —
    /// `simulate_keystrokes`, not `dispatch_action` directly — because that
    /// is the only path that also proves the "Review" key context and the
    /// binding itself, not just the method behind it. `register_keys` is
    /// called explicitly first: `fixtures::build` (what poses the gallery
    /// states, including the one below) never registers any app's key
    /// bindings — only `main.rs`'s three entry points do, next to
    /// `merge::register_keys`, which no test here calls either.
    ///
    /// ⌘A must SELECT `plan.safe_order` (collapsed files included), never
    /// toggle it: pressed twice the selection is unchanged both times, not
    /// emptied the second time.
    ///
    /// MUTATION: swap the two `on_action`s in `render_inner` → both
    /// assertions below fail (Esc selects everything instead of clearing
    /// the pose's starting selection; ⌘A then empties it instead of
    /// completing it). MUTATION: `select_all_safe` calls `toggle_all`
    /// unconditionally instead of only when the state is not already On →
    /// the second ⌘A empties the selection it just filled.
    #[gpui::test]
    fn escape_and_cmd_a_drive_the_selection_through_real_key_bindings(cx: &mut TestAppContext) {
        cx.update(review::register_keys);
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-multiselect", smoke_paths(), cx)
                .expect("review-multiselect fixture")
        });
        let review = review_of(&shell, vis);

        // The pose starts with 3 of its 5 safe rows ticked (see
        // `fixtures::build`'s "review-multiselect" arm) — a non-empty,
        // non-full selection, so Esc clearing it and ⌘A completing it are
        // both observable.
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selection.members().len()),
            3,
            "the pose's own starting selection"
        );

        // Give the view's "Review" key context focus — nothing does this
        // automatically on render (unlike the merge editor's root, which
        // grabs focus whenever it is loaded); Review only takes it on a
        // mouse-down, so the test stands in for that click.
        vis.update(|window, cx| {
            review.update(cx, |view, _cx| {
                view.focus_handle.focus(window);
            });
        });

        vis.simulate_keystrokes("escape");
        assert!(
            vis.update(|_window, cx| review.read(cx).selection.is_empty()),
            "Esc reached the binding and cleared the selection"
        );

        let safe_len =
            vis.update(|_window, cx| review.update(cx, |view, cx| view.plan(cx).safe_order.len()));
        assert_eq!(safe_len, 5, "the pose's own safe-group size");

        vis.simulate_keystrokes("cmd-a");
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selection.members().len()),
            safe_len,
            "⌘A selected every safe row, collapsed files included"
        );

        vis.simulate_keystrokes("cmd-a");
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selection.members().len()),
            safe_len,
            "a second ⌘A is a no-op: select, never toggle"
        );
    }

    /// §2.3's OTHER half — "the root takes focus on any click inside the
    /// view" — proven with REAL clicks (`simulate_click`), which the smoke
    /// test above cannot do: it focuses `view.focus_handle` programmatically
    /// and says so in its own comment, so it can tell "the binding fires
    /// when focused" but not "a click gives the RIGHT element focus". That
    /// gap is exactly how fix round 1's regression got through: a
    /// hand-written `on_mouse_down` on the root re-focused the root on every
    /// click, including one inside the diff editor's own pane, so ⌘A there
    /// fired `SelectAllSafe` instead of the editor's own `SelectAll` and the
    /// diff stopped being copyable. Two real clicks on `review-multiselect`
    /// (a diff is showing; the pose's ~10 rows leave the scrollable rows
    /// column well short of the sidebar's full height, so its floor is
    /// genuinely empty):
    ///
    /// 1. Click inside the diff editor's own scroll pane (bounds read back
    ///    through `debug_selector`/`debug_bounds`, so this lands on real
    ///    rendered text rather than a guessed offset) → the editor's OWN
    ///    `track_focus` claims focus before the root's ever runs (gpui's
    ///    bubble order runs innermost first). ⌘A then reaches
    ///    "TextAreaReadOnly"'s `SelectAll` — registered by
    ///    `merge::register_keys`, called here for exactly that reason, the
    ///    same way `main.rs` always calls it next to `review::register_keys`
    ///    — not Review's `SelectAllSafe`: the sidebar selection stays at the
    ///    pose's starting 3, and the editor's own selection is non-empty.
    /// 2. Click in the same column's empty space, below the last row →
    ///    nothing there claims focus, so the root's `track_focus` is the
    ///    only listener that can. Esc then reaches `ClearSelection` and
    ///    empties that same 3-member selection.
    ///
    /// MUTATION: reinstate the `on_mouse_down` block fix round 1 deleted
    /// (`render_inner`, right after the two `on_action`s: `.on_mouse_down(
    /// gpui::MouseButton::Left, cx.listener(|view, _ev, window, _cx|
    /// view.focus_handle.focus(window)))`) → step 1's diff click focuses the
    /// root instead of the editor, so ⌘A selects the pose's 5 safe rows
    /// instead of the diff's text — the first two assertions below fail.
    #[gpui::test]
    fn a_click_in_the_diff_keeps_its_focus_and_a_sidebar_click_takes_the_root(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            // The diff editor's own cmd-a/cmd-c bindings — `main.rs` always
            // registers this next to `review::register_keys`, and step 1
            // needs it to tell "the editor's SelectAll fired" from "nothing
            // fired".
            merge::register_keys(cx);
            review::register_keys(cx);
        });
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-multiselect", smoke_paths(), cx)
                .expect("review-multiselect fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();

        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selection.members().len()),
            3,
            "the pose's own starting selection"
        );

        // 1. Click inside the diff editor's own pane.
        let diff_bounds = vis
            .debug_bounds("review-diff-scroll")
            .expect("the diff pane rendered (review-multiselect selects settings.json)");
        vis.simulate_click(
            diff_bounds.origin + gpui::point(px(20.), px(20.)),
            gpui::Modifiers::default(),
        );
        vis.simulate_keystrokes("cmd-a");
        assert_eq!(
            vis.update(|_window, cx| review.read(cx).selection.members().len()),
            3,
            "⌘A reached the diff editor's own SelectAll, not Review's \
             SelectAllSafe — the sidebar selection is untouched"
        );
        let editor_selected_len = vis.update(|_window, cx| {
            review
                .read(cx)
                .preview_editor
                .as_ref()
                .and_then(|editor| editor.read(cx).selected_text().map(str::len))
        });
        assert!(
            editor_selected_len.is_some_and(|len| len > 0),
            "the click kept the diff editor focused, so ⌘A selected its own \
             text: {editor_selected_len:?}"
        );

        // 2. Click in the rows column's empty space, below the last row.
        let rows_bounds = vis
            .debug_bounds("review-sidebar-rows")
            .expect("the sidebar rows column rendered");
        let empty_point = gpui::point(
            rows_bounds.origin.x + px(20.),
            rows_bounds.origin.y + rows_bounds.size.height - px(8.),
        );
        vis.simulate_click(empty_point, gpui::Modifiers::default());
        vis.simulate_keystrokes("escape");
        assert!(
            vis.update(|_window, cx| review.read(cx).selection.is_empty()),
            "a click on empty sidebar space still gave the root focus \
             (nothing else claimed it), and Esc reached it"
        );
    }

    /// The selection bar lives in the sidebar, with the selection it acts
    /// on (spec 2026-09-23 §2.4 as amended 2026-10-12): it renders with NO
    /// file open, pinned under the rows inside the sidebar column, and its
    /// buttons are wired — a real click on its Keep disk queues the ticked
    /// files, a real click on its ✕ clears them.
    ///
    /// MUTATION: build the bar inside `detail` again (below its
    /// "select a file to review" early return) → no file open, no bar: the
    /// first lookup fails.
    /// MUTATION: `.children(self.selection_bar(theme, cx))` on the detail
    /// pane instead of the sidebar → the bar lands right of the column and
    /// the containment assertion fails.
    /// MUTATION: `|_, _, _| {}` as the Keep disk listener → nothing queued.
    #[gpui::test]
    fn the_selection_bar_is_in_the_sidebar_even_with_no_file_open(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-multiselect-closed", smoke_paths(), cx)
                .expect("review-multiselect-closed fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        assert!(
            vis.update(|_w, cx| review.read(cx).selected.is_none()),
            "pose premise: no file open"
        );
        assert_eq!(
            vis.update(|_w, cx| review.read(cx).selection.members().len()),
            3
        );
        let column = vis
            .debug_bounds("review-sidebar-rows")
            .expect("sidebar column");
        let bar = vis
            .debug_bounds("review-selection-bar")
            .expect("the selection bar renders with no file open");
        assert!(
            bar.left() >= column.left() - px(1.) && bar.right() <= column.right() + px(1.),
            "bar {bar:?} spans the sidebar column {column:?}"
        );
        assert!(
            bar.top() >= column.bottom() - px(1.),
            "bar {bar:?} is pinned under the rows {column:?}"
        );
        let clear = vis.debug_bounds("review-bulk:clear").expect("the bar's ✕");
        assert!(
            clear.right() <= column.right() + px(1.),
            "✕ {clear:?} inside the column {column:?}"
        );
        let keep_disk = vis
            .debug_bounds("review-bulk:keep-disk")
            .expect("the bar's Keep disk");
        vis.simulate_click(keep_disk.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        assert_eq!(
            vis.update(|_w, cx| shell.read(cx).queue.processing().len()),
            3,
            "one real click on the bar's Keep disk queued every ticked file"
        );
        assert!(
            vis.update(|_w, cx| review.read(cx).selection.is_empty()),
            "and the selection is spent"
        );
    }

    /// The List/Tree control is a toggle on the SAFE TO RESOLVE header
    /// (spec 2026-10-11 §2.1), reached by a real click, and its lit state
    /// follows the mode (the selector name carries `:on` / `:off`).
    ///
    /// MUTATION: pass `|_, _, _| {}` as the toggle's listener in
    /// `group_header_el` → the click lands and the mode stays Flat.
    /// MUTATION: `active: mode != ReviewMode::Tree` → the `:on`/`:off`
    /// lookups swap and the first assertion fails.
    #[gpui::test]
    fn the_header_toggle_flips_the_mode_with_a_real_click(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-multiselect", smoke_paths(), cx)
                .expect("review-multiselect fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        assert_eq!(
            vis.update(|_w, cx| review.read(cx).mode),
            review::ReviewMode::Flat
        );
        assert!(
            vis.debug_bounds("review-mode-toggle:on").is_none(),
            "Flat mode: the toggle is not lit"
        );
        let toggle = vis
            .debug_bounds("review-mode-toggle:off")
            .expect("the SAFE TO RESOLVE header rendered its unlit toggle");
        vis.simulate_click(toggle.center(), gpui::Modifiers::default());
        assert_eq!(
            vis.update(|_w, cx| review.read(cx).mode),
            review::ReviewMode::Tree
        );
        vis.run_until_parked();
        // Only positive lookups after the first re-render: gpui 0.2.2 never
        // clears `debug_bounds` between frames, so a stale `:off` entry
        // outlives the mode flip. The first frame is fresh, which is why
        // the initial `:on` absence check above is sound.
        let toggle = vis
            .debug_bounds("review-mode-toggle:on")
            .expect("lit in Tree mode");
        vis.simulate_click(toggle.center(), gpui::Modifiers::default());
        assert_eq!(
            vis.update(|_w, cx| review.read(cx).mode),
            review::ReviewMode::Flat
        );
    }

    /// At the sidebar's minimum width the header still holds its label, the
    /// toggle and the checkbox: the toggle is inside the sidebar, 20 px
    /// wide, not pushed past the edge.
    ///
    /// MUTATION: make the toggle a two-segment "List | Tree" control (two
    /// `ui::button`s, `ButtonSize::Micro`) → its right edge overruns the
    /// 180 px column and the containment assertion fails.
    #[gpui::test]
    fn the_header_toggle_fits_at_the_minimum_sidebar_width(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-tree", smoke_paths(), cx).expect("review-tree fixture")
        });
        let review = review_of(&shell, vis);
        vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                view.sidebar_width = Some(review::MIN_SIDEBAR_W);
                cx.notify();
            })
        });
        vis.run_until_parked();
        let column = vis
            .debug_bounds("review-sidebar-rows")
            .expect("sidebar column");
        let toggle = vis
            .debug_bounds("review-mode-toggle:on")
            .expect("lit toggle (review-tree is Tree mode)");
        assert!(
            toggle.right() <= column.right() && toggle.left() >= column.left(),
            "toggle {toggle:?} inside the column {column:?}"
        );
        assert_eq!(toggle.size.width, px(20.));
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
            type_change: None,
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
                s.deferred_repo_op = Some(PendingRepoOp::AbortMerge {
                    rolled_back: merge::Rollback::Files(1),
                    dropped: Vec::new(),
                });
                s.undo_last(cx);
                assert!(matches!(
                    s.deferred_repo_op,
                    Some(PendingRepoOp::AbortMerge { .. })
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

        let done = abort_banner(AbortOutcome::RolledBack(Ok(())), &rolled_back, &[]);
        assert!(
            done.text.contains("rolled back") && done.text.contains("2 source file(s)"),
            "{}",
            done.text
        );
        assert_eq!(done.tint, review::BannerTint::Drift);

        let stale = abort_banner(
            AbortOutcome::RolledBack(Err(ResolveError::Failed("daemon gone".into()))),
            &rolled_back,
            &[],
        );
        assert!(
            stale.text.contains("rolled back") && stale.text.contains("may be stale"),
            "a rescan failure still means the rollback happened: {}",
            stale.text
        );

        // The dropped saves went at REQUEST time, so a panic partway through
        // the roll-back does not give them back. Saying so here matters most:
        // it is the arm where the user has least else to go on.
        let boom = abort_banner(
            AbortOutcome::Panicked("index out of bounds".into()),
            &rolled_back,
            &[PathBuf::from("/a"), PathBuf::from("/b")],
        );
        assert!(
            boom.text.contains("2 queued save(s) were dropped"),
            "a panicked roll-back still owes the user its dropped saves: {}",
            boom.text
        );
        assert!(
            boom.text.contains("save again"),
            "and what to do about them: {}",
            boom.text
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

    /// RULING 30. `merge_abort` used to discard its own result, so a
    /// roll-back that git REFUSED — an `index.lock` a concurrent scan holds
    /// past `run`'s six retries, the contention `85fec09` met elsewhere —
    /// arrived here as `RolledBack(Ok(()))` and was reported as "Merge with
    /// origin rolled back". The merge was still open, the user's answers were
    /// still staged in it, and the banner said the opposite.
    ///
    /// The fixed-prefix check of
    /// `a_panicked_abort_never_claims_the_roll_back_happened`, the other way
    /// round: this arm must never contain the sentence that arm is allowed to.
    ///
    /// MUTATION: fold `AbortFailed` into the `RolledBack(Err(..))` arm (the
    /// pre-Task-6 shape, where a failed abort was indistinguishable from a
    /// failed rescan) → the first assertion fails. MUTATION: give it
    /// `BannerTint::Drift` → the last fails: a merge still open over answers
    /// the user was told were discarded is not a routine drift outcome.
    #[test]
    fn a_failed_abort_never_claims_the_roll_back_happened() {
        let rolled_back = merge::Rollback::Files(2);
        let failed = abort_banner(
            AbortOutcome::AbortFailed(ResolveError::Failed(
                "git exited with code 128: fatal: Unable to create index.lock: File exists".into(),
            )),
            &rolled_back,
            &[PathBuf::from("/a")],
        );
        assert!(
            !failed.text.contains("rolled back"),
            "a roll-back that FAILED must never assert it happened: {}",
            failed.text
        );
        assert!(
            failed.text.contains("still in progress") && failed.text.contains("nothing was undone"),
            "and must say what is true instead — the merge is still there: {}",
            failed.text
        );
        assert!(
            failed.text.contains("index.lock"),
            "carrying git's own reason, so the user can act on it: {}",
            failed.text
        );
        assert!(
            failed.text.contains("1 queued save(s) were dropped"),
            "the saves went at REQUEST time, so they are owed on this arm too: {}",
            failed.text
        );
        assert_eq!(
            failed.tint,
            review::BannerTint::Conflict,
            "a merge left open over answers the user was told were discarded is \
             not a routine drift outcome"
        );
        assert!(
            !failed.undoable,
            "there is nothing to undo: nothing changed"
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

    /// And the OTHER half of that same gap: which outcome the abort's own
    /// `Result` earns, and whether a rescan runs behind it.
    ///
    /// `a_failed_abort_never_claims_the_roll_back_happened` drives
    /// `abort_banner` directly, so it proves the banner MAPPING and nothing
    /// about the routing — and the routing is where Ruling 30's bug actually
    /// lived. `run_abort`'s closure is a background spawn over a live engine
    /// and cannot be asserted; [`abort_outcome_of`] can.
    ///
    /// The rescan is a closure precisely so "it did not run" is assertable.
    /// A rescan on the failed arm is not a cosmetic slip: it is ~17 s of held
    /// daemon core lock spent re-walking the tree to confirm the repo the
    /// last scan already described, because nothing was restored.
    ///
    /// MUTATION: route `Err` through `RolledBack(Err(e))` — the exact
    /// pre-Task-6 shape, where a roll-back git REFUSED reported as one that
    /// ran with a failed rescan behind it → the first assertion fails.
    /// MUTATION: call `rescan()` unconditionally (hoist it above the `match`,
    /// or add it to the `Err` arm) → `rescans` is 1 on the failed arm and the
    /// second assertion fails.
    #[test]
    fn a_failed_abort_is_routed_to_abort_failed_and_never_rescans() {
        use std::cell::Cell;

        // The abort FAILED: the merge is still open, nothing was restored.
        let rescans = Cell::new(0);
        let failed = abort_outcome_of(
            Err(ResolveError::Failed("index.lock: File exists".into())),
            || {
                rescans.set(rescans.get() + 1);
                Ok(())
            },
        );
        assert!(
            matches!(failed, AbortOutcome::AbortFailed(ref e) if e.to_string().contains("index.lock")),
            "a roll-back git refused must NOT come back as `RolledBack` — that is the \
             pre-Task-6 lie Ruling 30 removes, and it carries git's reason. Got: {failed:?}"
        );
        assert_eq!(
            rescans.get(),
            0,
            "and nothing changed, so the daemon must not be asked to re-walk the tree"
        );

        // The abort WORKED: the list on screen describes the merged repo
        // until a rescan replaces it, so that rescan is not optional.
        let rescans = Cell::new(0);
        let done = abort_outcome_of(Ok(()), || {
            rescans.set(rescans.get() + 1);
            Ok(())
        });
        assert!(matches!(done, AbortOutcome::RolledBack(Ok(()))), "{done:?}");
        assert_eq!(rescans.get(), 1, "the restored repo has to be rescanned");

        // A rescan that fails still means the roll-back happened — the
        // distinction `AbortOutcome` exists to keep.
        let failed_rescan =
            abort_outcome_of(Ok(()), || Err(ResolveError::Failed("daemon gone".into())));
        assert!(
            matches!(failed_rescan, AbortOutcome::RolledBack(Err(_))),
            "{failed_rescan:?}"
        );
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
                    Some("dropped: a roll-back was requested before this save ran — save again"),
                    "the orphaned save says so on its row, and says what to do"
                );
                assert_eq!(
                    s.queue.decision_label(Path::new("/plain")),
                    Some("Keep disk"),
                    "a decision that does not depend on the merge stays queued"
                );
                assert_eq!(s.queue.depth(), 1);
                assert!(
                    matches!(s.deferred_repo_op, Some(PendingRepoOp::AbortMerge { .. })),
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

    /// The banner names the LAST target to land, and that is Undo's
    /// correctness rather than a preference.
    ///
    /// The reviewer's trace: Save A in the merge editor, then keep-disk B, C,
    /// D in Review. If only the save banners, the queue idles with "Merged ·
    /// committed & pushed" on screen and `undo_affordance` turns Undo live —
    /// but `undo_last` reverts the LAST FINISHED session, which is D. The
    /// button would revert a different file than the sentence above it names.
    ///
    /// Driven through `batch_finished` with a batch's worth of reports rather
    /// than a live engine: the rule under test is "one banner per landed
    /// target, in result order", and reports are exactly what `run_batch`
    /// hands it.
    ///
    /// MUTATION: `landed.pop()` instead of the loop (or a `queued_outcome_banner`
    /// that only answers for a merge save) → the banner names A while Undo
    /// reverts D. MUTATION: iterate `landed` in reverse → it names B.
    #[gpui::test]
    fn the_banner_names_the_last_target_to_land(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Review, cx));

        // Each of the three lands with a DIFFERENT sentence, so "the last
        // one" is distinguishable from "the first" and from "any of them" —
        // three identical successes would let a mutation that keeps the wrong
        // one pass unnoticed.
        let done = |note: Option<&str>| {
            Ok(BatchResult::Resolved(ResolveOutcome::Done {
                session: 1,
                committed: note.is_none(),
                pushed: note.is_none(),
                note: note.map(str::to_owned),
            }))
        };
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                // A merge save landed first and set the banner it owed.
                s.show_outcome(
                    OutcomeBanner {
                        text: "Merged · committed & pushed".into(),
                        tint: review::BannerTint::Ok,
                        undoable: true,
                    },
                    cx,
                );
                // Then a sweep of three, taken as one batch.
                for (t, decision) in [
                    ("/b", Decision::KeepDisk),
                    ("/c", Decision::KeepSourceLocal),
                    ("/d", Decision::KeepDisk),
                ] {
                    s.queue.push(QueuedAction {
                        target: PathBuf::from(t),
                        decision,
                        notify_on_done: false,
                    });
                }
                let batch = s.queue.take_batch();
                assert_eq!(batch.len(), 3, "one batch, three targets");
                let notes = [None, None, Some("push failed")];
                s.batch_finished(
                    batch
                        .into_iter()
                        .zip(notes)
                        .map(|(a, note)| (a.target, a.decision.label(), done(note)))
                        .collect(),
                    None,
                    cx,
                );
            });
        });

        let review = vis
            .update(|_window, cx| shell.read(cx).review.clone())
            .expect("the banners made a review view");
        let banner = vis
            .update(|_window, cx| review.read(cx).last_outcome.clone())
            .expect("a landing is never silent");
        assert_eq!(
            banner.text.as_ref(),
            "Kept disk version · d · push failed",
            "the sweep's LAST file owns the banner, BY NAME — not the merge \
             save before the sweep, and not the first or middle file of it"
        );
        assert_eq!(
            banner.tint,
            review::BannerTint::Drift,
            "and the tint is that landing's too, not a leftover Ok"
        );
        assert!(
            banner.undoable,
            "and it is undoable, because `undo_last` reverts exactly that session"
        );
    }

    /// A roll-back that orphans queued saves has to SAY so, in the banner.
    ///
    /// The rows cannot be relied on for it: `fail_pending_where` puts the
    /// message on each row, but a file that was a review row only because the
    /// merge left it conflicted stops being drifted once the roll-back lands,
    /// and `model_changed` prunes the message with the row. Without the
    /// banner the user's save would vanish leaving only a log line.
    ///
    /// MUTATION: drop the `dropped` field from the op (or stop filling it in
    /// `request_repo_op`) → the count never reaches the banner and the
    /// assertion fails.
    #[gpui::test]
    fn a_roll_back_says_how_many_queued_saves_went_with_it(cx: &mut TestAppContext) {
        let _lab = EngineLab::publish(cx);
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Merge, cx));

        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                s.queue.push(QueuedAction {
                    target: PathBuf::from("/staged"),
                    decision: Decision::RepoConflict {
                        source_path: PathBuf::from("/src/staged"),
                        resolved: "x".into(),
                    },
                    notify_on_done: false,
                });
                s.abort_origin_merge(merge::Rollback::Files(2), cx);
            });
        });
        vis.run_until_parked();
        let review = vis
            .update(|_window, cx| shell.read(cx).review.clone())
            .expect("the abort's banner made a review view");
        let banner = vis
            .update(|_window, cx| review.read(cx).last_outcome.clone())
            .expect("the rollback is reported, never silent");
        assert!(
            banner.text.contains("1 queued save(s) were dropped"),
            "the banner has to carry what the rows will not keep: {}",
            banner.text
        );
        assert!(
            banner.text.contains("save again"),
            "and say what to do about it: {}",
            banner.text
        );
    }

    /// A decision made while the daemon is away is QUEUED, not refused.
    ///
    /// Parking is the queue's model: `pump` leaves everything queued when no
    /// engine is published and `render`'s connect latch releases it on the
    /// next (re)connect. Refusing the press instead would be the same mistake
    /// the `in_flight` booleans were — declining to remember a decision
    /// because it cannot run this second.
    ///
    /// No `EngineSlot` is published here, which is exactly the disconnected
    /// case. Both surfaces are checked: Review's buttons and the dashboard's
    /// quick actions have to be LIVE, and the press has to land on the queue
    /// without being taken out of it.
    ///
    /// MUTATION: reinstate the `!engine_ready` arm in `review::blocked_reason`
    /// or in `dashboard::quick_action_blocked` → the first two assertions
    /// fail and the click can never be made at all.
    #[gpui::test]
    fn a_decision_made_while_disconnected_is_queued_not_refused(cx: &mut TestAppContext) {
        use review::ResolveAction;
        use std::path::Path;

        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review", smoke_paths(), cx).expect("review fixture")
        });
        let review = vis
            .update(|_window, cx| shell.read(cx).review.clone())
            .expect("the fixture poses a review view");

        // Nothing is connected and nothing is owed, so neither surface has a
        // reason to be inert.
        assert_eq!(
            review::blocked_reason(&Default::default(), Path::new("/anything")),
            None,
            "Review's decision buttons stay live with no daemon: the press parks"
        );
        assert_eq!(
            dashboard::quick_action_blocked(false),
            None,
            "and so do the dashboard's quick actions"
        );

        let selected = vis
            .update(|_window, cx| review.read(cx).selected.clone())
            .expect("the fixture selects a file");
        vis.update(|_window, cx| {
            review.update(cx, |view, cx| view.run_action(ResolveAction::KeepDisk, cx))
        });
        vis.run_until_parked();

        vis.update(|_window, cx| {
            let s = shell.read(cx);
            assert_eq!(s.queue.depth(), 1, "the decision is remembered");
            assert!(
                s.queue.running().is_empty(),
                "and still PENDING — parked, not taken out of the queue and lost"
            );
            assert_eq!(
                s.queue.decision_label(&selected),
                Some("Keep disk"),
                "the row says which decision is waiting"
            );
            assert!(
                s.queue.failure(&selected).is_none(),
                "parking is not a failure to report"
            );
            assert!(
                !s.queue_pumping,
                "nothing is in flight, so the next connect is free to pump"
            );
        });
    }

    /// A FAILED landing keeps its row chip and leaves the banner alone.
    ///
    /// Spec §2.5: the queue moves on within milliseconds of an error, so a
    /// transient banner would vanish unseen — the row is where a failure
    /// lives, and choosing again there is the retry. The banner is for
    /// SUCCESSES, because it is what carries Undo and has to name what Undo
    /// reverts. So a failure landing after a success must not clear that
    /// success's banner, and a failure landing last must not claim it.
    ///
    /// MUTATION: banner the `Err` arm of `queued_outcome_banner` → the first
    /// batch's banner names `/b`'s failure instead of `/c`'s landing, and the
    /// second batch loses `/a`'s undoable banner to a failure that has its
    /// own row.
    #[gpui::test]
    fn a_failed_landing_keeps_its_row_and_leaves_the_banner_alone(cx: &mut TestAppContext) {
        use std::path::Path;

        let landed = |note: Option<&str>| {
            Ok(BatchResult::Resolved(ResolveOutcome::Done {
                session: 1,
                committed: note.is_none(),
                pushed: note.is_none(),
                note: note.map(str::to_owned),
            }))
        };
        let failed = || Err(ResolveError::Failed("op timed out".into()));

        // [Done a, Err b, Done c] — c is the last thing that landed.
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Review, cx));
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                for t in ["/a", "/b", "/c"] {
                    s.queue.push(QueuedAction {
                        target: PathBuf::from(t),
                        decision: Decision::KeepDisk,
                        notify_on_done: false,
                    });
                }
                s.queue.take_batch();
                s.batch_finished(
                    vec![
                        (PathBuf::from("/a"), "Keep disk", landed(None)),
                        (PathBuf::from("/b"), "Keep disk", failed()),
                        (
                            PathBuf::from("/c"),
                            "Keep disk",
                            landed(Some("push failed")),
                        ),
                    ],
                    None,
                    cx,
                );
            });
        });
        let review = vis
            .update(|_window, cx| shell.read(cx).review.clone())
            .expect("the landing made a review view");
        vis.update(|_window, cx| {
            let banner = review
                .read(cx)
                .last_outcome
                .clone()
                .expect("a landing is never silent");
            assert_eq!(
                banner.text.as_ref(),
                "Kept disk version · c · push failed",
                "the banner names the last thing that LANDED, not the failure between"
            );
            assert!(banner.undoable, "and it is what Undo would revert");
            let s = shell.read(cx);
            assert_eq!(
                s.queue.failure(Path::new("/b")).map(|m| m.as_ref()),
                Some("Keep disk failed: op timed out"),
                "the failure is on its row, where choosing again is the retry"
            );
            assert!(
                s.queue.failure(Path::new("/c")).is_none(),
                "and only on the row it belongs to"
            );
        });

        // [Done a, Err b] — the failure lands LAST and still must not take
        // the banner from the success before it.
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                for t in ["/a", "/b"] {
                    s.queue.push(QueuedAction {
                        target: PathBuf::from(t),
                        decision: Decision::KeepSourceLocal,
                        notify_on_done: false,
                    });
                }
                s.queue.take_batch();
                s.batch_finished(
                    vec![
                        (
                            PathBuf::from("/a"),
                            "Keep source · this machine",
                            landed(None),
                        ),
                        (PathBuf::from("/b"), "Keep source · this machine", failed()),
                    ],
                    None,
                    cx,
                );
            });
        });
        vis.update(|_window, cx| {
            let banner = review
                .read(cx)
                .last_outcome
                .clone()
                .expect("the success still owns the banner");
            assert_eq!(
                banner.text.as_ref(),
                "Restored chezmoi's version · a · committed & pushed",
                "the success still owns the banner, and still names its file"
            );
            assert!(banner.undoable);
            assert_eq!(
                shell
                    .read(cx)
                    .queue
                    .failure(Path::new("/b"))
                    .map(|m| m.as_ref()),
                Some("Keep source · this machine failed: op timed out"),
            );
        });
    }

    /// The dashboard's row lock is WIRED to the queue.
    ///
    /// `quick_action_blocked` is a pure rule with its own test, but nothing
    /// tied it to the Shell: replacing `dashboard_snapshot`'s `running` field
    /// with `Vec::new()` left every test green and no screenshot could catch
    /// it — the quick actions are `opacity(0.)` until the row is hovered. So
    /// this drives the join: a decision RUNNING on one file makes that file's
    /// buttons inert and leaves every other file's alone.
    ///
    /// MUTATION: `running: Vec::new()` in `dashboard_snapshot` (or mapping
    /// `processing()` instead of `running()`) → the first assertion fails, or
    /// the third does when a merely queued row goes dead.
    #[gpui::test]
    fn the_dashboard_row_lock_is_wired_to_the_queue(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Dashboard, cx));
        let running = PathBuf::from("/running");
        let queued = PathBuf::from("/queued");

        vis.update(|_window, cx| {
            shell.update(cx, |s, _cx| {
                s.queue.push(QueuedAction {
                    target: running.clone(),
                    decision: Decision::KeepDisk,
                    notify_on_done: false,
                });
                s.queue.take_batch(); // under the latch now
                s.queue.push(QueuedAction {
                    target: queued.clone(),
                    decision: Decision::KeepDisk,
                    notify_on_done: false,
                });
            });
        });

        let snapshot = vis.update(|_window, cx| shell.read(cx).dashboard_snapshot());
        let blocked = |t: &PathBuf| {
            dashboard::quick_action_blocked(snapshot.running.iter().any(|r| r == t))
                .map(|r| r.to_string())
        };
        assert_eq!(
            blocked(&running),
            Some("applying this file's decision now".to_string()),
            "the running file's quick actions are inert: a second press could \
             only duplicate a decision already in flight"
        );
        assert_eq!(
            blocked(&PathBuf::from("/elsewhere")),
            None,
            "and no other file is touched — that is the whole point of the queue"
        );
        assert_eq!(
            blocked(&queued),
            None,
            "a merely QUEUED decision can still be replaced (spec §2.4), so its \
             row stays live"
        );
    }

    /// A second Discard must not lose the first one's dropped saves.
    ///
    /// The first request drains the orphaned saves out of `pending`, so the
    /// second's `fail_pending_where` finds nothing — and the second op is the
    /// one that runs and banners. Without carrying them across, the banner
    /// would report zero dropped saves while the user's saves are just as
    /// gone. Reachable with no contrivance: Discard, latch held, back to
    /// Review, open the editor on another file, Discard again.
    ///
    /// MUTATION: drop the carry-across from `request_repo_op` → the
    /// superseding op's `dropped` is empty and the banner says nothing.
    #[gpui::test]
    fn a_superseding_roll_back_keeps_the_saves_the_first_one_dropped(cx: &mut TestAppContext) {
        let _lab = EngineLab::publish(cx);
        let (shell, vis) = cx.add_window_view(|_window, cx| latch_shell(Route::Merge, cx));

        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                // The latch is held, so both requests only ever sit in the
                // slot — the second replaces the first.
                s.queue_pumping = true;
                s.queue.push(QueuedAction {
                    target: PathBuf::from("/staged"),
                    decision: Decision::RepoConflict {
                        source_path: PathBuf::from("/src/staged"),
                        resolved: "x".into(),
                    },
                    notify_on_done: false,
                });
                s.abort_origin_merge(merge::Rollback::Files(2), cx);
                // Nothing left pending for the second request to find.
                s.abort_origin_merge(merge::Rollback::Files(3), cx);

                let Some(PendingRepoOp::AbortMerge { dropped, .. }) = &s.deferred_repo_op else {
                    panic!("the roll-back is still the pending op");
                };
                assert_eq!(
                    dropped,
                    &[PathBuf::from("/staged")],
                    "the save the FIRST roll-back dropped travels to the one that replaces it"
                );
            });
        });

        // And it reaches the user, in the banner of the op that actually ran.
        vis.update(|_window, cx| {
            shell.update(cx, |s, cx| {
                s.queue_pumping = false;
                s.pump(cx);
            })
        });
        vis.run_until_parked();
        let review = vis
            .update(|_window, cx| shell.read(cx).review.clone())
            .expect("the roll-back's banner made a review view");
        let banner = vis
            .update(|_window, cx| review.read(cx).last_outcome.clone())
            .expect("the roll-back is reported, never silent");
        assert!(
            banner.text.contains("1 queued save(s) were dropped"),
            "the superseding roll-back still owes the user that save: {}",
            banner.text
        );
        assert!(
            banner.text.contains("3 source file(s)"),
            "and names what the SECOND confirmation named: {}",
            banner.text
        );
    }

    /// The sidebar is a virtualized `list` (spec 2026-10-11 §2.2): a
    /// 500-row plan renders the rows in view plus the overdraw, not all of
    /// them.
    ///
    /// MUTATION: render every row (a `div().flex_col().children(all rows)`
    /// column instead of `list`, the 0.1.20 shape) → 500+ rows per frame.
    #[gpui::test]
    fn a_500_row_sidebar_renders_only_the_rows_in_view(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-many", smoke_paths(), cx).expect("review-many fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        let (rows, rendered) = vis.update(|_w, cx| {
            let v = review.read(cx);
            (v.plan_rows_len_for_test(), v.rows_rendered.get())
        });
        assert!(rows > 450, "the pose has {rows} rows");
        assert!(
            rendered > 0 && rendered < 100,
            "{rendered} of {rows} rows rendered in the last frame"
        );
    }

    /// The scroll anchor must survive a plan rebuild that changes nothing
    /// visible, and the list must drop its "nothing to review" item when
    /// rows arrive (and take it back when they all go).
    ///
    /// MUTATION: `splice` instead of `reset` on the empty↔rows flip → the
    /// item count is off by one after the flip. MUTATION: `reset` on every
    /// rebuild → the height cache is dropped: row 420, measured while the
    /// list stood at 400 and off-screen since, has no bounds after the no-op
    /// update. (The scroll top itself would survive that mutation: the
    /// re-anchor puts it back on its row.)
    #[gpui::test]
    fn the_list_resets_when_the_plan_flips_between_empty_and_rows(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-many", smoke_paths(), cx).expect("review-many fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        let item_count = |vis: &mut VisualTestContext| {
            vis.update(|_w, cx| {
                let v = review.read(cx);
                (v.list_state.item_count(), v.plan_rows_len_for_test())
            })
        };
        let (items, rows) = item_count(vis);
        assert_eq!(items, rows + 1, "rows plus the In sync footer");

        // Visit item 400 so the rows below it are measured, then scroll up
        // to 200 and send a model update that changes no row.
        vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                view.list_state.scroll_to(gpui::ListOffset {
                    item_ix: 400,
                    offset_in_item: px(0.),
                });
                cx.notify();
            })
        });
        vis.run_until_parked();
        vis.update(|_w, cx| {
            review.read(cx).list_state.scroll_to(gpui::ListOffset {
                item_ix: 200,
                offset_in_item: px(0.),
            })
        });
        let state = vis.update(|_w, cx| shell.read(cx).state.clone());
        vis.update(|_w, cx| state.update(cx, |_model, cx| cx.notify()));
        vis.run_until_parked();
        assert_eq!(
            vis.update(|_w, cx| review.read(cx).list_state.logical_scroll_top().item_ix),
            200,
            "an unchanged plan keeps the scroll anchor"
        );
        assert!(
            vis.update(|_w, cx| review.read(cx).list_state.bounds_for_item(420))
                .is_some(),
            "an unchanged plan keeps the measured heights of rows off screen"
        );

        // Everything resolves: the plan is empty.
        let saved = vis.update(|_w, cx| state.read(cx).drifted.clone());
        vis.update(|_w, cx| {
            state.update(cx, |model, cx| {
                model.drifted.clear();
                cx.notify();
            })
        });
        vis.update(|_w, cx| {
            // Notified, as every real queue change is: the Review entity
            // re-renders with its Shell and reads the emptied queue.
            shell.update(cx, |s, cx| {
                s.queue = Default::default();
                s.queue_pumping = false;
                cx.notify();
            })
        });
        vis.run_until_parked();
        assert_eq!(item_count(vis), (2, 0), "the note and the footer");

        // And drift comes back.
        vis.update(|_w, cx| {
            state.update(cx, |model, cx| {
                model.drifted = saved;
                cx.notify();
            })
        });
        vis.run_until_parked();
        let (items, rows) = item_count(vis);
        assert!(rows > 450);
        assert_eq!(items, rows + 1);
    }

    /// Clicks reach rows THROUGH the list element (which owns a hitbox for
    /// scrolling): a real click on a safe row selects it.
    ///
    /// MUTATION: pass `true` as `locked` to `target_row` from `sidebar_item`
    /// → the row has no `on_click` and `selected` does not change.
    #[gpui::test]
    fn a_real_click_on_a_row_inside_the_list_selects_it(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-multiselect", smoke_paths(), cx)
                .expect("review-multiselect fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        let zshrc = fixtures::home(".zshrc");
        let ix = vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                let plan = view.plan(cx);
                plan.rows
                    .iter()
                    .position(|r| {
                        matches!(r, review::SidebarRow::Target { entry, .. } if entry.target == zshrc)
                    })
                    .expect(".zshrc is a row of the pose")
            })
        });
        let selector: &'static str = Box::leak(format!("review-row-{ix}").into_boxed_str());
        let row = vis.debug_bounds(selector).expect("row rendered");
        vis.simulate_click(row.center(), gpui::Modifiers::default());
        assert_eq!(
            vis.update(|_w, cx| review.read(cx).selected.clone()),
            Some(zshrc)
        );
    }

    /// A row inside the list spans the sidebar's full width, as it did in
    /// the plain column: the list lays each item out as a root and leaves
    /// its width to the item, so without a stretching wrapper a row shrinks
    /// to its content and its selected / ticked wash stops short (seen in
    /// the first `review-multiselect` screenshot of the list, 2026-10-11).
    ///
    /// MUTATION: return the bare `sidebar_item` element from the list's
    /// item closure (drop the `div().w_full().flex().flex_col()` wrapper)
    /// → the row is narrower than the rows column.
    #[gpui::test]
    fn a_row_inside_the_list_spans_the_sidebar_width(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-multiselect", smoke_paths(), cx)
                .expect("review-multiselect fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        let zshrc = fixtures::home(".zshrc");
        let ix = vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                let plan = view.plan(cx);
                plan.rows
                    .iter()
                    .position(|r| {
                        matches!(r, review::SidebarRow::Target { entry, .. } if entry.target == zshrc)
                    })
                    .expect(".zshrc is a row of the pose")
            })
        });
        let selector: &'static str = Box::leak(format!("review-row-{ix}").into_boxed_str());
        let row = vis.debug_bounds(selector).expect("row rendered");
        let column = vis
            .debug_bounds("review-sidebar-rows")
            .expect("the sidebar rows column rendered");
        assert_eq!(
            row.size.width, column.size.width,
            "the row spans the rows column (row {row:?}, column {column:?})"
        );
    }

    /// Keep disk on a row far down a long list, through the real path (the
    /// decision enqueued, then the auto-advance — what `run_action` does):
    /// the next row is revealed (spec 2026-10-11 §2.2), not left off-screen.
    /// The decision moves the row up into PROCESSING, splicing hundreds of
    /// rows that stay unmeasured (height 0) even after the next frame's
    /// layout, which measures only the rows on screen; the next row itself
    /// was measured on an earlier visit. Its bounds therefore put it near
    /// the top, so the reveal must not trust them for a row the last frame
    /// did not render (`rendered_range`).
    ///
    /// MUTATION: treat every row as measured (`measured = true`) → the
    /// stale bounds read "nearly visible", `scroll_to_reveal_item` aims
    /// with zero heights, and the scroll top lands at 5, not near 447.
    /// MUTATION: never set `reveal` in `advance_from` → scroll top 0.
    /// (Applying the reveal on the splice frame is pinned by
    /// `an_auto_advance_onto_the_row_below_the_viewport_scrolls_it_in`.)
    #[gpui::test]
    fn an_auto_advance_far_down_the_list_scrolls_to_the_new_selection(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-many", smoke_paths(), cx).expect("review-many fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        let (resolved, next) = vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                let plan = view.plan(cx);
                (
                    plan.visible_safe[400].clone(),
                    plan.visible_safe[401].clone(),
                )
            })
        });
        // The user has been down there before: the rows around `next` were
        // rendered, so the list holds MEASURED heights for them, and the
        // view is back at the top. On the splice frame `bounds_for_item`
        // then finds `next` measured but every row between the top and it
        // spliced to height 0 — `next` reads visible near the viewport top.
        let resolved_ix = vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                view.plan(cx)
                    .rows
                    .iter()
                    .position(|r| {
                        matches!(r, review::SidebarRow::Target { entry, .. } if entry.target == resolved)
                    })
                    .expect("the resolved row is in the plan")
            })
        });
        for item_ix in [resolved_ix.saturating_sub(5), 0] {
            vis.update(|_w, cx| {
                review.update(cx, |view, cx| {
                    view.list_state.scroll_to(gpui::ListOffset {
                        item_ix,
                        offset_in_item: px(0.),
                    });
                    cx.notify();
                })
            });
            vis.run_until_parked();
        }
        vis.update(|_w, cx| {
            review.update(cx, |view, _| view.selected = Some(resolved.clone()));
            // No engine is published and the pose's queue is pumping, so
            // the decision parks under PROCESSING.
            shell.update(cx, |s, cx| {
                s.enqueue(
                    QueuedAction {
                        target: resolved.clone(),
                        decision: Decision::KeepDisk,
                        notify_on_done: false,
                    },
                    cx,
                )
            });
            review.update(cx, |view, cx| view.advance_from(&resolved, cx));
        });
        // The splice frame, then the frame the deferred reveal asked for.
        vis.run_until_parked();
        vis.run_until_parked();
        assert_eq!(
            vis.update(|_w, cx| review.read(cx).selected.clone()),
            Some(next.clone())
        );
        let (top, next_ix) = vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                let plan = view.plan(cx);
                let next_ix = plan
                    .rows
                    .iter()
                    .position(|r| {
                        matches!(r, review::SidebarRow::Target { entry, .. } if entry.target == next)
                    })
                    .expect("the next row is in the plan");
                (view.list_state.logical_scroll_top().item_ix, next_ix)
            })
        });
        assert!(
            top <= next_ix && next_ix - top < 40,
            "the list scrolled to the new selection (scroll top {top}, next row {next_ix})"
        );
    }

    /// The first decision on an UNSCROLLED long list: the new PROCESSING
    /// group must appear at the top, not above the viewport. gpui's own
    /// splice leaves a top-of-list anchor alone (new rows at index 0 are
    /// simply shown); the by-key re-anchor must not override that by
    /// following the old first row down.
    ///
    /// MUTATION: drop the `top == {0, 0}` guard (re-anchor unconditionally)
    /// → the scroll top lands on item 2 and PROCESSING is hidden.
    #[gpui::test]
    fn the_first_decision_on_an_unscrolled_list_keeps_processing_in_view(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-many", smoke_paths(), cx).expect("review-many fixture")
        });
        let review = review_of(&shell, vis);
        // No PROCESSING group yet: the pose's queue is emptied, so the
        // first decision below CREATES the group at index 0.
        vis.update(|_w, cx| {
            shell.update(cx, |s, cx| {
                s.queue = Default::default();
                s.queue_pumping = false;
                cx.notify();
            })
        });
        vis.run_until_parked();
        let (top, first_is_processing) = vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                let plan = view.plan(cx);
                let is_processing = matches!(
                    plan.rows.first(),
                    Some(review::SidebarRow::Group {
                        label: "PROCESSING",
                        ..
                    })
                );
                (view.list_state.logical_scroll_top().item_ix, is_processing)
            })
        });
        assert_eq!(top, 0, "the list starts unscrolled");
        assert!(
            !first_is_processing,
            "no PROCESSING group before the decision"
        );
        let first_safe = vis
            .update(|_w, cx| review.update(cx, |view, cx| view.plan(cx).visible_safe[0].clone()));
        vis.update(|_w, cx| {
            shell.update(cx, |s, cx| {
                s.enqueue(
                    QueuedAction {
                        target: first_safe.clone(),
                        decision: Decision::KeepDisk,
                        notify_on_done: false,
                    },
                    cx,
                )
            });
            review.update(cx, |view, cx| {
                view.selected = Some(first_safe.clone());
                view.advance_from(&first_safe, cx);
            });
        });
        vis.run_until_parked();
        vis.run_until_parked();
        let (top, first_is_processing) = vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                let plan = view.plan(cx);
                let is_processing = matches!(
                    plan.rows.first(),
                    Some(review::SidebarRow::Group {
                        label: "PROCESSING",
                        ..
                    })
                );
                (view.list_state.logical_scroll_top().item_ix, is_processing)
            })
        });
        assert!(
            first_is_processing,
            "the decision created a PROCESSING group at the top"
        );
        assert_eq!(top, 0, "and the list still shows the top");
    }

    /// Keep disk on the last file of a folder at the bottom edge: the next
    /// file is the first of the NEXT folder, below that folder's row, which
    /// is the first row under the edge. The decided row leaves for
    /// PROCESSING and everything below it moves up one row — still leaving
    /// the next file under the edge. The decision splices every row from
    /// the PROCESSING group down to the decided one; on that frame those
    /// rows are unmeasured (height 0), so `bounds_for_item` puts the next
    /// file near the top and it reads visible. The reveal must wait for the
    /// next frame, whose layout measured the rows on screen, and scroll it
    /// in. (When the next file is the very next row it simply moves up into
    /// the decided row's place and no scroll is needed.)
    ///
    /// MUTATION: apply the reveal on the splice frame (drop `!changed &&`
    /// from the reveal's `if`) → the next row stays below the bottom edge.
    #[gpui::test]
    fn an_auto_advance_onto_the_row_below_the_viewport_scrolls_it_in(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-many", smoke_paths(), cx).expect("review-many fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        // Find a scroll top whose first row below the bottom edge (`below`)
        // is a folder row between two file rows: the advance from the last
        // file of one folder lands on the first file of the next.
        let mut found = None;
        for start in 200..230 {
            vis.update(|_w, cx| {
                review.update(cx, |view, cx| {
                    view.list_state.scroll_to(gpui::ListOffset {
                        item_ix: start,
                        offset_in_item: px(0.),
                    });
                    cx.notify();
                })
            });
            vis.run_until_parked();
            found = vis.update(|_w, cx| {
                review.update(cx, |view, cx| {
                    let plan = view.plan(cx);
                    let viewport = view.list_state.viewport_bounds();
                    let below = (start..plan.rows.len()).find(|&ix| {
                        view.list_state
                            .bounds_for_item(ix)
                            .is_some_and(|b| b.bottom() > viewport.bottom())
                    })?;
                    let target = |ix: usize| match plan.rows.get(ix) {
                        Some(review::SidebarRow::Target {
                            entry,
                            selectable: true,
                            ..
                        }) => Some(entry.target.clone()),
                        _ => None,
                    };
                    let folder_row = matches!(
                        plan.rows.get(below),
                        Some(review::SidebarRow::Folder { .. })
                    );
                    if !folder_row {
                        return None;
                    }
                    Some((target(below - 1)?, target(below + 1)?))
                })
            });
            if found.is_some() {
                break;
            }
        }
        let (resolved, next) = found.expect("a scroll top with a folder row at the bottom edge");
        vis.update(|_w, cx| {
            review.update(cx, |view, _| view.selected = Some(resolved.clone()));
            shell.update(cx, |s, cx| {
                s.enqueue(
                    QueuedAction {
                        target: resolved.clone(),
                        decision: Decision::KeepDisk,
                        notify_on_done: false,
                    },
                    cx,
                )
            });
            review.update(cx, |view, cx| view.advance_from(&resolved, cx));
        });
        vis.run_until_parked();
        vis.run_until_parked();
        assert_eq!(
            vis.update(|_w, cx| review.read(cx).selected.clone()),
            Some(next.clone())
        );
        let (bounds, viewport) = vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                let plan = view.plan(cx);
                let ix = plan
                    .rows
                    .iter()
                    .position(|r| {
                        matches!(r, review::SidebarRow::Target { entry, .. } if entry.target == next)
                    })
                    .expect("the next row is in the plan");
                (
                    view.list_state.bounds_for_item(ix),
                    view.list_state.viewport_bounds(),
                )
            })
        });
        let bounds = bounds.expect("the next row was rendered");
        assert!(
            bounds.top() >= viewport.top() && bounds.bottom() <= viewport.bottom(),
            "the next row is fully on screen: {bounds:?} in {viewport:?}"
        );
    }

    /// Scroll the pose to item 200, render, and hand back the row key the
    /// anchor sits on.
    fn anchor_at_200(review: &Entity<ReviewView>, vis: &mut VisualTestContext) -> review::RowKey {
        vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                view.list_state.scroll_to(gpui::ListOffset {
                    item_ix: 200,
                    offset_in_item: px(0.),
                });
                cx.notify();
            })
        });
        vis.run_until_parked();
        vis.update(|_w, cx| {
            let view = review.read(cx);
            assert_eq!(view.list_state.logical_scroll_top().item_ix, 200);
            view.row_keys[200].clone()
        })
    }

    /// Enqueue Keep disk on `target` (parks: no engine, the pose pumps).
    fn decide(shell: &Entity<Shell>, target: PathBuf, vis: &mut VisualTestContext) {
        vis.update(|_w, cx| {
            shell.update(cx, |s, cx| {
                s.enqueue(
                    QueuedAction {
                        target,
                        decision: Decision::KeepDisk,
                        notify_on_done: false,
                    },
                    cx,
                )
            })
        });
        vis.run_until_parked();
    }

    /// The scroll anchor's row, by key, in the list's current rows.
    fn anchor_key_ix(
        review: &Entity<ReviewView>,
        key: &review::RowKey,
        vis: &mut VisualTestContext,
    ) -> (usize, usize) {
        vis.update(|_w, cx| {
            let view = review.read(cx);
            (
                view.list_state.logical_scroll_top().item_ix,
                view.row_keys
                    .iter()
                    .position(|k| k == key)
                    .expect("the anchor row is still listed"),
            )
        })
    }

    /// A decision on a row INSIDE the viewport, below the scroll top, moves
    /// that row up into PROCESSING: the splice window runs from the
    /// PROCESSING group down past the anchor, and gpui snaps an anchor
    /// inside the window to the window's start. The view must put it back
    /// on its row — the list must not jump to the top on every decision.
    ///
    /// MUTATION: drop the re-anchor in `render_inner` (`if false && ..`) →
    /// the scroll top snaps to the splice start (~5). MUTATION:
    /// `re_anchor_index` returns `Some(anchor_ix)` unchanged → off by one
    /// (the decided row now sits above the anchor).
    #[gpui::test]
    fn a_decision_inside_the_viewport_keeps_the_scroll_anchor(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-many", smoke_paths(), cx).expect("review-many fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        let key = anchor_at_200(&review, vis);
        let decided = vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                let plan = view.plan(cx);
                plan.rows[205..]
                    .iter()
                    .find_map(|r| match r {
                        review::SidebarRow::Target {
                            entry,
                            selectable: true,
                            ..
                        } => Some(entry.target.clone()),
                        _ => None,
                    })
                    .expect("a safe row just below the anchor")
            })
        });
        decide(&shell, decided, vis);
        let (top, key_ix) = anchor_key_ix(&review, &key, vis);
        assert_eq!(top, key_ix, "the scroll top stays on the row it was on");
    }

    /// A decision on a row ABOVE the viewport: the splice window ends above
    /// the anchor, so gpui shifts the anchor by the count change itself —
    /// and the re-anchor must agree. The pose's queue is emptied first so
    /// the decision also brings the PROCESSING header back: +2 rows above,
    /// −1 where the row left, so the anchor row moves down by one.
    ///
    /// MUTATION: `re_anchor_index` returns `Some(anchor_ix)` unchanged → the
    /// scroll top stays at 200 while its row moved to 201.
    #[gpui::test]
    fn a_decision_on_a_row_above_the_viewport_keeps_the_scroll_anchor(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-many", smoke_paths(), cx).expect("review-many fixture")
        });
        let review = review_of(&shell, vis);
        vis.update(|_w, cx| {
            shell.update(cx, |s, cx| {
                // Still pumping: a decision parks rather than run.
                s.queue = Default::default();
                cx.notify();
            })
        });
        vis.run_until_parked();
        let key = anchor_at_200(&review, vis);
        let decided = vis
            .update(|_w, cx| review.update(cx, |view, cx| view.plan(cx).visible_safe[50].clone()));
        let decided_ix =
            vis.update(|_w, cx| {
                review.read(cx).row_keys.iter().position(
                    |k| matches!(k, review::RowKey::Target { path, .. } if *path == decided),
                )
            });
        assert!(
            decided_ix.is_some_and(|ix| ix < 200),
            "{decided_ix:?} is above the anchor"
        );
        decide(&shell, decided, vis);
        let (top, key_ix) = anchor_key_ix(&review, &key, vis);
        assert_eq!(
            key_ix, 201,
            "the PROCESSING header and row above, one row gone"
        );
        assert_eq!(top, key_ix, "the scroll top stays on the row it was on");
    }

    /// An ordinary rebuild that changes the row count keeps the list's
    /// item count in step with the plan (rows + the In sync footer).
    ///
    /// MUTATION: skip the splice (`let _ = (range, count);`) → the list
    /// keeps the old count and the two diverge.
    #[gpui::test]
    fn an_ordinary_queue_change_keeps_the_item_count_in_step(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-many", smoke_paths(), cx).expect("review-many fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        let before = vis.update(|_w, cx| review.read(cx).plan_rows_len_for_test());
        let state = vis.update(|_w, cx| shell.read(cx).state.clone());
        vis.update(|_w, cx| {
            state.update(cx, |model, cx| {
                model.drifted.pop();
                cx.notify();
            })
        });
        vis.run_until_parked();
        let (items, rows) = vis.update(|_w, cx| {
            let v = review.read(cx);
            (v.list_state.item_count(), v.plan_rows_len_for_test())
        });
        assert_ne!(rows, before, "the change altered the row count");
        assert_eq!(items, rows + 1, "rows plus the In sync footer");
    }

    // ---- Structure mode in the merge editor (structured-merge Task 10) ----

    /// `debug_bounds` wants a `&'static str`; leaking a short selector per
    /// lookup is the price of keying it by decision id.
    fn leak(selector: String) -> &'static str {
        Box::leak(selector.into_boxed_str())
    }

    /// A merge editor posed on `inputs` inside a Shell on the merge route,
    /// with the merge editor's key bindings registered.
    fn structured_merge_window(
        cx: &mut TestAppContext,
        inputs: tomte_app::merge_inputs::MergeInputs,
    ) -> (
        Entity<Shell>,
        Entity<merge::MergeView>,
        &mut VisualTestContext,
    ) {
        cx.update(merge::register_keys);
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            let mut s = fixtures::build("merge-resolved", smoke_paths(), cx)
                .expect("merge-resolved fixture");
            s.merge = Some(fixtures::posed_merge(cx, inputs, false));
            s
        });
        let merge = vis
            .update(|_w, cx| shell.read(cx).merge.clone())
            .expect("posed merge");
        vis.run_until_parked();
        (shell, merge, vis)
    }

    fn result_text(merge: &Entity<merge::MergeView>, vis: &mut VisualTestContext) -> String {
        vis.update(|_w, cx| merge.read(cx).result_text_for_test(cx))
    }

    /// The rows the merge editor planned last, for tests.
    fn tree_rows(
        merge: &Entity<merge::MergeView>,
        vis: &mut VisualTestContext,
    ) -> Vec<tomte_app::structured_tree::TreeRow> {
        vis.update(|_w, cx| merge.read(cx).tree_rows_for_test())
    }

    /// Decision `id`'s row in `rows`.
    fn decision_row(
        rows: &[tomte_app::structured_tree::TreeRow],
        id: usize,
    ) -> (Vec<tomte_app::structured_tree::Chip>, bool) {
        use tomte_app::structured_tree::TreeRow;
        rows.iter()
            .find_map(|row| match row {
                TreeRow::Decision {
                    id: d, chips, open, ..
                } if *d == id => Some((chips.clone(), *open)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("decision {id} has a row"))
    }

    /// A JSON file opens in Structure mode as one tree of rows: a row per
    /// decision, no input panes and no result pane.
    ///
    /// MUTATION: `body` draws the panes beside the tree in Structure mode
    /// (the tree rows still render) → a `merge-pane-divider` bound exists.
    /// A body that draws the panes INSTEAD of the tree fails earlier, on
    /// the missing `tree-row:<id>`.
    /// MUTATION: `structured_load` returns an error instead of running
    /// `detect` → no structured document, and the "a structured document"
    /// expectation fails before any row is checked.
    #[gpui::test]
    fn json_inputs_open_in_structure_mode_as_a_tree_with_no_panes(cx: &mut TestAppContext) {
        use tomte_app::structured_tree::TreeRow;
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        let (mode, ids) = vis.update(|_w, cx| {
            let view = merge.read(cx);
            let loaded = view.loaded.as_ref().expect("loaded");
            let state = loaded.structured.as_ref().expect("a structured document");
            (
                loaded.mode,
                state.doc.decisions.iter().map(|d| d.id).collect::<Vec<_>>(),
            )
        });
        assert_eq!(mode, merge::MergeMode::Structure);
        assert_eq!(
            ids.len(),
            5,
            "theme, font_size, vim_mode, one plugins element, tab_size"
        );
        for id in ids {
            let selector = leak(format!("tree-row:{id}"));
            assert!(vis.debug_bounds(selector).is_some(), "{selector} renders");
        }
        assert!(
            vis.debug_bounds("merge-pane-divider").is_none(),
            "no input panes"
        );
        assert!(
            vis.debug_bounds("merge-result-scroll").is_none(),
            "no result pane"
        );
        let first_key = tree_rows(&merge, vis)
            .into_iter()
            .find_map(|row| match row {
                TreeRow::Decision { key, .. } => Some(key),
                _ => None,
            });
        assert_eq!(first_key, Some(Some("\"theme\"".to_string())));
    }

    /// A file whose disk text equals its source (only origin differs)
    /// opens in Lines with the "disk matches source" note, still offering
    /// Structure; in Structure the note stands in for the progress words,
    /// points to Lines, and Save is enabled at once (nothing is open).
    /// What the chip drew is read from `take_same_notes_drawn`, because
    /// `debug_bounds` keeps the Lines frame's entry after the switch.
    ///
    /// MUTATION: `with_structured` keeps Structure as the default → mode is Structure.
    /// MUTATION: the note is built from `ours == base` → no note (origin case).
    /// The fixture's base equals its source, so that second mutation is
    /// killed by the assertion on inputs whose base differs, below.
    /// MUTATION: `header_controls` draws nothing in its note branch → no
    /// note is drawn after the switch to Structure.
    /// MUTATION: Lines mode draws the see-Lines wording → the Lines header
    /// drew "… · see Lines".
    #[gpui::test]
    fn disk_equal_to_source_opens_in_lines_with_the_note(cx: &mut TestAppContext) {
        use tomte_app::structured_tree::TreeRow;
        const LINES_NOTE: &str = "disk matches source · origin differs";
        const STRUCTURE_NOTE: &str = "disk matches source · origin differs · see Lines";
        merge_structured::take_same_notes_drawn();
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_same_inputs());
        let (mode, has_structure) = vis.update(|_w, cx| {
            let loaded = merge.read(cx).loaded.as_ref().expect("loaded");
            (loaded.mode, loaded.structured.is_some())
        });
        assert_eq!(mode, merge::MergeMode::Lines);
        assert!(has_structure, "Structure stays on offer");
        let drawn = merge_structured::take_same_notes_drawn();
        assert!(!drawn.is_empty(), "the Lines header draws the note");
        assert!(drawn.iter().all(|n| *n == LINES_NOTE), "{drawn:?}");
        let structure = vis
            .debug_bounds("merge-mode-structure")
            .expect("the mode toggle offers Structure");
        // The note reads the two texts, not the base.
        let base_differs = tomte_app::merge_inputs::MergeInputs {
            base: fixtures::json_inputs().base,
            ..fixtures::json_same_inputs()
        };
        assert_eq!(
            merge_structured::same_text_note(&base_differs, merge::MergeMode::Lines),
            Some(LINES_NOTE)
        );

        vis.simulate_click(structure.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        let (mode, gate) = vis.update(|_w, cx| {
            let view = merge.read(cx);
            (view.loaded.as_ref().expect("loaded").mode, view.save_gate())
        });
        assert_eq!(mode, merge::MergeMode::Structure);
        assert!(
            !tree_rows(&merge, vis)
                .iter()
                .any(|row| matches!(row, TreeRow::Decision { open: true, .. })),
            "nothing is open"
        );
        // The mouse-down redraws the Lines header before the switch; the
        // frame after it is Structure's.
        let drawn = merge_structured::take_same_notes_drawn();
        assert_eq!(drawn.last(), Some(&STRUCTURE_NOTE), "{drawn:?}");
        // The progress words never rendered in this window: the note
        // stands in for them in Structure, and Lines has its own.
        assert!(vis.debug_bounds("merge-progress").is_none());
        assert_eq!(gate, Some(merge::SaveGate::Enabled));
        assert!(vis.debug_bounds("merge-save").is_some());
    }

    /// A real click on a chip picks that side: the assembled text changes,
    /// the row's chip reads picked and its edge goes, and ⌘Z through the
    /// merge editor's binding puts it back.
    ///
    /// MUTATION: the chip click handler moves the cursor only → the
    /// assembled text still reads "Solarized".
    #[gpui::test]
    fn a_chip_click_picks_that_side_and_changes_the_assembled_text(cx: &mut TestAppContext) {
        use tomte_app::structured_tree::ChipSide;
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        let assembled = |vis: &mut VisualTestContext| {
            vis.update(|_w, cx| {
                let state = merge
                    .read(cx)
                    .loaded
                    .as_ref()
                    .unwrap()
                    .structured
                    .as_ref()
                    .unwrap();
                state.assembled().expect("valid")
            })
        };
        assert_eq!(assembled(vis), None, "the theme conflict is open");
        let chip = vis
            .debug_bounds("chip:0:disk")
            .expect("the theme conflict offers disk");
        vis.simulate_click(chip.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        let text = assembled(vis).expect("every conflict picked");
        assert!(
            text.contains("\"Gruvbox\"") && !text.contains("\"Solarized\""),
            "disk's theme replaced source's: {text}"
        );
        let (chips, open) = decision_row(&tree_rows(&merge, vis), 0);
        assert!(!open, "a picked row loses its edge");
        let disk = chips.iter().find(|c| c.side == ChipSide::Disk).unwrap();
        assert!(disk.picked, "the disk chip reads picked");

        vis.simulate_keystrokes("cmd-z");
        vis.run_until_parked();
        assert_eq!(assembled(vis), None, "undo reopens the conflict");
        let (chips, open) = decision_row(&tree_rows(&merge, vis), 0);
        assert!(open);
        assert!(chips.iter().all(|c| !c.picked));
    }

    /// An unpicked conflict offers source, disk and last written, none of
    /// them picked.
    ///
    /// MUTATION: `open` chips drawn with a pick → the conflict row has a
    /// picked chip.
    #[gpui::test]
    fn an_open_conflict_row_shows_three_chips_and_no_pick(cx: &mut TestAppContext) {
        use tomte_app::structured_tree::ChipSide;
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        for side in ["source", "disk", "last-written"] {
            let selector = leak(format!("chip:0:{side}"));
            assert!(vis.debug_bounds(selector).is_some(), "{selector} renders");
        }
        let (chips, open) = decision_row(&tree_rows(&merge, vis), 0);
        assert!(open, "the theme conflict needs the user");
        assert_eq!(
            chips.iter().map(|c| c.side).collect::<Vec<_>>(),
            vec![ChipSide::Source, ChipSide::Disk, ChipSide::LastWritten]
        );
        assert!(chips.iter().all(|c| !c.picked), "{chips:?}");
    }

    /// The tree is planned once per change, not once per frame.
    ///
    /// MUTATION: `replan` ignores `tree_dirty` and plans every render →
    /// plan_runs grows across two idle frames.
    #[gpui::test]
    fn the_plan_is_computed_once_per_change(cx: &mut TestAppContext) {
        use tomte_core::structured::Pick;
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        let runs = |vis: &mut VisualTestContext| {
            vis.update(|_w, cx| merge.read(cx).loaded.as_ref().unwrap().plan_runs)
        };
        // Two more frames with nothing changed.
        for _ in 0..2 {
            vis.update(|_w, cx| merge.update(cx, |_, cx| cx.notify()));
            vis.run_until_parked();
        }
        assert_eq!(
            runs(vis),
            1,
            "the first frame planned, the idle ones did not"
        );
        vis.update(|_w, cx| merge.update(cx, |v, cx| v.apply_pick(0, Pick::Disk, cx)));
        vis.run_until_parked();
        assert_eq!(runs(vis), 2, "one pick, one plan");
    }

    /// A container identical on every side opens folded; a click on its row
    /// unfolds it in place and a second click folds it again.
    ///
    /// MUTATION: `toggle_container` never flips the override → the row
    /// count is unchanged.
    /// MUTATION: the planner emits a `Close` after a folded `Open` → the
    /// unfolded count is `folded + 2`.
    #[gpui::test]
    fn clicking_an_identical_container_row_folds_and_unfolds_it(cx: &mut TestAppContext) {
        use tomte_app::structured_tree::TreeRow;
        let base = "{\n  \"o\": {\n    \"p\": 1,\n    \"q\": 2\n  },\n  \"a\": 1\n}\n";
        let ours = "{\n  \"o\": {\n    \"p\": 1,\n    \"q\": 2\n  },\n  \"a\": 2\n}\n";
        let inputs = tomte_app::merge_inputs::MergeInputs {
            target: PathBuf::from("/tmp/smoke/settings.json"),
            ours: ours.into(),
            theirs: base.into(),
            base: Some(base.into()),
            source_path: PathBuf::from("/tmp/smoke/src/settings.json"),
            templated: false,
            template: None,
            span_map: None,
            origin: None,
        };
        let (_shell, merge, vis) = structured_merge_window(cx, inputs);
        let rows = tree_rows(&merge, vis);
        let ix = rows
            .iter()
            .position(|row| {
                matches!(
                    row,
                    TreeRow::Open {
                        same: true,
                        folded: true,
                        ..
                    }
                )
            })
            .expect("the identical object opens folded");
        let folded = rows.len();
        let selector = leak(format!("tree-line:{ix}"));
        let click = |vis: &mut VisualTestContext| {
            let row = vis
                .debug_bounds(selector)
                .expect("the container row renders");
            vis.simulate_click(row.center(), gpui::Modifiers::default());
            vis.run_until_parked();
        };
        click(vis);
        let rows = tree_rows(&merge, vis);
        // The folded row was the whole container; open, it is the `Open`
        // row, p, q and the `Close` row.
        assert_eq!(
            rows.len(),
            folded + 3,
            "p, q and the close are listed in place"
        );
        assert!(matches!(rows[ix], TreeRow::Open { folded: false, .. }));
        click(vis);
        assert_eq!(tree_rows(&merge, vis).len(), folded, "folded again");
    }

    /// Save in Structure mode queues the structured assembly, not the line
    /// merge's text.
    ///
    /// MUTATION: `save` ignores the mode and assembles the LINE state → the
    /// line merge still has an open conflict, nothing is queued and the
    /// `expect` on the queued action fails.
    #[gpui::test]
    fn save_in_structure_mode_enqueues_the_assembled_json(cx: &mut TestAppContext) {
        use tomte_core::structured::Pick;
        let (shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        let expected = vis.update(|_w, cx| {
            merge.update(cx, |view, cx| {
                let conflicts = view
                    .loaded
                    .as_ref()
                    .unwrap()
                    .structured
                    .as_ref()
                    .unwrap()
                    .conflicts();
                assert_eq!(conflicts, vec![0]);
                for id in conflicts {
                    view.apply_pick(id, Pick::Disk, cx);
                }
                let state = view.loaded.as_ref().unwrap().structured.as_ref().unwrap();
                state.assembled().expect("valid").expect("fully resolved")
            })
        });
        vis.update(|_w, cx| merge.update(cx, |view, cx| view.save(cx)));
        vis.run_until_parked();
        let action = vis
            .update(|_w, cx| {
                shell.update(cx, |s, _| {
                    s.queue
                        .running()
                        .first()
                        .cloned()
                        .or_else(|| s.queue.take_batch().into_iter().next())
                })
            })
            .expect("the save is on the queue");
        match action.decision {
            Decision::Merged { resolved, .. } => {
                assert_eq!(resolved, expected);
                assert!(resolved.contains("\"Gruvbox\""));
                assert!(tomte_core::structured::jsonc::parse(&resolved).is_ok());
            }
            other => panic!("expected a merged decision, got {:?}", other.label()),
        }
    }

    /// The Lines toggle switches at once with no picks made; with a pick it
    /// arms a confirmation first and switches on the second click.
    ///
    /// MUTATION: `set_mode` switches without arming when picks exist → the
    /// mode is Lines after the first click and the banner assertion fails.
    /// MUTATION: `body` keeps drawing the tree in Lines mode → no
    /// `merge-pane-divider` renders.
    #[gpui::test]
    fn the_lines_toggle_shows_the_line_editor_and_arms_a_confirmation_when_picks_exist(
        cx: &mut TestAppContext,
    ) {
        use tomte_core::structured::Pick;
        {
            let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
            assert!(vis.debug_bounds("tree-row:0").is_some(), "the tree renders");
            assert!(vis.debug_bounds("merge-pane-divider").is_none());
            let lines = vis
                .debug_bounds("merge-mode-lines")
                .expect("the toggle renders");
            vis.simulate_click(lines.center(), gpui::Modifiers::default());
            vis.run_until_parked();
            let (mode, banner) = vis.update(|_w, cx| {
                let v = merge.read(cx);
                (v.loaded.as_ref().unwrap().mode, v.banner.clone())
            });
            assert_eq!(
                mode,
                merge::MergeMode::Lines,
                "no picks: one click switches"
            );
            assert!(
                vis.debug_bounds("merge-pane-divider").is_some(),
                "the line editor's panes render"
            );
            assert!(banner.is_none(), "nothing to confirm");
            let text = result_text(&merge, vis);
            assert!(
                text.contains("\"Gruvbox\"") && text.contains("\"Solarized\""),
                "the line editor inlines disk then source for the open conflict: {text}"
            );
        }
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        vis.update(|_w, cx| merge.update(cx, |v, cx| v.apply_pick(0, Pick::Disk, cx)));
        vis.run_until_parked();
        let lines = vis
            .debug_bounds("merge-mode-lines")
            .expect("the toggle renders");
        vis.simulate_click(lines.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        let (mode, banner) = vis.update(|_w, cx| {
            let v = merge.read(cx);
            (v.loaded.as_ref().unwrap().mode, v.banner.clone())
        });
        assert_eq!(
            mode,
            merge::MergeMode::Structure,
            "the first click only arms"
        );
        let banner = banner.expect("the armed switch says what it discards");
        assert!(
            banner.text.starts_with("Switching to Lines discards"),
            "{}",
            banner.text
        );
        vis.simulate_click(lines.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        let (mode, pick, banner) = vis.update(|_w, cx| {
            let v = merge.read(cx);
            let loaded = v.loaded.as_ref().unwrap();
            (
                loaded.mode,
                loaded.structured.as_ref().unwrap().resolution.get(0),
                v.banner.clone(),
            )
        });
        assert_eq!(mode, merge::MergeMode::Lines, "the second click switches");
        assert_eq!(pick, None, "the structure picks are discarded");
        assert!(banner.is_none());
    }

    /// JSON inputs whose keys `names` are each a conflict (base 1, disk 2,
    /// source 3), with `filler` identical keys after the first.
    fn conflicts_inputs(names: &[&str], filler: usize) -> tomte_app::merge_inputs::MergeInputs {
        let side = |v: u8| {
            let mut entries: Vec<String> = Vec::new();
            for (i, name) in names.iter().enumerate() {
                entries.push(format!("  \"{name}\": {v}"));
                if i == 0 {
                    entries.extend((0..filler).map(|k| format!("  \"same{k}\": 0")));
                }
            }
            format!("{{\n{}\n}}\n", entries.join(",\n"))
        };
        tomte_app::merge_inputs::MergeInputs {
            target: PathBuf::from("/tmp/smoke/settings.json"),
            ours: side(2),
            theirs: side(3),
            base: Some(side(1)),
            source_path: PathBuf::from("/tmp/smoke/src/settings.json"),
            templated: false,
            template: None,
            span_map: None,
            origin: None,
        }
    }

    fn structured_cursor(
        merge: &Entity<merge::MergeView>,
        vis: &mut VisualTestContext,
    ) -> Option<usize> {
        vis.update(|_w, cx| {
            merge
                .read(cx)
                .loaded
                .as_ref()
                .unwrap()
                .structured
                .as_ref()
                .unwrap()
                .cursor
        })
    }

    /// A chip click on a row the cursor is not on picks there and moves
    /// the cursor the way a pick key would from that row: to the next open
    /// conflict after it.
    ///
    /// MUTATION: the chip handler drops its `select_decision` → `pick` sees
    /// the cursor elsewhere, leaves it on `a`, and the cursor reads 0.
    #[gpui::test]
    fn a_chip_click_moves_the_cursor_from_that_row(cx: &mut TestAppContext) {
        let (_shell, merge, vis) =
            structured_merge_window(cx, conflicts_inputs(&["a", "b", "c"], 0));
        assert_eq!(
            structured_cursor(&merge, vis),
            Some(0),
            "the cursor starts on a"
        );
        let chip = vis.debug_bounds("chip:1:disk").expect("b offers disk");
        vis.simulate_click(chip.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        let pick = vis.update(|_w, cx| {
            merge
                .read(cx)
                .loaded
                .as_ref()
                .unwrap()
                .structured
                .as_ref()
                .unwrap()
                .resolution
                .get(1)
        });
        assert_eq!(pick, Some(tomte_core::structured::Pick::Disk));
        assert_eq!(
            structured_cursor(&merge, vis),
            Some(2),
            "on to c, the next open row after b"
        );
    }

    /// Decision `id` of the keyed row `name` in `rows`.
    fn keyed_decision(rows: &[tomte_app::structured_tree::TreeRow], name: &str) -> usize {
        use tomte_app::structured_tree::TreeRow;
        rows.iter()
            .find_map(|row| match row {
                TreeRow::Decision {
                    id, key: Some(k), ..
                } if k == name => Some(*id),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{name} has a decision row"))
    }

    /// Decision `id`'s explicit pick, if any.
    fn structured_pick(
        merge: &Entity<merge::MergeView>,
        vis: &mut VisualTestContext,
        id: usize,
    ) -> Option<tomte_core::structured::Pick> {
        vis.update(|_w, cx| {
            merge
                .read(cx)
                .loaded
                .as_ref()
                .unwrap()
                .structured
                .as_ref()
                .unwrap()
                .resolution
                .get(id)
        })
    }

    /// A pick key for a side the row shows no chip for does nothing (spec
    /// §3.5): 3 on "font_size", changed on disk only, whose row offers no
    /// last written; 1 on the agreed "editor.formatOnSave", whose one chip
    /// reads `source · disk`.
    ///
    /// MUTATION: `pick_allowed` allows last written wherever the base has
    /// the entry (the old `has_base`-only rule) → 3 on "font_size" picks
    /// last written.
    /// MUTATION: `pick_allowed` allows disk and source on every unlocked
    /// row → 1 on "editor.formatOnSave" picks disk.
    #[gpui::test]
    fn a_pick_key_for_a_chip_the_row_lacks_does_nothing(cx: &mut TestAppContext) {
        use tomte_app::structured_tree::ChipSide;
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_tree_inputs());
        let rows = tree_rows(&merge, vis);
        let font = keyed_decision(&rows, "\"font_size\"");
        let agreed = keyed_decision(&rows, "\"editor.formatOnSave\"");
        for (id, key, side) in [
            (font, "3", ChipSide::LastWritten),
            (agreed, "1", ChipSide::Disk),
        ] {
            let (chips, _) = decision_row(&tree_rows(&merge, vis), id);
            assert!(
                chips.iter().all(|c| c.side != side),
                "row {id} shows no {side:?} chip: {chips:?}"
            );
            merge.update(vis, |m, cx| m.select_decision(id, cx));
            vis.run_until_parked();
            assert_eq!(structured_cursor(&merge, vis), Some(id));
            let before = structured_pick(&merge, vis, id);
            vis.simulate_keystrokes(key);
            vis.run_until_parked();
            assert_eq!(
                structured_pick(&merge, vis, id),
                before,
                "{key} on row {id} picked nothing"
            );
            assert_eq!(decision_row(&tree_rows(&merge, vis), id).0, chips);
        }
    }

    /// A container chip has two targets: its body picks that side, its
    /// `▸` glyph unfolds the preview. The gallery's appended hooks object
    /// is an open conflict whose chips are both containers, so the body
    /// is the mouse's only way to pick it.
    ///
    /// MUTATION: the glyph's handler also picks (`apply_pick` beside
    /// `toggle_chip`) → the source glyph click picks source.
    /// MUTATION: the body's handler unfolds instead of picking (the old
    /// container branch) → no pick, and preview rows appear.
    /// MUTATION: `ui::value_chip`'s glyph handler drops its
    /// `stop_propagation` → the click bubbles to the body and picks source.
    #[gpui::test]
    fn a_container_chip_click_picks_and_its_glyph_unfolds(cx: &mut TestAppContext) {
        use tomte_app::structured_tree::TreeRow;
        use tomte_core::structured::Pick;
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_tree_inputs());
        let previews = |rows: &[TreeRow]| {
            rows.iter()
                .filter(|r| matches!(r, TreeRow::Preview { .. }))
                .count()
        };
        let rows = tree_rows(&merge, vis);
        let hooks = rows
            .iter()
            .find_map(|row| match row {
                TreeRow::Decision {
                    id,
                    key: None,
                    chips,
                    open: true,
                    ..
                } if chips.iter().filter(|c| c.container).count() == 2 => Some(*id),
                _ => None,
            })
            .expect("the hooks conflict has two container chips");
        assert_eq!(previews(&rows), 0, "every chip starts folded");
        assert_eq!(structured_pick(&merge, vis, hooks), None);
        merge.update(vis, |m, cx| {
            m.scroll_to_decision(hooks);
            cx.notify();
        });
        vis.run_until_parked();

        // The source glyph unfolds and picks nothing.
        let glyph = vis
            .debug_bounds(leak(format!("chip-unfold:{hooks}:source")))
            .expect("the source chip has a glyph");
        vis.simulate_click(glyph.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        assert_eq!(
            structured_pick(&merge, vis, hooks),
            None,
            "the glyph does not pick"
        );
        let rows = tree_rows(&merge, vis);
        assert!(previews(&rows) > 0, "the glyph unfolded the preview");
        let (chips, _) = decision_row(&rows, hooks);
        assert!(chips.iter().any(|c| c.unfolded));
        // Fold it again so the body click below starts from no previews.
        let glyph = vis
            .debug_bounds(leak(format!("chip-unfold:{hooks}:source")))
            .expect("the unfolded chip keeps its glyph");
        vis.simulate_click(glyph.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        assert_eq!(previews(&tree_rows(&merge, vis)), 0, "a second click folds");

        // The disk chip's body picks disk and unfolds nothing.
        let body = vis
            .debug_bounds(leak(format!("chip:{hooks}:disk")))
            .expect("the disk chip renders");
        let glyph = vis
            .debug_bounds(leak(format!("chip-unfold:{hooks}:disk")))
            .expect("the disk chip has a glyph");
        // Click the body to the right of the glyph, clear of it.
        let point = gpui::point((glyph.right() + body.right()) / 2., body.center().y);
        vis.simulate_click(point, gpui::Modifiers::default());
        vis.run_until_parked();
        assert_eq!(structured_pick(&merge, vis, hooks), Some(Pick::Disk));
        assert_eq!(
            previews(&tree_rows(&merge, vis)),
            0,
            "the body does not unfold"
        );
    }

    /// A pick that moves the cursor to an off-screen conflict scrolls the
    /// tree to that conflict, where the next pick key lands.
    ///
    /// MUTATION: `apply_pick` scrolls to the picked id → `last_scroll_row`
    /// is a's row and the list stays at the top.
    #[gpui::test]
    fn a_pick_scrolls_to_where_the_cursor_went(cx: &mut TestAppContext) {
        use tomte_app::structured_tree::scroll_target;
        let (_shell, merge, vis) = structured_merge_window(cx, conflicts_inputs(&["a", "b"], 200));
        let b_row = scroll_target(&tree_rows(&merge, vis), 1).expect("b has a row");
        assert!(b_row > 200, "b sits below the filler: row {b_row}");
        vis.simulate_keystrokes("1");
        vis.run_until_parked();
        assert_eq!(
            structured_cursor(&merge, vis),
            Some(1),
            "the pick moved the cursor to b"
        );
        let (last, offset) = vis.update(|_w, cx| {
            let v = merge.read(cx);
            let offset = v.tree_scroll.0.borrow().base_handle.offset().y;
            (v.last_scroll_row, offset)
        });
        assert_eq!(last, Some(b_row));
        // b's row is centred: the list scrolled about b_row rows down.
        assert!(
            offset < px(-(b_row as f32 - 60.) * tomte_ui::components::TREE_ROW_H),
            "the list scrolled down to b (offset {offset:?})"
        );
    }

    /// Next ↓ in Structure mode scrolls the tree to the row it moved the
    /// cursor to: first the open conflict, then, once it is picked, every
    /// changed row in turn.
    ///
    /// MUTATION: `next` keeps calling the removed pane jump → last_scroll_row stays None.
    #[gpui::test]
    fn next_scrolls_the_tree_to_the_target_row(cx: &mut TestAppContext) {
        use tomte_app::structured_tree::scroll_target;
        use tomte_core::structured::Pick;
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        let mut landed = Vec::new();
        for press in 0..4 {
            if press == 1 {
                // Pick the one open conflict, so Next walks the changed rows.
                vis.update(|_w, cx| merge.update(cx, |v, cx| v.apply_pick(0, Pick::Disk, cx)));
                vis.run_until_parked();
            }
            vis.update(|_w, cx| merge.update(cx, |v, _| v.last_scroll_row = None));
            vis.update(|_w, cx| merge.update(cx, |v, cx| v.next(cx)));
            vis.run_until_parked();
            let cursor = structured_cursor(&merge, vis).expect("Next lands on a decision");
            let last = vis.update(|_w, cx| merge.read(cx).last_scroll_row);
            let target = scroll_target(&tree_rows(&merge, vis), cursor);
            assert!(
                target.is_some(),
                "press {press}: decision {cursor} has a row"
            );
            assert_eq!(
                last, target,
                "press {press}: the tree scrolled to the cursor"
            );
            landed.push(cursor);
        }
        assert_eq!(landed[0], 0, "the open conflict first");
        assert!(
            landed[1] != landed[2] && landed[2] != landed[3],
            "Next walks the changed rows: {landed:?}"
        );
    }

    /// "Fold identical" collapses every run of identical rows into one
    /// `Fold` row and keeps every decision row; a second click restores
    /// the runs. One click, one plan.
    ///
    /// MUTATION: the toggle flips `fold_identical` but not `tree_dirty` → rows unchanged.
    /// MUTATION: the toggle skips the `kept_top` scroll → `last_scroll_row`
    /// stays None.
    #[gpui::test]
    fn fold_identical_collapses_identical_runs_and_keeps_decisions(cx: &mut TestAppContext) {
        use tomte_app::structured_tree::{TreeRow, scroll_target};
        // json_inputs' identical runs are one row each; three identical keys
        // between two conflicts make a run that folds shorter.
        let (_shell, merge, vis) = structured_merge_window(cx, conflicts_inputs(&["a", "b"], 3));
        let runs = |vis: &mut VisualTestContext| {
            vis.update(|_w, cx| merge.read(cx).loaded.as_ref().unwrap().plan_runs)
        };
        let ids: Vec<usize> = vis.update(|_w, cx| {
            let loaded = merge.read(cx).loaded.as_ref().unwrap();
            let state = loaded.structured.as_ref().unwrap();
            state.doc.decisions.iter().map(|d| d.id).collect()
        });
        let folds = |rows: &[TreeRow]| {
            rows.iter()
                .filter(|row| matches!(row, TreeRow::Fold { .. }))
                .count()
        };
        let unfolded = tree_rows(&merge, vis);
        assert_eq!(folds(&unfolded), 0, "Fold identical starts off");
        let before = runs(vis);

        vis.update(|_w, cx| merge.update(cx, |v, _| v.last_scroll_row = None));
        let toggle = vis
            .debug_bounds("merge-fold-identical")
            .expect("the header draws Fold identical");
        vis.simulate_click(toggle.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        assert_eq!(
            vis.update(|_w, cx| merge.read(cx).last_scroll_row),
            Some(0),
            "the root row, at the top before, stays at the top"
        );
        let folded = tree_rows(&merge, vis);
        assert!(folds(&folded) > 0, "an identical run folded: {folded:?}");
        assert!(folded.len() < unfolded.len());
        for &id in &ids {
            assert!(
                scroll_target(&folded, id).is_some(),
                "decision {id} keeps its row"
            );
            let selector = leak(format!("tree-row:{id}"));
            assert!(vis.debug_bounds(selector).is_some(), "{selector} renders");
        }
        assert_eq!(runs(vis), before + 1, "one click, one plan");

        let toggle = vis.debug_bounds("merge-fold-identical").unwrap();
        vis.simulate_click(toggle.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        let restored = tree_rows(&merge, vis);
        assert_eq!(folds(&restored), 0, "the second click restores the runs");
        assert_eq!(restored, unfolded);
    }

    /// "Fold identical" keeps the row at the viewport's top: scrolled so
    /// decision a's row is at the top (offset 40 px, inside the list's 4 px
    /// top padding plus one 20 px row), the toggle keeps a at the top
    /// instead of the 200-row identical run below it.
    ///
    /// MUTATION: `tree_top_row` returns 0 → the root row, Some(0).
    /// MUTATION: `tree_top_row` ignores the list's top padding → row 2, the
    /// first identical row, which folds: the fold row, Some(2).
    #[gpui::test]
    fn fold_identical_keeps_the_scrolled_top_row(cx: &mut TestAppContext) {
        use tomte_app::structured_tree::{TreeRow, scroll_target};
        let (_shell, merge, vis) = structured_merge_window(cx, conflicts_inputs(&["a", "b"], 200));
        let old = tree_rows(&merge, vis);
        assert_eq!(scroll_target(&old, 0), Some(1), "a is row 1");
        assert!(
            matches!(old[2], TreeRow::Same { .. }),
            "the run starts at 2"
        );
        vis.update(|_w, cx| {
            merge.update(cx, |v, _| {
                v.tree_scroll
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(gpui::point(px(0.), px(-40.)));
                v.last_scroll_row = None;
            })
        });
        vis.run_until_parked();
        let offset =
            vis.update(|_w, cx| merge.read(cx).tree_scroll.0.borrow().base_handle.offset().y);
        assert_eq!(offset, px(-40.), "the offset held through a frame");

        let toggle = vis.debug_bounds("merge-fold-identical").unwrap();
        vis.simulate_click(toggle.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        let new = tree_rows(&merge, vis);
        assert!(new.len() < old.len(), "the run folded");
        let last = vis.update(|_w, cx| merge.read(cx).last_scroll_row);
        assert_eq!(last, merge_structured::kept_top(&old, &new, 1));
        assert_eq!(last, scroll_target(&new, 0), "a stays at the top");
    }

    /// A save the write-back would refuse never reaches the queue: two free
    /// picks on adjacent lines of different template literals read as one
    /// edit. The message box names it and the picks stay.
    ///
    /// MUTATION: `save` skips `template_refusal` → the save is queued (the
    /// engine would refuse it later) and `error` is None.
    #[gpui::test]
    fn a_structured_save_the_write_back_refuses_is_a_message_box(cx: &mut TestAppContext) {
        let (shell, merge, vis) =
            structured_merge_window(cx, fixtures::json_adjacent_literals_inputs());
        vis.update(|_w, cx| merge.update(cx, |v, cx| v.save(cx)));
        vis.run_until_parked();
        let (error, mode) = vis.update(|_w, cx| {
            let v = merge.read(cx);
            (v.error.clone(), v.loaded.as_ref().unwrap().mode)
        });
        let error = error.expect("a message box");
        assert!(
            error.starts_with("the merged text touches a templated line: "),
            "{error}"
        );
        assert_eq!(mode, merge::MergeMode::Structure);
        vis.update(|_w, cx| {
            let s = shell.read(cx);
            assert_eq!(s.queue.depth(), 0, "nothing was queued");
            assert_eq!(s.route, Route::Merge, "the editor stays open");
        });
    }

    /// An assembly that does not re-parse is the message-box state: the
    /// picks stay, nothing is queued, and the header still offers Lines.
    ///
    /// MUTATION: map `Err(Invalid)` to a disabled Save (return early without
    /// setting `error`) → no message box (`error` is None), nothing logged.
    #[gpui::test]
    fn an_invalid_assembly_is_a_message_box_that_keeps_the_picks_and_offers_lines(
        cx: &mut TestAppContext,
    ) {
        use tomte_core::structured::Pick;
        let (shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        vis.update(|_w, cx| {
            merge.update(cx, |v, cx| {
                v.loaded
                    .as_mut()
                    .unwrap()
                    .structured
                    .as_mut()
                    .unwrap()
                    .doc
                    .corrupt_source_for_tests();
                v.apply_pick(0, Pick::Disk, cx);
                v.save(cx);
            })
        });
        vis.run_until_parked();
        let (error, pick, mode) = vis.update(|_w, cx| {
            let v = merge.read(cx);
            let loaded = v.loaded.as_ref().unwrap();
            (
                v.error.clone(),
                loaded.structured.as_ref().unwrap().resolution.get(0),
                loaded.mode,
            )
        });
        let error = error.expect("a message box");
        assert!(error.contains("does not parse"), "{error}");
        assert_eq!(pick, Some(Pick::Disk), "the picks stay");
        assert_eq!(mode, merge::MergeMode::Structure);
        vis.update(|_w, cx| {
            let s = shell.read(cx);
            assert_eq!(s.queue.depth(), 0, "nothing was queued");
            assert_eq!(s.route, Route::Merge, "the editor stays open");
        });
        assert!(
            vis.debug_bounds("merge-mode-lines").is_some(),
            "the header offers Lines"
        );
    }

    /// The pick keys land on the structured cursor in Structure mode; a key
    /// the decision cannot take (b on a key row) is ignored and leaves the
    /// line resolution alone.
    ///
    /// MUTATION: `pick_key` drops the Structure branch's `apply_pick` →
    /// "1" changes nothing and the resolution stays None.
    /// MUTATION: `pick_allowed` accepts Both on a key row → "b" lands a pick.
    #[gpui::test]
    fn pick_keys_route_to_the_structured_cursor(cx: &mut TestAppContext) {
        use tomte_core::structured::Pick;
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        let picks = |vis: &mut VisualTestContext| {
            vis.update(|_w, cx| {
                let loaded = merge.read(cx).loaded.as_ref().unwrap();
                (
                    loaded.structured.as_ref().unwrap().resolution.get(0),
                    loaded.state.resolution.get(0).cloned(),
                )
            })
        };
        vis.simulate_keystrokes("b");
        vis.run_until_parked();
        assert_eq!(picks(vis), (None, None), "a key row has no both");
        vis.simulate_keystrokes("1");
        vis.run_until_parked();
        assert_eq!(
            picks(vis),
            (Some(Pick::Disk), None),
            "1 picks disk on the theme conflict"
        );
        assert!(result_text(&merge, vis).contains("\"Gruvbox\""));
    }

    /// The Shell reuses one MergeView across files, so opening file B must
    /// forget file A's undo history: A's decision 3 does not exist in B.
    ///
    /// MUTATION: drop the clearing in load (`start_session` keeps `undo`,
    /// `redo` and `revisiting`) → ⌘Z unsets B's id 3, and ⇧⌘Z's
    /// `StructuredState::pick(3)` panics out of bounds on B's two decisions.
    #[gpui::test]
    fn opening_another_file_clears_the_undo_stacks(cx: &mut TestAppContext) {
        use std::sync::Arc;
        use tomte_core::structured::Pick;
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        vis.update(|_w, cx| merge.update(cx, |v, cx| v.apply_pick(3, Pick::Disk, cx)));
        let picked = vis.update(|_w, cx| {
            merge
                .read(cx)
                .loaded
                .as_ref()
                .unwrap()
                .structured
                .as_ref()
                .unwrap()
                .resolution
                .get(3)
        });
        assert_eq!(picked, Some(Pick::Disk), "file A has a decision 3 to pick");
        // Open file B (two decisions) in the same view. `present_repo_conflict`
        // is the entry point that needs no subprocess; `load` shares its
        // reset (`start_session`) but would run the real chezmoi.
        vis.update(|_w, cx| {
            merge.update(cx, |v, cx| {
                v.present_repo_conflict(Arc::new(fixtures::json_templated_inputs()), cx)
            })
        });
        vis.run_until_parked();
        vis.simulate_keystrokes("cmd-z");
        vis.run_until_parked();
        vis.simulate_keystrokes("cmd-shift-z");
        vis.run_until_parked();
        let picks = vis.update(|_w, cx| {
            let state = merge
                .read(cx)
                .loaded
                .as_ref()
                .unwrap()
                .structured
                .as_ref()
                .unwrap();
            assert_eq!(state.doc.decisions.len(), 2);
            (0..4)
                .map(|id| state.resolution.get(id))
                .collect::<Vec<_>>()
        });
        assert_eq!(picks, vec![None; 4], "file B's resolution is untouched");
    }

    /// A templated key's row offers only its source value, with the lock
    /// whose hover names the template line.
    ///
    /// MUTATION: `render_tree` drops the `lock_mark` on a locked row → no
    /// `lock:<id>` bound.
    /// MUTATION: `render_tree` words the lock reasons itself (calls
    /// `lock_reasons` per render) → the count grows across two frames.
    #[gpui::test]
    fn a_locked_row_offers_only_source_and_a_lock(cx: &mut TestAppContext) {
        use tomte_app::structured_tree::ChipSide;
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_templated_inputs());
        let (locked, reason) = vis.update(|_w, cx| {
            let v = merge.read(cx);
            let loaded = v.loaded.as_ref().unwrap();
            assert_eq!(loaded.mode, merge::MergeMode::Structure);
            let state = loaded.structured.as_ref().unwrap();
            let d = state
                .doc
                .decisions
                .iter()
                .find(|d| d.locked.is_some())
                .expect("the templated email is locked");
            (
                d.id,
                merge_structured::lock_reason(
                    d,
                    loaded.inputs.template.as_deref(),
                    loaded.inputs.span_map.as_ref(),
                ),
            )
        });
        assert!(
            reason.contains("{{ .email }}"),
            "the hover names the template line: {reason}"
        );
        let (chips, open) = decision_row(&tree_rows(&merge, vis), locked);
        assert!(!open, "a locked row never needs the user");
        assert_eq!(
            chips.iter().map(|c| c.side).collect::<Vec<_>>(),
            vec![ChipSide::Source]
        );
        assert!(
            vis.debug_bounds(leak(format!("chip:{locked}:source")))
                .is_some()
        );
        assert!(
            vis.debug_bounds(leak(format!("chip:{locked}:disk")))
                .is_none(),
            "a locked row has no disk chip"
        );
        assert!(
            vis.debug_bounds(leak(format!("lock:{locked}"))).is_some(),
            "the lock renders"
        );
        // The hover is worded once per load, not once per frame.
        let worded = merge_structured::lock_reasons_worded();
        for _ in 0..2 {
            vis.update(|_w, cx| merge.update(cx, |_, cx| cx.notify()));
            vis.run_until_parked();
        }
        assert_eq!(
            merge_structured::lock_reasons_worded(),
            worded,
            "two more frames word no lock reason"
        );
    }

    // ---- Structure mode's Text stage (structured-merge Task 12) ----

    /// Click the header's Edit button (the Text stage opens on the cursor's
    /// decision) and let the frame settle.
    fn click_header_edit(vis: &mut VisualTestContext) {
        let edit = vis
            .debug_bounds("merge-edit")
            .expect("the header offers Edit");
        vis.simulate_click(edit.center(), gpui::Modifiers::default());
        vis.run_until_parked();
    }

    /// Replace the result editor's text from the test's own update (never
    /// from inside the editor's closure), then let the frame settle.
    fn type_into_result(merge: &Entity<merge::MergeView>, vis: &mut VisualTestContext, text: &str) {
        let text = text.to_owned();
        vis.update(|_w, cx| {
            let editor = merge.read(cx).result_editor_for_test();
            // Typing notifies the editor; `set_content` alone does not.
            editor.update(cx, |area, cx| {
                area.set_content(text);
                cx.notify();
            });
        });
        vis.run_until_parked();
    }

    fn stage_of(
        merge: &Entity<merge::MergeView>,
        vis: &mut VisualTestContext,
    ) -> tomte_app::structured_state::Stage {
        vis.update(|_w, cx| {
            merge
                .read(cx)
                .loaded
                .as_ref()
                .unwrap()
                .structured
                .as_ref()
                .unwrap()
                .stage
        })
    }

    fn queued_merge_text(shell: &Entity<Shell>, vis: &mut VisualTestContext) -> String {
        let action = vis
            .update(|_w, cx| {
                shell.update(cx, |s, _| {
                    s.queue
                        .running()
                        .first()
                        .cloned()
                        .or_else(|| s.queue.take_batch().into_iter().next())
                })
            })
            .expect("the save is on the queue");
        match action.decision {
            Decision::Merged { resolved, .. } => resolved,
            other => panic!("expected a merged decision, got {:?}", other.label()),
        }
    }

    /// Save in the Text stage queues what the user typed, not the picks'
    /// assembly — with the theme conflict still open, which the Text stage
    /// does not require to be decided.
    ///
    /// MUTATION: Save in Text stage uses `structured.assembled()` → the
    /// typed text is lost (the open conflict assembles to nothing, nothing
    /// is queued, and the `expect` on the queued action fails).
    #[gpui::test]
    fn edit_then_save_enqueues_the_editor_text_verbatim(cx: &mut TestAppContext) {
        use tomte_app::structured_state::Stage;
        let (shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        click_header_edit(vis);
        assert_eq!(stage_of(&merge, vis), Stage::Text);
        let typed = format!("{}\n// hand\n", result_text(&merge, vis));
        type_into_result(&merge, vis, &typed);
        vis.update(|_w, cx| merge.update(cx, |view, cx| view.save(cx)));
        vis.run_until_parked();
        let resolved = queued_merge_text(&shell, vis);
        assert!(resolved.ends_with("// hand\n"), "{resolved}");
        assert_eq!(resolved, typed, "the editor text, verbatim");
    }

    /// Back to structure with the text untouched returns at once: the tree
    /// renders again and the pick made before Edit stands.
    ///
    /// MUTATION: `back_to_structure` leaves `editing` true → the tree is
    /// absent after the round trip (no tree row renders).
    /// MUTATION: `back_to_structure` resets the resolution (or reloads the
    /// structured state) → `resolution.get(0)` is None.
    #[gpui::test]
    fn edit_then_back_to_structure_restores_the_tree_and_keeps_the_picks(cx: &mut TestAppContext) {
        use tomte_app::structured_state::Stage;
        use tomte_core::structured::Pick;
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        vis.update(|_w, cx| merge.update(cx, |v, cx| v.apply_pick(0, Pick::Disk, cx)));
        vis.run_until_parked();
        // `debug_bounds` keeps a selector from an earlier frame, so the
        // tree's presence is read from the rows its list drew.
        let drawn =
            |vis: &mut VisualTestContext| vis.update(|_w, cx| merge.read(cx).tree_rows_drawn.get());
        assert!(drawn(vis) > 0, "the Structure stage draws the tree");
        assert!(vis.debug_bounds("tree-row:0").is_some());
        click_header_edit(vis);
        assert_eq!(stage_of(&merge, vis), Stage::Text);
        assert!(
            vis.debug_bounds("merge-result-scroll").is_some(),
            "the Text stage shows the result pane"
        );
        let before = drawn(vis);
        vis.update(|_w, cx| merge.update(cx, |_, cx| cx.notify()));
        vis.run_until_parked();
        assert_eq!(drawn(vis), before, "no tree row in the Text stage");
        let back = vis
            .debug_bounds("merge-back-to-structure")
            .expect("the Text stage offers Back to structure");
        vis.simulate_click(back.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        assert_eq!(stage_of(&merge, vis), Stage::Structure);
        assert!(drawn(vis) > before, "the tree is back");
        let (banner, pick) = vis.update(|_w, cx| {
            let v = merge.read(cx);
            let state = v.loaded.as_ref().unwrap().structured.as_ref().unwrap();
            (v.banner.clone(), state.resolution.get(0))
        });
        assert!(banner.is_none(), "an untouched text needs no confirmation");
        assert_eq!(pick, Some(Pick::Disk), "the pick survives the round trip");
    }

    /// Back to structure with edited text arms a confirmation first; the
    /// second press (here Escape, the Text stage's CancelEdit) discards the
    /// edit and re-seeds the picks' text. ⌘↩ (ApplyEdit) does nothing here.
    ///
    /// MUTATION: `back_to_structure` returns to the tree without arming →
    /// the stage is Structure after the first click and the banner is None.
    /// MUTATION: `cancel_edit`'s Structure branch keeps Task 10's direct
    /// return (not `back_to_structure`) → Escape skips the arming, which the
    /// first-press assertion on Escape catches.
    /// MUTATION: `commit_edit` drops its Structure-mode return → ⌘↩ maps the
    /// edit onto line regions and leaves the Text stage.
    /// MUTATION: bind Escape back to the "RegionEditor" context no element
    /// carries → Escape does nothing and the first-press assertion fails.
    /// MUTATION: `start_session` keeps `text_discard_armed` → the flag
    /// survives into the next file.
    #[gpui::test]
    fn back_to_structure_arms_when_the_text_changed(cx: &mut TestAppContext) {
        use tomte_app::structured_state::Stage;
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        let seed = result_text(&merge, vis);
        let banner_text = |vis: &mut VisualTestContext| {
            vis.update(|_w, cx| merge.read(cx).banner.as_ref().map(|b| b.text.to_string()))
        };
        let warning = "Discard the text edits and return to the structure picks?";

        // Escape as the first press arms too.
        click_header_edit(vis);
        type_into_result(&merge, vis, &format!("{seed}// a\n"));
        vis.simulate_keystrokes("escape");
        vis.run_until_parked();
        assert_eq!(stage_of(&merge, vis), Stage::Text, "escape only arms");
        assert_eq!(banner_text(vis).as_deref(), Some(warning));

        // ⌘↩ is not Apply here: still the Text stage, the typed text stays.
        vis.simulate_keystrokes("cmd-enter");
        vis.run_until_parked();
        assert_eq!(stage_of(&merge, vis), Stage::Text);
        assert_eq!(result_text(&merge, vis), format!("{seed}// a\n"));

        // The second press (Escape again) discards and re-seeds.
        vis.simulate_keystrokes("escape");
        vis.run_until_parked();
        assert_eq!(stage_of(&merge, vis), Stage::Structure);
        assert_eq!(result_text(&merge, vis), seed, "the text edit is gone");
        assert!(banner_text(vis).is_none());

        // The header button arms on the first click and confirms on the second.
        click_header_edit(vis);
        type_into_result(&merge, vis, &format!("{seed}// b\n"));
        let back = vis
            .debug_bounds("merge-back-to-structure")
            .expect("the Text stage offers Back to structure");
        vis.simulate_click(back.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        assert_eq!(stage_of(&merge, vis), Stage::Text, "the first click arms");
        assert_eq!(banner_text(vis).as_deref(), Some(warning));
        assert!(vis.update(|_w, cx| merge.read(cx).text_discard_armed));
        let back = vis
            .debug_bounds("merge-back-to-structure")
            .expect("the armed button is still there");
        vis.simulate_click(back.center(), gpui::Modifiers::default());
        vis.run_until_parked();
        assert_eq!(stage_of(&merge, vis), Stage::Structure);
        assert_eq!(result_text(&merge, vis), seed);
        assert!(banner_text(vis).is_none());
        assert!(!vis.update(|_w, cx| merge.read(cx).text_discard_armed));

        // An armed confirmation does not survive into the next file.
        click_header_edit(vis);
        type_into_result(&merge, vis, &format!("{seed}// c\n"));
        vis.update(|_w, cx| merge.update(cx, |v, cx| v.back_to_structure(cx)));
        assert!(vis.update(|_w, cx| merge.read(cx).text_discard_armed));
        vis.update(|_w, cx| {
            merge.update(cx, |v, cx| {
                v.present_repo_conflict(std::sync::Arc::new(fixtures::json_templated_inputs()), cx)
            })
        });
        vis.run_until_parked();
        assert!(!vis.update(|_w, cx| merge.read(cx).text_discard_armed));
    }

    /// A text that no longer parses shows the chip with its line, and Save
    /// stays enabled: the user may save an invalid file on purpose.
    ///
    /// MUTATION: the Text stage's Save gate keeps Task 10's "apply or cancel
    /// the edit first" (Disabled while editing) → the gate is not Enabled.
    /// MUTATION: `invalid_json_line` returns the error's byte offset instead
    /// of its line → the chip reads "line 12".
    /// MUTATION: the chip parses the seed (`self.seed`) instead of the
    /// editor's text → the seed parses and the chip is None.
    #[gpui::test]
    fn the_invalid_json_chip_names_the_line_and_save_stays_enabled(cx: &mut TestAppContext) {
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        assert_eq!(
            vis.update(|_w, cx| merge.read(cx).json_chip(cx)),
            None,
            "no chip in the Structure stage"
        );
        click_header_edit(vis);
        type_into_result(&merge, vis, "{\n  \"a\": \n}");
        let (chip, gate) = vis.update(|_w, cx| {
            let v = merge.read(cx);
            (v.json_chip(cx), v.save_gate())
        });
        assert_eq!(chip.as_deref(), Some("not valid JSON · line 3"));
        assert!(vis.debug_bounds("merge-json-chip").is_some());
        assert_eq!(gate, Some(merge::SaveGate::Enabled), "chip or not, Save");
        assert!(vis.debug_bounds("merge-save").is_some());
        // Typing a valid text again clears the chip (the editor is the
        // source of truth, read at render).
        type_into_result(&merge, vis, "{\n  \"a\": 1\n}\n");
        assert_eq!(vis.update(|_w, cx| merge.read(cx).json_chip(cx)), None);
    }

    /// Entering the Text stage keeps the decision in view: the editor
    /// scrolls so the decision's line sits two lines below the top.
    ///
    /// MUTATION: `start_edit`'s Structure branch drops its editor scroll →
    /// the editor stays at the parked offset.
    #[gpui::test]
    fn entering_the_text_stage_keeps_the_decision_in_view(cx: &mut TestAppContext) {
        let (_shell, merge, vis) = structured_merge_window(cx, fixtures::json_inputs());
        let parked = px(-999.);
        let last = vis.update(|_w, cx| {
            merge
                .read(cx)
                .loaded
                .as_ref()
                .unwrap()
                .structured
                .as_ref()
                .unwrap()
                .doc
                .decisions
                .len()
                - 1
        });
        // Read the offset in the same update that enters the stage: the
        // fixture fits the editor, so the next layout clamps it to 0.
        let (offset, line) = vis.update(|_w, cx| {
            merge.update(cx, |view, cx| {
                view.set_editor_offset_for_test(parked);
                view.start_edit(last, cx);
                let state = view.loaded.as_ref().unwrap().structured.as_ref().unwrap();
                let line = state.result_lines()[last];
                (view.editor_offset_for_test(), line)
            })
        });
        let want = line.saturating_sub(merge::STRIP_ROWS) as f32 * merge::PANE_ROW_H;
        assert_eq!(offset, px(-want), "decision line {line} sits in view");
    }

    /// A JSON-looking file that does not parse opens in the line editor
    /// with a muted header note naming why; a shell file whose sides are
    /// just as unparseable says nothing (spec §7).
    ///
    /// MUTATION: show the note for every NotStructured (drop the
    /// `looked_like_json` check in `merge_structured::not_structured_note`)
    /// → the .zshrc case renders it.
    #[gpui::test]
    fn a_json_looking_file_that_does_not_parse_says_so_and_a_shell_file_does_not(
        cx: &mut TestAppContext,
    ) {
        fn inputs(target: &str, ours: &str, theirs: &str) -> tomte_app::merge_inputs::MergeInputs {
            tomte_app::merge_inputs::MergeInputs {
                target: PathBuf::from(target),
                ours: ours.into(),
                theirs: theirs.into(),
                base: None,
                source_path: PathBuf::from("/tmp/smoke/src/file"),
                templated: false,
                template: None,
                span_map: None,
                origin: None,
            }
        }
        {
            let (_shell, merge, vis) =
                structured_merge_window(cx, inputs("/tmp/smoke/settings.json", "{", "{}\n"));
            let (mode, note) = vis.update(|_w, cx| {
                let view = merge.read(cx);
                let mode = view.loaded.as_ref().expect("loaded").mode;
                (mode, view.not_structured_note())
            });
            assert_eq!(
                mode,
                merge::MergeMode::Lines,
                "no Structure for a parse error"
            );
            assert_eq!(
                note.as_deref(),
                Some("disk is not valid JSON (line 1): line merge")
            );
            assert!(
                vis.debug_bounds("merge-not-structured").is_some(),
                "the chip renders"
            );
        }
        let (_shell, merge, vis) =
            structured_merge_window(cx, inputs("/tmp/smoke/.zshrc", "x\n", "y\n"));
        let note = vis.update(|_w, cx| merge.read(cx).not_structured_note());
        assert_eq!(note, None, "a shell file says nothing about JSON");
        assert!(vis.debug_bounds("merge-not-structured").is_none());
    }
}
