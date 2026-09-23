# Review Multi-Select and Folder Tree Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the user tick several SAFE TO RESOLVE rows (or a whole folder in a new tree view) and resolve them with one Keep disk / Keep source press that runs as ONE batch.

**Architecture:** Two new pure modules beside `review.rs` — `review_selection.rs` (the ordered selection with ⌘/⇧/folder/header semantics) and `review_tree.rs` (the safe group projected as a compacted tree) — feed a rebuilt `sidebar_rows` plan that the view renders verbatim. The Shell gains `enqueue_many`, which pushes every decision and pumps once so a bulk press is one re-add/apply/commit/push. `tomte-ui` gains a tri-state `checkbox` and a `folder_row` builder. All new state (mode, selection, collapsed set) is session-only on the view.

**Tech Stack:** Rust 2024, GPUI 0.2.2 (pinned), tomte-ui builders, the existing `EngineLab` for a real-daemon story, `scripts/shoot.sh` for screenshots.

**Spec:** `.agents/artifacts/specs/2026-09-23-review-multiselect-tree-design.md`

## Global Constraints

- Only SAFE TO RESOLVE rows are selectable; bulk actions are exactly Keep disk and Keep source · this machine (spec §2.1).
- A bulk press is ONE batch: `Shell::enqueue_many` pushes all, pumps once (spec §2.4).
- Mode, selection and collapse state are session-only; nothing is persisted (spec §2.6).
- In Tree mode only the SAFE TO RESOLVE group changes shape; PROCESSING and NEEDS A DECISION stay flat (spec §2.5).
- GPUI: never update an entity from inside its own update closure. The view→Shell hand-offs here (`enqueue_many`) are safe only because `pump` never synchronously updates the review view (see `AGENTS.md`, Rules → GPUI).
- gpui `.truncate()` never ellipsizes: use `line_clamp(1)` over a definite width, or shorten content.
- Every new test names, in a comment, the mutation that fails it; run at least the named mutations.
- The gate before every commit, IN THE FOREGROUND: `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E '^warning' | grep -v -E 'block v0\.1\.6|proc-macro-error2'` (must print nothing); `cargo test --workspace`; `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null cargo test --workspace`.
- Screenshots are part of any UI task: `./scripts/shoot.sh <state> dark|light`, then READ the PNG. `screencapture` fails in a sandboxed shell; run it unsandboxed.
- Commits are SSH-signed via 1Password; on failure leave staged and report, never `--no-gpg-sign`.
- New reusable styling goes in `crates/ui` first, never inline in a view (DESIGN.md).

---

### Task 1: `tomte-ui`: tri-state `checkbox` and `folder_row`

**Files:**
- Create: `crates/ui/src/components/checkbox.rs`
- Modify: `crates/ui/src/components/mod.rs` (add `pub mod checkbox; pub use checkbox::*;` in the existing alphabetical lists)
- Modify: `crates/ui/src/preview/mod.rs` (two new entries in `COMPONENTS`, two render arms)

**Interfaces:**
- Produces:
  ```rust
  pub enum CheckState { Off, Partial, On }              // Copy, Eq, Debug
  impl CheckState { pub fn of(selected: usize, total: usize) -> Self; pub fn toggled(self) -> Self }
  pub fn checkbox(theme: Theme, id: impl Into<ElementId>, state: CheckState,
                  on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Stateful<Div>
  pub struct FolderRow { pub depth: usize, pub label: SharedString, pub collapsed: bool,
                         pub count: SharedString, pub check: CheckState, pub show_check: bool }
  pub fn folder_row(theme: Theme, id: impl Into<ElementId>, row: FolderRow,
                    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
                    on_check: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Stateful<Div>
  ```

- [ ] **Step 1: Verify two gpui facts against the pinned source**

Run:
```bash
G=$(ls -d ~/.cargo/registry/src/*/gpui-0.2.2 | head -1)
rg -n 'pub fn stop_propagation' $G/src/app.rs $G/src/window.rs
rg -n 'pub fn with_transformation' $G/src/elements/svg.rs
rg -n 'pub fn radians' $G/src/geometry.rs
```
Expected: `stop_propagation` exists on `App` (so a child's click can stop the parent row's `on_click`); `Svg::with_transformation` and `radians` exist. If `stop_propagation` lives on `Window` instead, call it on the window argument — say which in the report.

- [ ] **Step 2: Write the unit tests for `CheckState`**

In `crates/ui/src/components/checkbox.rs` (the file does not exist yet — create it with the tests at the bottom and the code from Step 4 above them):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// MUTATION: make `of` return `Partial` for `selected == total` → the
    /// second assertion fails; make `toggled` map Partial → Off → the last
    /// fails.
    #[test]
    fn check_state_of_counts_and_toggles_partial_to_on() {
        assert_eq!(CheckState::of(0, 3), CheckState::Off);
        assert_eq!(CheckState::of(3, 3), CheckState::On);
        assert_eq!(CheckState::of(1, 3), CheckState::Partial);
        assert_eq!(CheckState::of(0, 0), CheckState::Off, "nothing to select is Off, not On");
        assert_eq!(CheckState::Off.toggled(), CheckState::On);
        assert_eq!(CheckState::On.toggled(), CheckState::Off);
        assert_eq!(CheckState::Partial.toggled(), CheckState::On, "a partial tick completes the set");
    }
}
```

- [ ] **Step 3: Run it to see it fail**

Run: `cargo test -p tomte-ui --lib check_state` → FAIL to compile (no `CheckState`).

- [ ] **Step 4: Implement the component**

```rust
//! Tri-state checkbox and the folder row of a file tree — pure builders
//! (Theme + data + plain click handlers, no entity coupling), like every
//! component here.

use gpui::{
    App, ClickEvent, Div, ElementId, SharedString, Stateful, Transformation, Window, div,
    prelude::*, px, radians,
};

use crate::theme::Theme;

use super::icon::chevron_down;

/// What a checkbox over a SET shows: none, some, or all of the set ticked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Off,
    Partial,
    On,
}

impl CheckState {
    /// The state of a checkbox standing for `total` items of which
    /// `selected` are ticked. An empty set is Off: there is nothing to be
    /// "all of".
    pub fn of(selected: usize, total: usize) -> Self {
        if total == 0 || selected == 0 {
            Self::Off
        } else if selected == total {
            Self::On
        } else {
            Self::Partial
        }
    }

    /// Where a click takes it: Off and Partial complete the set, On clears
    /// it — the macOS rule, and the one that makes "tick the folder" mean
    /// "everything under it" even when some of it already was.
    pub fn toggled(self) -> Self {
        match self {
            Self::On => Self::Off,
            Self::Off | Self::Partial => Self::On,
        }
    }
}

/// A 14px square. Off: hairline border. On: accent fill with a check.
/// Partial: accent fill with a dash. The handler runs INSTEAD of the row's
/// own click: the click is stopped here so a tick never also selects.
pub fn checkbox(
    theme: Theme,
    id: impl Into<ElementId>,
    state: CheckState,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let filled = state != CheckState::Off;
    div()
        .id(id)
        .w(px(14.))
        .h(px(14.))
        .flex_none()
        .rounded(px(3.))
        .border_1()
        .border_color(if filled { theme.accent } else { theme.border })
        .when(filled, |el| el.bg(theme.accent))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when(state == CheckState::On, |el| {
            el.text_color(theme.bg).text_xs().child("✓")
        })
        .when(state == CheckState::Partial, |el| {
            el.child(div().w(px(6.)).h(px(2.)).rounded(px(1.)).bg(theme.bg))
        })
        .on_click(move |ev, window, cx| {
            cx.stop_propagation();
            on_click(ev, window, cx);
        })
}

/// One folder of a file tree: chevron (rotated when collapsed), the label
/// (ending in `/`), a count chip, and — when the tree is in selection
/// mode — a checkbox for everything under it.
pub struct FolderRow {
    pub depth: usize,
    pub label: SharedString,
    pub collapsed: bool,
    /// "3" or "3 of 5" — what a tick reaches, of what is there.
    pub count: SharedString,
    pub check: CheckState,
    /// Forced visible (a selection exists) rather than hover-revealed.
    pub show_check: bool,
}

/// Indent per tree level, in px.
pub const TREE_INDENT: f32 = 12.;

pub fn folder_row(
    theme: Theme,
    id: impl Into<ElementId>,
    row: FolderRow,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_check: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let id = id.into();
    let check_id = ElementId::Name(format!("{}-check", id_name(&id)).into());
    let chevron = if row.collapsed {
        chevron_down(theme).with_transformation(Transformation::rotate(radians(
            -std::f32::consts::FRAC_PI_2,
        )))
    } else {
        chevron_down(theme)
    };
    div()
        .id(id)
        .group("tree-folder")
        .px_3()
        .py_1()
        .pl(px(12. + TREE_INDENT * row.depth as f32))
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .hover(|el| el.bg(Theme::wash(theme.text, 0.04)))
        .child(
            checkbox(theme, check_id, row.check, on_check)
                .opacity(if row.show_check { 1. } else { 0. })
                .group_hover("tree-folder", |el| el.opacity(1.)),
        )
        .child(chevron)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(theme.text)
                .line_clamp(1)
                .child(row.label),
        )
        .child(super::chip::chip(theme, row.count, super::chip::ChipVariant::Wash(theme.text_muted)))
        .on_click(on_toggle)
}

fn id_name(id: &ElementId) -> String {
    format!("{id:?}")
}
```

`ChipVariant::Wash(tint)` is whatever `crates/ui/src/components/chip.rs` names its tinted-text-on-wash variant (its first variant, "class chips, count badges") — read that file and use the real name; if the variant carries no tint, pass none. If `Theme::wash` is not a public associated fn, use whatever the other components use for a hover wash (grep `wash(` in `crates/ui/src/components/`).

- [ ] **Step 5: Register the module and the previews**

In `crates/ui/src/components/mod.rs` add `pub mod checkbox;` and `pub use checkbox::*;` in alphabetical position. In `crates/ui/src/preview/mod.rs` add to `COMPONENTS`:

```rust
    ("checkbox", "off / partial / on, and forced-visible in a row"),
    ("folder-row", "expanded, collapsed, partial-checked, n of m"),
```

and two arms in `render_component`, in the style of the `"dropdown"` arm (a column of `variant(theme, caption, el)` blocks):

```rust
        "checkbox" => div()
            .flex()
            .flex_col()
            .gap_6()
            .child(variant(theme, "off", checkbox(theme, "cb-off", CheckState::Off, noop).into_any_element()))
            .child(variant(theme, "partial", checkbox(theme, "cb-partial", CheckState::Partial, noop).into_any_element()))
            .child(variant(theme, "on", checkbox(theme, "cb-on", CheckState::On, noop).into_any_element()))
            .into_any_element(),
        "folder-row" => div()
            .flex()
            .flex_col()
            .gap_2()
            .w(px(260.))
            .child(folder_row(theme, "fr-1", FolderRow { depth: 0, label: ".config/".into(), collapsed: false, count: "4 of 6".into(), check: CheckState::Partial, show_check: true }, noop, noop))
            .child(folder_row(theme, "fr-2", FolderRow { depth: 1, label: "nvim/".into(), collapsed: true, count: "2".into(), check: CheckState::On, show_check: true }, noop, noop))
            .child(folder_row(theme, "fr-3", FolderRow { depth: 1, label: "zed/".into(), collapsed: false, count: "1".into(), check: CheckState::Off, show_check: false }, noop, noop))
            .into_any_element(),
```

- [ ] **Step 6: Run, shoot, read**

Run: `cargo test -p tomte-ui` → PASS. Then:
```bash
./scripts/shoot.sh comp:checkbox dark; ./scripts/shoot.sh comp:checkbox light
./scripts/shoot.sh comp:folder-row dark; ./scripts/shoot.sh comp:folder-row light
```
READ all four PNGs: the check glyph and the dash are centered; the collapsed chevron points right; the count chip sits at the row's right; the third folder row's checkbox is invisible at rest (hover-revealed).

- [ ] **Step 7: Gate and commit**

```bash
git add crates/ui/src/components/checkbox.rs crates/ui/src/components/mod.rs crates/ui/src/preview/mod.rs
git commit -m "feat(ui): tri-state checkbox and folder row"
```

---

### Task 2: `Selection` — the pure selection model

**Files:**
- Create: `crates/app/src/views/review_selection.rs`
- Modify: `crates/app/src/views/mod.rs` (add `mod review_selection;` next to `mod review;`)

**Interfaces:**
- Consumes: `tomte_ui::components::CheckState` (Task 1).
- Produces:
  ```rust
  pub(super) struct Selection { .. }   // Default, Debug, Clone, PartialEq, Eq
  impl Selection {
      pub(super) fn members(&self) -> &[PathBuf];
      pub(super) fn is_empty(&self) -> bool;
      pub(super) fn contains(&self, target: &Path) -> bool;
      pub(super) fn toggle(&mut self, target: &Path, safe_order: &[PathBuf]);
      pub(super) fn extend_range(&mut self, to: &Path, visible_order: &[PathBuf], fallback_anchor: Option<&Path>);
      pub(super) fn toggle_all(&mut self, targets: &[PathBuf], safe_order: &[PathBuf]);
      pub(super) fn clear(&mut self);
      pub(super) fn prune(&mut self, safe_order: &[PathBuf]);
      pub(super) fn check_state(&self, targets: &[PathBuf]) -> CheckState;
  }
  ```

- [ ] **Step 1: Write the failing tests**

Create `crates/app/src/views/review_selection.rs` with this test module at the bottom (the code from Step 3 goes above it):

```rust
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
        assert_eq!(s.members(), &[p("/a")], "a non-safe target is never a member");
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
        assert_eq!(s.check_state(&[]), CheckState::Off, "an empty set is never On");
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
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p tomte-app --bin tomte review_selection` → FAIL to compile.

- [ ] **Step 3: Implement**

Above the tests in `crates/app/src/views/review_selection.rs`:

```rust
//! The Review sidebar's multi-selection (spec §2.2–§2.3): which SAFE TO
//! RESOLVE rows are ticked, in display order, plus the anchor a ⇧-click
//! extends from. Pure — every gesture is a function over the plan the
//! sidebar is showing, so the rules are testable without a window.
//!
//! Membership is limited to what the caller passes as `safe_order`: the
//! plan's safe targets in the order they are displayed (a collapsed folder's
//! files included). Nothing else can ever become a member.

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
        let (lo, hi) = if from_ix <= to_ix { (from_ix, to_ix) } else { (to_ix, from_ix) };
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
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tomte-app --bin tomte review_selection` → 6 passed. Run the two named mutations (toggle pushes without reorder; extend_range ignores the anchor) and confirm the named tests fail; revert by hand.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/app/src/views/review_selection.rs crates/app/src/views/mod.rs
git commit -m "feat(review): the pure multi-selection model"
```

---

### Task 3: `tree_rows` — the safe group as a compacted tree

**Files:**
- Create: `crates/app/src/views/review_tree.rs`
- Modify: `crates/app/src/views/mod.rs` (add `mod review_tree;`)

**Interfaces:**
- Produces:
  ```rust
  pub(super) struct FolderNode { pub path: PathBuf, pub depth: usize, pub label: SharedString,
                                 pub safe: usize, pub total: usize, pub collapsed: bool }
  pub(super) enum TreeRow { Folder(FolderNode), File { target: PathBuf, depth: usize } }
  pub(super) fn tree_rows(safe: &[PathBuf], excluded: &[PathBuf], collapsed: &BTreeSet<PathBuf>, home: &Path) -> Vec<TreeRow>;
  pub(super) fn safe_descendants(safe: &[PathBuf], folder: &Path, home: &Path) -> Vec<PathBuf>;
  pub(super) fn tree_order(safe: &[PathBuf], home: &Path) -> Vec<PathBuf>;   // every safe file in tree order, collapse ignored
  ```
  `FolderNode::path` is the ABSOLUTE directory the row stands for (the deepest directory of a compacted chain); `collapsed` is looked up by that path.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/Users/x")
    }
    fn h(rel: &str) -> PathBuf {
        home().join(rel)
    }
    fn folders(rows: &[TreeRow]) -> Vec<(String, usize, usize, usize)> {
        rows.iter()
            .filter_map(|r| match r {
                TreeRow::Folder(f) => Some((f.label.to_string(), f.depth, f.safe, f.total)),
                _ => None,
            })
            .collect()
    }
    fn files(rows: &[TreeRow]) -> Vec<(PathBuf, usize)> {
        rows.iter()
            .filter_map(|r| match r {
                TreeRow::File { target, depth } => Some((target.clone(), *depth)),
                _ => None,
            })
            .collect()
    }

    /// MUTATION: compact only by NAME match, or never compact → the first
    /// folder reads ".config/" with "nvim/" under it; compact past a folder
    /// that holds a file → ".config/nvim/lua/" swallows "init.lua"'s parent.
    #[test]
    fn single_child_directory_chains_compact_and_stop_at_a_file() {
        let safe = vec![h(".config/nvim/init.lua"), h(".config/nvim/lua/plugins.lua")];
        let rows = tree_rows(&safe, &[], &BTreeSet::new(), &home());
        assert_eq!(
            folders(&rows),
            vec![(".config/nvim/".into(), 0, 2, 2), ("lua/".into(), 1, 1, 1)]
        );
        assert_eq!(
            files(&rows),
            vec![(h(".config/nvim/lua/plugins.lua"), 2), (h(".config/nvim/init.lua"), 1)],
            "folders first, then files; each at its own depth"
        );
    }

    /// MUTATION: sort case-sensitively → "Zed" sorts before "nvim".
    #[test]
    fn directories_before_files_both_case_insensitive() {
        let safe = vec![h("b.txt"), h("A.txt"), h("zed/s.json"), h("Nvim/i.lua")];
        let rows = tree_rows(&safe, &[], &BTreeSet::new(), &home());
        assert_eq!(
            folders(&rows).iter().map(|f| f.0.clone()).collect::<Vec<_>>(),
            vec!["Nvim/", "zed/"]
        );
        let names: Vec<String> = files(&rows)
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["i.lua", "s.json", "A.txt", "b.txt"]);
    }

    /// MUTATION: count `excluded` into `safe` → the folder says 3 of 3.
    #[test]
    fn a_folder_counts_what_a_tick_reaches_of_what_is_there() {
        let safe = vec![h(".config/a"), h(".config/b")];
        let excluded = vec![h(".config/needs-you")];
        let rows = tree_rows(&safe, &excluded, &BTreeSet::new(), &home());
        assert_eq!(folders(&rows), vec![(".config/".into(), 0, 2, 3)]);
        assert_eq!(files(&rows).len(), 2, "the excluded file is not a row here");
    }

    /// MUTATION: emit a folder with no safe descendants → an empty ".other/"
    /// row appears for a folder the user cannot act on.
    #[test]
    fn a_folder_with_nothing_safe_under_it_is_not_shown() {
        let rows = tree_rows(&[h("a")], &[h(".other/x")], &BTreeSet::new(), &home());
        assert!(folders(&rows).is_empty());
    }

    /// MUTATION: `collapsed` hides the folder row itself, or `safe_descendants`
    /// honours `collapsed` → the last assertion fails.
    #[test]
    fn a_collapsed_folder_hides_its_rows_but_a_tick_still_reaches_them() {
        let safe = vec![h(".config/a"), h(".config/b"), h("top")];
        let collapsed = BTreeSet::from([h(".config")]);
        let rows = tree_rows(&safe, &[], &collapsed, &home());
        assert_eq!(folders(&rows), vec![(".config/".into(), 0, 2, 2)]);
        assert!(matches!(&rows[0], TreeRow::Folder(f) if f.collapsed));
        assert_eq!(files(&rows), vec![(h("top"), 0)], "the collapsed folder's files are gone from the plan");
        assert_eq!(
            safe_descendants(&safe, &h(".config"), &home()),
            vec![h(".config/a"), h(".config/b")]
        );
    }

    /// MUTATION: root everything at home → "/etc/hosts" becomes "etc/".
    #[test]
    fn a_target_outside_home_roots_at_slash() {
        let rows = tree_rows(&[PathBuf::from("/etc/hosts"), h("a")], &[], &BTreeSet::new(), &home());
        assert_eq!(folders(&rows), vec![("/etc/".into(), 0, 1, 1)]);
    }

    /// MUTATION: `tree_order` honours a collapse set (it must not take one)
    /// or lists files before folders → the order differs from the rows'.
    #[test]
    fn tree_order_is_the_rows_file_order_with_nothing_collapsed() {
        let safe = vec![h("z"), h(".config/nvim/init.lua"), h(".config/a")];
        let expected: Vec<PathBuf> = files(&tree_rows(&safe, &[], &BTreeSet::new(), &home()))
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(tree_order(&safe, &home()), expected);
        assert_eq!(expected, vec![h(".config/nvim/init.lua"), h(".config/a"), h("z")]);
    }
}
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p tomte-app --bin tomte review_tree` → FAIL to compile.

- [ ] **Step 3: Implement**

```rust
//! The SAFE TO RESOLVE group as a folder tree (spec §2.5): a pure projection
//! of target paths into folder and file rows, rooted at `$HOME` (which is not
//! shown), single-child directory chains compacted, folders before files,
//! case-insensitive. `excluded` are the targets that sit in the other two
//! groups: they are never rows here, but a folder counts them so its chip
//! can say "2 of 3" — what a tick reaches, of what is there.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use gpui::SharedString;

pub(super) struct FolderNode {
    /// The absolute directory this row stands for — the DEEPEST directory of
    /// a compacted chain, which is what `collapsed` is keyed by.
    pub path: PathBuf,
    pub depth: usize,
    /// `.config/nvim/` — the compacted chain, trailing slash.
    pub label: SharedString,
    pub safe: usize,
    pub total: usize,
    pub collapsed: bool,
}

pub(super) enum TreeRow {
    Folder(FolderNode),
    File { target: PathBuf, depth: usize },
}

#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    /// Safe files directly here: (sort key, target).
    safe_files: Vec<(String, PathBuf)>,
    /// Files here that are NOT safe (needs-you / processing): counted, never listed.
    excluded_here: usize,
    safe: usize,
    total: usize,
}

impl Node {
    fn insert(&mut self, rel: &[String], target: &Path, safe: bool) {
        self.total += 1;
        if safe {
            self.safe += 1;
        }
        match rel {
            [] => unreachable!("a target has a file name"),
            [name] => {
                if safe {
                    self.safe_files.push((name.to_lowercase(), target.to_path_buf()));
                } else {
                    self.excluded_here += 1;
                }
            }
            [dir, rest @ ..] => self.dirs.entry(dir.clone()).or_default().insert(rest, target, safe),
        }
    }

    fn has_files(&self) -> bool {
        !self.safe_files.is_empty() || self.excluded_here > 0
    }
}

/// Split `target` into the components under its root: home-relative when
/// under `home`, else absolute with a leading "" so the first label reads
/// "/etc/". Returns (root-is-slash, components).
fn split(target: &Path, home: &Path) -> (bool, Vec<String>) {
    if let Ok(rel) = target.strip_prefix(home) {
        let comps = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        return (false, comps);
    }
    let comps = target
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    (true, comps)
}

fn build(safe: &[PathBuf], excluded: &[PathBuf], home: &Path) -> (Node, Node) {
    let mut under_home = Node::default();
    let mut under_root = Node::default();
    for (targets, is_safe) in [(safe, true), (excluded, false)] {
        for t in targets {
            let (slash, comps) = split(t, home);
            if comps.is_empty() {
                continue;
            }
            let node = if slash { &mut under_root } else { &mut under_home };
            node.insert(&comps, t, is_safe);
        }
    }
    (under_home, under_root)
}

/// Walk one directory node: emit its (compacted) folder row, then — unless
/// collapsed — its children. `dir_path` is the absolute path of `node`.
fn walk(
    name: &str,
    node: &Node,
    dir_path: PathBuf,
    depth: usize,
    collapsed: &BTreeSet<PathBuf>,
    out: &mut Vec<TreeRow>,
) {
    if node.safe == 0 {
        return;
    }
    // Compact: a directory with no files and exactly one child directory is
    // one row with its child. By structure, never by name.
    let mut label = format!("{name}/");
    let mut node = node;
    let mut path = dir_path;
    while !node.has_files() && node.dirs.len() == 1 {
        let (child_name, child) = node.dirs.iter().next().expect("len checked");
        label.push_str(child_name);
        label.push('/');
        path = path.join(child_name);
        node = child;
    }
    let is_collapsed = collapsed.contains(&path);
    out.push(TreeRow::Folder(FolderNode {
        path: path.clone(),
        depth,
        label: label.into(),
        safe: node.safe,
        total: node.total,
        collapsed: is_collapsed,
    }));
    if is_collapsed {
        return;
    }
    emit_children(node, &path, depth + 1, collapsed, out);
}

fn emit_children(node: &Node, dir_path: &Path, depth: usize, collapsed: &BTreeSet<PathBuf>, out: &mut Vec<TreeRow>) {
    let mut dirs: Vec<(&String, &Node)> = node.dirs.iter().collect();
    dirs.sort_by_key(|(name, _)| name.to_lowercase());
    for (name, child) in dirs {
        walk(name, child, dir_path.join(name), depth, collapsed, out);
    }
    let mut files = node.safe_files.clone();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, target) in files {
        out.push(TreeRow::File { target, depth });
    }
}

pub(super) fn tree_rows(
    safe: &[PathBuf],
    excluded: &[PathBuf],
    collapsed: &BTreeSet<PathBuf>,
    home: &Path,
) -> Vec<TreeRow> {
    let (under_home, under_root) = build(safe, excluded, home);
    let mut out = Vec::new();
    // Under home: the root is not shown; its children start at depth 0.
    emit_children(&under_home, home, 0, collapsed, &mut out);
    // Outside home: each top-level directory reads "/etc/".
    let mut dirs: Vec<(&String, &Node)> = under_root.dirs.iter().collect();
    dirs.sort_by_key(|(name, _)| name.to_lowercase());
    for (name, child) in dirs {
        walk(&format!("/{name}"), child, PathBuf::from("/").join(name), 0, collapsed, &mut out);
    }
    out
}

/// Every safe file in tree order, collapse ignored — the order `Selection`
/// keeps members in while the tree is showing.
pub(super) fn tree_order(safe: &[PathBuf], home: &Path) -> Vec<PathBuf> {
    tree_rows(safe, &[], &BTreeSet::new(), home)
        .into_iter()
        .filter_map(|r| match r {
            TreeRow::File { target, .. } => Some(target),
            TreeRow::Folder(_) => None,
        })
        .collect()
}

/// Every safe file under `folder` (an absolute directory), in tree order,
/// collapsed or not — what ticking the folder reaches.
pub(super) fn safe_descendants(safe: &[PathBuf], folder: &Path, home: &Path) -> Vec<PathBuf> {
    tree_order(safe, home)
        .into_iter()
        .filter(|t| t.starts_with(folder))
        .collect()
}
```

Note the `walk` for outside-home passes a label already prefixed with `/`: `format!("{name}/")` then reads `/etc/`. Keep `label` construction consistent with the test (`"/etc/"`).

- [ ] **Step 4: Run the tests; run two named mutations; revert by hand**

Run: `cargo test -p tomte-app --bin tomte review_tree` → 7 passed.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/app/src/views/review_tree.rs crates/app/src/views/mod.rs
git commit -m "feat(review): the safe group as a compacted folder tree (pure)"
```

---

### Task 4: `Shell::enqueue_many` — one press, one batch

**Files:**
- Modify: `crates/app/src/views/mod.rs` (`Shell::enqueue` ~:840, its doc; the `EngineLab` tests near `three_decisions_queued_before_the_drain_are_one_push`)

**Interfaces:**
- Produces: `pub fn Shell::enqueue_many(&mut self, actions: Vec<QueuedAction>, cx: &mut Context<Self>)`; `enqueue` delegates to it.

- [ ] **Step 1: Write the failing lab story**

Beside `three_decisions_queued_before_the_drain_are_one_push` in `crates/app/src/views/mod.rs`'s test module:

```rust
    /// A bulk press is ONE batch: `enqueue_many` pushes every decision and
    /// pumps once, so three files are one re-add, one commit, one push.
    /// `enqueue` per file would pump on the first push and drain it alone
    /// (queue spec §2.2's "first click drains alone").
    /// MUTATION: make `enqueue_many` call `enqueue` per item → `running()`
    /// is 1 after the call and `engine_pushes()` is 2.
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
                assert!(s.queue.failure(target).is_none(), "{:?}", s.queue.failure(target));
            }
        });
        assert_eq!(origin_commit_count(&lab), before + 1, "ONE commit reaching origin");
        assert_eq!(lab.engine_pushes(), 1, "ONE push invocation: {:?}", lab.git_invocations());
    }
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p tomte-app --bin tomte a_bulk_press` → FAIL to compile (no `enqueue_many`).

- [ ] **Step 3: Implement**

Replace `Shell::enqueue`'s body and add `enqueue_many` right after it:

```rust
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
```

Keep `enqueue`'s doc comment above `enqueue`; move nothing else.

- [ ] **Step 4: Run the story and the named mutation**

Run: `cargo test -p tomte-app --bin tomte a_bulk_press` → PASS. Mutate (`for action in actions { self.enqueue(action, cx) }`) → FAIL on `running().len()`; revert by hand.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/app/src/views/mod.rs
git commit -m "feat(shell): enqueue_many — a bulk press is one batch"
```

---

### Task 5: the sidebar plan, v2

**Files:**
- Modify: `crates/app/src/views/review.rs` (`SidebarRow` ~:953, `sidebar_rows` ~:977, `sidebar()` ~:1543, `advance_from` ~:1234, the `sidebar_rows`/`SidebarRow` tests in `mod tests`)

**Interfaces:**
- Consumes: `Selection` (Task 2), `tree_rows`/`tree_order`/`safe_descendants`/`FolderNode` (Task 3), `CheckState` (Task 1).
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub(super) enum ReviewMode { Flat, Tree }
  pub(super) enum SidebarRow {
      Group { label: &'static str, check: Option<CheckState> },
      Folder { node: FolderNode, check: CheckState },
      Target { entry: DriftSummary, queued: Option<QueuedDecision>, locked: bool, depth: usize, selectable: bool, checked: bool },
  }
  pub(super) struct SidebarPlan { pub rows: Vec<SidebarRow>, pub safe_order: Vec<PathBuf>, pub visible_safe: Vec<PathBuf> }
  pub(super) struct SidebarInputs<'a> { pub drifted: &'a [DriftSummary], pub queue: &'a QueueSnapshot, pub mode: ReviewMode,
                                        pub collapsed: &'a BTreeSet<PathBuf>, pub selection: &'a Selection, pub home: &'a Path }
  pub(super) fn sidebar_rows(inputs: SidebarInputs<'_>) -> SidebarPlan;
  ```
  `safe_order` = every safe target in display order, collapsed ones included (what `Selection` reorders/prunes by); `visible_safe` = the `Target` rows with `selectable`, in row order (what ⇧-ranges and `next_after` walk).

- [ ] **Step 1: Rewrite the existing `sidebar_rows` tests and add the new ones**

Find every test in `review.rs`'s `mod tests` that constructs `SidebarRow::Group(..)`/`SidebarRow::Target { entry, queued, locked }` or calls `sidebar_rows(&drifted, &queue)` (`rg -n 'sidebar_rows\(|SidebarRow::' crates/app/src/views/review.rs`). Add a helper and re-express them:

```rust
    fn plan_of(drifted: &[DriftSummary], queue: &QueueSnapshot) -> SidebarPlan {
        sidebar_rows(SidebarInputs {
            drifted,
            queue,
            mode: ReviewMode::Flat,
            collapsed: &BTreeSet::new(),
            selection: &Selection::default(),
            home: Path::new("/Users/x"),
        })
    }
```

Their assertions keep their meaning: `SidebarRow::Group("PROCESSING")` becomes `SidebarRow::Group { label: "PROCESSING", .. }`, and `Target { entry, queued, locked }` patterns gain `..`. Then add:

```rust
    /// Flat mode: safe rows are selectable, needs-you and processing rows
    /// are not; `safe_order` and `visible_safe` agree and list the safe rows
    /// in group order; the SAFE header carries the tri-state.
    /// MUTATION: mark every Target selectable → the needs-you assertion
    /// fails; drop the header check → the last fails.
    #[test]
    fn flat_plan_marks_only_safe_rows_selectable_and_orders_them() {
        let drifted = vec![
            s("/Users/x/.config/a", "conflict"),
            s("/Users/x/b", "destination_drift"),
            s("/Users/x/c", "remote_ahead"),
        ];
        let plan = plan_of(&drifted, &QueueSnapshot::default());
        let selectable: Vec<(PathBuf, bool)> = plan
            .rows
            .iter()
            .filter_map(|r| match r {
                SidebarRow::Target { entry, selectable, .. } => Some((entry.target.clone(), *selectable)),
                _ => None,
            })
            .collect();
        assert_eq!(
            selectable,
            vec![(p("/Users/x/.config/a"), false), (p("/Users/x/b"), true), (p("/Users/x/c"), true)]
        );
        assert_eq!(plan.safe_order, vec![p("/Users/x/b"), p("/Users/x/c")]);
        assert_eq!(plan.visible_safe, plan.safe_order);
        assert!(plan.rows.iter().any(|r| matches!(r, SidebarRow::Group { label: "SAFE TO RESOLVE", check: Some(CheckState::Off) })));
        assert!(plan.rows.iter().any(|r| matches!(r, SidebarRow::Group { label: "NEEDS A DECISION", check: None })));
    }

    /// Tree mode: needs-you and processing stay flat above; the safe group is
    /// folders + files; a folder's check reflects the selection over its
    /// descendants; collapsed files leave `visible_safe` but not `safe_order`.
    /// MUTATION: build the tree over ALL drifted rows → the conflict shows
    /// up as a file row; use `visible_safe` for `safe_order` → the collapsed
    /// file is missing from it.
    #[test]
    fn tree_plan_keeps_the_other_groups_flat_and_tracks_collapse_and_ticks() {
        let drifted = vec![
            s("/Users/x/.config/needs", "conflict"),
            s("/Users/x/.config/nvim/init.lua", "destination_drift"),
            s("/Users/x/.config/nvim/lua/p.lua", "source_ahead"),
            s("/Users/x/top", "remote_ahead"),
        ];
        let mut selection = Selection::default();
        let order = vec![p("/Users/x/.config/nvim/init.lua"), p("/Users/x/.config/nvim/lua/p.lua"), p("/Users/x/top")];
        selection.toggle(&p("/Users/x/.config/nvim/init.lua"), &order);
        let collapsed = BTreeSet::from([p("/Users/x/.config/nvim/lua")]);
        let plan = sidebar_rows(SidebarInputs {
            drifted: &drifted,
            queue: &QueueSnapshot::default(),
            mode: ReviewMode::Tree,
            collapsed: &collapsed,
            selection: &selection,
            home: Path::new("/Users/x"),
        });
        let labels: Vec<String> = plan
            .rows
            .iter()
            .map(|r| match r {
                SidebarRow::Group { label, .. } => format!("G:{label}"),
                SidebarRow::Folder { node, check } => format!("F:{}:{}:{:?}", node.label, node.collapsed, check),
                SidebarRow::Target { entry, depth, selectable, checked, .. } => {
                    format!("T:{}:{depth}:{selectable}:{checked}", entry.target.file_name().unwrap().to_string_lossy())
                }
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                "G:NEEDS A DECISION",
                "T:needs:0:false:false",
                "G:SAFE TO RESOLVE",
                // `.config/` holds the excluded `needs`, so it is NOT
                // compacted into `nvim/` (a folder with a file stops the
                // chain — by structure, not by name).
                "F:.config/:false:Partial",
                "F:nvim/:false:Partial",
                "F:lua/:true:Off",
                "T:init.lua:2:true:true",
                "T:top:0:true:false",
            ]
        );
        assert_eq!(plan.safe_order, vec![p("/Users/x/.config/nvim/lua/p.lua"), p("/Users/x/.config/nvim/init.lua"), p("/Users/x/top")]);
        assert_eq!(plan.visible_safe, vec![p("/Users/x/.config/nvim/init.lua"), p("/Users/x/top")]);
    }
```

(`s(path, class)` and `p(..)` are the file's existing test helpers; add `p` if missing: `fn p(s: &str) -> PathBuf { PathBuf::from(s) }`.)

- [ ] **Step 2: Run to see them fail** — `cargo test -p tomte-app --bin tomte plan` → FAIL to compile.

- [ ] **Step 3: Implement the plan**

Replace `SidebarRow` and `sidebar_rows` in `review.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReviewMode {
    Flat,
    Tree,
}

pub(super) enum SidebarRow {
    Group {
        label: &'static str,
        /// The SAFE TO RESOLVE header's select-all; `None` on the others.
        check: Option<CheckState>,
    },
    /// Tree mode only: a folder of the safe group.
    Folder { node: FolderNode, check: CheckState },
    Target {
        entry: DriftSummary,
        queued: Option<QueuedDecision>,
        /// Past changing — see the note on the old field; still derived
        /// HERE from `queued`, never at the render site.
        locked: bool,
        /// Tree indent level; 0 in flat mode.
        depth: usize,
        /// A safe row: carries a checkbox, answers ⌘/⇧-clicks.
        selectable: bool,
        checked: bool,
    },
}

pub(super) struct SidebarPlan {
    pub rows: Vec<SidebarRow>,
    /// Every safe target in display order, collapsed ones included — what
    /// `Selection` is kept in the order of and pruned against.
    pub safe_order: Vec<PathBuf>,
    /// The selectable `Target` rows actually on screen, in row order — what
    /// a ⇧-range and `next_after` walk.
    pub visible_safe: Vec<PathBuf>,
}

pub(super) struct SidebarInputs<'a> {
    pub drifted: &'a [DriftSummary],
    pub queue: &'a QueueSnapshot,
    pub mode: ReviewMode,
    pub collapsed: &'a BTreeSet<PathBuf>,
    pub selection: &'a Selection,
    pub home: &'a Path,
}

/// Plan the sidebar: the PROCESSING group first (spec §2.1 — the row MOVES
/// there), then NEEDS A DECISION flat, then SAFE TO RESOLVE flat or as a
/// tree (spec §2.5 of the multi-select design). Only the safe group's rows
/// are selectable, and the header's checkbox stands for all of them.
pub(super) fn sidebar_rows(inputs: SidebarInputs<'_>) -> SidebarPlan {
    let SidebarInputs { drifted, queue, mode, collapsed, selection, home } = inputs;
    let mut rows = Vec::new();
    if !queue.processing.is_empty() {
        rows.push(SidebarRow::Group { label: "PROCESSING", check: None });
        for (target, queued) in &queue.processing {
            let entry = drifted
                .iter()
                .find(|d| d.target == *target)
                .cloned()
                .unwrap_or_else(|| DriftSummary {
                    target: target.clone(),
                    class: String::new(),
                    since_ts: None,
                    origin_differs: false,
                });
            rows.push(SidebarRow::Target {
                entry,
                queued: Some(*queued),
                locked: queued.locks_row(),
                depth: 0,
                selectable: false,
                checked: false,
            });
        }
    }
    let (needs_you, one_click) = severity_groups(drifted);
    let needs_you: Vec<DriftSummary> = needs_you.into_iter().filter(|d| !queue.is_processing(&d.target)).collect();
    let safe: Vec<DriftSummary> = one_click.into_iter().filter(|d| !queue.is_processing(&d.target)).collect();
    if !needs_you.is_empty() {
        rows.push(SidebarRow::Group { label: "NEEDS A DECISION", check: None });
        rows.extend(needs_you.iter().map(|entry| SidebarRow::Target {
            entry: entry.clone(),
            queued: None,
            locked: false,
            depth: 0,
            selectable: false,
            checked: false,
        }));
    }
    let mut safe_order: Vec<PathBuf> = Vec::new();
    let mut visible_safe: Vec<PathBuf> = Vec::new();
    if !safe.is_empty() {
        let safe_targets: Vec<PathBuf> = safe.iter().map(|d| d.target.clone()).collect();
        safe_order = match mode {
            ReviewMode::Flat => safe_targets.clone(),
            ReviewMode::Tree => tree_order(&safe_targets, home),
        };
        rows.push(SidebarRow::Group {
            label: "SAFE TO RESOLVE",
            check: Some(selection.check_state(&safe_order)),
        });
        let target_row = |entry: &DriftSummary, depth: usize| SidebarRow::Target {
            entry: entry.clone(),
            queued: None,
            locked: false,
            depth,
            selectable: true,
            checked: selection.contains(&entry.target),
        };
        match mode {
            ReviewMode::Flat => {
                for entry in &safe {
                    visible_safe.push(entry.target.clone());
                    rows.push(target_row(entry, 0));
                }
            }
            ReviewMode::Tree => {
                // Excluded = every drifted row that is not safe (needs-you or
                // processing): counted by the folder chips, never listed here.
                let excluded: Vec<PathBuf> = drifted
                    .iter()
                    .filter(|d| !safe_targets.contains(&d.target))
                    .map(|d| d.target.clone())
                    .collect();
                for row in tree_rows(&safe_targets, &excluded, collapsed, home) {
                    match row {
                        TreeRow::Folder(node) => {
                            let under = safe_descendants(&safe_targets, &node.path, home);
                            let check = selection.check_state(&under);
                            rows.push(SidebarRow::Folder { node, check });
                        }
                        TreeRow::File { target, depth } => {
                            let entry = safe.iter().find(|d| d.target == target).expect("tree rows come from `safe`");
                            visible_safe.push(target.clone());
                            rows.push(target_row(entry, depth));
                        }
                    }
                }
            }
        }
    }
    SidebarPlan { rows, safe_order, visible_safe }
}
```

Imports at the top of `review.rs`: `use std::collections::BTreeSet;` (if absent), `use super::review_selection::Selection;`, `use super::review_tree::{FolderNode, TreeRow, safe_descendants, tree_order, tree_rows};`, `use tomte_ui::components::CheckState;`.

- [ ] **Step 4: Make the view compile against the new plan (no behaviour change yet)**

In `sidebar()`, build the plan with `ReviewMode::Flat`, an empty `BTreeSet`, `&Selection::default()` and `home()`, and match the new shapes: `Group { label, .. } => group_header(label, theme)`, `Folder { .. } => unreachable in Flat` (render nothing for now: `continue`), `Target { entry, queued, locked, .. } => self.target_row(...)` as today. Add a private `fn home() -> PathBuf { std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/")) }` next to `journal_path()`. In `advance_from`, replace the hand-built `ordered` with `needs-you targets` followed by `plan.safe_order` built the same way (so the walk order is one function's).

- [ ] **Step 5: Run everything in the file** — `cargo test -p tomte-app --bin tomte review` → all pass, the two new ones included. Run `./scripts/shoot.sh review dark` and READ it: unchanged from before this task.

- [ ] **Step 6: Gate and commit**

```bash
git add crates/app/src/views/review.rs
git commit -m "feat(review): plan the sidebar with selection and tree inputs (flat rendering unchanged)"
```

---

### Task 6: Review wiring, flat mode — gestures, selection bar, bulk press

**Files:**
- Modify: `crates/app/src/views/review.rs` (`ReviewView` fields + `new`; `render_inner`; `sidebar()`; `target_row`; `group_header` → method; `detail`'s header actions ~:1785-1850; new methods below)
- Modify: `crates/app/src/views/fixtures.rs` (`STATES` + a `review-multiselect` pose)

**Interfaces:**
- Consumes: Tasks 1, 2, 4, 5.
- Produces: `ReviewView { mode, selection, collapsed, .. }` (`pub(super)` for fixtures); `fn toggle_member`, `fn extend_to`, `fn toggle_all_safe`, `fn run_bulk(action: ResolveAction)`; `pub(super) fn bulk_actions() -> [ResolveAction; 2]`.

- [ ] **Step 1: Write the failing unit test for the bulk decision mapping**

```rust
    /// The bulk pair is exactly the one-click pair and nothing else (spec
    /// §2.1). MUTATION: add KeepSourceOrigin → fails.
    #[test]
    fn bulk_actions_are_exactly_keep_disk_and_keep_source_this_machine() {
        assert_eq!(bulk_actions(), [ResolveAction::KeepDisk, ResolveAction::KeepSourceLocal]);
        assert_eq!(bulk_decision(ResolveAction::KeepDisk), Decision::KeepDisk);
        assert_eq!(bulk_decision(ResolveAction::KeepSourceLocal), Decision::KeepSourceLocal);
    }
```

`Decision` derives `PartialEq`? Check `action_queue.rs`; if not, assert with `matches!`.

- [ ] **Step 2: Run to see it fail.**

- [ ] **Step 3: Add the view state and the gesture methods**

Fields on `ReviewView` (and `new`): `pub(super) mode: ReviewMode` (`Flat`), `pub(super) selection: Selection` (`default`), `pub(super) collapsed: BTreeSet<PathBuf>` (`new`).

```rust
    /// The plan the sidebar is showing right now, from the same inputs
    /// `render_inner` uses. Cheap (a few dozen rows); rebuilt per gesture so
    /// every rule reads one snapshot of the world.
    fn plan(&self, cx: &Context<Self>) -> SidebarPlan {
        let queue = self.queue_snapshot(cx);
        let drifted = &self.state.read(cx).drifted;
        sidebar_rows(SidebarInputs {
            drifted,
            queue: &queue,
            mode: self.mode,
            collapsed: &self.collapsed,
            selection: &self.selection,
            home: &home(),
        })
    }

    /// ⌘-click or a checkbox on a safe row.
    fn toggle_member(&mut self, target: &Path, cx: &mut Context<Self>) {
        let plan = self.plan(cx);
        self.selection.toggle(target, &plan.safe_order);
        cx.notify();
    }

    /// ⇧-click on a safe row.
    fn extend_to(&mut self, target: &Path, cx: &mut Context<Self>) {
        let plan = self.plan(cx);
        let fallback = self.selected.clone().filter(|s| plan.safe_order.contains(s));
        self.selection.extend_range(target, &plan.visible_safe, fallback.as_deref());
        cx.notify();
    }

    /// The SAFE TO RESOLVE header's checkbox.
    fn toggle_all_safe(&mut self, cx: &mut Context<Self>) {
        let plan = self.plan(cx);
        let all = plan.safe_order.clone();
        self.selection.toggle_all(&all, &plan.safe_order);
        cx.notify();
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selection.clear();
        cx.notify();
    }

    /// A bulk press: one `QueuedAction` per member, ONE batch (spec §2.4).
    /// The selection clears, the rows move to PROCESSING, and the detail
    /// selection advances past the last member.
    fn run_bulk(&mut self, action: ResolveAction, cx: &mut Context<Self>) {
        let members = self.selection.members().to_vec();
        let Some(last) = members.last().cloned() else {
            return;
        };
        let decision = bulk_decision(action);
        let actions: Vec<QueuedAction> = members
            .iter()
            .map(|target| QueuedAction {
                target: target.clone(),
                decision: decision.clone(),
                notify_on_done: false,
            })
            .collect();
        let count = actions.len();
        if let Err(e) = self.shell.update(cx, |shell, cx| shell.enqueue_many(actions, cx)) {
            tomte_core::log_error!("resolve", "the bulk decision for {count} files was not queued: {e}");
        }
        self.selection.clear();
        self.last_outcome = None;
        self.advance_from(&last, cx);
        cx.notify();
    }
```

And the pure pair, next to `source_actions`:

```rust
/// The bulk pair: exactly the one-click decisions (spec §2.1). Keep source ·
/// origin needs each file's origin content shown; Keep both needs a dry-run
/// per file. Neither is a bulk.
pub(super) fn bulk_actions() -> [ResolveAction; 2] {
    [ResolveAction::KeepDisk, ResolveAction::KeepSourceLocal]
}

pub(super) fn bulk_decision(action: ResolveAction) -> Decision {
    match action {
        ResolveAction::KeepDisk => Decision::KeepDisk,
        ResolveAction::KeepSourceLocal => Decision::KeepSourceLocal,
        // Structural: `bulk_actions` never yields it. Falling back to the
        // local source would silently do something the user did not press.
        ResolveAction::KeepSourceOrigin => unreachable!("origin is never a bulk action"),
    }
}
```

`Decision: Clone` — it is (it holds `Arc`s). Reentrancy note for the implementer: `run_bulk` updates the Shell from inside this view's update; that is safe only because `enqueue_many → pump` never synchronously updates the review view (it only spawns), the same argument `enqueue` already relies on — read `Shell::pump` and confirm in the report.

- [ ] **Step 4: Prune on every render, and wire the rows**

In `render_inner`, right after `let queue = self.queue_snapshot(cx);` and before the sidebar is built, prune against the current safe order FIRST, then build the plan the render uses (so its `checked` flags reflect the pruned selection):

```rust
        let home = home();
        let inputs = |selection: &Selection| SidebarInputs {
            drifted: &drifted,
            queue: &queue,
            mode: self.mode,
            collapsed: &self.collapsed,
            selection,
            home: &home,
        };
        // A member that resolved, got queued or turned into a conflict leaves
        // the selection here, silently (spec §2.2). Pruned against the safe
        // order of THIS render's inputs, then the plan is built from the
        // pruned selection so every `checked` flag is current.
        let safe_order = sidebar_rows(inputs(&self.selection)).safe_order;
        self.selection.prune(&safe_order);
        let plan = sidebar_rows(inputs(&self.selection));
```

(The closure borrows `drifted`, `queue`, `self.mode`, `self.collapsed` immutably while `self.selection` is mutated between the two calls — if the borrow checker objects, clone `self.mode` and take `&self.collapsed` into locals before the closure, as the sketch's captures imply.) Pass `&plan` into `sidebar(..)` (change its signature to take `plan: &SidebarPlan` instead of `drifted`).

In `sidebar()`, render `Group { label, check }` through a new method `self.group_header_el(label, check, theme, cx)` that appends `ui::checkbox(theme, "safe-select-all", state, cx.listener(|view, _ev, _w, cx| view.toggle_all_safe(cx)))` at the right when `check.is_some()` (a `flex().items_center().justify_between()` wrapper around today's `group_header`). Render `Folder { .. }` as `continue` for now (Task 7).

In `target_row`, add parameters `depth: usize, selectable: bool, checked: bool, selection_active: bool` (from `!self.selection.is_empty()`), and:
- give the row `.group("review-row")` and `.pl(px(12. + TREE_INDENT * depth as f32))` (`tomte_ui::components::TREE_INDENT`) instead of `px_3` on the left,
- when `selectable`, insert before the severity dot: `ui::checkbox(theme, ElementId::named_usize("review-check", ix), if checked { CheckState::On } else { CheckState::Off }, cx.listener(move |view, _ev, _w, cx| view.toggle_member(&t, cx))).opacity(if selection_active { 1. } else { 0. }).group_hover("review-row", |el| el.opacity(1.))` (`t` is a clone of the target),
- when `checked`, tint the row: `.bg(Theme::wash(theme.accent, 0.10))` (in addition to the `selected` surface),
- replace the click listener body with the gesture switch:

```rust
                el.on_click(cx.listener(move |view, ev: &ClickEvent, _window, cx| {
                    let mods = ev.modifiers();
                    if selectable && mods.platform {
                        view.toggle_member(&target, cx);
                    } else if selectable && mods.shift {
                        view.extend_to(&target, cx);
                    } else {
                        view.selection.clear();
                        view.select(target.clone(), cx);
                    }
                }))
```

- [ ] **Step 5: The selection bar**

In `detail`, where `header_actions` is assembled: when `!self.selection.is_empty()`, build INSTEAD:

```rust
        let n = self.selection.members().len();
        let mut header_actions: Vec<AnyElement> = vec![
            div()
                .text_sm()
                .text_color(theme.text_muted)
                .flex_shrink_0()
                .child(format!("{n} selected"))
                .into_any_element(),
        ];
        header_actions.extend(bulk_actions().into_iter().map(|action| {
            ui::button(
                theme,
                format!("bulk-{}", action.button_id()),
                action.button_label(false).into(),
                ui::ButtonVariant::Tonal(action.tint(theme)),
                ui::ButtonSize::Sm,
                cx.listener(move |view, _ev, _window, cx| view.run_bulk(action, cx)),
            )
            .flex_shrink_0()
            .into_any_element()
        }));
        header_actions.push(
            ui::button(
                theme,
                "bulk-clear",
                "✕".into(),
                ui::ButtonVariant::Ghost,
                ui::ButtonSize::Sm,
                cx.listener(|view, _ev, _window, cx| view.clear_selection(cx)),
            )
            .tooltip(ui::text_tooltip("Clear selection"))
            .flex_shrink_0()
            .into_any_element(),
        );
```

(`button_id()`/`button_label(split)`/`tint(theme)` are `ResolveAction`'s existing methods — read them; `format!` for the id needs `ElementId::from(String)` — use `ElementId::Name(..into())` as the file does elsewhere.) The bar replaces the per-file actions; the diff below still shows `selected`.

- [ ] **Step 6: The `review-multiselect` pose**

In `fixtures.rs`: add to `STATES` `("review-multiselect", "three safe rows ticked: the selection bar replaces the file's actions, checkboxes visible")`. Add a `multiselect_model()` that is `rich_model()` plus three safe rows: `.config/starship.toml` (`destination_drift`), `.zshrc` (`source_ahead`), `.config/nvim/init.lua` (`destination_drift`). In the review arm list add `"review-multiselect"`, and after the pose is built set `view.selection` by toggling `.gitconfig`, `.config/starship.toml`, `.zshrc` against the plan's `safe_order` (compute via `sidebar_rows` with `ReviewMode::Flat`; the fixture needs `pub(super)` access to `sidebar_rows`/`SidebarInputs`, which they are).

- [ ] **Step 7: Shoot and READ**

```bash
./scripts/shoot.sh review-multiselect dark; ./scripts/shoot.sh review-multiselect light
./scripts/shoot.sh review dark; ./scripts/shoot.sh review-processing dark
```
Check: three ticked rows with a faint accent wash and visible checkboxes; every other safe row shows its checkbox too (selection active); the NEEDS A DECISION row has none; the header reads "3 selected · Keep disk · Keep source · this machine · ✕" and nothing overflows the header row (wrap, not clip); `review` and `review-processing` are unchanged.

- [ ] **Step 8: Gate and commit**

```bash
git add crates/app/src/views/review.rs crates/app/src/views/fixtures.rs
git commit -m "feat(review): multi-select safe rows with ⌘/⇧-click and checkboxes; a bulk press is one batch"
```

---

### Task 7: Review wiring, tree mode — the toggle, folder rows, collapse, recursive ticks

**Files:**
- Modify: `crates/app/src/views/review.rs` (`sidebar()`: the mode control and `Folder` rows; new methods `toggle_mode`, `toggle_collapse`, `toggle_folder`)
- Modify: `crates/app/src/views/fixtures.rs` (`review-tree`, `review-tree-selected` poses)

**Interfaces:**
- Consumes: Task 1 (`folder_row`, `FolderRow`, `TREE_INDENT`), Task 3, Task 5, Task 6.

- [ ] **Step 1: Write the failing unit test for the folder tick**

```rust
    /// Ticking a folder reaches its collapsed descendants too (spec §2.3).
    /// MUTATION: tick only `visible_safe` under the folder → the collapsed
    /// file is missing.
    #[test]
    fn a_folder_tick_reaches_collapsed_descendants() {
        let safe = vec![p("/Users/x/.config/nvim/init.lua"), p("/Users/x/.config/nvim/lua/p.lua"), p("/Users/x/top")];
        let home = Path::new("/Users/x");
        let mut selection = Selection::default();
        let order = tree_order(&safe, home);
        let under = safe_descendants(&safe, Path::new("/Users/x/.config/nvim"), home);
        selection.toggle_all(&under, &order);
        assert_eq!(selection.members(), &[p("/Users/x/.config/nvim/lua/p.lua"), p("/Users/x/.config/nvim/init.lua")]);
        assert_eq!(selection.check_state(&under), CheckState::On);
    }
```

- [ ] **Step 2: Run it** — passes already if Tasks 2/3 are right (it pins the composition the view relies on); keep it.

- [ ] **Step 3: The methods**

```rust
    fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            ReviewMode::Flat => ReviewMode::Tree,
            ReviewMode::Tree => ReviewMode::Flat,
        };
        // Members keep their identity across modes; only their order changes,
        // and the next render's prune re-sorts them.
        cx.notify();
    }

    fn toggle_collapse(&mut self, folder: &Path, cx: &mut Context<Self>) {
        if !self.collapsed.remove(folder) {
            self.collapsed.insert(folder.to_path_buf());
        }
        cx.notify();
    }

    /// The folder's checkbox, or ⌘-click on the folder row: every safe file
    /// under it, collapsed or not.
    fn toggle_folder(&mut self, folder: &Path, cx: &mut Context<Self>) {
        let plan = self.plan(cx);
        let under = safe_descendants(&plan.safe_order, folder, &home());
        self.selection.toggle_all(&under, &plan.safe_order);
        cx.notify();
    }
```

- [ ] **Step 4: Render the control and the folder rows**

In `sidebar()`, above the rows, a right-aligned segmented control (same shape as `comparison_selector`: two `ui::button`s, the current one `Tonal(theme.accent)`, the other `Ghost`, `ButtonSize::Micro`, labels `"List"` and `"Tree"`, tooltips "Flat list" / "Folder tree", both listeners → `toggle_mode` when not already that mode). Render `SidebarRow::Folder { node, check }` as:

```rust
                SidebarRow::Folder { node, check } => {
                    let path = node.path.clone();
                    let path2 = node.path.clone();
                    let count: SharedString = if node.safe == node.total {
                        node.safe.to_string().into()
                    } else {
                        format!("{} of {}", node.safe, node.total).into()
                    };
                    rows.push(
                        ui::folder_row(
                            theme,
                            ElementId::named_usize("review-folder", ix),
                            ui::FolderRow {
                                depth: node.depth,
                                label: node.label.clone(),
                                collapsed: node.collapsed,
                                count,
                                check,
                                show_check: selection_active,
                            },
                            cx.listener(move |view, ev: &ClickEvent, _w, cx| {
                                if ev.modifiers().platform {
                                    view.toggle_folder(&path, cx);
                                } else {
                                    view.toggle_collapse(&path, cx);
                                }
                            }),
                            cx.listener(move |view, _ev, _w, cx| view.toggle_folder(&path2, cx)),
                        )
                        .into_any_element(),
                    );
                    ix += 1;
                }
```

File rows already take `depth` (Task 6).

- [ ] **Step 5: Poses**

`STATES`: `("review-tree", "folder tree: .config/nvim/ collapsed, .config/ partially ticked, an \"n of m\" chip")`, `("review-tree-selected", "a whole folder ticked: its checkbox On, selection bar for its files")`. Both use `multiselect_model()` with `view.mode = ReviewMode::Tree`. `review-tree`: `collapsed = {home(".config/nvim")}`, selection = `.config/starship.toml` only (so `.config/` is Partial and shows `3 of 5`: safe `starship.toml`, `zed/settings.json`, `nvim/init.lua`; excluded `nushell/env.nu`, `secrets.yaml`). `review-tree-selected`: nothing collapsed; selection = `safe_descendants(.., home(".config/nvim"), ..)` (one file here, `init.lua`) plus `.config/zed/settings.json` — so `.config/` reads Partial and `nvim/` reads On with the bar "2 selected".

- [ ] **Step 6: Shoot and READ**

```bash
for s in review-tree review-tree-selected; do ./scripts/shoot.sh $s dark; ./scripts/shoot.sh $s light; done
./scripts/shoot.sh review dark; ./scripts/shoot.sh review-multiselect dark
```
Check: the List/Tree control sits top-right of the sidebar; NEEDS A DECISION is flat above the tree; `.config/` has a chevron, `3 of 5`, a Partial dash; `nvim/` is collapsed with a right-pointing chevron and no children; file rows indent 12px per level and show the file name only; nothing clips (labels use `line_clamp(1)`); the flat poses are unchanged.

- [ ] **Step 7: Gate and commit**

```bash
git add crates/app/src/views/review.rs crates/app/src/views/fixtures.rs
git commit -m "feat(review): folder tree for the safe group, with recursive folder ticks"
```

---

### Task 8: Keyboard, changelog, docs, gallery sweep

**Files:**
- Modify: `crates/app/src/views/review.rs` (`actions!`, `register_keys`, focus handle, `key_context`, `on_action`s)
- Modify: `crates/app/src/main.rs` (call `views::review::register_keys(cx)` at each of the three sites where `views::merge::register_keys(cx)` is called: `rg -n 'merge::register_keys' crates/app/src/main.rs`)
- Modify: `CHANGELOG.md` (`## Unreleased` → `### Added`), `AGENTS.md` (workspace map line for the two new modules; the gallery list)

- [ ] **Step 1: Actions and bindings**

Next to `ResolveAction` in `review.rs`:

```rust
gpui::actions!(review, [ClearSelection, SelectAllSafe]);

/// Key bindings for Review (active while its root has focus via the
/// "Review" key context). Registered by every app entry point next to the
/// merge editor's.
pub fn register_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        gpui::KeyBinding::new("escape", ClearSelection, Some("Review")),
        gpui::KeyBinding::new("cmd-a", SelectAllSafe, Some("Review")),
    ]);
}
```

Add `focus_handle: gpui::FocusHandle` to the view (`cx.focus_handle()` in `new`). On the root `div()` in `render_inner`: `.track_focus(&self.focus_handle).key_context("Review").on_action(cx.listener(|view, _: &ClearSelection, _w, cx| view.clear_selection(cx))).on_action(cx.listener(|view, _: &SelectAllSafe, _w, cx| { let plan = view.plan(cx); let all = plan.safe_order.clone(); view.selection.toggle_all(&all, &plan.safe_order); if view.selection.check_state(&all) != CheckState::On { view.selection.toggle_all(&all, &plan.safe_order); } cx.notify(); }))` — ⌘A must SELECT all, not toggle: write a small `select_all_safe` method that adds without toggling (call `toggle_all` only when the state is not already On), and use it. Give focus on click: `.on_mouse_down(gpui::MouseButton::Left, cx.listener(|view, _ev, window, _cx| view.focus_handle.focus(window)))` — check `merge.rs:2320-2330` for the exact focus call shape on this gpui.

- [ ] **Step 2: A render-smoke check that the bindings fire**

In `views/mod.rs`'s test module there is a render-smoke harness for review poses; add a `#[gpui::test]` that poses `review-multiselect`, dispatches `ClearSelection` on the review entity (`cx.dispatch_action` on the window/focused element, the way `merge.rs`'s tests dispatch `PickDisk` — copy that shape), and asserts `view.selection.is_empty()`; then `SelectAllSafe` and assert the count equals the plan's `safe_order.len()`. MUTATION: swap the two `on_action`s → both fail.

- [ ] **Step 3: Changelog and AGENTS.md**

`CHANGELOG.md`, a new `## Unreleased` above `## 0.1.20`:

```markdown
## Unreleased

### Added

- **Resolve several safe files at once.** In Review, ⌘-click or tick the
  checkbox on any SAFE TO RESOLVE row (⇧-click for a range, the header's
  checkbox for all of them), then press Keep disk or Keep source · this
  machine once. The whole selection is one batch: one re-add or apply, one
  commit, one push, still one journal session per file so Undo stays per
  file. Esc clears, ⌘A selects all. Rows that need a decision cannot be
  ticked; they keep their own buttons.
- **A folder tree for the safe group.** The List / Tree switch at the top
  of the sidebar shows SAFE TO RESOLVE as folders under `~`, single-child
  chains folded (`.config/nvim/`). Ticking a folder ticks every safe file
  under it, collapsed or not; its chip says `3 of 5` when two of its files
  need a decision and stay out. The other two groups stay flat. The choice
  and the collapse state last for the session.
```

`AGENTS.md`: in the workspace map's `crates/app` line add `review_selection` and `review_tree` (pure) beside `views/`; in the gallery paragraph mention `review-multiselect`, `review-tree`, `review-tree-selected`.

- [ ] **Step 4: Full gallery sweep**

```bash
rm -f shots/*.png && ./scripts/shoot.sh all dark
```
READ `review`, `review-multiselect`, `review-tree`, `review-tree-selected`, `review-processing`, `review-failed`, `merge`, `dashboard`. Nothing outside Review may have changed.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/app/src/views/review.rs crates/app/src/main.rs crates/app/src/views/mod.rs CHANGELOG.md AGENTS.md
git commit -m "feat(review): Esc / ⌘A for the selection; changelog"
```

---

## Self-Review

**Spec coverage.** §2.1 (safe only, the bulk pair) → Tasks 5, 6 (`selectable`, `bulk_actions`). §2.2 (model, prune, session-only) → Tasks 2, 6. §2.3 gestures: plain/⌘/⇧/checkbox → Task 6; folder tick recursive, tri-state, header → Tasks 5, 7; hover-reveal / forced visible → Tasks 1, 6, 7; Esc/⌘A → Task 8. §2.4 one batch, selection bar, advance past the last member → Tasks 4, 6. §2.5 tree shape, compaction, counts, chevron, indent, collapse, mode control → Tasks 3, 5, 7. §2.6 none persisted → nowhere writes. §2.7 edge cases → prune (Task 6), running rows excluded by the plan (Task 5), "1 selected" (Task 6). §3 signatures → Tasks 1–5. §4 tests → each task; the `enqueue_many` story → Task 4; poses → Tasks 6, 7; smoke → Task 8.

**Placeholders.** Task 2's `visible_order_union` sketch is explicitly replaced by the written-out `extend_range` body; Task 6's stale-plan line is explicitly deleted with the rebuild order spelled out. No TBDs.

**Type consistency.** `CheckState` (Task 1) is used by Tasks 2, 5, 6, 7. `Selection` methods named identically in Tasks 2, 5, 6, 7, 8. `tree_rows`/`tree_order`/`safe_descendants`/`FolderNode`/`TreeRow` (Task 3) match Tasks 5, 7. `SidebarPlan`/`SidebarInputs`/`SidebarRow` (Task 5) match Tasks 6, 7, 8. `enqueue_many(Vec<QueuedAction>, cx)` (Task 4) matches Task 6. `FolderRow`/`folder_row`/`TREE_INDENT` (Task 1) match Tasks 6, 7.

**Known risk.** `cx.stop_propagation()` on a child click (Task 1) is verified against the pinned gpui source in Task 1 Step 1; if it lives on `Window`, the checkbox handler calls it there and every later task's listener shape is unchanged.
