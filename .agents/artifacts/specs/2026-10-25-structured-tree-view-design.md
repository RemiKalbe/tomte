# Structure mode as a tree: the file in order, a chip per side, a pick per row

**Status:** decided (conversation, 2026-10-25, with mockups in the browser).
Supersedes §6 "Structure stage" and §7 "Nothing to decide" of
`2026-10-21-structured-merge-design.md`; every other section of that spec
stands (parser, decisions, locking, assembly, Text stage, Save, errors).
**Implemented by:** `.agents/artifacts/plans/2026-10-25-structured-tree-view.md`

> "the so called structured view has nothing structured about it, this is
> literally the same editor" · "when we brainstormed at no point was it
> supposed to be some bs in the strip"

## 1. What happens today, and why it is wrong

Structure mode (branch `feat/structured-merge`, HEAD 9d37146) renders the
three input text panes and the result text area of the line editor, with a
decision strip above each changed key in the result pane and the input
panes' lines tinted. The structure lives only in the engine; on screen it is
the line editor with different strips. The first spec's §6 described this
layout in one paragraph and it was approved without ever being shown. It
fails the original request, "a ui where you can pick and choose how the new
json is merged", because:

- A key removed on disk shows its path and the words "removed on disk" and
  never its value.
- A removed row leaves the "on disk" pane at the top of the file, so the
  panes look unrelated.
- The user cannot see the shape of the file; they see lines.
- A file whose disk text equals its source text (only origin differs) opens
  in Structure with the note "same settings, formatting differs", which is
  false.

The engine (`tomte_core::structured`), the pure state
(`structured_state.rs`), the Text stage, Save and the locks are right and
stay. The view is replaced.

## 2. Decisions

Chosen from mockups, one question at a time:

1. **Layout B, the whole file as a tree.** Over A (a change list of only
   the differing keys with a result preview) and C (source, disk and result
   as aligned columns). No text panes and no result pane in Structure mode:
   the tree is the result.
2. **Density: everything in place, dimmed.** Every key in file order;
   identical keys are dim one-liners; a run of identical rows is not folded
   away by default. A header toggle folds identical runs.
3. **Every row kind reads from its chips alone.** No legend, no per-row
   words. The chip labels (source, disk, last written, both) and the chip
   text (a value, `removed`, `absent`, `{ 3 keys }`) say what happened.
4. **Pre-picked rows.** A row the engine can settle (`auto_pick` is `Some`)
   shows its pick filled. A row that needs the user (an unlocked conflict
   with no pick) carries a coloured left edge until picked.
5. **Keys and stages unchanged.** 1 disk, 2 source, 3 last written, b both,
   n next, ⌘Z / ⌘⇧Z; Edit text opens the Text stage on the assembled
   result; Save from either stage.

My calls, said in chat and accepted: comments stay in the tree as dim rows
in place; a chip holding an object or array shows a count and unfolds in
place, read only; a picked row loses its edge and keeps its tint; the fold
toggle is off by default and remembered for the editor session; Save is
enabled only once every required row has a pick (as today).

Calls made while writing this document (not shown in a mockup):

- **Colours are the editor's own tokens**, not the mockup's placeholders.
  Disk chips tint `theme.drift` (amber) and source chips `theme.accent`
  (blue), the tints the line editor already gives the two sides; last
  written `theme.text_muted`; both `theme.ok`. The edge of a row that
  needs the user is `theme.conflict`, the colour an open conflict strip
  has today. The mockup drew source purple, disk green and the edge
  orange; those were stand-ins.
- **Row height** is the pane row height (`PANE_ROW_H`, 20 px); every row
  is one line, so the tree rides `uniform_list`.
- **A long value** is clamped to one line inside its chip
  (`line_clamp(1)` in a bounded chip; `.truncate()` never ellipsizes in
  gpui 0.2.2) and the full text is a `TextTooltip` on hover.

## 3. The tree

### 3.1 Rows

The tree is a flat `Vec<TreeRow>` planned by a pure function, rendered one
row per item. Rows in order:

```
enum TreeRow {
    /// `"key": {` / `[`, or the root's `{` / `[`. Click folds or unfolds.
    Open { depth, key: Option<String>, bracket: Bracket, folded: bool,
           summary: String /* "3 keys" | "12 elements" */, same: bool },
    /// `}` / `]`, with the trailing comma if the entry has one.
    Close { depth, bracket },
    /// One comment line from an entry's leading block, in place.
    Comment { depth, text },
    /// An entry or element equal on every side. One line: `"key": value,`
    /// and the same-line trailing comment, dimmed.
    Same { depth, key: Option<String>, text },
    /// A decision: the key (or nothing for an element) then one chip per
    /// side. `open` = an unlocked conflict without a pick.
    Decision { depth, id: usize, key: Option<String>, chips: Vec<Chip>,
               open: bool, locked: Option<SharedString> },
    /// The read-only rows of an unfolded container chip, dimmed, under
    /// their decision row.
    Preview { depth, text },
    /// Fold toggle on: one row standing for a run of identical rows.
    Fold { depth, count: usize, what: Fold /* Keys | Elements */ },
}

struct Chip {
    side: ChipSide,            // Disk | Source | LastWritten | Both
    text: SharedString,        // one line, see 3.2
    picked: bool,              // the row's current pick
    container: Option<Container>, // Some => "{ n keys }" chip that unfolds
    unfolded: bool,
}
```

Order within a container is the result order the engine already uses for
decisions: source order, with a disk-only entry placed after the entry that
precedes it on disk when that entry is in the result, else at the end of the
container (spec 2026-10-21 §5.2). That is exactly the engine's `Site`
(`container`, `entry`, `after`, `elements`), which becomes public so the
planner can place rows without a second walk.

Depth is the nesting depth; rows indent by depth. The root `Open` and
`Close` rows are depth 0. A root that is an array lists elements the same
way.

A `Decision` row's key is the entry's unescaped key (quoted as JSON in the
row); its `path` (`editor.fontSize`) is not shown, the tree's indentation
carries it. Element rows show no key.

A container value that is identical on every side is one `Open` row with
`same: true`, folded by default, with its summary; unfolding it lists its
rows (all `Same`, or nested) in place. A container holding at least one
decision opens by default and cannot be hidden by the fold toggle. The root
is always open.

### 3.2 Chips

One chip per side the engine has for the row:

| Side | Shown when | Text |
| --- | --- | --- |
| source | always | the source value, one line; `removed` when the base had the entry and source does not; `absent` when neither has it |
| disk | always | the disk value, one line; `removed` / `absent` by the same rule |
| last written | `has_base(id)` and the row is a conflict or a same-change row | the base value, one line |
| both | `both_allowed` | the disk run then the source run, comma separated |

The missing-side words are symmetric. In two-way mode (no usable base,
base := source) a disk-only key therefore reads `absent` on its source chip
and a source-only key `removed` on its disk chip, which is what happened
from the user's point of view.

**Same change on both sides** (`RegionKind::BothSame`: disk and source
agree, the base differs): one chip labelled `source · disk` carrying the
shared value, which picks Source, plus a last written chip when the base
had the entry, so the user can revert the change. Such a row is never
`open`, counts in M, and Next visits it. A key source repeats with the
same value on disk is also `BothSame` and shows the two ordinary chips
with `×n` on the source one.

Rules:

- A scalar's text is its exact source bytes (quotes, number spelling)
  with runs of whitespace collapsed to one space.
- A container's text is `{ n keys }` or `[ n elements ]` and the chip
  unfolds on click into `Preview` rows under the row: the container's own
  entries, one per line, dimmed, read only, folded again on a second click.
  Unfolding never changes the pick.
- A conflicting element run (`Change::Elements`) shows each side's run
  comma separated, e.g. `"codex", "gemini"`; an element that is a
  container appears in the run as `{ n keys }` / `[ n elements ]`, and the
  run chip unfolds into one `Preview` row per element, each element's text
  on one line with whitespace collapsed. Objects inside array elements are
  not merged key by key (first spec §9); this is the only place a whole
  object is picked as one value besides a type change. A key whose value
  is an object on both sides never becomes one row: the engine recurses,
  so the mockup's "conflict on an object" row cannot occur and is not a
  fixture case.
- A key repeated on one side shows `×n` after that side's text, n being
  the copies; the effective value is the last copy, as the engine already
  resolves it. The earlier copies get no rows of their own: the decision
  row at the last copy stands for all of them.
- Comment rows come from the side whose text the result carries for the
  entry: source's leading block for an entry present in source (removed
  on disk included), disk's for a disk-only entry, matching what Save
  splices (first spec §5.4).
- A type change is an ordinary row; the chips show the two shapes.
- A source chip on a locked row is drawn as a plain value with a lock
  glyph after it and no disk chip; the lock reason (the template line,
  `lock_reason`) is its tooltip. A locked row is never `open`.
- `picked` is the row's effective pick: the resolution's entry, else
  `auto_pick`. An open row has no picked chip.

Clicking a chip picks that side (`StructuredState::pick`) and moves the
cursor to the row. Clicking anywhere else on a `Decision` row moves the
cursor there without picking. The cursor row has the same wash the line
editor gives its focused strip.

### 3.3 Fold toggle

A header control "Fold identical", off by default, kept for the editor
session (dropped with `LoadedMerge`). On:

- A maximal run of consecutive `Same`, `Comment`, and `same: true` `Open`
  rows (with their `Close`) inside one container collapses to one `Fold`
  row: `▸ 4 identical keys` / `▸ 251 identical elements`. Clicking a
  `Fold` row expands that one run in place for the session.
- `Decision`, `Preview`, and the `Open` / `Close` of a container holding a
  decision never fold.

Off: no `Fold` rows; folded `Open` rows still fold their own children.

### 3.4 Header

Left to right: the target path, then the controls `merge_structured::
header_controls` already renders, with these changes:

- Progress reads `N of M picked · K to decide` where M is the count of
  unlocked decisions, N those with a pick (explicit or auto), K the open
  ones; `K to decide` in `theme.conflict` when K > 0, else omitted and the
  text in `theme.ok`. The existing "auto-resolved · n changed entries"
  and "n conflicts, m resolved" strings go.
- "Fold identical" toggle (3.3).
- Next ↓ keeps `next_target` (open rows first, then every unlocked row)
  and scrolls the tree so the target row is visible.
- Structure | Lines, Edit text, Save as today. "origin differs · see
  Lines" stays when `inputs.origin` is `Some`.

### 3.5 Keys

Unchanged bindings, routed as today to the structured cursor: 1 disk, 2
source, 3 last written, b both, n next, ⌘Z undo, ⌘⇧Z redo. A pick key on
a row that does not offer that side does nothing (`pick_allowed`). ↑ / ↓
are not bound in this round.

### 3.6 Scrolling

The tree is a `uniform_list` over the planned rows, row height
`PANE_ROW_H`. Next and a cursor change scroll with
`UniformListScrollHandle::scroll_to_item`. Folding changes the row count;
the handle keeps the viewport where it is, except that a fold whose
collapsed run contained the viewport's first row scrolls to the `Fold`
row.

## 4. When Structure is not the default

`LoadedMerge::with_structured` picks the mode:

- `structured` is `None`: Lines (as today, with the not-structured note).
- `inputs.ours == inputs.theirs`: Lines, whatever `structured` holds, with
  the header note `disk matches source · origin differs · see Lines` when
  `inputs.origin` is `Some`, else `disk matches source`. The Structure
  toggle is still offered; switching shows a tree with no open row (dim
  rows, plus `source · disk` rows where both sides changed the same way
  since the last write) and the same note in place of the progress text,
  and Save stays enabled (it saves source as is).
- Otherwise Structure.

A file whose sides differ in text but not in value (`decisions.is_empty()`
with `ours != theirs`) opens in Structure with the progress text `same
settings, formatting differs · Save keeps the source layout`, as before.

## 5. What goes

From `merge_structured.rs` and `merge.rs`, the Structure-mode strip
machinery: `StripKind`, `StructuredStrip`, `strip_specs`, `strip_block`,
`text_row`, `result_decors`, `pane_decors`, `structured_pane_decors`,
`jump_lines`, the Structure branch of `sync_from` and `jump_to`, the
`structured_blocks` test seam, and the Structure branch that seeds the
result `TextArea` (the seed is still computed for the Text stage and for
Save). `STRIP_ROWS` stays for the line editor. The `StripState::Locked`
variant and the path label on `ui::decision_strip` stay only if the line
editor uses them; otherwise they go too.

The three input panes, the fourth origin pane and the result pane render
only in Lines mode and in the Text stage.

## 6. Files

- `crates/core/src/structured/merge.rs`: `Site` and its fields become
  `pub`; `StructuredDocument::side_doc(Side) -> &jsonc::Document` is added
  beside `side_text`. No behaviour change.
- `crates/app/src/structured_tree.rs` (new, lib, pure, no gpui): the
  planner. `plan(state: &StructuredState, view: &TreeView) ->
  Vec<TreeRow>` where `TreeView { fold_identical: bool, folded:
  HashSet<NodeKey>, unfolded_chips: HashSet<(usize, ChipSide)>,
  expanded_folds: HashSet<FoldKey> }` holds the session's fold state.
  `NodeKey` identifies a container by side and `NodeId`; `FoldKey` by
  container and first row. Also `scroll_target(rows, id) -> Option<usize>`
  (the index of decision `id`'s row).
- `crates/ui/src/components/structure_row.rs` (new): `tree_row(theme,
  depth, …) -> Div` for the row chrome (indent, dim, edge, cursor wash) and
  `value_chip(theme, side, text, picked, on_click) -> Div`; a `comp:
  structure-row` preview in `crates/ui/src/preview/`.
- `crates/app/src/views/merge_structured.rs`: keeps `header_controls`,
  `progress_text` (rewritten), `lock_reason`, `pick_allowed`, `has_picks`,
  `next_target`, `save_gate`, `invalid_json_line`, `not_structured_note`,
  `template_refusal`, `json_chip_text`, `TextStage`, `mode_toggle`,
  `switch_warning`; gains `render_tree` (the `uniform_list` and its row
  closure over the plan) and the fold toggle. Loses §5.
- `crates/app/src/views/merge.rs`: `LoadedMerge` gains `tree: TreeView`
  and the mode rule of §4; the Structure branches of render, `jump_to`,
  `sync_from` and the seed go; `apply_pick` and `next` call the tree's
  scroll.
- `crates/app/src/views/fixtures.rs`: `merge-structured` (the row kinds of
  §3.2 on one settings file: changed, added, removed, a conflict with a
  last-written chip, a type change, an object added on disk, a locked
  key, a repeated key, a same-change row with a last written chip, array
  rows with a conflicting run, an array of objects with a conflicting
  run), `merge-structured-locked` (unchanged inputs, now a tree),
  `merge-structured-folded` (the same file with Fold identical on),
  `merge-structured-same` (disk equals source, origin differs: Lines with
  the §4 note), `merge-structured-text` unchanged.
- `AGENTS.md` (workspace map line for `merge_structured`, gallery pose
  list, the branch paragraph under Current state), `README.md` and
  `PRODUCT.md` where they describe Structure mode.

## 7. Error handling

Unchanged from the first spec §7 except "Nothing to decide", replaced by
§4 above. The tree has no failure mode of its own: it is a projection of a
document that already computed. A decision whose `Site` the planner cannot
place (an engine bug) is appended at the end of its container with its
chips, logged with `log_error!`, never dropped.

## 8. Testing

Each test names, in a comment, the mutation that fails it, and the
mutation is run.

**Planner** (`structured_tree.rs`, table-driven over small documents)
- One case per row kind of §3.2 asserting the exact `Vec<TreeRow>`:
  changed, added (placed after its disk predecessor), removed (`removed`
  chip), absent, conflict with and without a base, type change, container
  chip text and its `Preview` rows when unfolded, locked (no disk chip, no
  `open`), repeated key `×2` with no rows for the earlier copies, a
  same-change row (`source · disk` chip picked, last written chip when
  the base had it), element removed, element added, a conflicting run
  with a both chip, a conflicting run of object elements (`{ n keys }` in
  the chip, one `Preview` row per element when unfolded), an identical
  container folded with its summary, the root as an array. Mutation per
  case.
- Comments: an entry's leading block becomes `Comment` rows above it,
  from source for a source entry and from disk for a disk-only one; a
  trailing comment is on the `Same` row. Mutation: drop `leading`.
- Fold identical: runs collapse to one `Fold` row with the right count;
  a run around a decision never folds; expanding one run leaves the others
  folded. Mutation: fold across a container boundary.
- `picked` follows the resolution, else `auto_pick`; an open row has no
  picked chip; `open` is false once picked and on locked rows. Mutation:
  always use `auto_pick`.
- `scroll_target` finds the row for every decision id, folded runs
  included (a fold holding the target is expanded first).
- Property over the engine's fixture corpus: every decision id appears in
  exactly one `Decision` row; every source entry appears in exactly one
  row when nothing is folded, the earlier copies of a repeated key
  excepted. Mutation: skip the last entry of a container.

**View** (`#[gpui::test]` in `views/mod.rs`, replacing the strip tests)
- JSON inputs open in Structure with one `tree-row:<id>` per decision and
  no text panes. Mutation: mode stays Lines.
- Clicking `chip:<id>:disk` sets the pick, fills the chip, clears the edge,
  and changes the assembled text. Mutation: the click moves the cursor
  only.
- Next scrolls the list to the target row (`scroll_to_item` called with
  its index). Mutation: `scroll_target` returns 0.
- Fold identical collapses the identical run and the decision rows stay.
- Disk equals source with an origin: opens in Lines with the §4 note;
  the Structure toggle shows an all-dim tree. Mutation: default Structure.
- Edit text enters the Text stage with the assembled text; Back to
  structure restores the tree with the picks (the existing tests,
  re-pointed at rows).
- Save enqueues the assembled text (existing test, unchanged).

**Gallery.** The five poses render in the smoke test and are shot and
read in dark and light; the read checks a removed row shows its value, a
conflict row shows three chips, and no text pane is visible.

**End to end.** `resolve_e2e.rs` stories X and Y are unchanged: the save
path did not move.

## 9. Out of scope

- Editing a value inline in the tree; the Text stage covers it.
- Origin as a chip; origin stays in Lines.
- ↑ / ↓ row navigation and a search box.
- Showing comments the parser does not attach to an entry (a comment
  after a blank line, or before a closing bracket); they survive in the
  output untouched.
- Recursing into array elements that are objects (first spec §9).
