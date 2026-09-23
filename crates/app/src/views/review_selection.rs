//! The Review sidebar's multi-selection (spec §2.2–§2.3): which SAFE TO
//! RESOLVE rows are ticked, in display order, plus the anchor a ⇧-click
//! extends from. Pure — every gesture is a function over the plan the
//! sidebar is showing, so the rules are testable without a window.
//!
//! Membership is limited to what the caller passes as `safe_order`: the
//! plan's safe targets in the order they are displayed (a collapsed folder's
//! files included). Nothing else can ever become a member.

// consumed by review.rs from Task 5; remove then
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tomte_ui::components::CheckState;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Selection {
    /// Ticked targets, in `safe_order` order.
    members: Vec<PathBuf>,
    /// Where the next ⇧-click extends from: the last ⌘-clicked or ticked
    /// row. `pub(super)` so a test can read it.
    pub(super) anchor: Option<PathBuf>,
}

impl Selection {
    pub(super) fn members(&self) -> &[PathBuf] {
        &self.members
    }

    pub(super) fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub(super) fn contains(&self, target: &Path) -> bool {
        self.members.iter().any(|m| m == target)
    }

    /// ⌘-click or a checkbox on one safe row: in ↔ out. A target the plan
    /// does not list as safe is refused, silently — the row it came from
    /// has no checkbox, so this is a structural guard, not a user path.
    pub(super) fn toggle(&mut self, target: &Path, safe_order: &[PathBuf]) {
        if !safe_order.iter().any(|t| t == target) {
            return;
        }
        if self.contains(target) {
            self.members.retain(|m| m != target);
        } else {
            self.members.push(target.to_path_buf());
            self.reorder(safe_order);
        }
        self.anchor = Some(target.to_path_buf());
    }

    /// ⇧-click: add every VISIBLE safe row between the anchor and `to`,
    /// inclusive, in either direction. The anchor stays put (Finder's rule),
    /// so a second ⇧-click re-shapes the range instead of chaining. With no
    /// anchor the range starts at `fallback_anchor` (the detail row, when it
    /// is safe), else at the first visible row.
    pub(super) fn extend_range(
        &mut self,
        to: &Path,
        visible_order: &[PathBuf],
        fallback_anchor: Option<&Path>,
    ) {
        let index = |t: &Path| visible_order.iter().position(|v| v == t);
        let Some(to_ix) = index(to) else {
            return;
        };
        let from_ix = self
            .anchor
            .as_deref()
            .and_then(index)
            .or_else(|| fallback_anchor.and_then(index))
            .unwrap_or(0);
        let (lo, hi) = if from_ix <= to_ix {
            (from_ix, to_ix)
        } else {
            (to_ix, from_ix)
        };
        for t in &visible_order[lo..=hi] {
            if !self.contains(t) {
                self.members.push(t.clone());
            }
        }
        // Reorder by the visible list first, then keep any member it does
        // not mention (one hidden in a collapsed folder) after them rather
        // than dropping it; the next render's `prune` puts everything back
        // in `safe_order`.
        let mut order: Vec<PathBuf> = visible_order.to_vec();
        for m in &self.members {
            if !order.contains(m) {
                order.push(m.clone());
            }
        }
        self.reorder(&order);
    }

    /// A folder's or the header's checkbox: if every one of `targets` is
    /// already a member, drop them all; otherwise add them all
    /// (`CheckState::toggled` in set form).
    pub(super) fn toggle_all(&mut self, targets: &[PathBuf], safe_order: &[PathBuf]) {
        if targets.is_empty() {
            return;
        }
        let all_in = targets.iter().all(|t| self.contains(t));
        if all_in {
            self.members.retain(|m| !targets.contains(m));
        } else {
            for t in targets {
                if !self.contains(t) && safe_order.contains(t) {
                    self.members.push(t.clone());
                }
            }
            self.reorder(safe_order);
        }
    }

    pub(super) fn clear(&mut self) {
        self.members.clear();
        self.anchor = None;
    }

    /// The plan was rebuilt: keep only what it still lists as safe, in its
    /// order, and forget an anchor whose row is gone.
    pub(super) fn prune(&mut self, safe_order: &[PathBuf]) {
        let keep: BTreeSet<&PathBuf> = safe_order.iter().collect();
        self.members.retain(|m| keep.contains(m));
        self.reorder(safe_order);
        if let Some(a) = &self.anchor
            && !keep.contains(a)
        {
            self.anchor = None;
        }
    }

    /// What a checkbox over `targets` shows.
    pub(super) fn check_state(&self, targets: &[PathBuf]) -> CheckState {
        let selected = targets.iter().filter(|t| self.contains(t)).count();
        CheckState::of(selected, targets.len())
    }

    /// Put `members` in `order`'s order; members `order` does not mention
    /// keep their relative order at the end (only reachable mid-`extend_range`).
    fn reorder(&mut self, order: &[PathBuf]) {
        let mut sorted: Vec<PathBuf> = order.iter().filter(|t| self.contains(t)).cloned().collect();
        for m in &self.members {
            if !sorted.contains(m) {
                sorted.push(m.clone());
            }
        }
        self.members = sorted;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }
    fn order(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| p(n)).collect()
    }

    /// MUTATION: `toggle` pushes instead of keeping `safe_order` → the
    /// members read [c, a]; drop the refusal of a non-safe target → the last
    /// assertion fails.
    #[test]
    fn toggle_adds_and_removes_in_display_order_and_refuses_non_safe() {
        let safe = order(&["/a", "/b", "/c"]);
        let mut s = Selection::default();
        s.toggle(&p("/c"), &safe);
        s.toggle(&p("/a"), &safe);
        assert_eq!(s.members(), &[p("/a"), p("/c")]);
        assert!(s.contains(&p("/c")) && !s.contains(&p("/b")));
        s.toggle(&p("/c"), &safe);
        assert_eq!(s.members(), &[p("/a")]);
        s.toggle(&p("/needs-you"), &safe);
        assert_eq!(
            s.members(),
            &[p("/a")],
            "a non-safe target is never a member"
        );
    }

    /// MUTATION: `extend_range` ignores the anchor and selects from the top →
    /// the first range reads [a, b, c, d]; make it move the anchor → the
    /// second range (⇧-click back toward the start) drops [b, c].
    #[test]
    fn extend_range_spans_anchor_to_target_over_visible_rows_and_keeps_the_anchor() {
        let visible = order(&["/a", "/b", "/c", "/d", "/e"]);
        let mut s = Selection::default();
        s.toggle(&p("/b"), &visible); // anchor = b
        s.extend_range(&p("/d"), &visible, None);
        assert_eq!(s.members(), &[p("/b"), p("/c"), p("/d")]);
        s.extend_range(&p("/a"), &visible, None);
        assert_eq!(
            s.members(),
            &[p("/a"), p("/b"), p("/c"), p("/d")],
            "the anchor stayed on b: a..b was added, b..d kept"
        );
    }

    /// MUTATION: ignore `fallback_anchor` → the range starts at the first
    /// visible row and reads [a, b, c].
    #[test]
    fn extend_range_without_an_anchor_uses_the_fallback_then_the_first_row() {
        let visible = order(&["/a", "/b", "/c", "/d"]);
        let mut s = Selection::default();
        s.extend_range(&p("/c"), &visible, Some(&p("/b")));
        assert_eq!(s.members(), &[p("/b"), p("/c")]);
        let mut s = Selection::default();
        s.extend_range(&p("/b"), &visible, None);
        assert_eq!(s.members(), &[p("/a"), p("/b")]);
    }

    /// A collapsed folder's files are not visible: a ⇧-range over the
    /// visible list cannot reach them. MUTATION: pass `safe_order` where
    /// `visible_order` is used → hidden shows up in the range.
    #[test]
    fn extend_range_skips_what_is_not_visible() {
        let visible = order(&["/a", "/c"]);
        let mut s = Selection::default();
        s.toggle(&p("/a"), &order(&["/a", "/hidden", "/c"]));
        s.extend_range(&p("/c"), &visible, None);
        assert_eq!(s.members(), &[p("/a"), p("/c")]);
    }

    /// MUTATION: `toggle_all` always adds → the second call keeps [a, b];
    /// make it clear only when the set is a subset → same failure.
    #[test]
    fn toggle_all_completes_a_partial_set_and_clears_a_complete_one() {
        let safe = order(&["/a", "/b", "/c"]);
        let folder = order(&["/a", "/b"]);
        let mut s = Selection::default();
        s.toggle(&p("/a"), &safe);
        assert_eq!(s.check_state(&folder), CheckState::Partial);
        s.toggle_all(&folder, &safe);
        assert_eq!(s.members(), &[p("/a"), p("/b")]);
        assert_eq!(s.check_state(&folder), CheckState::On);
        s.toggle_all(&folder, &safe);
        assert!(s.is_empty());
        assert_eq!(s.check_state(&folder), CheckState::Off);
        assert_eq!(
            s.check_state(&[]),
            CheckState::Off,
            "an empty set is never On"
        );
    }

    /// MUTATION: `prune` keeps members in their old order → [c, a]; forget to
    /// drop the anchor → a later ⇧-range starts from a row that is gone.
    #[test]
    fn prune_keeps_the_intersection_in_the_new_order_and_drops_a_gone_anchor() {
        let safe = order(&["/a", "/b", "/c"]);
        let mut s = Selection::default();
        s.toggle(&p("/c"), &safe);
        s.toggle(&p("/a"), &safe);
        s.toggle(&p("/b"), &safe); // anchor = b
        s.prune(&order(&["/c", "/a"])); // b resolved; the order changed
        assert_eq!(s.members(), &[p("/c"), p("/a")]);
        assert_eq!(s.anchor, None, "the anchor left with its row");
        s.clear();
        assert!(s.is_empty());
    }
}
