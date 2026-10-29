//! A serial queue for resolve actions, so confirming a file never blocks
//! confirming the next one.
//!
//! Before this, `ReviewView::action_in_flight` and `MergeView::saving` were
//! single booleans guarding an early `return`. Confirming one file did not
//! merely disable its own button — it made every other file's button a no-op,
//! and the click was DISCARDED rather than remembered. Each action takes
//! seconds (a `chezmoi re-add` plus a push, and longer behind a daemon
//! rescan), so answering a queue of thirty files meant thirty waits.
//!
//! The work itself still runs one at a time, and that is not a limitation to
//! be optimised away: every action mutates one git repository — index,
//! merge state, and chezmoi's own autoCommit/autoPush. Two in parallel would
//! contend for `index.lock` and interleave commits. What changes is that the
//! *interface* stops waiting: a decision is recorded the moment it is made,
//! the row moves to "processing", and the user carries on.
//!
//! Decisions are DATA ([`Decision`]), not opaque closures: [`ActionQueue`]
//! has to be able to tell a one-click `keep_disk`/`keep_source` apart from a
//! merge-editor save in order to group the former into the shared-tail batch
//! [`crate::resolve::ResolveEngine::resolve_batch`] runs (one `re-add`, one
//! commit, one push for many files) instead of paying that per file.
//! [`ActionQueue::take_batch`] hands back the run to execute next: every
//! consecutive batchable decision at the front of the queue, or exactly one
//! non-batchable one when a batchable run isn't there to take.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::SharedString;

use crate::merge_inputs::MergeInputs;
use crate::resolve::{BatchDecision, BatchItem};

/// What the user chose for one target, carrying exactly what that choice
/// needs to run later — nothing more.
///
/// An enum of decision shapes rather than the boxed closure this replaced:
/// the merge editor's payload (resolved text, loaded inputs, a repo-conflict
/// source path) has nothing in common with a one-click `keep_disk`, but the
/// queue now needs to SEE the shape to decide what can run together
/// ([`Self::batchable`]) — a closure hides that from it entirely.
#[derive(Debug, Clone)]
pub enum Decision {
    /// Keep the on-disk version: re-add it into the source state.
    KeepDisk,
    /// Keep a link that replaced a folder, after copying the files the user
    /// picked (relative to the target) from the source into the link target.
    /// Queued only with picks; with none the press is [`Self::KeepDisk`].
    KeepLink { copies: Arc<[PathBuf]> },
    /// Restore chezmoi's (local source's) version: apply the target.
    KeepSourceLocal,
    /// Keep origin's version of the source, as the view showed it —
    /// [`crate::resolve::ResolveEngine::keep_source_origin`] refuses the
    /// decision if origin moved since `seen` was captured.
    KeepSourceOrigin { seen: String },
    /// Merge-editor Save on an ordinary (non-mid-merge) file.
    Merged {
        resolved: String,
        inputs: Arc<MergeInputs>,
    },
    /// Review's "Keep both": the clean 3-way auto-merge, written exactly as
    /// [`Self::Merged`] is. A variant of its own only so the words the user
    /// pressed travel with it — a failure that reads "Save failed" on a row
    /// whose button said "Keep both" names a button that is not on screen.
    KeepBoth {
        resolved: String,
        inputs: Arc<MergeInputs>,
    },
    /// Merge-editor Save on a file left conflicted by an open merge with
    /// origin — resolved against the index stages, not the working tree.
    RepoConflict {
        source_path: PathBuf,
        resolved: String,
    },
}

impl Decision {
    /// What the user chose, in their words. Used verbatim in the failure a
    /// row shows, so a queued action that fails minutes later still says
    /// which choice failed — and in the PROCESSING row, so a queue of thirty
    /// says what each one is waiting to do.
    pub fn label(&self) -> &'static str {
        match self {
            Decision::KeepDisk => "Keep disk",
            Decision::KeepLink { .. } => "Keep the link",
            Decision::KeepSourceLocal => "Keep source · this machine",
            Decision::KeepSourceOrigin { .. } => "Keep source · origin",
            Decision::KeepBoth { .. } => "Keep both",
            Decision::Merged { .. } | Decision::RepoConflict { .. } => "Save",
        }
    }

    /// What the choice DID, past tense — the words a landed decision reports
    /// itself with, minutes after the click, in the outcome banner and in the
    /// out-of-band notification a dashboard decision earns.
    ///
    /// Separate from [`Self::label`] on purpose: a success says what happened
    /// ("Kept disk version · committed & pushed"), a failure names the button
    /// that failed ("Keep disk failed: …"). Both are the same choice in the
    /// user's vocabulary, which is why they live side by side here rather
    /// than being derived from each other.
    pub fn verb(&self) -> &'static str {
        match self {
            Decision::KeepDisk => "Kept disk version",
            Decision::KeepLink { .. } => "Kept the link",
            Decision::KeepSourceLocal => "Restored chezmoi's version",
            Decision::KeepSourceOrigin { .. } => "Restored origin's version",
            Decision::Merged { .. } | Decision::KeepBoth { .. } => "Merged",
            // Its banner comes from the merge's own conclusion, never from
            // here — a staged conflict has not landed anywhere yet.
            Decision::RepoConflict { .. } => "Recorded the resolution",
        }
    }

    /// Can this decision run inside the shared-tail batch
    /// ([`crate::resolve::ResolveEngine::resolve_batch`]) alongside other
    /// batchable decisions, rather than needing the repo to itself?
    ///
    /// `KeepDisk` and `KeepSourceLocal` are exactly [`BatchDecision`]'s two
    /// variants — see [`Self::as_batch`] — because those are the only two
    /// tails `resolve_batch` actually implements: one shared `re-add`, one
    /// shared `apply`. `keep_source_origin` and every merge-editor save each
    /// write their OWN source text (write-after-verify, or origin's
    /// content), and `resolve_batch` has no shared tail for that — batching
    /// them would mean giving each its own write-after-verify path inside
    /// the batch, which does not exist.
    ///
    /// That is not the same claim as "batchable means it cannot fail per
    /// file": a templated `KeepDisk` is still refused per file inside the
    /// batch, in `resolve_batch`'s pre-flight pass, exactly as single-file
    /// `keep_disk` refuses it.
    pub fn batchable(&self) -> bool {
        self.as_batch().is_some()
    }

    /// This decision as the shared-tail batch item it is, or `None` for a
    /// decision that has to run alone.
    pub fn as_batch(&self) -> Option<BatchDecision> {
        match self {
            Decision::KeepDisk | Decision::KeepLink { .. } => Some(BatchDecision::KeepDisk),
            Decision::KeepSourceLocal => Some(BatchDecision::KeepSourceLocal),
            Decision::KeepSourceOrigin { .. }
            | Decision::Merged { .. }
            | Decision::KeepBoth { .. }
            | Decision::RepoConflict { .. } => None,
        }
    }

    /// The files to copy into the link target before Keep disk; empty for
    /// every decision but [`Self::KeepLink`].
    pub fn copies(&self) -> &[PathBuf] {
        match self {
            Decision::KeepLink { copies } => copies,
            _ => &[],
        }
    }
}

#[derive(Clone)]
pub struct QueuedAction {
    /// The destination file this decision is about. Identity for the UI: the
    /// row it came from, and the row a failure attaches to.
    pub target: PathBuf,
    pub decision: Decision,
    /// Say so out of band when this one lands.
    ///
    /// TRUE only for the dashboard's quick actions. That surface has no
    /// banner and no processing group — the timeline row it was pressed on
    /// just stops being drifted — so without a notification the decision
    /// lands in silence, minutes later, quite possibly while the user is in
    /// another application. Review and the merge editor both have a place on
    /// screen to say it, so they set FALSE and a second, redundant
    /// notification per file never fires.
    pub notify_on_done: bool,
}

impl QueuedAction {
    /// This action as the item [`crate::resolve::ResolveEngine::resolve_batch`]
    /// takes, or `None` for a decision that has to run alone. The copy picks
    /// travel on the item, not on [`BatchDecision`].
    pub fn into_batch_item(self) -> Option<BatchItem> {
        let decision = self.decision.as_batch()?;
        Some(BatchItem {
            copies: self.decision.copies().to_vec(),
            target: self.target,
            decision,
        })
    }
}

/// Pending and running decisions, plus the failures that have not been
/// acknowledged yet.
#[derive(Default)]
pub struct ActionQueue {
    pending: VecDeque<QueuedAction>,
    /// The batch currently executing, in the order [`Self::take_batch`] took
    /// it. A `Vec` rather than the single `Option<PathBuf>` this replaced: a
    /// batch takes several targets running at once, all under one in-flight
    /// resolve call.
    ///
    /// The whole ACTION, not just its target: `take_batch` hands a CLONE to
    /// whoever runs it and keeps this copy, because two questions outlive the
    /// hand-off. A processing row has to say WHICH choice is running (a queue
    /// of thirty saying "running…" says nothing), and
    /// [`QueuedAction::notify_on_done`] has to survive to the completion that
    /// decides whether to announce it. Both were unanswerable while this held
    /// bare paths.
    running: Vec<QueuedAction>,
    /// Targets whose last queued action failed, with the message. Kept until
    /// the user retries ([`Self::push`]) or the target stops being drifted
    /// ([`Self::retain_failures`]) — a failure that vanished on the next
    /// render would be a failure nobody saw.
    failed: BTreeMap<PathBuf, SharedString>,
}

impl ActionQueue {
    /// Queue a decision. A second decision about the SAME target replaces any
    /// still-pending one: the user changed their mind before the first ran,
    /// and running both would apply the discarded choice first.
    pub fn push(&mut self, action: QueuedAction) {
        self.failed.remove(&action.target);
        self.pending.retain(|p| p.target != action.target);
        self.pending.push_back(action);
    }

    /// Take back a decision that has not started (spec 2026-10-11 §2.4).
    /// `true` when a pending entry for `target` was removed; `false` — and
    /// nothing else — when none was pending: a RUNNING decision is one
    /// in-flight re-add/apply/commit/push on the user's repo, and the
    /// engine is blocking in a background thread for it. Records no
    /// failure: the user withdrew the decision, nothing failed.
    pub fn cancel(&mut self, target: &Path) -> bool {
        let before = self.pending.len();
        self.pending.retain(|p| p.target != target);
        self.pending.len() != before
    }

    /// Take the next run of work: every consecutive batchable decision at
    /// the front of the queue, or exactly one non-batchable one. Marks every
    /// returned target as running.
    ///
    /// Empty while anything is already running — that guarantee lives here,
    /// in one place, exactly as it did for the single-action `take_next` this
    /// replaced: the work itself still drains one run at a time, one git
    /// repo. Empty too when there is nothing pending. Otherwise returns AT
    /// LEAST the front item — a "batch" of one non-batchable decision, or of
    /// one batchable decision with no batchable neighbour, is a normal
    /// result, not a special case the caller has to guard against.
    pub fn take_batch(&mut self) -> Vec<QueuedAction> {
        if !self.running.is_empty() {
            return Vec::new();
        }
        let Some(front) = self.pending.front() else {
            return Vec::new();
        };
        let mut batch = Vec::new();
        if front.decision.batchable() {
            while let Some(next) = self.pending.front() {
                if !next.decision.batchable() {
                    break;
                }
                batch.push(self.pending.pop_front().expect("front just checked Some"));
            }
        } else {
            batch.push(self.pending.pop_front().expect("front just checked Some"));
        }
        self.running.extend(batch.iter().cloned());
        batch
    }

    /// Record a whole batch's results at once: `err` present for a target
    /// means it failed and its row must say so.
    ///
    /// Clears `running` WHOLESALE rather than name-by-name. `take_batch`
    /// always hands out the running set as one call, and `resolve_batch`
    /// always reports exactly one entry per item it was given — but nothing
    /// here enforces that a caller reports every target it received.
    /// Draining by name would let one dropped result (upstream loses a
    /// message, a future caller has a bug) strand its siblings in `running`
    /// forever: `take_batch` would then return empty for good, wedging the
    /// WHOLE queue, not just the target whose result went missing. A batch
    /// is the unit `take_batch` hands out, so it is the unit `finish_all`
    /// closes out, whatever `results` does or doesn't name.
    pub fn finish_all(&mut self, results: &[(PathBuf, Option<SharedString>)]) {
        self.running.clear();
        for (target, err) in results {
            if let Some(message) = err {
                self.failed.insert(target.clone(), message.clone());
            }
        }
    }

    /// The batch currently executing, in the order [`Self::take_batch`] took
    /// it. Callers that only want the targets map over it.
    pub fn running(&self) -> &[QueuedAction] {
        &self.running
    }

    /// The choice recorded for one target, in the user's own words
    /// ([`Decision::label`]) — running first, then pending.
    ///
    /// Running first because that is the order the two can only ever be in:
    /// [`Self::push`] replaces a target's PENDING entry, so a target cannot
    /// be pending twice, and a target that is running and then re-chosen has
    /// its new choice pending behind the old one — the running one is what is
    /// actually happening to the file right now, and the row says that.
    pub fn decision_label(&self, target: &Path) -> Option<&'static str> {
        self.running
            .iter()
            .chain(self.pending.iter())
            .find(|a| a.target == target)
            .map(|a| a.decision.label())
    }

    /// Everything queued or running, in the order the user chose it: the
    /// running batch first (in the order it was taken), then the rest still
    /// pending.
    ///
    /// These are the rows the UI renders under "Processing". Being here does
    /// NOT make a row inert: only a RUNNING decision cannot be changed any
    /// more. A row whose decision is still pending stays selectable and
    /// re-choosable, and [`Self::push`] replaces the pending choice — spec
    /// §2.4, "a re-queued target replaces its pending choice". Use
    /// [`Self::running`] for the ones that are past changing.
    ///
    /// DEDUPED, running first: a target that is running and is then
    /// re-chosen (merge-editor Save on a file whose batch already started)
    /// holds two entries, and the UI renders one row per target — without
    /// this it drew that file twice, both "Processing". [`Self::depth`] is
    /// deliberately NOT deduped: two entries really are two units of work
    /// still owed.
    pub fn processing(&self) -> Vec<PathBuf> {
        let mut seen: Vec<PathBuf> = Vec::with_capacity(self.depth());
        for action in self.running.iter().chain(self.pending.iter()) {
            if !seen.contains(&action.target) {
                seen.push(action.target.clone());
            }
        }
        seen
    }

    /// How many decisions are still owed work, including any running.
    pub fn depth(&self) -> usize {
        self.pending.len() + self.running.len()
    }

    pub fn failure(&self, target: &Path) -> Option<&SharedString> {
        self.failed.get(target)
    }

    pub fn failures(&self) -> impl Iterator<Item = (&PathBuf, &SharedString)> {
        self.failed.iter()
    }

    /// Drop every remembered failure whose target `keep` says no to.
    ///
    /// The OTHER half of the promise on `Self::failed`. Retrying clears a
    /// failure, but a row the user never retried has to lose its message
    /// once the file stops being drifted — otherwise the row comes back
    /// months later, on the file's next unrelated drift, still showing an
    /// error from a decision that has nothing to do with it.
    ///
    /// A predicate rather than a target: the caller holds the drifted set
    /// (the model), and asking it once per remembered failure keeps the
    /// queue from having to know what "still drifted" means.
    pub fn retain_failures(&mut self, keep: impl Fn(&Path) -> bool) {
        self.failed.retain(|target, _| keep(target));
    }

    /// Drop every PENDING decision `pred` matches and record `msg` as its
    /// failure. Returns the targets, so the caller can log what it just took
    /// away from the user.
    ///
    /// Fail-fast for decisions a repo-level answer is about to invalidate: an
    /// abort of the merge with origin rolls the source repo back, and a
    /// merge-editor save that was going to be staged INTO that merge has
    /// nowhere left to go. Running it afterwards would either fail obscurely
    /// or, worse, quietly start a second merge — so it is refused here, in
    /// the user's terms, on the row it came from (spec §2.5).
    ///
    /// RUNNING items are deliberately untouched: they are in flight under the
    /// latch, this queue cannot recall them, and pretending otherwise would
    /// put a failure on a row whose work is still going to land.
    pub fn fail_pending_where(
        &mut self,
        pred: impl Fn(&QueuedAction) -> bool,
        msg: &str,
    ) -> Vec<PathBuf> {
        let mut failed = Vec::new();
        self.pending.retain(|action| {
            if pred(action) {
                failed.push(action.target.clone());
                false
            } else {
                true
            }
        });
        for target in &failed {
            self.failed.insert(target.clone(), msg.to_string().into());
        }
        failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Queued or running, as the sidebar asks it. A free function here
    /// rather than a method: production renders the whole
    /// [`ActionQueue::processing`] list, so a one-target query would be a
    /// public API with no caller outside these tests.
    fn is_processing(q: &ActionQueue, target: &str) -> bool {
        q.processing().contains(&PathBuf::from(target))
    }

    fn action(target: &str, decision: Decision) -> QueuedAction {
        QueuedAction {
            target: PathBuf::from(target),
            decision,
            notify_on_done: false,
        }
    }

    /// The targets of a batch, for assertions that are about ORDER rather
    /// than about what each one carries.
    fn targets(batch: &[QueuedAction]) -> Vec<&Path> {
        batch.iter().map(|a| a.target.as_path()).collect()
    }

    /// A minimal `Arc<MergeInputs>` for decisions that only need to exist,
    /// not to carry meaningful merge content. Fields beyond identity are
    /// empty/`None` — the queue never inspects them.
    fn fake_inputs(target: &str) -> Arc<MergeInputs> {
        Arc::new(MergeInputs {
            target: PathBuf::from(target),
            ours: String::new(),
            theirs: String::new(),
            base: None,
            source_path: PathBuf::from(target),
            templated: false,
            template: None,
            span_map: None,
            origin: None,
        })
    }

    /// A Keep the link with copy picks joins a run of one-click decisions and
    /// reaches the batch as Keep disk carrying its picks.
    /// MUTATION: build the item with `copies: Vec::new()` in
    /// `QueuedAction::into_batch_item` → the picks never reach the engine.
    #[test]
    fn keep_link_with_picks_is_batched_with_copies() {
        let picks: Arc<[PathBuf]> = vec![PathBuf::from("WARP.md")].into();
        let mut q = ActionQueue::default();
        q.push(action(
            "/skills/humanizer",
            Decision::KeepLink { copies: picks },
        ));
        q.push(action("/b", Decision::KeepDisk));
        let batch = q.take_batch();
        assert_eq!(
            targets(&batch),
            [Path::new("/skills/humanizer"), Path::new("/b")]
        );
        assert_eq!(
            q.decision_label(Path::new("/skills/humanizer")),
            Some("Keep the link")
        );
        let items: Vec<BatchItem> = batch
            .into_iter()
            .map(|a| a.into_batch_item().expect("both are batchable"))
            .collect();
        assert_eq!(items[0].decision, BatchDecision::KeepDisk);
        assert_eq!(items[0].copies, [PathBuf::from("WARP.md")]);
        assert_eq!(items[1].decision, BatchDecision::KeepDisk);
        assert!(items[1].copies.is_empty());
    }

    /// The point of the whole thing: a second decision is accepted while the
    /// first is still running, instead of being discarded by an
    /// `if in_flight { return }` guard.
    #[test]
    fn a_second_decision_is_queued_while_the_first_runs() {
        let mut q = ActionQueue::default();
        q.push(action("/a", Decision::KeepDisk));
        let first = q.take_batch();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].target, Path::new("/a"));

        q.push(action("/b", Decision::KeepSourceLocal));
        assert!(
            is_processing(&q, "/b"),
            "the second decision is remembered, not dropped"
        );
        assert!(
            q.take_batch().is_empty(),
            "but it does not start until the first finishes — one git repo"
        );
        assert_eq!(q.depth(), 2);

        q.finish_all(&[(PathBuf::from("/a"), None)]);
        let second = q.take_batch();
        assert_eq!(
            targets(&second),
            [Path::new("/b")],
            "and starts as soon as the repo is free"
        );
    }

    /// Changing your mind before the action ran must not run both choices.
    #[test]
    fn requeueing_the_same_target_replaces_the_pending_choice() {
        let mut q = ActionQueue::default();
        q.push(action("/a", Decision::KeepDisk)); // keeps the repo busy
        q.take_batch();
        q.push(action("/b", Decision::KeepDisk));
        q.push(action("/b", Decision::KeepSourceLocal));
        assert_eq!(q.depth(), 2, "/b is queued once, not twice");

        q.finish_all(&[(PathBuf::from("/a"), None)]);
        let next = q.take_batch();
        assert_eq!(next.len(), 1, "/b runs on its own, /a already ran");
        assert!(
            matches!(next[0].decision, Decision::KeepSourceLocal),
            "the LAST choice wins; applying the abandoned one first would be wrong"
        );
    }

    /// A failure has to outlive the action, or nobody sees it: the queue moves
    /// on within milliseconds of the error.
    #[test]
    fn a_failure_is_remembered_against_its_target_until_retried() {
        let mut q = ActionQueue::default();
        q.push(action("/a", Decision::KeepDisk));
        q.take_batch();
        q.finish_all(&[(PathBuf::from("/a"), Some("op timed out".to_string().into()))]);

        assert_eq!(
            q.failure(Path::new("/a")).map(|m| m.as_ref()),
            Some("op timed out")
        );
        assert!(
            !is_processing(&q, "/a"),
            "a failed target is not still processing — it is answerable again"
        );

        // Retrying clears it, so the row cannot show a stale error beside a
        // fresh attempt.
        q.push(action("/a", Decision::KeepDisk));
        assert!(q.failure(Path::new("/a")).is_none());
    }

    #[test]
    fn processing_lists_the_running_action_first() {
        let mut q = ActionQueue::default();
        q.push(action("/a", Decision::KeepDisk));
        q.push(action(
            "/b",
            Decision::KeepSourceOrigin {
                seen: "x".to_string(),
            },
        ));
        q.take_batch(); // /a alone: /b is not batchable, so it stays pending.
        assert_eq!(
            q.processing(),
            vec![PathBuf::from("/a"), PathBuf::from("/b")],
            "the running one leads; the rest follow in the order chosen"
        );
    }

    /// A running target re-chosen holds TWO entries — and is still one row.
    ///
    /// Reachable for real: the merge editor's Save on a file whose batch has
    /// already started. `processing` is what the UI renders rows from, so
    /// without the dedupe Review drew that file twice, both "Processing".
    /// MUTATION: drop the `seen.contains` guard in `processing` and this
    /// fails with `["/a", "/a"]`. `depth` stays 2 on purpose — two entries
    /// are two units of work still owed — and the running entry is the one
    /// that survives, because it is what is actually happening to the file.
    #[test]
    fn processing_lists_a_re_chosen_running_target_once() {
        let mut q = ActionQueue::default();
        q.push(action("/a", Decision::KeepDisk));
        q.take_batch();
        q.push(action(
            "/a",
            Decision::Merged {
                resolved: "x".into(),
                inputs: fake_inputs("/a"),
            },
        ));

        assert_eq!(
            q.processing(),
            vec![PathBuf::from("/a")],
            "one row per target, however many decisions it owes"
        );
        assert_eq!(q.depth(), 2, "but two units of work are still owed");
        assert!(is_processing(&q, "/a"));
        assert_eq!(
            targets(q.running()),
            [Path::new("/a")],
            "the running batch is untouched"
        );
        assert_eq!(
            q.decision_label(Path::new("/a")),
            Some(Decision::KeepDisk.label()),
            "and the running choice is the one the row names"
        );
    }

    /// The whole point of [`Decision::batchable`]: a leading run of
    /// consecutive one-click decisions is handed over together, so the
    /// caller can pass it to `resolve_batch` for one shared commit and push
    /// — and a decision that cannot join a batch runs alone, isolating
    /// neither its slower path nor its own failure from the rest of the
    /// queue.
    #[test]
    fn take_batch_groups_consecutive_one_click_decisions_and_isolates_the_rest() {
        let mut q = ActionQueue::default();
        q.push(action("/a", Decision::KeepDisk));
        q.push(action("/b", Decision::KeepSourceLocal));
        q.push(action(
            "/c",
            Decision::Merged {
                resolved: "x".into(),
                inputs: fake_inputs("/c"),
            },
        ));
        q.push(action("/d", Decision::KeepDisk));

        let first = q.take_batch();
        assert_eq!(
            targets(&first),
            [Path::new("/a"), Path::new("/b")],
            "the leading run of batchable decisions goes together"
        );
        assert!(
            q.take_batch().is_empty(),
            "nothing starts while that batch runs"
        );
        q.finish_all(&[(PathBuf::from("/a"), None), (PathBuf::from("/b"), None)]);

        let second = q.take_batch();
        assert_eq!(second.len(), 1, "a merge-editor save runs alone");
        assert!(matches!(second[0].decision, Decision::Merged { .. }));
        q.finish_all(&[(PathBuf::from("/c"), None)]);

        let third = q.take_batch();
        assert_eq!(
            third[0].target,
            Path::new("/d"),
            "the trailing keep-disk is its own batch"
        );
    }

    /// `take_batch` returns a batch of exactly one just as readily as a
    /// longer run — a single batchable decision with no batchable neighbour
    /// is not a case the caller has to special-case.
    #[test]
    fn take_batch_returns_a_batch_of_one_when_nothing_batchable_follows() {
        let mut q = ActionQueue::default();
        q.push(action("/a", Decision::KeepDisk));
        q.push(action(
            "/b",
            Decision::RepoConflict {
                source_path: PathBuf::from("/src/b"),
                resolved: "x".into(),
            },
        ));

        let batch = q.take_batch();
        assert_eq!(
            targets(&batch),
            [Path::new("/a")],
            "a batch of one is still a valid batch"
        );
        assert_eq!(targets(q.running()), [Path::new("/a")]);
        assert!(
            is_processing(&q, "/a") && is_processing(&q, "/b"),
            "/a is running, /b still pending — both count as processing"
        );
    }

    /// A batch is the unit `take_batch` hands out, so it must be the unit
    /// `finish_all` closes out — a caller that reports results for only
    /// SOME of a batch's targets must not strand the rest in `running`
    /// forever, wedging the whole queue behind them.
    #[test]
    fn finish_all_clears_the_whole_running_batch_even_with_a_partial_report() {
        let mut q = ActionQueue::default();
        q.push(action("/a", Decision::KeepDisk));
        q.push(action("/b", Decision::KeepSourceLocal));
        let batch = q.take_batch();
        assert_eq!(batch.len(), 2, "both are batchable, taken together");

        // Queued while the batch above is still running.
        q.push(action("/c", Decision::KeepDisk));
        assert!(q.take_batch().is_empty(), "the batch is still running");

        // Only /a's result comes back — /b's went missing upstream.
        q.finish_all(&[(PathBuf::from("/a"), None)]);

        let next = q.take_batch();
        assert_eq!(
            next[0].target,
            Path::new("/c"),
            "a partial report must not strand /b in `running` and wedge the queue"
        );
    }

    /// MUTATION: also drain `running` in `cancel` → `/a` stops processing.
    /// MUTATION: insert a failure for the cancelled target → the `failure`
    /// assertion fails: a withdrawn decision is not a failed one.
    #[test]
    fn a_pending_decision_can_be_cancelled_but_a_running_one_cannot() {
        let mut q = ActionQueue::default();
        q.push(action("/a", Decision::KeepDisk));
        let _running = q.take_batch();
        q.push(action("/b", Decision::KeepSourceLocal));
        assert!(q.cancel(Path::new("/b")));
        assert!(!is_processing(&q, "/b"));
        assert!(
            q.failure(Path::new("/b")).is_none(),
            "withdrawn, not failed"
        );
        assert!(!q.cancel(Path::new("/a")), "running: cannot be recalled");
        assert!(is_processing(&q, "/a"));
        assert_eq!(q.depth(), 1);
    }

    /// A target that is running AND re-queued (a Save on a file whose batch
    /// already started) holds two entries; the cross takes back only the
    /// pending one and the running row keeps saying "running…".
    ///
    /// MUTATION: make `cancel` return `true` without removing anything when
    /// the target is also running → the depth assertion fails.
    #[test]
    fn cancelling_a_re_queued_running_target_removes_only_the_pending_copy() {
        let mut q = ActionQueue::default();
        q.push(action("/a", Decision::KeepDisk));
        let _running = q.take_batch();
        q.push(action("/a", Decision::KeepSourceLocal));
        assert_eq!(q.depth(), 2);
        assert!(q.cancel(Path::new("/a")));
        assert_eq!(q.depth(), 1);
        assert_eq!(q.running().len(), 1);
        assert_eq!(q.decision_label(Path::new("/a")), Some("Keep disk"));
    }

    /// A processing row has to say WHICH choice it is waiting on: thirty rows
    /// all reading "running…" say nothing about what is happening to any of
    /// them. `take_batch` used to keep only the targets, so a running row's
    /// decision was genuinely unreadable.
    ///
    /// The two cases are asserted separately because a target cannot be both
    /// at once: `push` replaces a target's pending entry, so nothing is ever
    /// pending twice, and a re-choice made while the first is running is the
    /// one case where both exist — covered last.
    ///
    /// MUTATION: have `decision_label` search `pending` before `running` →
    /// the last assertion names the choice that is NOT the one running.
    #[test]
    fn decision_label_names_the_choice_a_row_is_waiting_on() {
        let mut q = ActionQueue::default();
        assert_eq!(q.decision_label(Path::new("/a")), None, "nothing recorded");

        // Pending: the label is readable before anything runs.
        q.push(action("/a", Decision::KeepDisk));
        assert_eq!(q.decision_label(Path::new("/a")), Some("Keep disk"));

        // Running: still readable AFTER `take_batch` handed the batch out.
        q.take_batch();
        assert_eq!(
            q.decision_label(Path::new("/a")),
            Some("Keep disk"),
            "the queue keeps the running action, not just its target"
        );

        // Both: re-chosen while the first is still running. What is happening
        // to the file right now is the RUNNING one.
        q.push(action("/a", Decision::KeepSourceLocal));
        assert_eq!(
            q.decision_label(Path::new("/a")),
            Some("Keep disk"),
            "the running choice is the one actually touching the file"
        );
    }

    /// An abort of the merge with origin rolls the source repo back, and a
    /// merge-editor save that was going to be staged INTO that merge has
    /// nowhere left to go. It is refused here rather than run against a repo
    /// that no longer has the merge — and the decisions that do NOT depend on
    /// the merge stay queued, because they run correctly either way.
    ///
    /// MUTATION: have `fail_pending_where` drain `running` too → `/c` is
    /// taken out from under the batch that is executing it.
    #[test]
    fn fail_pending_where_takes_only_the_pending_decisions_it_names() {
        let mut q = ActionQueue::default();
        q.push(action("/c", Decision::KeepDisk));
        q.take_batch(); // /c is running, under the latch
        q.push(action(
            "/a",
            Decision::RepoConflict {
                source_path: PathBuf::from("/src/a"),
                resolved: "x".into(),
            },
        ));
        q.push(action("/b", Decision::KeepDisk));

        let failed = q.fail_pending_where(
            |a| matches!(a.decision, Decision::RepoConflict { .. }),
            "dropped: a roll-back was requested before this save ran — save again",
        );

        assert_eq!(failed, [PathBuf::from("/a")], "only the save it orphans");
        assert_eq!(
            q.failure(Path::new("/a")).map(|m| m.as_ref()),
            Some("dropped: a roll-back was requested before this save ran — save again"),
            "and the row says why, in the user's terms"
        );
        assert_eq!(
            targets(q.running()),
            [Path::new("/c")],
            "a running decision is in flight under the latch and cannot be recalled"
        );
        assert_eq!(
            q.decision_label(Path::new("/b")),
            Some("Keep disk"),
            "a keep-disk does not depend on the merge: it stays queued and \
             runs against the rolled-back repo"
        );
        assert_eq!(q.depth(), 2, "/a is gone, /b and /c remain");
    }
}
