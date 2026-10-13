# Review: multi-select, bulk actions, and a folder tree

**Status:** decided (conversation, 2026-09-23). Sections 2.1–2.4 and the
tree shape were chosen by the user; the rest are the controller's decisions,
recorded here so they can be overruled before or during implementation.
**Implemented by:** `.agents/artifacts/plans/2026-09-23-review-multiselect-tree.md`

> "In the review tab, add a way to multiselect safe to merge changes, and
> allow bulk action over those + add a way to change the way changes are
> displayed, from flat list (default) to tree view and making it possible to
> select a folder where bulk action would be recursive over descendants."

## 1. What exists

The Review sidebar is planned as data before it renders (`sidebar_rows` in
`crates/app/src/views/review.rs`): a PROCESSING group first, then NEEDS A
DECISION (`conflict`, `local_source_diverged`, `eval_failed`), then SAFE TO
RESOLVE (everything else — `destination_drift`, `source_ahead`,
`remote_ahead`). One row is `selected` and drives the detail pane; the header
above the diff carries that row's actions (Keep disk, Keep source · this
machine, Keep source · origin when loaded, Keep both when clean, Open merge
editor). A decision goes `run_action → Shell::enqueue → pump`, and `enqueue`
pumps on every call, so the first of several quick clicks always drains
alone and only the ones made *while* it runs share a batch (queue spec §2.2).
Rows are ~28px, plain `overflow_y_scroll`, no keyboard handling, no focus
handle. The UI crate has a chevron icon but no checkbox and no tree row.

## 2. Decisions

### 2.1 Only safe rows are selectable

Multi-selection membership is limited to rows in SAFE TO RESOLVE. NEEDS A
DECISION rows and PROCESSING rows carry no checkbox and cannot be added by
any gesture. Bulk actions are exactly the one-click pair: **Keep disk** and
**Keep source · this machine**. Keep source · origin is per-file (it needs
that file's origin content loaded and shown) and Keep both is per-file (a
dry-run per file); neither is offered in bulk.

### 2.2 The selection model

```
Selection { members: Vec<PathBuf>  // display order, safe rows only
            anchor:  Option<PathBuf> } // for ⇧-click ranges
```

- `selected: Option<PathBuf>` (the detail row) stays what it is today and is
  independent of `Selection`: the diff pane shows the last plainly clicked
  file even while a multi-selection exists.
- Every mutation is a pure function over the current plan and the gesture
  (§3). The plan is rebuilt on every render from the model and the queue,
  and `Selection::prune(plan)` runs each time: a member that resolved, got
  queued, or turned into a conflict leaves the selection silently. Nothing
  is ever selected that the plan does not list as safe.
- The selection is session state on the view: cleared by ✕, by Esc, by a
  bulk action, and by a plain click; not persisted; it survives navigation
  away and back within the session.

### 2.3 Gestures

On a **safe file row**:

| gesture | effect |
|---|---|
| plain click | `selected = row`; `Selection` cleared (today's behaviour, unchanged) |
| ⌘-click (`Modifiers::platform`) | toggle membership; `anchor = row`; `selected` unchanged |
| ⇧-click (`Modifiers::shift`) | members = every safe file row between `anchor` and the row in **visible** display order (collapsed folders' files are not visible and are not included), added to the current members; `anchor` unchanged. With no anchor, the anchor is `selected` if it is safe, else the first visible safe row. |
| checkbox click | same as ⌘-click; the click does not reach the row |

On a **folder row** (tree view only): the checkbox, or ⌘-click on the row,
toggles **all safe descendants, collapsed or not** — this is the recursive
bulk the feature exists for. Plain click on a folder row toggles its
collapse. A folder's checkbox is tri-state: Off (no safe descendant
selected), Partial, On (every safe descendant selected); clicking Partial
selects all, clicking On clears all.

On the **SAFE TO RESOLVE header**: a tri-state checkbox over every safe row
(tree: every safe file whether visible or collapsed). Same Partial/On rule.

The checkbox on a row fades in on hover, exactly like the dashboard's
hover-revealed quick actions (`group_hover`), and is **forced visible on
every safe row and folder while the selection is non-empty**, so a
selection in progress is always legible.

Keyboard, under a `Review` key context on the view's root (the pattern the
merge editor uses with `MergeEditor`): `escape` clears the selection;
`cmd-a` selects every safe row. The root takes focus on any click inside
the view. Nothing else is bound.

### 2.4 Bulk actions are one batch

When `members` is non-empty, a **selection bar** is pinned under the
sidebar's rows, and the detail header's action slot is empty:

```
3 selected                       ✕
[Keep disk]  [Keep source]
```

The bar lives with the selection it acts on, not in the detail pane
(amended 2026-10-12): that pane shows whichever row `selected` names —
often none of the ticked files, and nothing at all when no file is open —
and a bar built there vanished with it. Pinned at the bottom rather than
under the SAFE TO RESOLVE header so that ticking never pushes the rows
down; two rows because at the sidebar's default width the pair has no room
beside the count. The source button reads "Keep source" here: origin is
never a bulk action (§2.1), so there is nothing to tell it apart from.

A bulk press builds one `QueuedAction` per member (display order, the same
`Decision` the single button would build, `notify_on_done: false`) and calls
the new `Shell::enqueue_many(Vec<QueuedAction>, cx)`, which pushes them all,
logs one line naming the label and the count, and pumps **once** — so the
whole selection is one batch: one `re-add` or `apply`, one commit, one
push, one session per file (queue spec §2.2, §2.3). The selection is then
cleared, the rows move to PROCESSING, and `selected` advances to the next
safe row after the *last* member in display order (`next_after`). Failures
land per row as today; the banner names the last landed file as today.

`enqueue_many` with one item behaves exactly like `enqueue`.

### 2.5 The tree

`ReviewMode { Flat, Tree }` on the view, **default Flat, session only**. A
two-segment control at the top-right of the sidebar switches it (same
segmented shape as the diff's comparison selector; tooltips "Flat list" and
"Folder tree").

In Tree mode **only the SAFE TO RESOLVE group changes shape**; PROCESSING
and NEEDS A DECISION stay flat lists above it, unchanged.

The tree is a pure projection of the safe rows:

- Rooted at `$HOME`, which is not shown; a target outside home roots at
  `/` and shows its full chain.
- Directories sort before files; both case-insensitively by name.
- A directory whose only child is a directory is **compacted** into its
  child: one row labelled `.config/nvim/`. Compaction is by structure, not
  by name, and stops at the first directory with a file or a second child.
- A folder row shows: chevron (rotated −90° when collapsed, the existing
  `chevron_down` asset via `Svg::with_transformation`), the label with a
  trailing `/`, and a count chip: `n` safe files under it, or `n of m`
  when the same folder also holds `m − n` files that are not safe (they sit
  in NEEDS A DECISION or PROCESSING), so a full-folder tick is honest
  about what it will *not* touch.
- File rows are the existing `target_row`, indented 12px per depth. The
  file's name alone is the label (the path is the tree).
- Collapse state is a session-only `BTreeSet<PathBuf>` of collapsed folder
  paths. A collapsed folder hides its rows from the plan (they cannot be
  `selected` by keyboard-less means and are skipped by ⇧-ranges and by
  `next_after`), but a folder tick still reaches them.

### 2.6 Persistence

None. Mode, collapse set and selection reset on launch (user's choice).

### 2.7 Edge cases, decided

- A row that is `selected` (detail) and also a member: a plain click on
  another safe row clears the members. Expected: plain click is single.
- A bulk action while one of the members is already RUNNING cannot happen:
  running rows are not in the safe group, so `prune` removed them.
- A rescan that reclassifies a member as a conflict: pruned; the count in
  the selection bar drops; no message (the row visibly moved groups).
- An empty safe group in Tree mode shows the same empty note as Flat.
- With `members == 1`, the selection bar still shows ("1 selected") — any
  checkbox or ⌘ gesture enters selection mode; ✕, Esc or a plain click
  leaves it.

## 3. Components and interfaces

Pure, window-free, in `crates/app/src/views/review.rs` (or a sibling
`review/selection.rs` and `review/tree.rs` if the file's size warrants it —
it is ~3,000 lines):

```rust
pub(super) enum ReviewMode { Flat, Tree }

pub(super) struct Selection { members: Vec<PathBuf>, anchor: Option<PathBuf> }
impl Selection {
    fn toggle(&mut self, target: &Path, safe_visible: &[PathBuf]);
    fn extend_range(&mut self, to: &Path, safe_visible: &[PathBuf], fallback_anchor: Option<&Path>);
    fn toggle_all(&mut self, targets: &[PathBuf]);          // folder / header tri-state
    fn clear(&mut self);
    fn prune(&mut self, safe_all: &[PathBuf]);              // keep members ∩ safe, in safe order
    fn check_state(&self, targets: &[PathBuf]) -> CheckState; // Off | Partial | On
}

pub(super) enum SidebarRow {
    Group { label, check: Option<CheckState> },             // SAFE TO RESOLVE carries the header checkbox
    Folder { path, depth, label, collapsed, safe: usize, total: usize, check: CheckState },
    Target { entry, queued, locked, depth, checked: bool, selectable: bool },
}
pub(super) fn sidebar_rows(drifted, queue, mode, collapsed, selection, home) -> Vec<SidebarRow>;
pub(super) fn tree_rows(safe: &[DriftSummary], excluded: &[PathBuf], collapsed, home) -> Vec<SidebarRow>;
pub(super) fn safe_descendants(rows: &[SidebarRow], folder: &Path) -> Vec<PathBuf>; // collapsed or not
pub(super) fn visible_safe(rows: &[SidebarRow]) -> Vec<PathBuf>;
```

`Shell::enqueue_many(&mut self, actions: Vec<QueuedAction>, cx)` in
`views/mod.rs`; `enqueue` stays and delegates.

`tomte-ui` gains `checkbox(theme, id, state: CheckState, on_click)` (14px,
border token, accent fill when On, a dash when Partial) and a `tree_row`
builder (chevron + label + count chip) — pure builders, no entity coupling,
with `comp:checkbox` and `comp:tree-row` previews.

## 4. Testing

- Every pure function above has unit tests that name the mutation that
  fails them: toggle/range/toggle_all/prune/check_state; `tree_rows`
  compaction (a chain compacts, a chain with a file does not, structure
  not names), ordering, counts (`n of m`), collapse hiding rows while
  `safe_descendants` still reaches them; `sidebar_rows` in both modes with
  PROCESSING and NEEDS A DECISION unchanged.
- `Shell::enqueue_many` on the `EngineLab`: three files → one batch, one
  push (mutation: pump per item → three pushes), selection cleared,
  `selected` advanced past the last member.
- Gallery poses: `review-multiselect` (flat, three members, selection bar,
  checkboxes forced visible), `review-tree` (tree, one folder collapsed,
  one Partial folder, an `n of m` chip), `review-tree-selected` (a folder
  On). Dark and light, read.
- The render-smoke test covers the new poses; the existing `review-*`
  poses must render byte-for-byte unchanged in Flat mode with no selection.

## 5. Out of scope, deliberately

- Bulk Keep source · origin, bulk Keep both, bulk open-in-editor.
- Selection in the dashboard's activity list.
- Persisting mode or collapse state.
- Drag-select, arrow-key navigation of rows (a keyboard-nav pass is its own
  piece of work, as the UI-refinement spec noted).
