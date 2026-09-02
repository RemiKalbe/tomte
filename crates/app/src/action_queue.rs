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
use crate::resolve::BatchDecision;

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
    /// Merge-editor Save on a file left conflicted by an open merge with
    /// origin — resolved against the index stages, not the working tree.
    RepoConflict {
        source_path: PathBuf,
        resolved: String,
    },
}

impl Decision {
    /// What the user chose, in their words. Used verbatim in the outcome
    /// banner, so a queued action that fails minutes later still says which
    /// choice failed.
    pub fn label(&self) -> &'static str {
        match self {
            Decision::KeepDisk => "Keep disk",
            Decision::KeepSourceLocal => "Keep source · this machine",
            Decision::KeepSourceOrigin { .. } => "Keep source · origin",
            Decision::Merged { .. } | Decision::RepoConflict { .. } => "Save",
        }
    }

    /// Can this decision run inside the shared-tail batch
    /// ([`crate::resolve::ResolveEngine::resolve_batch`]) alongside other
    /// batchable decisions, rather than needing the repo to itself?
    ///
    /// Only the two decisions with no per-file failure mode beyond the
    /// shared tail's own: `keep_source_origin` can reject a protected span,
    /// and every merge-editor save writes source text that has to be its own
    /// affair. `KeepDisk` and `KeepSourceLocal` are exactly
    /// [`BatchDecision`]'s two variants — see [`Self::as_batch`].
    pub fn batchable(&self) -> bool {
        self.as_batch().is_some()
    }

    /// This decision as the shared-tail batch item it is, or `None` for a
    /// decision that has to run alone.
    pub fn as_batch(&self) -> Option<BatchDecision> {
        match self {
            Decision::KeepDisk => Some(BatchDecision::KeepDisk),
            Decision::KeepSourceLocal => Some(BatchDecision::KeepSourceLocal),
            Decision::KeepSourceOrigin { .. }
            | Decision::Merged { .. }
            | Decision::RepoConflict { .. } => None,
        }
    }
}

pub struct QueuedAction {
    /// The destination file this decision is about. Identity for the UI: the
    /// row it came from, and the row a failure attaches to.
    pub target: PathBuf,
    pub decision: Decision,
}

/// Pending and running decisions, plus the failures that have not been
/// acknowledged yet.
#[derive(Default)]
pub struct ActionQueue {
    pending: VecDeque<QueuedAction>,
    /// Targets currently executing, in the order [`Self::take_batch`] took
    /// them. A `Vec` rather than the single `Option<PathBuf>` this replaced:
    /// a batch takes several targets running at once, all under one
    /// in-flight resolve call.
    running: Vec<PathBuf>,
    /// Targets whose last queued action failed, with the message. Kept until
    /// the user retries or the target stops being drifted — a failure that
    /// vanished on the next render would be a failure nobody saw.
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
        self.running.extend(batch.iter().map(|a| a.target.clone()));
        batch
    }

    /// Record a whole batch's results at once: `err` present for a target
    /// means it failed and its row must say so. Every target moves out of
    /// `running` regardless — a batch is drained together (see
    /// [`Self::take_batch`]), so it is recorded together.
    pub fn finish_all(&mut self, results: &[(PathBuf, Option<SharedString>)]) {
        for (target, err) in results {
            self.running.retain(|running| running != target);
            if let Some(message) = err {
                self.failed.insert(target.clone(), message.clone());
            }
        }
    }

    /// Is this target queued or running? Such a row renders under
    /// "processing" and its buttons are inert — the only row in the app whose
    /// buttons are.
    pub fn is_processing(&self, target: &Path) -> bool {
        self.running.iter().any(|r| r == target) || self.pending.iter().any(|p| p.target == target)
    }

    /// Targets currently executing, in the order [`Self::take_batch`] took
    /// them.
    pub fn running(&self) -> &[PathBuf] {
        &self.running
    }

    /// Everything queued or running, in the order the user chose it: the
    /// running batch first (in the order it was taken), then the rest still
    /// pending.
    pub fn processing(&self) -> Vec<PathBuf> {
        self.running
            .iter()
            .cloned()
            .chain(self.pending.iter().map(|p| p.target.clone()))
            .collect()
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

    pub fn clear_failure(&mut self, target: &Path) {
        self.failed.remove(target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(target: &str, decision: Decision) -> QueuedAction {
        QueuedAction {
            target: PathBuf::from(target),
            decision,
        }
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
            q.is_processing(Path::new("/b")),
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
            second
                .iter()
                .map(|a| a.target.as_path())
                .collect::<Vec<_>>(),
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
            !q.is_processing(Path::new("/a")),
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
            first.iter().map(|a| a.target.as_path()).collect::<Vec<_>>(),
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
            batch.iter().map(|a| a.target.as_path()).collect::<Vec<_>>(),
            [Path::new("/a")],
            "a batch of one is still a valid batch"
        );
        assert_eq!(q.running(), [PathBuf::from("/a")]);
        assert!(
            q.is_processing(Path::new("/a")) && q.is_processing(Path::new("/b")),
            "/a is running, /b still pending — both count as processing"
        );
    }
}
