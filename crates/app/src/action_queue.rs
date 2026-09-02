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

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use gpui::SharedString;

use crate::resolve::{ResolveEngine, ResolveError, ResolveOutcome};

/// One queued decision's actual work.
///
/// A boxed closure rather than an enum of every action shape: the merge
/// editor's payload (resolved text, loaded inputs, a repo-conflict source
/// path) has nothing in common with a one-click `keep_disk`, and modelling
/// both in one type meant carrying the union everywhere. Each call site
/// already holds exactly what it needs — it hands over the call, not the
/// arguments.
pub type Work =
    Box<dyn FnOnce(&ResolveEngine) -> Result<ResolveOutcome, ResolveError> + Send + 'static>;

pub struct QueuedAction {
    /// The destination file this decision is about. Identity for the UI: the
    /// row it came from, and the row a failure attaches to.
    pub target: PathBuf,
    /// What the user chose, in their words ("Keep disk", "Keep source ·
    /// origin", "Save"). Used verbatim in the outcome banner, so a queued
    /// action that fails minutes later still says which choice failed.
    pub label: SharedString,
    pub work: Work,
}

/// Pending and running decisions, plus the failures that have not been
/// acknowledged yet.
#[derive(Default)]
pub struct ActionQueue {
    pending: VecDeque<QueuedAction>,
    /// The one action currently executing, if any.
    running: Option<PathBuf>,
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

    /// Take the next action and mark it running. `None` while one is already
    /// running — the serialisation guarantee lives here, in one place.
    pub fn take_next(&mut self) -> Option<QueuedAction> {
        if self.running.is_some() {
            return None;
        }
        let next = self.pending.pop_front()?;
        self.running = Some(next.target.clone());
        Some(next)
    }

    /// Record an action's result. `err` present means it failed and the row
    /// must say so.
    pub fn finish(&mut self, target: &Path, err: Option<SharedString>) {
        if self.running.as_deref() == Some(target) {
            self.running = None;
        }
        if let Some(message) = err {
            self.failed.insert(target.to_path_buf(), message);
        }
    }

    /// Is this target queued or running? Such a row renders under
    /// "processing" and its buttons are inert — the only row in the app whose
    /// buttons are.
    pub fn is_processing(&self, target: &Path) -> bool {
        self.running.as_deref() == Some(target) || self.pending.iter().any(|p| p.target == target)
    }

    pub fn running(&self) -> Option<&Path> {
        self.running.as_deref()
    }

    /// Everything queued or running, in the order the user chose it.
    pub fn processing(&self) -> Vec<PathBuf> {
        self.running
            .iter()
            .cloned()
            .chain(self.pending.iter().map(|p| p.target.clone()))
            .collect()
    }

    /// How many decisions are still owed work, including the running one.
    pub fn depth(&self) -> usize {
        self.pending.len() + usize::from(self.running.is_some())
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

    fn action(target: &str, label: &str) -> QueuedAction {
        QueuedAction {
            target: PathBuf::from(target),
            label: label.to_string().into(),
            work: Box::new(|_| Ok(ResolveOutcome::NeedsMergeEditor)),
        }
    }

    /// The point of the whole thing: a second decision is accepted while the
    /// first is still running, instead of being discarded by an
    /// `if in_flight { return }` guard.
    #[test]
    fn a_second_decision_is_queued_while_the_first_runs() {
        let mut q = ActionQueue::default();
        q.push(action("/a", "Keep disk"));
        let first = q.take_next().expect("the first action runs");
        assert_eq!(first.target, Path::new("/a"));

        q.push(action("/b", "Keep source"));
        assert!(
            q.is_processing(Path::new("/b")),
            "the second decision is remembered, not dropped"
        );
        assert!(
            q.take_next().is_none(),
            "but it does not start until the first finishes — one git repo"
        );
        assert_eq!(q.depth(), 2);

        q.finish(Path::new("/a"), None);
        assert_eq!(
            q.take_next().map(|a| a.target),
            Some(PathBuf::from("/b")),
            "and starts as soon as the repo is free"
        );
    }

    /// Changing your mind before the action ran must not run both choices.
    #[test]
    fn requeueing_the_same_target_replaces_the_pending_choice() {
        let mut q = ActionQueue::default();
        q.push(action("/a", "keeps the repo busy"));
        q.take_next();
        q.push(action("/b", "Keep disk"));
        q.push(action("/b", "Keep source"));
        assert_eq!(q.depth(), 2, "/b is queued once, not twice");

        q.finish(Path::new("/a"), None);
        let next = q.take_next().expect("/b runs");
        assert_eq!(
            next.label.as_ref(),
            "Keep source",
            "the LAST choice wins; applying the abandoned one first would be wrong"
        );
    }

    /// A failure has to outlive the action, or nobody sees it: the queue moves
    /// on within milliseconds of the error.
    #[test]
    fn a_failure_is_remembered_against_its_target_until_retried() {
        let mut q = ActionQueue::default();
        q.push(action("/a", "Keep disk"));
        q.take_next();
        q.finish(Path::new("/a"), Some("op timed out".to_string().into()));

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
        q.push(action("/a", "Keep disk"));
        assert!(q.failure(Path::new("/a")).is_none());
    }

    #[test]
    fn processing_lists_the_running_action_first() {
        let mut q = ActionQueue::default();
        q.push(action("/a", "x"));
        q.push(action("/b", "y"));
        q.take_next();
        assert_eq!(
            q.processing(),
            vec![PathBuf::from("/a"), PathBuf::from("/b")],
            "the running one leads; the rest follow in the order chosen"
        );
    }
}
