# Structured tree view Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Structure mode's text panes and strips with one tree of the file in file order, a chip per side on every changed key or element, a pick per row.

**Architecture:** A pure planner in `tomte_app` (`structured_tree.rs`) projects `StructuredState` plus the session's fold state into a flat `Vec<TreeRow>`; two small `tomte_ui` builders draw a row and a chip; `merge_structured.rs` renders the rows in a `uniform_list` and `merge.rs` swaps the panes-and-result body for it in Structure mode. The engine gains a public `Site`, a `value` range on `SideText` and a side-document accessor; nothing else in core moves. The Text stage, Save, locks and the line editor are untouched.

**Tech Stack:** Rust 2024, gpui 0.2.2 (pinned), `tomte_core::structured`, `tomte_ui` components.

**Spec:** `.agents/artifacts/specs/2026-10-25-structured-tree-view-design.md` (supersedes §6 and §7 of `.agents/artifacts/specs/2026-10-21-structured-merge-design.md`; the engine sections there still apply).

## Global Constraints

- Branch `feat/structured-merge` at 9d37146; every task commits on it. Conventional subjects; the body in the user's terms; commit messages end with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_019Ly84NKVSjcQEhVk14LvJU`. Commits are SSH-signed via 1Password: if signing fails, leave the work staged and continue; never `--no-gpg-sign`, never touch git config.
- The gate before every commit, in the foreground, output read: `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings > /tmp/clippy.log 2>&1; echo "clippy exit=$?"` must print 0 and `rg -n '^(error|warning)' /tmp/clippy.log | rg -v 'block v0\.1\.6|proc-macro-error2'` must print nothing; `cargo test --workspace`. Never run it in the background.
- Load the `gpui` and `gpui-tomte` skills before touching a view, an entity update, `cx.spawn`, layout, or a `#[gpui::test]`. Never update an entity from inside its own update closure. `.truncate()` never ellipsizes: use `line_clamp(1)` in a bounded container. Verify gpui claims against `~/.cargo/registry/src/*/gpui-0.2.2/`, never the web.
- Views use Theme tokens only (no raw hex), humanized labels only; new reusable styling goes in `crates/ui` first. Any scrollable list rides `uniform_list` (fixed-height rows) or gpui's `list`.
- Every test names, in a comment, the mutation that fails it, and the mutation is run against production code. No sleeps; everything through `run_until_parked`.
- Use `fd` and `rg`, never `find` or `grep`. Screenshots are READ, never assumed. `scripts/shoot.sh` runs unsandboxed.
- Colours: disk `theme.drift`, source `theme.accent`, last written `theme.text_muted`, both `theme.ok`, the open-row edge `theme.conflict`, washes via `Theme::wash`. Row height `PANE_ROW_H` (20 px), file content in Menlo.
- Copy, verbatim: progress `N of M picked · K to decide`; `same settings, formatting differs · Save keeps the source layout`; `disk matches source · origin differs · see Lines`; `disk matches source`; fold toggle `Fold identical`; fold rows `▸ 4 identical keys` / `▸ 251 identical elements`; chip labels `source`, `disk`, `last written`, `both`, `source · disk`; missing-side words `removed`, `absent`; container chips `{ n keys }` / `[ n elements ]`; repeated key suffix `×n`.

## Review Focus

- A real settings file with hundreds of identical entries (the user's skill-lock has 251): planning rows once per pick or fold, never per frame, or the tree stutters. Pinned in Task 6 (`the_plan_is_computed_once_per_change`).
- A key that needs JSON escapes (`"a\"b"`, `"é"`): the row must show the key as written in the file, not its unescaped form. Pinned in Task 2 (`keys_show_as_written`).
- Several keys added on disk in a row: they keep disk's order, after the entry that precedes the first on disk. Pinned in Task 2 (`a_chain_of_disk_added_keys_keeps_disk_order`).
- A CRLF file: chip text and row text carry no `\r`; the plan equals the LF plan. Pinned in Task 2 (`crlf_plans_like_lf`).
- Nesting deeper than the window is wide (the parser allows 256 levels): the indent caps so the row's text stays on screen. Pinned in Task 5 (`indent_caps_at_depth_24`).

---

### Task 1: Core seam — public `Site`, `SideText.value`, `side_doc`

**Files:**
- Modify: `crates/core/src/structured/merge.rs` (`Site` ~l.176, `SideText` ~l.154, `Found::side_text` ~l.465, `StructuredDocument` impl ~l.262)
- Modify: `crates/core/src/structured/merge.rs` l.626 (`ArrayView::text`, the second `SideText {` construction)
- Test: `crates/core/src/structured/merge.rs` `#[cfg(test)]` module

**Interfaces:**
- Produces: `pub struct Site { pub container: Option<NodeId>, pub entry: Option<usize>, pub after: Option<usize>, pub elements: Range<usize> }` and `pub site: Site` on `Decision`.
- Produces: `pub struct SideText { pub range: Range<usize>, pub value: Range<usize>, pub text: String }` where `value` is the value node's byte range on that side (equal to `range` for `Change::Value` and for element rows).
- Produces: `impl StructuredDocument { pub fn side_doc(&self, side: Side) -> &jsonc::Document }`.

- [ ] **Step 1: Write the failing tests**

```rust
/// MUTATION: `side_doc` returns `ours` for every side → the Theirs text
/// assertion fails.
#[test]
fn side_doc_returns_each_sides_parsed_document() {
    let doc = StructuredDocument::compute(Some("{}"), "{\"a\":1}", "{\"a\":2}", None).unwrap();
    assert_eq!(doc.side_doc(Side::Ours).text, "{\"a\":1}");
    assert_eq!(doc.side_doc(Side::Theirs).text, "{\"a\":2}");
    assert_eq!(doc.side_doc(Side::Base).text, "{}");
}

/// MUTATION: `value` is set to `range` for every change → the Added row's
/// value slice still starts with the key.
#[test]
fn side_text_value_is_the_value_only() {
    let doc = StructuredDocument::compute(
        Some("{\"a\": 1}"), "{\"a\": 1, \"b\": {\"c\": 2}}", "{\"a\": 1}", None).unwrap();
    let d = &doc.decisions[0];               // "b" added on disk
    let ours = d.ours.as_ref().unwrap();
    assert_eq!(doc.side_doc(Side::Ours).slice(&ours.value), "{\"c\": 2}");
    assert!(ours.range.start < ours.value.start, "range covers the key too");
    assert_eq!(d.site.container, doc.side_doc(Side::Theirs).root);
    assert_eq!(d.site.entry, None);
    assert_eq!(d.site.after, Some(0));
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p tomte-core --lib structured::merge::tests::side_doc_returns_each_sides_parsed_document structured::merge::tests::side_text_value_is_the_value_only`
Expected: compile errors (`side_doc` missing, `value` missing, `site` private).

- [ ] **Step 3: Implement**

Make `Site` and its four fields `pub`, `Decision::site` `pub`, add `pub value: Range<usize>` to `SideText` (set to `self.doc.node(self.node).range` in `Found::side_text`, to `range.clone()` in `ArrayView::text`), add `side_doc` matching `Side::{Base, Ours, Theirs}` to the three documents. Keep `pub(crate)` nothing else; `lock_against` and `splice.rs` compile unchanged.

- [ ] **Step 4: Run the whole core suite**

Run: `cargo test -p tomte-core`
Expected: all green, the two new tests included.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/core/src/structured/merge.rs
git commit -m "feat(core): expose a decision's site, its value range and each side's document

The tree view places rows from Site and reads chip text from the value
range, so the view needs no second walk of the file."
```

---

### Task 2: Planner — objects, chips, comments, decision rows

**Files:**
- Create: `crates/app/src/structured_tree.rs`
- Modify: `crates/app/src/lib.rs` (add `pub mod structured_tree;`)
- Test: `crates/app/src/structured_tree.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 1's `Site`, `SideText.value`, `side_doc`; `StructuredState { doc, resolution, cursor }`, `StructuredDocument::{auto_pick, has_base}`, `Decision::{kind, what, base, ours, theirs, duplicated, both_allowed, locked}`, `jsonc::{Document, Node, NodeKind, Entry, Element}`.
- Produces (all `pub`, no gpui):

```rust
pub enum ChipSide { Disk, Source, LastWritten, Both }
impl ChipSide { pub fn pick(self) -> Pick; pub fn label(self) -> &'static str /* "disk" | "source" | "last written" | "both" */; pub fn selector(self) -> &'static str /* "disk" | "source" | "last-written" | "both" */ }
pub enum Bracket { Object, Array }
pub enum FoldWhat { Keys, Elements }
pub struct NodeKey { pub side: Side, pub node: NodeId }
pub struct FoldKey { pub container: NodeKey, pub first: usize }
pub struct Chip { pub side: ChipSide, pub text: String, pub picked: bool, pub agreed: bool /* "source · disk" */, pub container: bool, pub unfolded: bool }
pub enum TreeRow {
    Open { depth: usize, key: Option<String>, bracket: Bracket, folded: bool, summary: String, same: bool, node: NodeKey },
    Close { depth: usize, bracket: Bracket, comma: bool },
    Comment { depth: usize, text: String },
    Same { depth: usize, text: String },
    Decision { depth: usize, id: usize, key: Option<String>, chips: Vec<Chip>, open: bool, locked: bool },
    Preview { depth: usize, text: String },
    Fold { depth: usize, count: usize, what: FoldWhat, key: FoldKey },
}
#[derive(Default)]
pub struct TreeView { pub fold_identical: bool, pub folded_overrides: HashMap<NodeKey, bool>, pub unfolded_chips: HashSet<(usize, ChipSide)>, pub expanded_folds: HashSet<FoldKey> }
pub fn plan(state: &StructuredState, view: &TreeView) -> Vec<TreeRow>;
pub fn one_line(text: &str) -> String;   // whitespace runs (incl. \r\n) → one space, trimmed
```

Row rules the signature does not determine (spec §3.1–3.2), in planner order:

- Walk the source document from its root. A container's children are its source entries in order; a decision with `site.container == c` and `site.entry == Some(i)` replaces entry `i`'s row; decisions with `site.entry == None` are emitted after entry `site.after` (`None` → after the last entry), in decision-id order. Entries before the last copy of a repeated key get no rows.
- An entry with no decision and a scalar value is `Same` with text `"key": value,` plus its same-line trailing comment. An entry with no decision and a container value: `Open … same: true` (folded unless `folded_overrides` says open), children, `Close`. A container holding any decision (a decision's `site.container` is it or a descendant; build the parent map once) opens by default; `folded_overrides` can fold it.
- `Comment` rows: each line of `entry.leading` (source's for a source entry, disk's for a disk-only one), trimmed.
- Key text: the key's own bytes, `doc.slice(&entry.key_range)`, quotes included. Element rows have `key: None`.
- Chips per decision: source and disk always; last written when `has_base(id)` and `kind` is `Conflict` or `BothSame`; both when `both_allowed`. Chip text = `one_line(side_doc(side).slice(&st.value))`; a missing side reads `removed` when `d.base.is_some()`, else `absent`. A container value reads `{ n keys }` / `[ n elements ]` (`jsonc::parse` the value text; `container: true`). `BothSame` with `duplicated == None`: one chip `Source` with `agreed: true`, plus last written. `duplicated == Some(side)`: that side's text gets ` ×n`, n = copies of the key in that side's object (`Document::duplicates`).
- `picked`: the chip whose `side.pick()` equals `resolution.get(id).or(auto_pick(id))`. `open` = unlocked, `kind == Conflict`, no pick. `locked = d.locked.is_some()`; a locked row has only its source chip (`picked: true`).
- A decision whose `site` the walk cannot place (`entry` or `after` past the container's entries, or `elements` past its elements) is appended as a `Decision` row at the end of its container, after `tomte_core::log_error!` names the path; it is never dropped (spec §7).

- [ ] **Step 1: Write the failing tests** (table helper `rows(base, ours, theirs) -> Vec<TreeRow>` builds the state through `StructuredDocument::compute` and a default `TreeView`; `decision(rows, id) -> &TreeRow`)

```rust
/// MUTATION: chip text uses `st.range` instead of `st.value` → the added
/// row's disk chip starts with `"vim_mode"`.
#[test] fn a_key_added_on_disk_is_a_decision_row_with_absent_source() { /* base {"a":1}, ours {"a":1,"vim_mode":true}, theirs {"a":1}: Decision{key:Some("\"vim_mode\""), chips:[source "absent", disk "true" picked], open:false} after the Same row for a */ }
/// MUTATION: missing side always reads `absent` → fails on "removed".
#[test] fn a_key_removed_on_disk_reads_removed_and_is_picked_disk() {}
/// MUTATION: `open` ignores the resolution → stays true after a pick.
#[test] fn a_conflict_row_has_three_chips_no_pick_and_closes_once_picked() { /* base theme Solarized, ours Ayu, theirs One Dark: chips [source, disk, last written] none picked, open; after resolution.set(id, Pick::Disk): disk picked, open false */ }
/// MUTATION: agreed chip emitted for every row → the changed row shows one chip.
#[test] fn a_same_change_on_both_sides_is_one_agreed_chip_plus_last_written() {}
/// MUTATION: container chips read the raw text → "{ 2 keys }" missing.
#[test] fn a_type_change_shows_the_object_as_a_count() {}
/// MUTATION: repeated-key suffix dropped.
#[test] fn a_key_source_repeats_is_one_row_at_the_last_copy_with_x2() { /* theirs {"m":1,"m":1}, ours {"m":1}: exactly one Decision for m, source chip ends with " ×2", no Same row for the first copy */ }
/// MUTATION: lock ignored → the disk chip renders.
#[test] fn a_locked_row_has_only_its_source_chip() { /* use fixtures-like templated inputs: lex + anchor as crates/app/src/views/fixtures.rs::json_templated_inputs does */ }
/// MUTATION: comments taken from source for a disk-only entry → the disk
/// comment is missing.
#[test] fn comment_rows_come_from_the_side_that_owns_the_entry() {}
/// MUTATION: key text uses `entry.key` → the escaped key shows unescaped.
#[test] fn keys_show_as_written() { /* theirs {"a\"b": 1} changed on disk: key == "\"a\\\"b\"" */ }
/// MUTATION: added keys emitted in reverse → order flips.
#[test] fn a_chain_of_disk_added_keys_keeps_disk_order() { /* ours adds x then y after a: rows a, x, y */ }
/// MUTATION: `one_line` keeps `\r` → texts differ.
#[test] fn crlf_plans_like_lf() { /* same inputs with \r\n: plan equal to the LF plan */ }
/// MUTATION: identical container opens by default → `folded` is false.
#[test] fn an_identical_object_is_a_folded_open_row_with_a_summary() { /* Open{same:true, folded:true, summary:"2 keys"} and no children, then Close */ }
/// MUTATION: a container with a decision inside folds → the decision row vanishes.
#[test] fn a_container_holding_a_decision_opens_by_default() {}
/// MUTATION: an unplaceable site is skipped → the decision id has no row.
#[test] fn an_unplaceable_site_lands_at_the_end_of_its_container() { /* set decisions[0].site.entry = Some(99) on a computed doc: the Decision row is the container's last child */ }
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p tomte-app --lib structured_tree`
Expected: compile error, module missing.

- [ ] **Step 3: Implement `structured_tree.rs`** with the types and `plan` above; `Fold` rows, `Preview` rows, arrays and `unfolded_chips` are Tasks 3–4 (leave `Preview` and `Fold` unreachable for now, `expanded_folds`/`unfolded_chips` unused).

- [ ] **Step 4: Run to see them pass**

Run: `cargo test -p tomte-app --lib structured_tree`
Expected: all green.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/app/src/structured_tree.rs crates/app/src/lib.rs
git commit -m "feat(app): plan Structure mode as tree rows: keys, chips, comments"
```

---

### Task 3: Planner — arrays, run chips, container chips that unfold

**Files:**
- Modify: `crates/app/src/structured_tree.rs`
- Test: same file

**Interfaces:**
- Consumes: Task 2's types.
- Produces: `TreeRow::Preview` rows and `TreeView::unfolded_chips` honoured; arrays planned.

Rules:

- An array's children are its source elements; a decision with `site.container == a` whose `elements` covers source index `k` replaces that element's row (a run `k..m` is one row, the covered elements get no rows); a decision whose `elements` is the empty range at `k` goes before element `k` (`k == len` → after the last), in decision-id order. An element with no decision is `Same` with its text and comma. The root may be an array.
- A `Change::Elements` chip shows each side's run as `one_line` of the run text; container elements inside a run read as `{ n keys }` / `[ n elements ]` each. The both chip's text is the disk run then the source run joined by `, `.
- A chip with `container: true` and `(id, side) ∈ unfolded_chips` has `unfolded: true` and is followed by `Preview` rows at `depth + 1`: one per entry or element of the parsed value (`one_line` of each entry's `range`). For a run chip, one `Preview` per element of the run.

- [ ] **Step 1: Write the failing tests**

```rust
/// MUTATION: removed elements emitted after added ones → order flips.
#[test] fn removed_and_added_elements_are_rows_at_their_source_positions() { /* base ["a","b","c"], theirs same, ours ["a","c","d"]: rows Same a, Decision(b removed), Same c, Decision(d added, source absent), Close */ }
/// MUTATION: both chip joins source first → text order flips.
#[test] fn a_conflicting_run_has_source_disk_and_both_chips() { /* base ["a","z"], theirs ["a","b","z"], ours ["a","c","z"]: one Decision between a and z with chips source "\"b\"", disk "\"c\"", both "\"c\", \"b\"", open */ }
/// MUTATION: container elements in a run read raw → "{ 2 keys }" missing.
#[test] fn a_run_of_object_elements_reads_as_counts_and_unfolds_per_element() { /* unfolded_chips {(id, Disk)}: two Preview rows */ }
/// MUTATION: `unfolded` ignored → no Preview rows.
#[test] fn an_object_chip_unfolds_into_preview_rows_under_its_row() {}
/// MUTATION: root arrays walk as objects → panics/empty.
#[test] fn a_root_array_lists_its_elements() {}
```

- [ ] **Step 2: Run to see them fail** — `cargo test -p tomte-app --lib structured_tree` — Expected: the five fail.
- [ ] **Step 3: Implement** the array walk and `Preview` emission.
- [ ] **Step 4: Run to see them pass** — Expected: green.
- [ ] **Step 5: Gate and commit** — `git commit -m "feat(app): tree rows for arrays, run chips and container chips that unfold"`

---

### Task 4: Planner — fold identical, reveal, scroll target, corpus property

**Files:**
- Modify: `crates/app/src/structured_tree.rs`
- Test: same file; fixtures read from `crates/core/tests/fixtures/jsonc/*` via `include_str!`-free `std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../core/tests/fixtures/jsonc/zed-settings.jsonc"))`.

**Interfaces:**
- Produces: `pub fn scroll_target(rows: &[TreeRow], id: usize) -> Option<usize>` (the index of decision `id`'s row); `pub fn reveal(view: &mut TreeView, state: &StructuredState, id: usize)` (sets `folded_overrides[c] = false` for every container on the path to `id`, and inserts into `expanded_folds` nothing — decisions never fold); `TreeView::fold_identical` and `expanded_folds` honoured.

Rules (spec §3.3): with `fold_identical`, a maximal run of consecutive `Same`, `Comment` and `same: true` `Open`…`Close` groups among one container's children collapses to `Fold { count, what, key: FoldKey { container, first } }` where `first` is the child index of the run's first row and `count` the children folded; a `FoldKey` in `expanded_folds` is emitted as its rows instead. `Decision`, `Preview` and the `Open`/`Close` of a container holding a decision never fold.

- [ ] **Step 1: Write the failing tests**

```rust
/// MUTATION: a fold crosses a Decision → count too high, decision row gone.
#[test] fn fold_identical_collapses_runs_but_never_a_decision() { /* 251-entry object with one changed key in the middle: rows Open, Fold{count:125}, Decision, Fold{count:125}, Close */ }
/// MUTATION: expanding one fold expands all → the other Fold disappears.
#[test] fn an_expanded_fold_opens_only_its_run() {}
/// MUTATION: `scroll_target` returns the first Decision for every id.
#[test] fn scroll_target_finds_every_decisions_row() {}
/// MUTATION: `reveal` leaves the override → the row stays hidden.
#[test] fn reveal_opens_a_container_the_user_folded() {}
/// MUTATION: planner skips a container's last entry → an entry is missing.
#[test] fn every_decision_and_every_source_entry_has_exactly_one_row() { /* over each corpus file as theirs, with ours = the same text with its first scalar value changed and base = theirs; count Decision ids and Same/Decision/Open rows per source entry, repeated keys' earlier copies excepted; fold_identical off, every container opened via overrides */ }
```

- [ ] **Step 2: Run to see them fail** — Expected: five failures.
- [ ] **Step 3: Implement** fold runs, `scroll_target`, `reveal`.
- [ ] **Step 4: Run to see them pass.**
- [ ] **Step 5: Gate and commit** — `git commit -m "feat(app): fold identical runs, reveal and scroll targets for the tree"`

---

### Task 5: UI builders — `tree_row`, `value_chip`, preview

**Files:**
- Create: `crates/ui/src/components/structure_row.rs`
- Modify: `crates/ui/src/components/mod.rs` (`pub mod structure_row; pub use structure_row::*;`)
- Modify: `crates/ui/src/preview/mod.rs` (`COMPONENTS` entry `("structure-row", "tree rows: same, open conflict, picked, locked, folded")` and a `"structure-row"` arm in `render_component`)
- Test: `crates/ui/src/components/structure_row.rs` (`indent_px`), the app's `every_gallery_state_builds_and_renders` (comp states derive from `COMPONENTS`)

**Interfaces:**
- Produces:

```rust
pub const TREE_INDENT_PX: f32 = 12.;
pub const TREE_MAX_INDENT_DEPTH: usize = 24;
pub fn indent_px(depth: usize) -> f32;                       // TREE_INDENT_PX * min(depth, 24)
pub enum RowTone { Plain, Same, Changed, Open, Comment, Fold }
pub fn tree_row(theme: Theme, id: impl Into<ElementId>, depth: usize, tone: RowTone, cursor: bool, on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Stateful<Div>;
    // h(PANE_ROW_H-equivalent 20px), Menlo text_xs, pl = indent_px(depth) + 8, flex row gap_2; Plain in theme.text; Comment/Same/Fold text in text_muted (Fold italic);
    // Changed/Open: bg wash(text, 0.05); Open: border_l_2 in theme.conflict; cursor: bg wash(text, 0.08) (the list_row selected wash)
pub fn value_chip(theme: Theme, id: impl Into<ElementId>, tint: Rgba, label: &'static str, text: SharedString, picked: bool, prefix: Option<&'static str>, on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Stateful<Div>;
    // rounded_sm, border_1 border_color(tint), px_1p5, max_w(px(360)), label text_xs text_muted then the text line_clamp(1);
    // picked: bg wash(tint, 0.18) + "● " before the text; prefix ("▸" / "▾" for a container chip) before the text; hover wash(tint, 0.08); tooltip = full text via TextTooltip when text is longer than 48 chars
pub fn lock_mark(theme: Theme, reason: SharedString) -> Div; // "🔒" in text_muted with TextTooltip(reason)
```

- [ ] **Step 1: Write the failing test**

```rust
/// MUTATION: `indent_px` has no cap → 30 * 12 != 24 * 12.
#[test] fn indent_caps_at_depth_24() { assert_eq!(indent_px(0), 0.); assert_eq!(indent_px(3), 36.); assert_eq!(indent_px(30), 288.); }
```

- [ ] **Step 2: Run** `cargo test -p tomte-ui indent_caps` — Expected: compile error.
- [ ] **Step 3: Implement** the builders and the preview arm (variants: a Same row, an open conflict row with three chips, a picked changed row, a locked row with `lock_mark`, a Fold row, a long value clamped).
- [ ] **Step 4: Run** `cargo test -p tomte-ui` and `cargo test -p tomte-app --bin tomte every_gallery_state_builds_and_renders` — Expected: green.
- [ ] **Step 5: Shoot and read** `./scripts/shoot.sh comp:structure-row dark` and `… light`; read both PNGs: chips show label then value, the picked chip is filled, the open row has a left edge, the long value ends inside its chip.
- [ ] **Step 6: Gate and commit** — `git commit -m "feat(ui): tree row and value chip builders for Structure mode"`

---

### Task 6: View — the tree replaces the panes and strips in Structure mode

**Files:**
- Modify: `crates/app/src/views/merge_structured.rs` (delete `StripKind`, `StructuredStrip`, `shows_both_sides`, `whole_lines`, `choice_kind`, `strip_specs`, `detail_words`, `text_row`, `result_decors`, `pane_decors`, `structured_pane_decors`, `jump_lines`, `strip_block`, `pane_side`, `side_piece`, `line_of` and their tests; add `render_tree`, `chip_tint`, `tree_rows`)
- Modify: `crates/app/src/views/merge.rs` (`LoadedMerge`, `with_structured`, `apply_pick`, `revisit`, `start_edit`, `back_to_structure`, `jump_to`, `install_result_decorator`, `render_inner`'s pane-editor build and focus rule, `body`, the `structured_blocks` seam, `undo`/`redo`)
- Modify: `crates/ui/src/components/merge_region.rs` (drop `StripState::Locked`, the `label`, `detail` and `selector_prefix` parameters of `decision_strip`; the line editor passes `None` for all three at `merge.rs` ~l.3443) and its preview variant
- Test: `crates/app/src/views/mod.rs` (structured tests ~l.6463–7260)

**Interfaces:**
- Consumes: Tasks 2–5.
- Produces, in `merge.rs`:

```rust
pub(super) struct LoadedMerge { …, pub(super) tree: TreeView, pub(super) tree_rows: Rc<Vec<TreeRow>>, pub(super) tree_dirty: bool, #[cfg(test)] pub(super) plan_runs: usize }
impl LoadedMerge { pub(super) fn replan(&mut self) /* when tree_dirty: tree_rows = Rc::new(plan(..)); tree_dirty = false; plan_runs += 1 */ }
impl MergeView {
    pub(super) tree_scroll: gpui::UniformListScrollHandle,
    #[cfg(test)] pub(super) last_scroll_row: Option<usize>,
    pub(super) fn scroll_to_decision(&mut self, id: usize) // reveal + replan + scroll_target → tree_scroll.scroll_to_item(ix, ScrollStrategy::Center); records last_scroll_row
    pub(super) fn toggle_container(&mut self, node: NodeKey, cx)   // flips folded_overrides, tree_dirty, notify
    pub(super) fn toggle_chip(&mut self, id: usize, side: ChipSide, cx)
    pub(super) fn expand_fold(&mut self, key: FoldKey, cx)
    #[cfg(test)] pub(super) fn tree_rows_for_test(&self) -> Vec<TreeRow>
}
```

- Produces, in `merge_structured.rs`: `pub(super) fn render_tree(view: &MergeView, loaded: &LoadedMerge, theme: Theme, cx: &mut Context<MergeView>) -> AnyElement` (a `uniform_list("merge-tree", rows.len(), …)` with `.track_scroll(view.tree_scroll.clone())`, `flex_1().min_h_0()`, Menlo `text_xs`; the closure clones the `Rc<Vec<TreeRow>>` and a `WeakEntity<MergeView>`); `pub(super) fn chip_tint(theme: Theme, side: ChipSide) -> Rgba`.

Behaviour to wire (spec §3.2, §3.5, §5):

- Row elements: `Decision` → `tree_row` id `("tree-row", id)` with `debug_selector` `tree-row:<id>`, tone `Open` when open else `Changed`, `cursor` when `state.cursor == Some(id)`, click → cursor; children: the key (when `Some`) then one `value_chip` per chip, id `("chip", id, side)` and selector `chip:<id>:<side.selector()>`, label `"source · disk"` when `agreed`, prefix `Some("▾")`/`Some("▸")` for a container chip, click → `toggle_chip` when `container` else `apply_pick(id, side.pick())`; a locked row draws its source chip then `lock_mark(lock_reason(..))`. `Open` → tone `Same` when `same` else `Plain`, text `▾`/`▸` + key + bracket (+ summary when folded), click → `toggle_container`. `Close` → `Plain`; `Same`, `Comment`, `Preview` → inert rows at tone `Same`/`Comment`/`Same`. `Fold` → tone `Fold`, `▸ n identical keys|elements`, click → `expand_fold`.
- `body`: when `loaded.mode == Structure` and `!self.editing`, return the header's banner-free column with `render_tree` only (no pane row, no dividers, no result pane). When in the Text stage (`editing`), return the result pane alone, full height. Lines mode is unchanged.
- `render_inner`: build pane editors only when `loaded.mode == Lines` (the Structure decors branch goes). Focus: when the result editor is rendered (Lines, or the Text stage) focus it as today; otherwise focus `self.focus_handle` so 1/2/3/b/n reach the `MergeEditor` context.
- `install_result_decorator`: Structure mode installs an empty decoration (no blocks, no lines); the `structured_blocks` seam goes. `seed_editor`'s Structure branch stays (the Text stage shows the seed).
- `apply_pick`, `undo`, `redo` (structured entries), `set_mode`, `back_to_structure`, `start_edit`: set `loaded.tree_dirty = true`; `apply_pick` and `revisit` call `scroll_to_decision` instead of `jump_to`; `jump_to` and `sync_from`'s Structure early-return go (the length check covers the Text stage). `start_edit`'s Structure branch keeps the caret placement and drops `jump_to`.
- `with_structured`: `tree: TreeView::default()`, `tree_rows: Rc::new(Vec::new())`, `tree_dirty: true`; `render_inner` calls `loaded.replan()` before `body`.

- [ ] **Step 1: Rewrite the failing view tests** (replace the strip tests; keep `structured_merge_window`, `result_text`, `stage_of`, `type_into_result`; `click_strip_edit` becomes `click_header_edit` on selector `merge-edit`)

```rust
/// MUTATION: `body` still renders the pane row in Structure mode → a
/// `merge-pane-divider` bound exists.
#[gpui::test] fn json_inputs_open_in_structure_mode_as_a_tree_with_no_panes(cx) { /* mode Structure; for each decision id `tree-row:<id>` has bounds; `merge-pane-divider` and `merge-result-scroll` have none; the first Decision row's key is "\"theme\"" */ }
/// MUTATION: the chip click handler moves the cursor only → the assembled
/// text still reads "Solarized".
#[gpui::test] fn a_chip_click_picks_that_side_and_changes_the_assembled_text(cx) { /* click `chip:0:disk`; state.assembled() contains "Gruvbox"; the row's disk chip is picked and the row is not open; cmd-z restores */ }
/// MUTATION: `open` chips drawn with a pick → the conflict row has a picked chip.
#[gpui::test] fn an_open_conflict_row_shows_three_chips_and_no_pick(cx) { /* chip:0:source, chip:0:disk, chip:0:last-written all render; none picked in tree_rows_for_test */ }
/// MUTATION: focus stays on the unrendered result editor → "1" never
/// reaches the MergeEditor context and the pick is missing.
#[gpui::test] fn pick_keys_route_to_the_structured_cursor(cx) { /* existing test, kept; must pass with the new focus rule */ }
/// MUTATION: `replan` ignores `tree_dirty` and plans every render → plan_runs grows across two idle frames.
#[gpui::test] fn the_plan_is_computed_once_per_change(cx) { /* run_until_parked twice: plan_runs == 1; apply_pick: == 2 */ }
/// MUTATION: `toggle_container` never flips the override → the row count is unchanged.
#[gpui::test] fn clicking_an_identical_container_row_folds_and_unfolds_it(cx) {}
/// MUTATION: `back_to_structure` leaves `editing` true → the tree is absent after the round trip.
#[gpui::test] fn edit_then_back_to_structure_restores_the_tree_and_keeps_the_picks(cx) { /* click merge-edit → stage Text, `merge-result-scroll` renders, no `tree-row:0`; click merge-back-to-structure → `tree-row:0` renders, pick survives */ }
```

Keep, unchanged: `save_in_structure_mode_enqueues_the_assembled_json`, `a_structured_save_the_write_back_refuses_is_a_message_box`, `back_to_structure_arms_when_the_text_changed` (re-pointed at `click_header_edit`). Delete: `json_inputs_open_in_structure_mode_with_one_strip_per_decision`, `a_strip_pick_changes_the_result_text`, `an_unpicked_structured_conflict_shows_disk_and_source_rows`, `next_in_structure_mode_moves_every_pane_to_the_decision` (Task 7 replaces it), `back_to_structure_restores_the_strips_and_keeps_the_picks`.

- [ ] **Step 2: Run** `cargo test -p tomte-app --bin tomte structured` — Expected: the new tests fail to compile or fail.
- [ ] **Step 3: Implement** the view changes listed above and the `merge_region.rs` cleanup.
- [ ] **Step 4: Run** `cargo test -p tomte-app --bin tomte` — Expected: green, the render-smoke test included (the three existing Structure poses still build).
- [ ] **Step 5: Shoot and read** `./scripts/shoot.sh merge-structured dark`: the tree, no panes, chips on the changed rows, the conflict row edged.
- [ ] **Step 6: Gate and commit** — `git commit -m "feat(merge): Structure mode is a tree of the file with a chip per side"` with a body naming what went (strips, tinted panes) and why (the user never saw structure on screen).

---

### Task 7: Header — progress words, Fold identical, Next scrolls the tree

**Files:**
- Modify: `crates/app/src/views/merge_structured.rs` (`progress_text`, `header_controls`)
- Modify: `crates/app/src/views/merge.rs` (`next`)
- Test: `crates/app/src/views/merge_structured.rs` tests; `crates/app/src/views/mod.rs`

**Interfaces:**
- Consumes: Task 6's `scroll_to_decision`, `last_scroll_row`, `LoadedMerge.tree`.
- Produces: `pub(super) fn progress_text(state: &StructuredState) -> String` returning `"{n} of {m} picked"` plus `" · {k} to decide"` when k > 0, where m = `state.changed().len()`, k = `state.unresolved().len()`, n = m − k; `header_controls` gains `fold_identical: bool` and draws `ui::button(theme, "merge-fold-identical", "Fold identical", Tonal(accent) when on else Ghost, Sm, → view.toggle_fold_identical(cx))` with selector `merge-fold-identical`; `MergeView::toggle_fold_identical(&mut self, cx)` flips the flag, marks `tree_dirty`, and keeps the viewport: it reads `tree_scroll.logical_scroll_top_index()` before replanning and, when that row is not a `Decision` row and folding removed it, calls `scroll_to_item(ix_of_the_fold_row_that_absorbed_it, ScrollStrategy::Top)` (spec §3.6); the progress text colour is `theme.conflict` when k > 0 else `theme.ok`.

- [ ] **Step 1: Write the failing tests**

```rust
/// MUTATION: m counts conflicts only → "0 of 1 picked" on json_inputs.
#[test] fn progress_text_counts_every_unlocked_decision() { /* json_inputs-shaped state: "4 of 5 picked · 1 to decide"; after picking: "5 of 5 picked" */ }
/// MUTATION: `next` keeps calling the removed pane jump → last_scroll_row stays None.
#[gpui::test] fn next_scrolls_the_tree_to_the_target_row(cx) { /* next(): last_scroll_row == scroll_target(rows, cursor) for four presses */ }
/// MUTATION: the toggle flips `fold_identical` but not `tree_dirty` → rows unchanged.
#[gpui::test] fn fold_identical_collapses_identical_runs_and_keeps_decisions(cx) { /* click merge-fold-identical: a Fold row exists, every tree-row:<id> still renders */ }
```

- [ ] **Step 2: Run** `cargo test -p tomte-app --bin tomte progress_text next_scrolls fold_identical` — Expected: fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p tomte-app --bin tomte` — Expected: green.
- [ ] **Step 5: Gate and commit** — `git commit -m "feat(merge): tree header counts picks, folds identical runs, Next scrolls the tree"`

---

### Task 8: Lines by default when disk equals source

**Files:**
- Modify: `crates/app/src/views/merge.rs` (`with_structured`)
- Modify: `crates/app/src/views/merge_structured.rs` (`same_text_note`, `header_controls`, `progress_text` call site)
- Modify: `crates/app/src/views/fixtures.rs` (`json_same_inputs`)
- Test: `crates/app/src/views/mod.rs`

**Interfaces:**
- Produces: `pub(super) fn same_text_note(inputs: &MergeInputs) -> Option<&'static str>`: `Some("disk matches source · origin differs · see Lines")` when `inputs.ours == inputs.theirs && inputs.origin.is_some()`, `Some("disk matches source")` when equal without origin, else `None`. `with_structured` picks `Lines` when `same_text_note(&inputs).is_some()`, else Structure when `structured.is_some()`. The line editor's header shows the note as a `ui::chip(.., Wash(theme.text_muted))` with selector `merge-same-note`; `header_controls` gains `note: Option<&'static str>` and, when `Some`, draws it in place of the progress text (same selector). `pub(super) fn json_same_inputs() -> MergeInputs`: `json_inputs()` with `ours = theirs`, `base = Some(theirs)`, `origin = Some(theirs with "font_size": 14 → 16)`.

- [ ] **Step 1: Write the failing test**

```rust
/// MUTATION: `with_structured` keeps Structure as the default → mode is Structure.
/// MUTATION: the note is built from `ours == base` → no note (origin case).
#[gpui::test] fn disk_equal_to_source_opens_in_lines_with_the_note(cx) { /* json_same_inputs: mode Lines, `merge-same-note` renders with the origin text, `merge-mode-structure` renders; click it: Structure, no open rows in tree_rows_for_test, Save enabled */ }
```

- [ ] **Step 2: Run** — Expected: fail on mode.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p tomte-app --bin tomte` — Expected: green.
- [ ] **Step 5: Gate and commit** — `git commit -m "fix(merge): a file whose disk text equals its source opens in Lines and says so"`

---

### Task 9: Gallery poses and screenshots

**Files:**
- Modify: `crates/app/src/views/fixtures.rs` (`STATES`, `build`, new `json_tree_inputs`)
- Test: `every_gallery_state_builds_and_renders`; the shots.

**Interfaces:**
- Produces: `pub(super) fn json_tree_inputs() -> MergeInputs` on `~/.claude/settings.json` with base/disk/source texts that produce, through the engine: `editor.formatOnSave` same change on both sides (base `true`, both `false`), `theme` conflict (base `"Solarized"`, source `"One Dark"`, disk `"Ayu Dark"`), `font_size` changed on disk, `tab_size` removed on disk, `vim_mode` added on disk, `telemetry` type change (source `"auto"`, disk `{"level": "off", "share": false}`), `permissions` object added on disk with three keys, `model` repeated in source, `lastSelectedAgents` with `antigravity` removed on disk and a conflicting run (source inserts `"gemini"`, disk inserts `"cursor"` at the same place), `hooks` an array of objects where source and disk append different objects, a `// editor` comment above the first key, and an identical `keybindings` object of two keys. STATES: `merge-structured` → `json_tree_inputs()` (description: `JSON settings as a tree: every row kind, a conflict row edged`), new `merge-structured-folded` (same inputs, `tree.fold_identical = true`), new `merge-structured-same` (`json_same_inputs()`: `disk equals source, origin differs: Lines with the note`); `merge-structured-locked` and `merge-structured-text` keep their inputs.

- [ ] **Step 1: Add the inputs and poses; run** `cargo test -p tomte-app --bin tomte every_gallery_state_builds_and_renders` — Expected: green (MUTATION: a pose name listed without a `build` arm → the smoke test panics `fixture missing`).
- [ ] **Step 2: Shoot** `./scripts/shoot.sh merge-structured dark`, `… light`, `merge-structured-folded dark`, `merge-structured-locked dark`, `merge-structured-same dark`, `merge-structured-text dark`.
- [ ] **Step 3: Read every PNG** and check: a removed row shows its value in the source chip and `removed` in the disk chip; the theme row has three chips and a red edge; no text pane in the tree poses; the folded pose shows `▸ … identical keys` rows and still every changed row; the locked pose shows the lock mark and no disk chip on `email`; the same pose is the line editor with the note; the chips' labels read `source`, `disk`, `last written`, `both`, `source · disk`. Fix and reshoot anything that reads wrong; a shot that fails to capture is a failed step.
- [ ] **Step 4: Gate and commit** — `git commit -m "feat(gallery): tree poses for Structure mode, folded, locked and disk-equals-source"`

---

### Task 10: Documentation

**Files:**
- Modify: `AGENTS.md` (workspace map line `merge_structured = …`, the gallery pose paragraph, the "On top of that, this branch adds Structure mode" paragraph, the `merge.rs` open thread)
- Modify: `CHANGELOG.md` (`## Unreleased` → the Structure mode bullet describes the tree)
- Modify: `.agents/artifacts/specs/2026-10-21-structured-merge-design.md` (one line under the status block: `§6 "Structure stage" and §7 "Nothing to decide" are superseded by 2026-10-25-structured-tree-view-design.md.`)
- Modify: `.agents/artifacts/plans/2026-10-21-structured-merge.md` (same one-line pointer under its header)

- [ ] **Step 1: Edit** the four files: AGENTS.md says `merge_structured = Structure mode's tree and header`, lists the five poses with what they show, and describes Structure mode as the tree (chips per side, pre-picked rows, fold toggle, Lines by default when disk equals source); CHANGELOG's bullet says the file shows as a tree with a chip per side and a pick per row.
- [ ] **Step 2: Verify** `rg -n "strip" AGENTS.md CHANGELOG.md` mentions no Structure-mode strips.
- [ ] **Step 3: Gate and commit** — `git commit -m "docs: Structure mode is a tree; poses and specs point at the tree-view design"`
