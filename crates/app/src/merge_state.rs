//! Pure merge-editor model (plan 7 Task 2): per-region choices, the focused
//! conflict cursor, and assembly gating over one `MergeDocument`. No gpui —
//! the view layer drives this and renders whatever it reports.

use czui_core::merge::{Choice, MergeDocument, MergeOptions, Resolution};

use crate::merge_inputs::MergeInputs;

/// Editor state for one target: the computed 3-way document plus the user's
/// running resolution.
#[derive(Debug)]
pub struct MergeState {
    pub doc: MergeDocument,
    pub resolution: Resolution,
    /// Focused region index.
    pub cursor: Option<usize>,
    /// Base snapshot was missing → the merge degraded to 2-way
    /// (base := theirs).
    pub degraded_base: bool,
}

impl MergeState {
    /// Compute the document from the loaded panes. A missing base degrades
    /// to 2-way by using theirs as the base; the cursor starts on the first
    /// conflict, if any.
    pub fn new(inputs: &MergeInputs) -> Self {
        let base = inputs.base.as_deref().unwrap_or(&inputs.theirs);
        let doc =
            MergeDocument::compute(base, &inputs.ours, &inputs.theirs, MergeOptions::default());
        let cursor = doc.required_decisions().first().copied();
        Self {
            doc,
            resolution: Resolution::new(),
            cursor,
            degraded_base: inputs.base.is_none(),
        }
    }

    /// Region indices that require a decision (`RegionKind::Conflict`).
    pub fn conflicts(&self) -> Vec<usize> {
        self.doc.required_decisions()
    }

    /// Conflicts the user has not chosen for yet.
    pub fn unresolved(&self) -> Vec<usize> {
        self.conflicts()
            .into_iter()
            .filter(|&region| self.resolution.get(region).is_none())
            .collect()
    }

    /// `(decided, total)` over the conflict regions.
    pub fn progress(&self) -> (usize, usize) {
        let total = self.conflicts().len();
        let undecided = self.unresolved().len();
        (total - undecided, total)
    }

    /// Record `choice` for `region`, overriding any earlier pick. Picking
    /// the focused region advances the cursor to the next unresolved
    /// conflict.
    pub fn pick(&mut self, region: usize, choice: Choice) {
        self.resolution.set(region, choice);
        if self.cursor == Some(region) {
            self.next_unresolved();
        }
    }

    /// Advance the cursor to the next unresolved conflict after the current
    /// one, wrapping around; clears the cursor and returns `None` once every
    /// conflict is resolved.
    pub fn next_unresolved(&mut self) -> Option<usize> {
        let unresolved = self.unresolved();
        let next = match self.cursor {
            Some(current) => unresolved
                .iter()
                .copied()
                .find(|&region| region > current)
                .or_else(|| unresolved.first().copied()),
            None => unresolved.first().copied(),
        };
        self.cursor = next;
        next
    }

    /// The assembled result once every conflict has a choice; `None` while
    /// any is still undecided.
    pub fn assembled(&self) -> Option<String> {
        if self.unresolved().is_empty() {
            self.doc.assemble(&self.resolution).ok()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use czui_core::merge::{Choice, RegionKind};

    use super::MergeState;
    use crate::merge_inputs::MergeInputs;

    /// Pane-only inputs: the write-back context is irrelevant to the model.
    fn inputs(base: Option<&str>, ours: &str, theirs: &str) -> MergeInputs {
        MergeInputs {
            target: PathBuf::from("/home/u/.testrc"),
            ours: ours.to_string(),
            theirs: theirs.to_string(),
            base: base.map(str::to_string),
            source_path: PathBuf::from("/src/dot_testrc"),
            templated: false,
            span_map: None,
        }
    }

    /// Two independent conflicts (v and w edited differently on both sides).
    fn two_conflicts() -> MergeState {
        MergeState::new(&inputs(
            Some("a\nv = 1\nz\nw = 1\ne\n"),
            "a\nv = 2\nz\nw = 2\ne\n",
            "a\nv = 3\nz\nw = 3\ne\n",
        ))
    }

    #[test]
    fn conflict_bookkeeping_tracks_decisions() {
        let mut state = MergeState::new(&inputs(
            Some("a\nv = 1\nz\n"),
            "a\nv = 2\nz\n",
            "a\nv = 3\nz\n",
        ));
        assert!(!state.degraded_base);
        let conflicts = state.conflicts();
        assert_eq!(conflicts.len(), 1);
        let region = conflicts[0];
        assert_eq!(state.doc.regions[region].kind, RegionKind::Conflict);
        assert_eq!(state.unresolved(), conflicts);
        assert_eq!(state.progress(), (0, 1));
        assert_eq!(state.cursor, Some(region), "cursor starts on the conflict");

        state.pick(region, Choice::Ours);
        assert!(state.unresolved().is_empty());
        assert_eq!(state.progress(), (1, 1));
    }

    #[test]
    fn missing_base_degrades_to_two_way_against_theirs() {
        let state = MergeState::new(&inputs(None, "a\nlocal\n", "a\nrendered\n"));
        assert!(state.degraded_base);
        // base := theirs, so a pure disk edit is OursOnly — no conflict.
        assert!(state.conflicts().is_empty());
        assert_eq!(state.progress(), (0, 0));
        assert_eq!(state.cursor, None);
        assert_eq!(state.assembled().as_deref(), Some("a\nlocal\n"));
    }

    #[test]
    fn pick_overrides_an_earlier_choice() {
        let mut state = two_conflicts();
        let region = state.conflicts()[0];
        state.pick(region, Choice::Ours);
        assert_eq!(state.resolution.get(region), Some(&Choice::Ours));
        state.pick(region, Choice::Theirs);
        assert_eq!(state.resolution.get(region), Some(&Choice::Theirs));
        assert_eq!(
            state.progress(),
            (1, 2),
            "an override is still one decision"
        );
    }

    #[test]
    fn pick_can_override_a_non_conflict_default() {
        let mut state = MergeState::new(&inputs(None, "new\n", "old\n"));
        assert_eq!(state.doc.regions[0].kind, RegionKind::OursOnly);
        assert_eq!(state.assembled().as_deref(), Some("new\n"));
        state.pick(0, Choice::Base);
        assert_eq!(state.assembled().as_deref(), Some("old\n"));
        assert_eq!(state.progress(), (0, 0), "non-conflicts never count");
    }

    #[test]
    fn pick_on_the_cursor_advances_it_elsewhere_it_stays() {
        let mut state = two_conflicts();
        let conflicts = state.conflicts();
        let (first, second) = (conflicts[0], conflicts[1]);
        assert_eq!(state.cursor, Some(first));

        // Picking a non-focused region leaves the cursor alone.
        state.pick(second, Choice::Theirs);
        assert_eq!(state.cursor, Some(first));

        // Picking the focused region advances (wrapping past the resolved
        // second conflict → nothing left → cleared).
        state.pick(first, Choice::Ours);
        assert_eq!(state.cursor, None);
    }

    #[test]
    fn next_unresolved_wraps_around() {
        let mut state = two_conflicts();
        let conflicts = state.conflicts();
        let (first, second) = (conflicts[0], conflicts[1]);
        assert_eq!(state.cursor, Some(first));
        assert_eq!(state.next_unresolved(), Some(second));
        assert_eq!(state.next_unresolved(), Some(first), "wraps to the start");
        assert_eq!(state.cursor, Some(first));

        // A single remaining conflict wraps onto itself.
        state.pick(second, Choice::Theirs);
        assert_eq!(state.next_unresolved(), Some(first));
        assert_eq!(state.next_unresolved(), Some(first));

        state.pick(first, Choice::Ours);
        assert_eq!(state.next_unresolved(), None);
        assert_eq!(state.cursor, None);
    }

    #[test]
    fn assembled_gates_on_full_resolution_then_matches_doc_assemble() {
        let mut state = two_conflicts();
        let conflicts = state.conflicts();
        assert_eq!(
            state.assembled(),
            None,
            "unresolved conflicts gate assembly"
        );

        state.pick(conflicts[0], Choice::Ours);
        assert_eq!(state.assembled(), None, "one conflict still open");

        state.pick(conflicts[1], Choice::Theirs);
        let assembled = state.assembled().expect("fully resolved must assemble");
        assert_eq!(assembled, "a\nv = 2\nz\nw = 3\ne\n");
        assert_eq!(
            assembled,
            state.doc.assemble(&state.resolution).unwrap(),
            "assembled() is exactly doc.assemble"
        );
    }
}
