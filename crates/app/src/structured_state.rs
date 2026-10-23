//! Pure structured-merge editor model: per-decision picks, the focused
//! decision cursor and assembly gating over one `StructuredDocument`. The
//! twin of `merge_state::MergeState` for JSON/JSONC files. No gpui; the view
//! layer drives this and renders whatever it reports.

use tomte_core::structured::{AssembleError, Pick, Resolution, StructuredDocument};

/// Which half of the structured editor is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The per-key decision list.
    Structure,
    /// The free-text result editor.
    Text,
}

/// Editor state for one target: the computed structured document plus the
/// user's running resolution.
#[derive(Debug)]
pub struct StructuredState {
    pub doc: StructuredDocument,
    pub resolution: Resolution,
    /// Focused decision id.
    pub cursor: Option<usize>,
    /// Base snapshot was missing or unusable, so the merge is two-way.
    pub degraded_base: bool,
    pub stage: Stage,
}

impl StructuredState {
    /// Wrap a computed document; the cursor starts on the first required
    /// decision, if any.
    pub fn new(doc: StructuredDocument) -> Self {
        let cursor = doc.required_decisions().first().copied();
        let degraded_base = doc.degraded_base;
        Self {
            doc,
            resolution: Resolution::default(),
            cursor,
            degraded_base,
            stage: Stage::Structure,
        }
    }

    /// Decision ids that require a pick (unlocked conflicts), in order.
    pub fn conflicts(&self) -> Vec<usize> {
        self.doc.required_decisions()
    }

    /// Conflicts the user has not chosen for yet.
    pub fn unresolved(&self) -> Vec<usize> {
        self.conflicts()
            .into_iter()
            .filter(|&id| self.resolution.get(id).is_none())
            .collect()
    }

    /// `(decided, total)` over the conflicts, the shape `MergeState` reports.
    pub fn progress(&self) -> (usize, usize) {
        let total = self.conflicts().len();
        let undecided = self.unresolved().len();
        (total - undecided, total)
    }

    /// Record `pick` for decision `id`, overriding any earlier pick. A locked
    /// decision follows source and ignores the pick. Picking the focused
    /// decision advances the cursor to the next unresolved conflict.
    pub fn pick(&mut self, id: usize, pick: Pick) {
        if self.doc.decisions[id].locked.is_some() {
            return;
        }
        self.resolution.set(id, pick);
        if self.cursor == Some(id) {
            self.next_unresolved();
        }
    }

    /// Advance the cursor to the next unresolved conflict after the current
    /// one, wrapping around; clears the cursor and returns `None` once every
    /// conflict is resolved.
    pub fn next_unresolved(&mut self) -> Option<usize> {
        let next = next_after(self.cursor, &self.unresolved());
        self.cursor = next;
        next
    }

    /// Every decision the user can still decide or revisit: all unlocked
    /// decisions, in order.
    pub fn changed(&self) -> Vec<usize> {
        self.doc
            .decisions
            .iter()
            .filter(|d| d.locked.is_none())
            .map(|d| d.id)
            .collect()
    }

    /// Advance the cursor to the next unlocked decision after the current
    /// one, wrapping.
    pub fn next_changed(&mut self) -> Option<usize> {
        let next = next_after(self.cursor, &self.changed());
        self.cursor = next;
        next
    }

    /// The assembled result once every conflict has a pick; `Ok(None)` while
    /// any is open. A result that does not re-parse is an error, never `None`.
    pub fn assembled(&self) -> Result<Option<String>, AssembleError> {
        match self.doc.assemble(&self.resolution) {
            Ok(text) => Ok(Some(text)),
            Err(AssembleError::Unresolved { .. }) => Ok(None),
            Err(invalid @ AssembleError::Invalid { .. }) => Err(invalid),
        }
    }

    /// The text the result editor starts from; unresolved decisions show
    /// source.
    pub fn seed(&self) -> String {
        self.doc.seed(&self.resolution)
    }

    /// The 0-based line in [`Self::seed`] where each decision's result
    /// starts (the insertion line when the decision's text is absent).
    pub fn result_lines(&self) -> Vec<usize> {
        let seed = self.seed();
        self.doc
            .result_ranges(&self.resolution)
            .into_iter()
            .map(|range| seed[..range.start].matches('\n').count())
            .collect()
    }

    /// Both sides agree everywhere, so there is nothing to decide.
    pub fn nothing_to_decide(&self) -> bool {
        self.doc.decisions.is_empty()
    }
}

/// The first of `ids` after `cursor`, wrapping to the first; `None` if empty.
fn next_after(cursor: Option<usize>, ids: &[usize]) -> Option<usize> {
    match cursor {
        Some(current) => ids
            .iter()
            .copied()
            .find(|&id| id > current)
            .or_else(|| ids.first().copied()),
        None => ids.first().copied(),
    }
}

#[cfg(test)]
mod tests {
    use tomte_core::merge::RegionKind;
    use tomte_core::structured::{AssembleError, Pick, StructuredDocument};
    use tomte_core::template::anchor::anchor;
    use tomte_core::template::lexer::lex;

    use super::{Stage, StructuredState};

    const BASE: &str = "{\n  \"a\": 1,\n  \"b\": 1\n}\n";
    const OURS: &str = "{\n  \"a\": 2,\n  \"b\": 2\n}\n";
    const THEIRS: &str = "{\n  \"a\": 3,\n  \"b\": 3\n}\n";

    /// Two independent conflicts (a and b edited differently on both sides).
    fn two_conflicts() -> StructuredState {
        let doc = StructuredDocument::compute(Some(BASE), OURS, THEIRS, None).expect("structured");
        StructuredState::new(doc)
    }

    const EMAIL_TMPL: &str = "{\n  \"email\": \"{{ .email }}\",\n  \"editor\": \"hx\"\n}\n";
    const EMAIL_THEIRS: &str = "{\n  \"email\": \"me@x.y\",\n  \"editor\": \"hx\"\n}\n";

    /// One locked conflict (the templated email) from a span map.
    fn locked_state() -> StructuredState {
        let base = EMAIL_THEIRS.replace("me@", "old@");
        let ours = "{\n  \"email\": \"other@x.y\",\n  \"editor\": \"hx\"\n}\n";
        let map = anchor(EMAIL_TMPL, &lex(EMAIL_TMPL).unwrap(), EMAIL_THEIRS);
        let doc = StructuredDocument::compute(Some(&base), ours, EMAIL_THEIRS, Some(&map))
            .expect("structured");
        StructuredState::new(doc)
    }

    // Mutation: `conflicts()` returns every decision instead of
    // `required_decisions()`, or `progress()` swaps decided and total.
    #[test]
    fn conflict_bookkeeping_tracks_decisions() {
        let doc = StructuredDocument::compute(
            Some("{\n  \"a\": 1\n}\n"),
            "{\n  \"a\": 2\n}\n",
            "{\n  \"a\": 3\n}\n",
            None,
        )
        .expect("structured");
        let mut state = StructuredState::new(doc);
        assert!(!state.degraded_base);
        assert_eq!(state.stage, Stage::Structure);
        let conflicts = state.conflicts();
        assert_eq!(conflicts, vec![0]);
        assert_eq!(state.unresolved(), conflicts);
        assert_eq!(state.progress(), (0, 1));
        assert_eq!(state.cursor, Some(0), "cursor starts on the conflict");

        state.pick(0, Pick::Disk);
        assert!(state.unresolved().is_empty());
        assert_eq!(state.progress(), (1, 1));
    }

    // Mutation: drop the `locked.is_some()` early return in `pick` -> the
    // resolution holds a pick for the locked decision and the cursor moves.
    #[test]
    fn pick_on_a_locked_decision_is_ignored() {
        let mut state = locked_state();
        assert!(state.doc.decisions[0].locked.is_some());
        state.cursor = Some(0);
        state.pick(0, Pick::Disk);
        assert_eq!(state.resolution.get(0), None);
        assert_eq!(
            state.cursor,
            Some(0),
            "an ignored pick does not move the cursor"
        );
        assert!(state.conflicts().is_empty());
        assert_eq!(state.progress(), (0, 0));
    }

    // Mutation: advance the cursor on every pick, or never.
    #[test]
    fn pick_on_the_cursor_advances_it_elsewhere_it_stays() {
        let mut state = two_conflicts();
        assert_eq!(state.cursor, Some(0));

        // Picking a non-focused decision leaves the cursor alone, even when
        // another conflict is open after the cursor.
        let doc = StructuredDocument::compute(
            Some("{\n  \"a\": 1,\n  \"b\": 1,\n  \"c\": 1\n}\n"),
            "{\n  \"a\": 2,\n  \"b\": 2,\n  \"c\": 2\n}\n",
            "{\n  \"a\": 3,\n  \"b\": 3,\n  \"c\": 3\n}\n",
            None,
        )
        .expect("structured");
        let mut three = StructuredState::new(doc);
        three.pick(2, Pick::Source);
        assert_eq!(three.cursor, Some(0));

        state.pick(1, Pick::Source);
        assert_eq!(state.cursor, Some(0));

        // Picking the focused one advances; nothing is left, so it clears.
        state.pick(0, Pick::Disk);
        assert_eq!(state.cursor, None);
    }

    // Mutation: drop the `or_else` wrap in `next_after`.
    #[test]
    fn next_unresolved_wraps_around() {
        let mut state = two_conflicts();
        assert_eq!(state.cursor, Some(0));
        assert_eq!(state.next_unresolved(), Some(1));
        assert_eq!(state.next_unresolved(), Some(0), "wraps to the start");

        // A single remaining conflict wraps onto itself.
        state.pick(1, Pick::Source);
        assert_eq!(state.next_unresolved(), Some(0));
        assert_eq!(state.next_unresolved(), Some(0));

        state.pick(0, Pick::Disk);
        assert_eq!(state.next_unresolved(), None);
        assert_eq!(state.cursor, None);
    }

    // Mutation: `changed()` keeps locked decisions, or `next_changed` stops
    // wrapping.
    #[test]
    fn next_changed_skips_locked_and_wraps() {
        let mut state = two_conflicts();
        assert_eq!(state.changed(), vec![0, 1]);
        state.cursor = Some(1);
        assert_eq!(state.next_changed(), Some(0));
        assert!(locked_state().changed().is_empty());
    }

    // Mutation: return Some(text) while a conflict is open, or build the
    // text without going through `doc.assemble`.
    #[test]
    fn assembled_gates_on_full_resolution_then_matches_doc_assemble() {
        let mut state = two_conflicts();
        assert!(state.assembled().unwrap().is_none(), "nothing picked yet");

        state.pick(0, Pick::Disk);
        assert!(
            state.assembled().unwrap().is_none(),
            "one conflict still open"
        );

        state.pick(1, Pick::Source);
        let assembled = state.assembled().unwrap().expect("fully resolved");
        assert_eq!(assembled, "{\n  \"a\": 2,\n  \"b\": 3\n}\n");
        assert_eq!(assembled, state.doc.assemble(&state.resolution).unwrap());
    }

    // Mutation: map `AssembleError::Invalid` to `Ok(None)` in `assembled`.
    #[test]
    fn assembled_surfaces_an_invalid_result_as_an_error() {
        let mut state = two_conflicts();
        state.doc.corrupt_source_for_tests();
        state.pick(0, Pick::Source);
        state.pick(1, Pick::Source);
        let err = state.assembled().expect_err("a broken result is an error");
        assert!(matches!(err, AssembleError::Invalid { .. }), "{err:?}");
    }

    // Mutation: copy `degraded_base` from nothing (hard-code false), or
    // count a two-way difference as a conflict.
    #[test]
    fn two_way_without_a_base_has_no_conflicts_and_offers_no_last_written() {
        let doc = StructuredDocument::compute(None, OURS, THEIRS, None).expect("structured");
        let state = StructuredState::new(doc);
        assert!(state.degraded_base);
        assert!(state.conflicts().is_empty());
        assert_eq!(state.progress(), (0, 0));
        assert_eq!(state.cursor, None);
        assert!(!state.nothing_to_decide(), "disk differs from source");
        // The engine stands source in for the missing base, so `has_base`
        // is true here; the view must gate LastWritten on `degraded_base`.
        assert!(
            state
                .doc
                .decisions
                .iter()
                .all(|d| d.kind == RegionKind::OursOnly)
        );
        assert!(state.assembled().unwrap().is_some());
    }

    // Mutation: report the byte offset, or the line of the range end, instead
    // of the 0-based line of the range start.
    #[test]
    fn result_lines_follow_the_seed() {
        let state = two_conflicts();
        let seed = state.seed();
        let lines = state.result_lines();
        assert_eq!(lines.len(), 2);
        let line_of = |needle: &str| seed.lines().position(|l| l.contains(needle)).unwrap();
        assert_eq!(lines[0], line_of("\"a\""));
        assert_eq!(lines[1], line_of("\"b\""));
        assert_eq!(lines[1], 2);
    }

    // Mutation: `nothing_to_decide` ignores the decision list.
    #[test]
    fn identical_sides_have_nothing_to_decide() {
        let doc = StructuredDocument::compute(Some(OURS), OURS, OURS, None).expect("structured");
        let state = StructuredState::new(doc);
        assert!(state.nothing_to_decide());
        assert!(state.result_lines().is_empty());
        assert_eq!(state.assembled().unwrap().as_deref(), Some(OURS));
    }
}
