# Review Sidebar Virtualized Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The List/Tree control stops pushing the groups down (it becomes a toggle on the SAFE TO RESOLVE header), and a 500-row Review sidebar stays smooth: the plan is rebuilt only when its inputs change and only the rows in view are rendered.

**Architecture:** A `tomte-ui` `icon_button` with a `folder_tree` icon replaces the control row. `ReviewView` caches its `SidebarPlan` behind a value-compared `PlanKey` plus a model-observer dirty flag, and renders the sidebar through gpui's variable-height `list` element, keeping the `ListState` in step with the plan through a pure `splice_window` over row keys. A 500-file gallery pose and a rendered-rows counter make the result measurable.

**Tech Stack:** Rust 2024, GPUI 0.2.2 (pinned; `elements/list.rs`), tomte-ui builders, the existing gallery fixtures and render-smoke test, `scripts/shoot.sh`.

**Spec:** `.agents/artifacts/specs/2026-10-11-review-sidebar-virtualized-design.md`

## Global Constraints

- The sidebar's rows render exactly as today; only their container and the mode control change (spec §2.2, §2.5). `sidebar_rows` and `SidebarRow` do not change shape (spec §2.1).
- The mode toggle sits on the SAFE TO RESOLVE header, right side, left of the select-all checkbox, always visible when that header is; nothing sits above the first group (spec §2.1).
- `ListState::splice` is called on the changed window only, never `0..old` for an unchanged-prefix plan (spec §2.2); `reset` only when the plan flips between empty and non-empty.
- The plan is rebuilt only when `PlanKey` differs or the model observer set `plan_dirty` (spec §2.3).
- GPUI: never update an entity from inside its own update closure. The list's item closure runs in prepaint with no entity leased, so `view.update(cx, ..)` there is a fresh update — verify at `~/.cargo/registry/src/*/gpui-0.2.2/src/view.rs` (`AnyView` request_layout / prepaint) before relying on it, and say so in the report. Views use Theme tokens only; new reusable styling goes in `crates/ui` first.
- `.truncate()` never ellipsizes in gpui 0.2.2; keep `line_clamp(1)` where the rows use it.
- Every new test names, in a comment, the mutation that fails it, and the implementer RUNS it against production code (edit, run, revert from a `cp` backup — never `git checkout --` on uncommitted work).
- The gate before every commit, IN THE FOREGROUND, and read: `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E '^warning' | grep -v -E 'block v0\.1\.6|proc-macro-error2'` (must print nothing); `cargo test --workspace`; `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null cargo test --workspace`.
- Screenshots are part of every UI task: `./scripts/shoot.sh <state> dark|light`, then READ the PNG. `screencapture` fails in a sandboxed shell; run it unsandboxed.
- Commits are SSH-signed via 1Password; on failure leave the work staged and report, never `--no-gpg-sign`.

## Review Focus

1. The sidebar dragged to its minimum width (180 px) with Tree on: the header must hold the label, the toggle and the checkbox without overflowing — Task 2, `the_header_toggle_fits_at_the_minimum_sidebar_width`.
2. The plan flipping between empty and non-empty (the last row resolves; the first drift arrives): the list must swap its "nothing to review" item for rows without leaving a stale item — Task 5, `the_list_resets_when_the_plan_flips_between_empty_and_rows`.
3. A click on a row INSIDE the list element must still select it (the element owns a hitbox for scrolling) — Task 5, `a_real_click_on_a_row_inside_the_list_selects_it`.
4. A failed row's wrapping message makes that row taller than its neighbours; the list measures it, and a plan rebuild that changes its key re-measures it — Task 4, `splice_window` keys carry `failed`; Task 5's `review-many` pose carries a failed row and is READ.
5. A model update that changes nothing visible (same rows) must not jump the scroll position — Task 4, `splice_window` returns `None` for identical keys; Task 5 calls `splice` only on `Some`.

---

### Task 1: `tomte-ui`: `folder_tree` icon and `icon_button`

**Files:**
- Create: `crates/ui/assets/icons/folder_tree.svg`
- Modify: `crates/ui/src/assets.rs` (the `ASSETS` table)
- Modify: `crates/ui/src/components/icon.rs` (new `folder_tree`)
- Modify: `crates/ui/src/components/button.rs` (new `icon_button`)
- Modify: `crates/ui/src/preview/mod.rs` (`COMPONENTS` entry + render arm), `crates/app/src/views/fixtures.rs` (`STATES`: the `comp:` entry next to `comp:checkbox`)

**Interfaces:**
- Produces:
  ```rust
  pub fn folder_tree(theme: Theme) -> Svg;                                   // 12px, text_muted
  pub fn icon_button(theme: Theme, id: impl Into<ElementId>, icon: Svg, active: bool,
                     tooltip: SharedString,
                     on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Stateful<Div>;
  ```

- [ ] **Step 1: Verify two gpui facts against the pinned source**

```bash
G=$(ls -d ~/.cargo/registry/src/*/gpui-0.2.2 | head -1)
rg -n 'impl Styled for Svg' $G/src/elements/svg.rs
rg -n 'pub fn stop_propagation' $G/src/app.rs
```
Expected: both exist (`Svg: Styled` is what lets `icon_button` tint the icon with `.text_color`; `App::stop_propagation` keeps the toggle's click off the header row). If either is elsewhere, use what you find and say so in the report.

- [ ] **Step 2: Add the icon**

`crates/ui/assets/icons/folder_tree.svg` (Lucide `folder-tree`, same stroke conventions as `chevron_down.svg` — gpui tints the whole glyph through `text_color`):

```svg
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 10a1 1 0 0 0 1-1V6a1 1 0 0 0-1-1h-2.5a1 1 0 0 1-.8-.4l-.9-1.2A1 1 0 0 0 15 3h-2a1 1 0 0 0-1 1v5a1 1 0 0 0 1 1Z"/><path d="M20 21a1 1 0 0 0 1-1v-3a1 1 0 0 0-1-1h-2.9a1 1 0 0 1-.88-.55l-.42-.85a1 1 0 0 0-.92-.6H13a1 1 0 0 0-1 1v5a1 1 0 0 0 1 1Z"/><path d="M3 5a2 2 0 0 0 2 2h3"/><path d="M3 3v13a2 2 0 0 0 2 2h3"/></svg>
```

`assets.rs`: add `("icons/folder_tree.svg", include_bytes!("../assets/icons/folder_tree.svg")),` to `ASSETS`.

`icon.rs`:

```rust
/// Folder tree (Lucide `folder-tree`), 12px, muted: the Review sidebar's
/// flat-list / folder-tree toggle. The button tints it accent when active.
pub fn folder_tree(theme: Theme) -> Svg {
    svg()
        .path("icons/folder_tree.svg")
        .w(px(12.))
        .h(px(12.))
        .flex_none()
        .text_color(theme.text_muted)
}
```

- [ ] **Step 3: Add `icon_button`**

`button.rs`, after `disabled_button`:

```rust
/// A 20×20 icon TOGGLE: ghost at rest, the `Tonal(accent)` field and an
/// accent-tinted icon when `active` — a toolbar's Bold, not a segmented
/// control. `icon` arrives already sized (see `icon::folder_tree`); this
/// only tints it and adds the hover wash, the tooltip and the click. The
/// click stops propagation: a toggle that lives on a clickable row (a
/// group header, a list row) must not also fire the row.
pub fn icon_button(
    theme: Theme,
    id: impl Into<ElementId>,
    icon: Svg,
    active: bool,
    tooltip: SharedString,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let accent = theme.accent;
    div()
        .id(id)
        .w_5()
        .h_5()
        .flex_none()
        .rounded_md()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when(active, |el| el.bg(Theme::wash(accent, 0.14)))
        .hover(move |el| {
            if active {
                el.bg(Theme::wash(accent, 0.22))
            } else {
                el.bg(Theme::wash(theme.text, 0.08))
            }
        })
        .child(icon.text_color(if active { accent } else { theme.text_muted }))
        .tooltip(text_tooltip(tooltip))
        .on_click(move |ev, window, cx| {
            cx.stop_propagation();
            on_click(ev, window, cx);
        })
}
```

Add `Svg` to `button.rs`'s `use gpui::{…}` line.

- [ ] **Step 4: Preview**

`preview/mod.rs` `COMPONENTS`: add `("icon-button", "folder-tree toggle: off and on"),`. Render arm, modelled on the `"dropdown"` arm:

```rust
        "icon-button" => div()
            .flex()
            .flex_col()
            .gap_6()
            .child(variant(
                theme,
                "off",
                icon_button(theme, "prev-ib-off", folder_tree(theme), false, "Folder tree".into(), noop)
                    .into_any_element(),
            ))
            .child(variant(
                theme,
                "on",
                icon_button(theme, "prev-ib-on", folder_tree(theme), true, "Folder tree".into(), noop)
                    .into_any_element(),
            ))
            .into_any_element(),
```

Import `icon_button` and `folder_tree` where the file imports `checkbox`/`folder_row`. In `fixtures.rs` `STATES`, next to the `comp:checkbox` entry, add `("comp:icon-button", "folder-tree toggle off / on"),`.

- [ ] **Step 5: Build, shoot, read, commit**

Run: `cargo build --workspace && ./scripts/shoot.sh comp:icon-button dark && ./scripts/shoot.sh comp:icon-button light`. READ both PNGs: a 20 px square, the glyph muted when off, accent glyph on an accent wash when on, no clipping. Run `cargo test -p tomte-app --bin tomte every_gallery_state` (the render-smoke test builds every pose, the comp one included). Gate, then:

```bash
git add crates/ui/assets/icons/folder_tree.svg crates/ui/src/assets.rs crates/ui/src/components/icon.rs crates/ui/src/components/button.rs crates/ui/src/preview/mod.rs crates/app/src/views/fixtures.rs
git commit -m "feat(ui): folder_tree icon and icon_button toggle"
```

---

### Task 2: The mode toggle lives on the SAFE TO RESOLVE header

**Files:**
- Modify: `crates/app/src/views/review.rs` (`sidebar()` ~2027–2145: drop `.child(self.mode_control(theme, cx))`; delete `mode_control` ~2147–2195; `group_header_el` ~2868–2900; `sidebar_width` field made `pub(super)`)
- Test: `crates/app/src/views/mod.rs` (tests module)

**Interfaces:**
- Produces: `group_header_el(label: &'static str, check: Option<CheckState>, selection_active: bool, mode: ReviewMode, theme: Theme, cx: &mut Context<ReviewView>) -> Div` (one new parameter).
- Consumes: `ui::icon_button`, `ui::folder_tree` from Task 1; `ReviewView::toggle_mode` (exists).

- [ ] **Step 1: Write the tests**

In `views/mod.rs` tests, after `a_click_in_the_diff_keeps_its_focus_and_a_sidebar_click_takes_the_root`:

```rust
    /// The List/Tree control is a toggle on the SAFE TO RESOLVE header
    /// (spec 2026-10-11 §2.1), reached by a real click.
    ///
    /// MUTATION: pass `|_, _, _| {}` as the toggle's listener in
    /// `group_header_el` → the click lands and the mode stays Flat.
    #[gpui::test]
    fn the_header_toggle_flips_the_mode_with_a_real_click(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-multiselect", smoke_paths(), cx).expect("review-multiselect fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        assert_eq!(vis.update(|_w, cx| review.read(cx).mode), ReviewMode::Flat);
        let toggle = vis
            .debug_bounds("review-mode-toggle")
            .expect("the SAFE TO RESOLVE header rendered its toggle");
        vis.simulate_click(toggle.center(), gpui::Modifiers::default());
        assert_eq!(vis.update(|_w, cx| review.read(cx).mode), ReviewMode::Tree);
        vis.run_until_parked();
        let toggle = vis.debug_bounds("review-mode-toggle").expect("still there in Tree mode");
        vis.simulate_click(toggle.center(), gpui::Modifiers::default());
        assert_eq!(vis.update(|_w, cx| review.read(cx).mode), ReviewMode::Flat);
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
        vis.update(|_w, cx| review.update(cx, |view, cx| {
            view.sidebar_width = Some(review::MIN_SIDEBAR_W);
            cx.notify();
        }));
        vis.run_until_parked();
        let column = vis.debug_bounds("review-sidebar-rows").expect("sidebar column");
        let toggle = vis.debug_bounds("review-mode-toggle").expect("toggle");
        assert!(
            toggle.right() <= column.right() && toggle.left() >= column.left(),
            "toggle {toggle:?} inside the column {column:?}"
        );
        assert_eq!(toggle.size.width, px(20.));
    }
```

`MIN_SIDEBAR_W` (review.rs:54) becomes `pub(super) const`; `sidebar_width` becomes `pub(super)`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p tomte-app --bin tomte header_toggle`
Expected: both fail — no element carries `review-mode-toggle`.

- [ ] **Step 3: Move the control**

`review.rs`:

- Delete `mode_control` and the `.child(self.mode_control(theme, cx))` line in `sidebar()`. `rg -n 'set_mode' crates/app/src` — if the only callers were the two segments, delete `set_mode` and its doc too; if a test calls it, keep it.
- `group_header_el` gains `mode: ReviewMode` after `selection_active`, and its `Some(state)` arm becomes:

```rust
        Some(state) => wrapper.child(group_header(label, theme)).child(
            div()
                .pr_3()
                .flex()
                .items_center()
                .gap_1p5()
                // The flat-list / folder-tree toggle (spec 2026-10-11 §2.1):
                // on THIS header because Tree mode reshapes only this group.
                // Always visible, unlike the hover-revealed checkbox beside
                // it: it is the mode's only entry point.
                .child(
                    ui::icon_button(
                        theme,
                        "review-mode-toggle",
                        ui::folder_tree(theme),
                        mode == ReviewMode::Tree,
                        "Folder tree".into(),
                        cx.listener(|view, _ev, _w, cx| view.toggle_mode(cx)),
                    )
                    // Test-only (no-op in release builds): a real click in
                    // the render-smoke test finds it by this name.
                    .debug_selector(|| "review-mode-toggle".into()),
                )
                .child(
                    ui::checkbox(
                        theme,
                        "safe-select-all",
                        state,
                        cx.listener(|view, _ev, _w, cx| view.toggle_all_safe(cx)),
                    )
                    .opacity(if selection_active { 1. } else { 0. })
                    .group_hover("review-safe-header", |el| el.opacity(1.)),
                ),
        ),
```

- The call in `sidebar()` passes `self.mode`: `group_header_el(label, *check, selection_active, self.mode, theme, cx)`.
- Update `toggle_mode`'s doc ("The List/Tree segmented control, top-right of the sidebar") to "The folder-tree toggle on the SAFE TO RESOLVE header".

- [ ] **Step 4: Run, mutate, shoot**

Run: `cargo test -p tomte-app --bin tomte` — all PASS (the T7 tree tests and T8 click/keyboard tests included). Run both mutations from Step 1; each fails its test; restore from a `cp` backup.

Shoot and READ: `./scripts/shoot.sh review-tree dark`, `review-tree light`, `review-multiselect dark`, `review light`: the first group starts at the top of the sidebar; the SAFE TO RESOLVE line carries the glyph at the right (accent-washed in `review-tree`, muted in the others) with the checkbox slot to its right; no clipping of the label.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/app/src/views/review.rs crates/app/src/views/mod.rs
git commit -m "feat(review): the folder-tree toggle lives on the SAFE TO RESOLVE header; the control row is gone"
```

---

### Task 3: The plan is cached (`PlanKey`, `plan_dirty`)

**Files:**
- Modify: `crates/app/src/views/review.rs` (`QueueSnapshot` derive ~898; `ReviewView` fields + `new` ~1199–1297; `plan` ~1504; gestures ~1518–1623; `render_inner` ~2757–2780)
- Test: `crates/app/src/views/mod.rs`

**Interfaces:**
- Produces:
  ```rust
  #[derive(PartialEq)] struct PlanKey { queue: QueueSnapshot, mode: ReviewMode, collapsed: BTreeSet<PathBuf>, selection: Selection }
  impl ReviewView {
      pub(super) fn plan(&mut self, cx: &Context<Self>) -> Rc<SidebarPlan>;          // was &self, returned by value
      fn plan_with(&mut self, queue: QueueSnapshot, cx: &App) -> Rc<SidebarPlan>;
  }
  // fields: plan_cache: Option<(PlanKey, Rc<SidebarPlan>)>, plan_dirty: bool
  ```
- Consumes: nothing new. `QueueSnapshot` gains `Clone, PartialEq`.

- [ ] **Step 1: Write the test**

In `views/mod.rs` tests:

```rust
    /// The sidebar plan is rebuilt when its inputs change, not when the
    /// window redraws (spec 2026-10-11 §2.3): the same `Rc` comes back
    /// until a gesture or the model changes it.
    ///
    /// MUTATION: return `Rc::new(sidebar_rows(..))` on every call (drop the
    /// cache lookup) → the first `ptr_eq` fails. MUTATION: drop
    /// `view.plan_dirty = true` from the model observer → the last
    /// assertion sees the old plan after the model changed.
    #[gpui::test]
    fn the_sidebar_plan_is_rebuilt_only_when_its_inputs_change(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-multiselect", smoke_paths(), cx).expect("review-multiselect fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        let first = vis.update(|_w, cx| review.update(cx, |view, cx| view.plan(cx)));
        vis.run_until_parked();
        let second = vis.update(|_w, cx| review.update(cx, |view, cx| view.plan(cx)));
        assert!(Rc::ptr_eq(&first, &second), "nothing changed: the cached plan is handed back");

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
        assert!(!Rc::ptr_eq(&third, &fourth), "the model observer marks the plan dirty");
        assert_eq!(fourth.rows.len(), third.rows.len() - 1);
    }
```

Add `use std::rc::Rc;` to the tests module if absent.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p tomte-app --bin tomte the_sidebar_plan_is_rebuilt`
Expected: compile error (`plan` returns `SidebarPlan`, not `Rc`).

- [ ] **Step 3: Implement the cache**

`review.rs`:

- `QueueSnapshot`: `#[derive(Default, Clone, PartialEq)]`.
- Above `pub struct ReviewView`:

```rust
/// What `sidebar_rows` reads besides the model, compared by value before
/// every rebuild (spec 2026-10-11 §2.3). The model is not in here: its
/// changes arrive through the view's observer as `plan_dirty`.
#[derive(PartialEq)]
struct PlanKey {
    queue: QueueSnapshot,
    mode: ReviewMode,
    collapsed: BTreeSet<PathBuf>,
    selection: Selection,
}
```

- Fields on `ReviewView` (after `focus_handle`):

```rust
    /// The plan the sidebar is showing, with the inputs it was built from.
    /// `plan` hands the `Rc` back while the key still matches and the
    /// model has not changed; a 500-row plan was being rebuilt on every
    /// hover before this (2026-10-11).
    plan_cache: Option<(PlanKey, Rc<SidebarPlan>)>,
    /// Set by the model observer; the next `plan` rebuilds.
    plan_dirty: bool,
```

initialised `plan_cache: None, plan_dirty: true`. The observer in `new` becomes:

```rust
        cx.observe(&state, |view, _, cx| {
            view.plan_dirty = true;
            cx.notify();
        })
        .detach();
```

- Replace `plan`:

```rust
    /// The plan the sidebar is showing right now, cached (spec 2026-10-11
    /// §2.3): rebuilt when the queue, the mode, the collapse set, the
    /// selection or the model changed since the last build, otherwise the
    /// same `Rc`. Every gesture and `render_inner` read through here, so
    /// every rule reads one snapshot of the world.
    ///
    /// `pub(super)` so the render-smoke tests can read `safe_order` the
    /// same way `select_all_safe` does.
    pub(super) fn plan(&mut self, cx: &Context<Self>) -> Rc<SidebarPlan> {
        let queue = self.queue_snapshot(cx);
        self.plan_with(queue, cx)
    }

    fn plan_with(&mut self, queue: QueueSnapshot, cx: &App) -> Rc<SidebarPlan> {
        let key = PlanKey {
            queue,
            mode: self.mode,
            collapsed: self.collapsed.clone(),
            selection: self.selection.clone(),
        };
        if !self.plan_dirty
            && let Some((cached, plan)) = &self.plan_cache
            && *cached == key
        {
            return plan.clone();
        }
        let drifted = &self.state.read(cx).drifted;
        let plan = Rc::new(sidebar_rows(SidebarInputs {
            drifted,
            queue: &key.queue,
            mode: key.mode,
            collapsed: &key.collapsed,
            selection: &key.selection,
            home: &home(),
        }));
        self.plan_cache = Some((key, plan.clone()));
        self.plan_dirty = false;
        plan
    }
```

- Every gesture that did `let plan = self.plan(cx);` keeps that line (the `Rc` derefs to `SidebarPlan`; `plan.safe_order`, `plan.visible_safe` read as before). `advance_from` keeps building its one-off "queue before this click" plan with `sidebar_rows` directly, unchanged.
- `render_inner`: replace the inline `sidebar_rows(SidebarInputs { … })` with `let plan = self.plan_with(queue.clone(), cx);`, keep `self.selection.prune(&plan.safe_order);` and add right after it:

```rust
        // A prune only removes members outside `safe_order` — nothing the
        // cached rows ever flagged `checked` — so the cache stays valid;
        // record the pruned selection in its key so the next frame does
        // not rebuild for a change that is already reflected.
        if let Some((key, _)) = &mut self.plan_cache {
            key.selection = self.selection.clone();
        }
```

and pass `&plan` to `self.sidebar(..)` as before (the `Rc` derefs).

- [ ] **Step 4: Run, mutate, commit**

Run: `cargo test -p tomte-app --bin tomte` — all PASS. Run the two mutations from Step 1; each fails the test; restore. Gate, then:

```bash
git add crates/app/src/views/review.rs crates/app/src/views/mod.rs
git commit -m "perf(review): cache the sidebar plan behind a value-compared key and a model dirty flag"
```

---

### Task 4: `row_keys` and `splice_window` (pure)

**Files:**
- Modify: `crates/app/src/views/review.rs` (new items after `SidebarPlan`; tests in the planner tests module)

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub(super) enum RowKey { Group(&'static str), Folder(PathBuf),
                           Target { path: PathBuf, queued: Option<QueuedStatus>, failed: bool } }
  pub(super) fn row_keys(plan: &SidebarPlan, queue: &QueueSnapshot) -> Vec<RowKey>;
  pub(super) fn splice_window(old: &[RowKey], new: &[RowKey]) -> Option<(std::ops::Range<usize>, usize)>;
  ```

- [ ] **Step 1: Write the tests**

In `review.rs`'s `#[cfg(test)] mod tests`:

```rust
    fn keys(names: &[&str]) -> Vec<RowKey> {
        names
            .iter()
            .map(|n| RowKey::Target {
                path: PathBuf::from(n),
                queued: None,
                failed: false,
            })
            .collect()
    }

    /// `ListState::splice(0..old, new)` snaps the scroll anchor to item 0
    /// whenever the anchor lies in the range — always, for a whole-list
    /// splice. So only the window that changed is spliced (spec 2026-10-11
    /// §2.2): common prefix and suffix are kept.
    ///
    /// MUTATION: ignore the suffix (`suffix = 0`) → "removed from the
    /// middle" reports `(1..3, 1)` instead of `(1..2, 0)`.
    #[test]
    fn splice_window_keeps_the_common_prefix_and_suffix() {
        assert_eq!(splice_window(&keys(&["a", "b", "c"]), &keys(&["a", "b", "c"])), None);
        assert_eq!(
            splice_window(&keys(&["a", "b", "c"]), &keys(&["p", "q", "a", "b", "c"])),
            Some((0..0, 2)),
            "a PROCESSING group appearing at the top"
        );
        assert_eq!(splice_window(&keys(&["a", "b", "c"]), &keys(&["a", "c"])), Some((1..2, 0)));
        assert_eq!(splice_window(&keys(&["a", "b"]), &keys(&["c", "d"])), Some((0..2, 2)));
        assert_eq!(splice_window(&keys(&["a", "a"]), &keys(&["a"])), Some((1..2, 0)), "no overlap");
        assert_eq!(splice_window(&[], &keys(&["a"])), Some((0..0, 1)));
    }

    /// A row that grew a sub-line (queued, or failed) is taller: its key
    /// changes so the list re-measures it in place.
    ///
    /// MUTATION: drop `queued` (or `failed`) from `RowKey::Target` → the
    /// two keys compare equal and the window is `None`.
    #[test]
    fn a_row_whose_status_changed_is_spliced_in_place() {
        let before = keys(&["a", "b", "c"]);
        let mut after = before.clone();
        after[1] = RowKey::Target {
            path: PathBuf::from("b"),
            queued: Some(QueuedStatus::Waiting),
            failed: false,
        };
        assert_eq!(splice_window(&before, &after), Some((1..2, 1)));
        let mut failed = before.clone();
        failed[2] = RowKey::Target {
            path: PathBuf::from("c"),
            queued: None,
            failed: true,
        };
        assert_eq!(splice_window(&before, &failed), Some((2..3, 1)));
    }

    /// `row_keys` reads the plan AND the failures: a failed row's key says
    /// so even though `SidebarRow::Target` does not carry the message.
    ///
    /// MUTATION: `failed: false` always → the last assertion fails.
    #[test]
    fn row_keys_follow_the_plan_and_the_failures() {
        let drifted = vec![entry("/a", "conflict"), entry("/b", "destination_drift")];
        let queue = QueueSnapshot {
            processing: Vec::new(),
            failures: BTreeMap::from([(PathBuf::from("/b"), "op: could not authenticate".into())]),
            busy: false,
        };
        let plan = plan_of(&drifted, &queue);
        let ks = row_keys(&plan, &queue);
        assert_eq!(ks[0], RowKey::Group("NEEDS A DECISION"));
        assert_eq!(ks[1], RowKey::Target { path: PathBuf::from("/a"), queued: None, failed: false });
        assert_eq!(ks[2], RowKey::Group("SAFE TO RESOLVE"));
        assert_eq!(ks[3], RowKey::Target { path: PathBuf::from("/b"), queued: None, failed: true });
    }
```

(`entry` and `plan_of` are the planner tests' existing helpers; use their real names and signatures.)

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p tomte-app --bin tomte splice_window row_keys`
Expected: compile errors.

- [ ] **Step 3: Implement**

After `SidebarPlan` in `review.rs`:

```rust
/// What identifies a sidebar row to the list element's height cache
/// (spec 2026-10-11 §2.2). A row whose key is unchanged across a plan
/// rebuild keeps its measured height; anything else is re-measured. The
/// queued status and the failure are in the key because they add a
/// sub-line: same path, different height.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RowKey {
    Group(&'static str),
    Folder(PathBuf),
    Target {
        path: PathBuf,
        queued: Option<QueuedStatus>,
        failed: bool,
    },
}

pub(super) fn row_keys(plan: &SidebarPlan, queue: &QueueSnapshot) -> Vec<RowKey> {
    plan.rows
        .iter()
        .map(|row| match row {
            SidebarRow::Group { label, .. } => RowKey::Group(label),
            SidebarRow::Folder { node, .. } => RowKey::Folder(node.path.clone()),
            SidebarRow::Target { entry, queued, .. } => RowKey::Target {
                path: entry.target.clone(),
                queued: queued.map(|q| q.status),
                failed: queue.failure(&entry.target).is_some(),
            },
        })
        .collect()
}

/// The window `ListState::splice` must be told about so that unchanged
/// rows keep their measured heights and the scroll anchor stays on its
/// row: `(old_range, new_count)` for the middle that differs, `None` when
/// the two lists are identical. `splice(0..old, new)` would snap the
/// anchor to item 0 on every rebuild (gpui 0.2.2 `list.rs`, `splice_focusable`).
pub(super) fn splice_window(old: &[RowKey], new: &[RowKey]) -> Option<(std::ops::Range<usize>, usize)> {
    if old == new {
        return None;
    }
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
        .min(max_suffix);
    Some((prefix..old.len() - suffix, new.len() - prefix - suffix))
}
```

`QueuedStatus` already derives `Debug, Clone, Copy, PartialEq, Eq`.

- [ ] **Step 4: Run, mutate, commit**

Run: `cargo test -p tomte-app --bin tomte splice_window row_keys` → PASS; run the three mutations; restore. Gate, then:

```bash
git add crates/app/src/views/review.rs
git commit -m "feat(review): row keys and the splice window for a virtualized sidebar"
```

---

### Task 5: The sidebar is a gpui `list`; `review-many`; reveal on auto-advance

**Files:**
- Modify: `crates/app/src/views/review.rs` (`ReviewView` fields + `new`; `advance_from` ~1470–1502; `sidebar()` ~2027–2145 → `sidebar()` + new `sidebar_item()`; `render_inner` ~2757–2843; `target_row` gets a `debug_selector`)
- Modify: `crates/app/src/views/fixtures.rs` (`STATES`, `build`'s review arm, new `many_model`)
- Test: `crates/app/src/views/mod.rs`
- Modify: `CHANGELOG.md`, `AGENTS.md`, `.claude/skills/gpui-tomte/SKILL.md`

**Interfaces:**
- Consumes: `row_keys`, `splice_window` (Task 4); `plan_with` (Task 3); `group_header_el(.., mode, ..)` (Task 2).
- Produces: `pub(super) rows_rendered: Rc<Cell<usize>>`, `pub(super) list_state: ListState`, `pub(super) fn advance_from`; the `review-many` pose; `debug_selector("review-row-{ix}")` on every target row.

- [ ] **Step 1: Verify the gpui facts this task leans on**

```bash
G=$(ls -d ~/.cargo/registry/src/*/gpui-0.2.2 | head -1)
rg -n 'pub fn list\b|impl Styled for List|pub struct ListOffset|pub fn (reset|splice|item_count|scroll_to|scroll_to_reveal_item|bounds_for_item|viewport_bounds|logical_scroll_top)\b' $G/src/elements/list.rs
rg -n 'pub use.*list|pub mod list' $G/src/elements.rs $G/src/elements/mod.rs $G/src/gpui.rs 2>/dev/null
rg -n 'fn request_layout|fn prepaint' -A 12 $G/src/view.rs | rg -n 'update|render|layout' | head
```
Expected: `list`, `ListState`, `ListAlignment`, `ListOffset` reachable as `gpui::{list, ListState, ListAlignment, ListOffset}`; `impl Styled for List`; in `view.rs`, `AnyView`'s render runs `view.update(..)` to PRODUCE the element and the element's layout/prepaint run after that update returned — so an entity update inside an item closure during prepaint is not nested. Quote the lines in the report.

- [ ] **Step 2: The `review-many` pose**

`fixtures.rs`:

- `STATES`: after `review-tree-selected`, add
  ```rust
      (
          "review-many",
          "500 safe files in Tree mode: queued, running, failed and ticked rows — the perf pose",
      ),
  ```
- After `multiselect_model`:

```rust
/// Five hundred drifted files under forty skill folders and twenty tool
/// folders — the user's real shape on 2026-10-11 (499 drifted under
/// `~/.agents/skills`), which the sidebar must render without lag.
fn many_model() -> SyncModel {
    let mut m = multiselect_model();
    let mut drifted = m.drifted.clone();
    for skill in 0..40u64 {
        for file in 0..12u64 {
            drifted.push(DriftSummary {
                target: home(&format!(".agents/skills/skill-{skill:02}/reference/doc-{file:02}.md")),
                class: if file % 7 == 0 { "source_ahead" } else { "destination_drift" }.into(),
                since_ts: Some(now() - 1_000 * (skill * 12 + file)),
                origin_differs: file % 11 == 0,
            });
        }
    }
    for tool in 0..20u64 {
        drifted.push(DriftSummary {
            target: home(&format!(".config/tool-{tool:02}/config.toml")),
            class: "destination_drift".into(),
            since_ts: Some(now() - 5_000 * tool),
            origin_differs: false,
        });
    }
    m.hydrate_status(drifted, 1_200, None, false, None);
    m
}
```

- In `build`: add `| "review-many"` to the review arm's name list; the model choice becomes `match name { "review-multiselect" | "review-tree" | "review-tree-selected" => multiselect_model(), "review-many" => many_model(), _ => rich_model() }`; after the `review-tree` block add:

```rust
            if name == "review-many" {
                // One failed, one running, three waiting: the sub-line rows
                // the list must size one by one (spec 2026-10-11 §2.4).
                let failed = home(".agents/skills/skill-02/reference/doc-03.md");
                s.queue.push(QueuedAction {
                    target: failed.clone(),
                    decision: Decision::KeepDisk,
                    notify_on_done: false,
                });
                let _ = s.queue.take_batch();
                s.queue
                    .finish_all(&[(failed, Some("op: could not authenticate".into()))]);
                s.queue.push(QueuedAction {
                    target: home(".agents/skills/skill-03/reference/doc-01.md"),
                    decision: Decision::KeepDisk,
                    notify_on_done: false,
                });
                let _running = s.queue.take_batch();
                for file in ["doc-02", "doc-04", "doc-05"] {
                    s.queue.push(QueuedAction {
                        target: home(&format!(".agents/skills/skill-03/reference/{file}.md")),
                        decision: Decision::KeepSourceLocal,
                        notify_on_done: false,
                    });
                }
                s.queue_pumping = true;
                let home_dir = home("");
                let collapsed: BTreeSet<PathBuf> = BTreeSet::from([
                    home(".agents/skills/skill-05/reference"),
                    home(".agents/skills/skill-06/reference"),
                ]);
                let plan = sidebar_rows(SidebarInputs {
                    drifted: &s.state.read(cx).drifted,
                    queue: &QueueSnapshot::default(),
                    mode: ReviewMode::Tree,
                    collapsed: &collapsed,
                    selection: &Selection::default(),
                    home: &home_dir,
                });
                review.update(cx, |view, _| {
                    view.mode = ReviewMode::Tree;
                    view.collapsed = collapsed.clone();
                    for file in 0..6u64 {
                        view.selection.toggle(
                            &home(&format!(".agents/skills/skill-10/reference/doc-{file:02}.md")),
                            &plan.safe_order,
                        );
                    }
                });
            }
```

(`finish_all`'s exact shape: mirror the `review-failed` block a few lines above. The collapsed keys are the DEEPEST directory of a compacted chain — `skill-05/reference`, not `skill-05` — per `FolderNode::path`'s doc; if `tree_rows` compacts differently, read a folder's `node.path` from `plan.rows` and use that.)

Run `cargo test -p tomte-app --bin tomte every_gallery_state` → PASS (the pose renders under the OLD column; that is the "before").

- [ ] **Step 3: Write the tests**

In `views/mod.rs` tests:

```rust
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
    /// rebuild → the scroll top returns to item 0 after the no-op update.
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

        // Scroll down, then a model update that changes no row.
        vis.update(|_w, cx| review.read(cx).list_state.scroll_to(gpui::ListOffset { item_ix: 200, offset_in_item: px(0.) }));
        let state = vis.update(|_w, cx| shell.read(cx).state.clone());
        vis.update(|_w, cx| state.update(cx, |_model, cx| cx.notify()));
        vis.run_until_parked();
        assert_eq!(
            vis.update(|_w, cx| review.read(cx).list_state.logical_scroll_top().item_ix),
            200,
            "an unchanged plan keeps the scroll anchor"
        );

        // Everything resolves: the plan is empty.
        let saved = vis.update(|_w, cx| state.read(cx).drifted.clone());
        vis.update(|_w, cx| state.update(cx, |model, cx| { model.drifted.clear(); cx.notify(); }));
        vis.update(|_w, cx| shell.update(cx, |s, _| { s.queue = Default::default(); s.queue_pumping = false; }));
        vis.run_until_parked();
        assert_eq!(item_count(vis), (2, 0), "the note and the footer");

        // And drift comes back.
        vis.update(|_w, cx| state.update(cx, |model, cx| { model.drifted = saved; cx.notify(); }));
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
            fixtures::build("review-multiselect", smoke_paths(), cx).expect("review-multiselect fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        let zshrc = fixtures::home(".zshrc");
        let ix = vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                let plan = view.plan(cx);
                plan.rows
                    .iter()
                    .position(|r| matches!(r, SidebarRow::Target { entry, .. } if entry.target == zshrc))
                    .expect(".zshrc is a row of the pose")
            })
        });
        let row = vis.debug_bounds(&format!("review-row-{ix}")).expect("row rendered");
        vis.simulate_click(row.center(), gpui::Modifiers::default());
        assert_eq!(vis.update(|_w, cx| review.read(cx).selected.clone()), Some(zshrc));
    }

    /// Keep disk on a row far down a long list: the next row is revealed
    /// (spec 2026-10-11 §2.2), not left off-screen.
    ///
    /// MUTATION: never set `reveal` in `advance_from` → the scroll top stays
    /// at item 0.
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
                (plan.visible_safe[400].clone(), plan.visible_safe[401].clone())
            })
        });
        vis.update(|_w, cx| {
            review.update(cx, |view, cx| {
                view.selected = Some(resolved.clone());
                view.advance_from(&resolved, cx);
            })
        });
        vis.run_until_parked();
        assert_eq!(vis.update(|_w, cx| review.read(cx).selected.clone()), Some(next));
        let top = vis.update(|_w, cx| review.read(cx).list_state.logical_scroll_top().item_ix);
        assert!(top > 300, "the list scrolled to the new selection (scroll top at item {top})");
    }
```

`plan_rows_len_for_test` is a one-line `pub(super) fn` on `ReviewView` returning `self.plan_cache.as_ref().map_or(0, |(_, p)| p.rows.len())`; `advance_from`, `list_state`, `rows_rendered` become `pub(super)`; `fixtures::home` is `pub(super)` (Plan A's Task 5 did this; if that plan has not landed yet, do it here).

- [ ] **Step 4: Run them to see them fail**

Run: `cargo test -p tomte-app --bin tomte 'list' 'auto_advance' 'row_inside'`
Expected: compile errors (no `list_state`, `rows_rendered`, `plan_rows_len_for_test`).

- [ ] **Step 5: Convert the sidebar**

`review.rs` — imports: add `gpui::{list, ListAlignment, ListOffset, ListState}` and `std::cell::Cell`.

Fields on `ReviewView` (after `plan_dirty`):

```rust
    /// The virtualized sidebar's scroll state and height cache (spec
    /// 2026-10-11 §2.2). Kept in step with the plan by `splice_window`
    /// over `row_keys`, never `reset` on an ordinary rebuild (that snaps
    /// the scroll anchor to the top). `pub(super)` for the tests.
    pub(super) list_state: ListState,
    /// The keys of the rows the list currently holds.
    row_keys: Vec<RowKey>,
    /// Rows the item closure rendered in the last frame — the proof that
    /// 500 rows do not render 500 elements. A test seam and a perf counter.
    pub(super) rows_rendered: Rc<Cell<usize>>,
    /// A target the next frame should scroll into view: set by an
    /// auto-advance, consumed by `render_inner`.
    reveal: Option<PathBuf>,
```

initialised `list_state: ListState::new(0, ListAlignment::Top, px(200.))`, `row_keys: Vec::new()`, `rows_rendered: Rc::new(Cell::new(0))`, `reveal: None`.

`advance_from`: where it calls `self.select(next, cx)`, do `self.reveal = Some(next.clone()); self.select(next, cx);`. Make it `pub(super)` with one doc line: "`pub(super)` so a test can drive an auto-advance without a live engine."

`render_inner`, right after the prune/key bookkeeping from Task 3 and before building the root `div()`:

```rust
        // Keep the list's height cache in step with the rows (spec
        // 2026-10-11 §2.2). `reset` only when the plan flips between empty
        // and non-empty (the "nothing to review" item comes and goes);
        // otherwise splice exactly the window that changed, so unchanged
        // rows keep their heights and the scroll anchor stays on its row.
        let keys = row_keys(&plan, &queue);
        let item_count = plan.rows.len().max(1) + 1;
        if self.list_state.item_count() == 0 || keys.is_empty() != self.row_keys.is_empty() {
            self.list_state.reset(item_count);
        } else if let Some((range, count)) = splice_window(&self.row_keys, &keys) {
            self.list_state.splice(range, count);
        }
        self.row_keys = keys;
        // An auto-advance landed on a row the user may not be able to see.
        // `bounds_for_item` is `Some` only for a rendered (hence measured)
        // row, and `scroll_to_reveal_item` is exact for those; a row that
        // was never rendered has no measured height to aim at, so it is
        // put near the top with `scroll_to` instead.
        if let Some(target) = self.reveal.take()
            && let Some(ix) = plan.rows.iter().position(
                |r| matches!(r, SidebarRow::Target { entry, .. } if entry.target == target),
            )
        {
            let viewport = self.list_state.viewport_bounds();
            match self.list_state.bounds_for_item(ix) {
                Some(b) if b.top() >= viewport.top() && b.bottom() <= viewport.bottom() => {}
                Some(_) => self.list_state.scroll_to_reveal_item(ix),
                None => self.list_state.scroll_to(ListOffset {
                    item_ix: ix.saturating_sub(2),
                    offset_in_item: px(0.),
                }),
            }
        }
        self.rows_rendered.set(0);
```

`sidebar()` keeps its signature and becomes:

```rust
    fn sidebar(
        &self,
        plan: &Rc<SidebarPlan>,
        queue: &QueueSnapshot,
        in_sync: u64,
        now: u64,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let plan = plan.clone();
        let queue = queue.clone();
        let weak = cx.weak_entity();
        let counter = self.rows_rendered.clone();
        // The item closure runs during the list's prepaint, after this
        // view's render returned and its lease was released (gpui 0.2.2
        // `view.rs`: `AnyView` produces the element under `update`, then
        // lays it out) — so updating the view here is a FRESH update, not
        // a nested one, and gives each row the `Context` its listeners need.
        let rows = list(self.list_state.clone(), move |ix, _window, cx| {
            let Some(view) = weak.upgrade() else {
                return div().into_any_element();
            };
            counter.set(counter.get() + 1);
            view.update(cx, |view, cx| {
                view.sidebar_item(ix, &plan, &queue, in_sync, now, theme, cx)
            })
        })
        .size_full();
        div()
            .id("review-sidebar")
            .w(px(self.sidebar_width.unwrap_or(SIDEBAR_WIDTH)))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("review-sidebar-rows")
                    // Test-only (no-op in release builds): lets a
                    // render-smoke test find this column's real bounds and
                    // click into its empty space below the last row.
                    .debug_selector(|| "review-sidebar-rows".into())
                    .flex_1()
                    .min_h_0()
                    .py_1()
                    .child(rows),
            )
    }

    /// One list item: a plan row, the "nothing to review" note (item 0 of
    /// an empty plan), or the In sync footer (the last item). The rows are
    /// `target_row` / `ui::folder_row` / `group_header_el` exactly as
    /// before the sidebar was virtualized; only the container changed.
    #[allow(clippy::too_many_arguments)]
    fn sidebar_item(
        &self,
        ix: usize,
        plan: &SidebarPlan,
        queue: &QueueSnapshot,
        in_sync: u64,
        now: u64,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selection_active = !self.selection.is_empty();
        match plan.rows.get(ix) {
            Some(SidebarRow::Group { label, check }) => {
                group_header_el(label, *check, selection_active, self.mode, theme, cx).into_any_element()
            }
            Some(SidebarRow::Folder { node, check }) => {
                let row_path = node.path.clone();
                let check_path = node.path.clone();
                ui::folder_row(
                    theme,
                    ElementId::named_usize("review-folder", ix),
                    ui::FolderRow {
                        depth: node.depth,
                        label: node.label.clone(),
                        collapsed: node.collapsed,
                        count: count_chip(node.safe, node.total),
                        check: *check,
                        show_check: selection_active,
                    },
                    cx.listener(move |view, ev: &ClickEvent, _w, cx| {
                        view.folder_click(&row_path, ev.modifiers(), cx);
                    }),
                    cx.listener(move |view, _ev, _w, cx| view.toggle_folder(&check_path, cx)),
                )
                .into_any_element()
            }
            Some(SidebarRow::Target { entry, queued, locked, cancellable, depth, selectable, checked }) => self
                .target_row(
                    ix, entry, now, *queued, *locked, *cancellable, *depth, *selectable, *checked,
                    selection_active, queue.failure(&entry.target), theme, cx,
                )
                .into_any_element(),
            None if plan.rows.is_empty() && ix == 0 => div()
                .p_3()
                .text_sm()
                .text_color(theme.text_muted)
                .child("nothing to review")
                .into_any_element(),
            // Collapsed in-sync group: a count line, not a list (§7.2).
            None => div()
                .px_3()
                .py_2()
                .mt_2()
                .border_t_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.text_muted)
                .child(format!("In sync ({in_sync})"))
                .into_any_element(),
        }
    }
```

(`cancellable` exists only once Plan A's Task 5 has landed; if it has not, leave it out of the destructure and the call.) `target_row` gets, right after `.id(ElementId::named_usize("review-target", ix))`:

```rust
            // Test-only (no-op in release builds): a real click in the
            // render-smoke tests finds this row by its plan index.
            .debug_selector(move || format!("review-row-{ix}").into())
```

Note the element ids now use the PLAN row index for every row kind (they were a running count over folder/target rows); `rg -n '"review-target"|"review-folder"|"review-check"' crates/app/src` to confirm no test depends on the old numbering.

Add `pub(super) fn plan_rows_len_for_test(&self) -> usize { self.plan_cache.as_ref().map_or(0, |(_, p)| p.rows.len()) }` next to `plan`.

- [ ] **Step 6: Run everything, then the mutations**

Run: `cargo test -p tomte-app` — every test PASS, the T8 click test (`a_click_in_the_diff_keeps_its_focus_and_a_sidebar_click_takes_the_root`) included: its click below the last row now lands in the list's empty space and must still reach the root's `track_focus`. If it fails, the list's bottom padding is the first thing to look at: the click point is `rows_bounds.bottom − 8 px`; the `py_1` wrapper keeps that point outside any row.

Run the four mutations named in Step 3; each fails its test; restore from `cp` backups.

- [ ] **Step 7: Shoot, read, measure**

`cargo build --workspace`, then `./scripts/shoot.sh review-many dark`, `review-many light`, `review-tree dark`, `review-processing dark`, `review-multiselect dark`. READ each: `review-many` shows the tree with its folders, a running row, three queued rows with their sub-lines, a failed row whose message wraps, six ticked rows, the toggle lit on the header, and the first visible rows start at the top; the others look exactly as they did after Task 2.

Then `TOMTE_PERF=1 target/debug/tomte --gallery review-many --dark 2>/tmp/perf.log & sleep 6; kill %1; cat /tmp/perf.log` and put any `perf[review-render]` lines in the report (the first frames); the controller measures hover separately.

- [ ] **Step 8: Changelog, engineering map, skill**

`CHANGELOG.md` `## Unreleased`:
- In the existing "A folder tree for the safe group" entry, replace "The List / Tree switch at the top of the sidebar shows" with "The folder-tree toggle on the SAFE TO RESOLVE header shows".
- Under `### Fixed` (create it above `### Added` if Plan A's entry is not there yet) add:

```markdown
- **Review stays smooth with hundreds of files.** The sidebar renders only
  the rows in view and rebuilds its plan only when something changed; with
  500 drifted files the tree used to lag on every mouse move. After a
  decision the next file scrolls into view instead of advancing
  off-screen.
```

`AGENTS.md`:
- Rules → Code, the bullet "Any scrollable list rides `uniform_list` with fixed-height rows; plain `overflow_y_scroll` columns drop frames." → "Any scrollable list rides `uniform_list` (fixed-height rows) or gpui's `list` (mixed heights — the Review sidebar, whose plan is spliced into its `ListState` by `splice_window` over `row_keys`, never `reset` on an ordinary rebuild); plain `overflow_y_scroll` columns drop frames."
- Current state: in the open list delete the bullet beginning "The Review sidebar still rides `overflow_y_scroll`"; in the branch paragraph add "The sidebar is virtualized (gpui `list`) with a cached plan; the folder-tree toggle sits on the SAFE TO RESOLVE header."
- Gallery section: add `review-many` to the sentence listing the multi-select poses: "`review-many` is the 500-file perf pose."

`.claude/skills/gpui-tomte/SKILL.md`, a new section before `## Tests`:

```markdown
## Virtualized lists (`list` / `ListState`)

- `gpui::list(ListState, FnMut(usize, &mut Window, &mut App) -> AnyElement)`
  is `uniform_list` for mixed heights: items are measured lazily, visible
  ones re-measured every frame, off-screen heights CACHED. Tell the state
  about changes with `splice(old_range, new_count)`; `splice(0..old, new)`
  snaps the scroll anchor to item 0 whenever the anchor is inside the
  range, i.e. always — splice only the changed window (`splice_window` in
  `review.rs`). `reset(n)` only when the item set is replaced wholesale.
- The item closure runs in the list's prepaint, after the owning view's
  `render` returned and its lease was released: `weak.upgrade().update(cx,
  |view, cx| …)` inside it is a fresh update, and the way a row gets a
  `Context` for `cx.listener` (`ReviewView::sidebar()`, 2026-10-11).
- `bounds_for_item(ix)` is `Some` only for a rendered item;
  `scroll_to_reveal_item(ix)` is exact only when every item up to `ix` is
  measured. For a far row use `scroll_to(ListOffset { item_ix, .. })`.
- A `Rc<Cell<usize>>` counter in the item closure, reset in `render`, is
  the test seam for "only the rows in view render" (`rows_rendered`).
```

- [ ] **Step 9: Gate and commit**

```bash
git add crates/app/src/views/review.rs crates/app/src/views/fixtures.rs crates/app/src/views/mod.rs CHANGELOG.md AGENTS.md .claude/skills/gpui-tomte/SKILL.md
git commit -m "perf(review): virtualize the sidebar with gpui's list; review-many pose; reveal the row an auto-advance lands on"
```
