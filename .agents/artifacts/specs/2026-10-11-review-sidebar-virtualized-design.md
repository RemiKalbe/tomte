# Review sidebar: the mode toggle in the header, a virtualized list

**Status:** decided (conversation, 2026-10-11). The two complaints are the
user's; the placement and the mechanism were presented in chat and
approved ("sure"). Recorded so they can be overruled before or during
implementation.
**Implemented by:** `.agents/artifacts/plans/2026-10-11-review-sidebar-virtualized.md`

> "The list/tree buttons pushing everything down looks so ugly" · "The
> tree mode LAGS when there is many files... like badly"

## 1. What exists

The multi-select branch (`2026-09-23-review-multiselect-tree-design.md`)
put a List/Tree segmented control in its own row at the top of the sidebar
(`ReviewView::mode_control`), above the PROCESSING / NEEDS A DECISION /
SAFE TO RESOLVE groups. It is the only thing in the sidebar that is not a
row of the list, and it moves every group down by one line.

The sidebar column is a plain `overflow_y_scroll` div that renders EVERY
row of the plan, every frame. gpui redraws the whole window on any hover
change, and `render_inner` rebuilds the plan (`sidebar_rows`, and in Tree
mode `tree_rows` plus a `starts_with` filter per folder, plus a linear
`Selection::contains` per row) on every one of those frames. With the
gallery's seven rows this was invisible. The user's machine has 499
drifted targets right now: 500+ rows of text shaping per frame, plus the
plan, on every mouse move. The AGENTS.md open thread already named the
blocker: a queued or failed row carries a sub-line and is taller than a
plain one, so `uniform_list` (fixed heights) could not take the sidebar.

## 2. Decisions

### 2.1 The mode toggle lives on the SAFE TO RESOLVE header

The toggle reshapes exactly one group — Tree mode never touches PROCESSING
or NEEDS A DECISION — so it sits on that group's header, at the right,
left of the select-all checkbox. The separate control row is deleted;
nothing sits above the first group any more.

It is ONE icon toggle, not a two-segment control: at the sidebar's minimum
width (180 px) the header's label, a 20 px button, the 14 px checkbox and
the paddings total ~171 px; two labelled segments do not fit. Shape:

- `ui::icon_button(theme, id, icon, active, tooltip, on_click)`: 20×20,
  `rounded_md`, ghost at rest (no fill), the hover wash of `ButtonVariant::
  Ghost`; `active` paints the `Tonal(theme.accent)` field and tints the
  icon accent — the same "a view option is on" reading as a toolbar's Bold.
- The icon is a Lucide-style `folder-tree.svg` (12 px, `text_muted` at
  rest), embedded like `chevron_down.svg`. Tooltip: "Folder tree".
- Always visible when the header is (unlike the hover-revealed checkbox:
  it is the only entry point to the mode, and an invisible toggle is one
  nobody finds). When the safe group is empty there is no header and no
  toggle; the mode is irrelevant then.
- It carries `debug_selector("review-mode-toggle")` so a test can click it.

`SidebarRow::Group` is unchanged: the view renders the toggle on the header
that carries `check: Some(..)`, which is the SAFE header by construction.
`set_mode` / `toggle_mode` are unchanged; the toggle calls `toggle_mode`.

### 2.2 The sidebar is a gpui `list`

`gpui::list(ListState, render_item)` is `uniform_list`'s sibling for rows
of differing heights: it lays out and measures only the rows in view (plus
`overdraw`), caches measured heights in a sum tree, and re-measures the
visible ones every frame. Group headers, folder rows, plain file rows,
queued rows with a sub-line and failed rows with a wrapping message all
ride it as they are — no two-height scheme, no fixed row height.

- Items are the plan's rows in order, then one footer item: the existing
  "In sync (N)" line. An empty plan renders "nothing to review" as item 0
  and the footer as item 1. The plan itself is unchanged; the mapping is
  `ix < rows.len()` → row, else footer.
- `ListState` lives on the view (`ListAlignment::Top`, `overdraw` 200 px).
  It is NOT reset on every rebuild: `ListState::splice(0..old, new)`
  snaps the scroll anchor to item 0 whenever the anchor lies in the
  spliced range, which is always. Instead the view splices only the window
  that changed: rows are keyed (`Group` by label; `Folder` by path;
  `Target` by path, queued status and whether it carries a failure, so a
  row that grew a sub-line is re-measured), the common prefix and suffix
  of old and new keys are kept, and `splice(prefix..old_len − suffix,
  new_len − prefix − suffix)` is called on the middle. A pure function
  computes that window. Unchanged plans splice nothing.
- After an auto-advance (`advance_from`), the list reveals the new
  selection with `scroll_to_reveal_item`; a plain click never scrolls (the
  clicked row is on screen by definition). Without this a 500-row list
  advances off-screen on every Keep disk.
- The column keeps its `id`, `debug_selector("review-sidebar-rows")`,
  `flex_1`, `min_h_0`, `py_1`: the existing click-below-the-last-row test
  must keep landing in the list's empty space and reaching the root's
  `track_focus`.
- Rows render exactly as today (`target_row`, `ui::folder_row`,
  `group_header_el`); only the container changes.

### 2.3 The plan is cached

`sidebar_rows` runs when its inputs change, not when the window redraws.

- `ReviewView` keeps `plan_cache: Option<(PlanKey, Rc<SidebarPlan>)>` and
  `plan_dirty: bool`. `PlanKey { queue: QueueSnapshot, mode, collapsed,
  selection }` derives `PartialEq` (`QueueSnapshot` gains `PartialEq`; it
  is a short list of paths and a map of failure strings). The model is not
  in the key: the view's existing `cx.observe(&state, ..)` sets
  `plan_dirty` (and notifies, as today).
- `ReviewView::plan(&mut self, cx) -> Rc<SidebarPlan>` is the one builder:
  `render_inner` and every gesture (`toggle_member`, `extend_to`,
  `toggle_all_safe`, `select_all_safe`, `toggle_folder`, `advance_from`)
  call it; it rebuilds when `plan_dirty` or the key differs, else hands
  back the cached `Rc`. `advance_from`'s "queue before this click" plan is
  a one-off built directly, as today.
- `Selection::prune` runs after the rebuild as now; the pruned selection is
  written back into the cached key, so a prune does not cost a second
  rebuild on the next frame (prune only removes members outside
  `safe_order`, which the cached rows never flagged `checked`).
- The `list` item closure captures the `Rc<SidebarPlan>`; its `ListState`
  splice happens in `render_inner` right after a rebuild, from the old
  and new key lists.

### 2.4 Measured, not assumed

- A new gallery pose `review-many`: 500 drifted targets under
  `~/.agents/skills/<40 folders>/…` with three queued, one running, one
  failed and six selected, in Tree mode with two folders collapsed. It is
  the pose this spec is judged on (`./scripts/shoot.sh review-many dark`
  and `--gallery review-many` under `TOMTE_PERF=1`), and the render-smoke
  test renders it.
- `ReviewView` counts the rows its item closure rendered in the last frame
  (`rows_rendered: Rc<Cell<usize>>`, reset each render). A `#[gpui::test]`
  draws `review-many` in a 900 px-tall window and asserts the count is
  under 80; rendering every row (the current code) gives 500+. It is a
  test seam and a perf counter; `TOMTE_PERF=1` prints it with the existing
  `review-render` timing.
- Before/after `perf[review-render]` averages for `review-many`, hovering
  the sidebar, go in the plan's final report.

### 2.5 Behaviour the user sees

- The groups start at the top of the sidebar; the tree toggle sits on the
  SAFE TO RESOLVE line, lit when Tree is on.
- Hovering and scrolling a 500-row sidebar is smooth in both modes.
- Keep disk on a row far down the list keeps the next row in view.
- Nothing else changes: gestures, Esc/⌘A, the selection bar, folder
  chips, checkboxes, the width drag, the click tests.

## 3. Components and interfaces

```rust
// crates/ui/src/components/button.rs (or icon.rs)
pub fn icon_button(theme: Theme, id: impl Into<ElementId>, icon: Svg, active: bool,
                   tooltip: &'static str,
                   on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Stateful<Div>;
// crates/ui/src/components/icon.rs + assets/icons/folder_tree.svg
pub fn folder_tree(theme: Theme) -> Svg;

// crates/app/src/views/review.rs
#[derive(PartialEq)] struct PlanKey { queue: QueueSnapshot, mode: ReviewMode,
                                      collapsed: BTreeSet<PathBuf>, selection: Selection }
pub(super) enum RowKey { Group(&'static str), Folder(PathBuf),
                         Target { path: PathBuf, queued: Option<QueuedStatus>, failed: bool } }
pub(super) fn row_keys(plan: &SidebarPlan, failures: &BTreeMap<PathBuf, SharedString>) -> Vec<RowKey>;
/// The window to `ListState::splice` so that unchanged rows keep their
/// measured heights and the scroll anchor stays put.
pub(super) fn splice_window(old: &[RowKey], new: &[RowKey]) -> Option<(Range<usize>, usize)>;
impl ReviewView {
    pub(super) fn plan(&mut self, cx: &Context<Self>) -> Rc<SidebarPlan>;   // cached
    pub(super) rows_rendered: Rc<Cell<usize>>;
    list_state: ListState, plan_cache: Option<(PlanKey, Rc<SidebarPlan>)>, plan_dirty: bool,
}
```

The `list` element's `render_item` is `FnMut(usize, &mut Window, &mut
App) -> AnyElement`: it has `&mut App`, not the view's `Context`, so row
listeners are built with `cx.listener(..)` inside the closure body via the
view's `Entity` handle (`entity.update` is not needed — `Context::listener`
closures are created in `render_inner` per visible row exactly as they are
today, but inside the item closure; the closure captures a `WeakEntity<ReviewView>`
and the `Rc<SidebarPlan>`). The implementer verifies the shape against the
pinned source (`elements/list.rs`, `window.rs::listener`) before writing
it; the dashboard's `uniform_list` closure is the in-repo precedent.

## 4. Testing

- `splice_window`: identical → `None`; one row inserted at the front (a
  PROCESSING group appearing) → `(0..0, 2)`; one removed from the middle
  → `(i..i+1, 0)`; a row whose key changed in place → `(i..i+1, 1)`; all
  different → `(0..old, new)`. Mutations: ignore the suffix; ignore the
  queued status in the key.
- Cache: a `#[gpui::test]` renders a posed Review twice with nothing
  changed and asserts `Rc::ptr_eq` on the two plans; `toggle_member` then
  yields a new `Rc`; a model update (`state.update`) yields a new `Rc`.
  Mutations: drop the cache; forget `plan_dirty` in the observer.
- Virtualization: draw `review-many` in a 900 px window →
  `rows_rendered < 80`. Mutation: render all rows.
- The toggle: `simulate_click` on `debug_bounds("review-mode-toggle")`
  flips `mode` (mutation: drop the listener); the existing T7 tree tests
  and T8 keyboard/click tests pass unchanged.
- `sidebar_rows` tests unchanged (the plan did not change shape).
- Gallery: `review-many`, `review-tree`, `review-tree-selected`,
  `review-multiselect`, `review-processing` dark and light, READ: the
  toggle on the header, nothing above the first group, no clipped header
  at 180 px (shoot one at the minimum width).

## 5. Out of scope, deliberately

- A set-backed `Selection::contains` (the cache makes it per change, not
  per frame; add it only if `review-many` still measures slow).
- Persisting the mode or the scroll position.
- Keyboard navigation of rows; the dashboard's activity list.
- Moving the planner into `review_plan.rs` (still the next cohesive chunk;
  this spec touches the planner's callers, not the planner).
