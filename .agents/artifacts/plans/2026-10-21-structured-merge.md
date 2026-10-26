# Structured Merge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

The Structure stage's view was replaced by 2026-10-25-structured-tree-view.md (plan) per 2026-10-25-structured-tree-view-design.md (spec); the engine tasks here still stand.

**Goal:** JSON/JSONC settings files merge per key and per array element in the merge editor, with commas, indents and comments handled by the engine, templated entries locked to source, and a Text stage to refine the assembled result before saving.

**Architecture:** A new `tomte_core::structured` module: a lossless, iterative JSONC parser (`jsonc.rs`), a differ that walks base/disk/source trees into a flat list of decisions and reuses the line engine's clustering for arrays (`merge.rs`), and a splicer that rebuilds only the containers that changed, byte-identical elsewhere (`splice.rs`). The app gains a pure `structured_state.rs` beside `merge_state.rs` and a `views/merge_structured.rs` that renders the Structure stage inside the existing result editor; Save enqueues the same `Decision::Merged` as today.

**Tech Stack:** Rust 2024, imara-diff 0.2 (already a dependency), serde_json (tests only), GPUI 0.2.2 pinned, the existing `tomte_ui::TextArea` blocks, `tomte_core::testsupport::Scratch` and real chezmoi for the write-back and end-to-end tests, `scripts/shoot.sh` for screenshots.

**Spec:** `.agents/artifacts/specs/2026-10-21-structured-merge-design.md`

## Global Constraints

- No new crate dependency: the JSONC parser is hand-written (spec §2.6).
- The parser is iterative; nesting depth never grows the Rust stack (spec §5.1).
- Equality is by value, never by text (spec §5.2): strings unescaped, numbers numeric with integers beyond 15 significant digits compared by text, objects regardless of key order, arrays in order.
- Untouched bytes of the result are identical to source; line endings and BOM follow source (spec §5.4).
- The lock rule is at line granularity over `SpanOrigin::Literal { repeated: false }` spans (spec §5.3); without a span map nothing is locked.
- Two-way (no base) sets base := source so every difference is OursOnly and auto-resolves to disk (spec §5.2), the line editor's rule.
- Save from either stage enqueues `Decision::Merged { resolved, inputs }`; the engine, journal (`merge`), banner and undo do not change (spec §4, §6).
- Origin is not offered in Structure mode (spec §6).
- Copy is fixed by the spec and used verbatim: "added on disk", "removed on disk", "changed in source", "conflict", "appears twice on disk · last value used", "type changed", "templated · follows source", "not valid JSON · line N", "disk is not valid JSON (line N): line merge", "disk and source are different JSON shapes: line merge", "same settings, formatting differs · Save keeps the source layout", "origin differs · see Lines".
- Views use Theme tokens only, humanized labels only; new reusable styling goes in `crates/ui` first (AGENTS.md).
- GPUI: never update an entity from inside its own update closure; hand-offs to the Shell go through `cx.spawn` then `shell.update` (AGENTS.md).
- Every new test names, in a comment, the mutation that fails it, and the implementer RUNS that mutation against production code (edit, run, revert from a `cp` backup, never `git checkout --` on uncommitted work).
- The gate before every commit, in the foreground, and read: `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings > /tmp/clippy.log 2>&1; echo "clippy exit=$?"` (must print 0) and `rg -n '^(error|warning)' /tmp/clippy.log | rg -v 'block v0\.1\.6|proc-macro-error2'` (must print nothing); `cargo test --workspace`; `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null cargo test --workspace`. Never in the background.
- Commits are SSH-signed via 1Password; on failure leave the work staged and report, never `--no-gpg-sign`, never touch git config. Conventional subjects. Every commit message ends with the two trailer lines `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_019Ly84NKVSjcQEhVk14LvJU`.
- `~/.config` and the user's dotfiles are chezmoi-managed: never edit them. Tests use `Scratch` and temp dirs only. Use `fd` not `find`, `rg` not `grep`.
- Screenshots are read, not assumed: `./scripts/shoot.sh <pose> dark` unsandboxed, then open the PNG.

## Review Focus

Inputs the spec implies but did not spell out; each has its test in the task that owns the code.

1. **Inline containers** (a minified or one-line `{"a": 1, "b": 2}`): glue must be `, ` and a removal must take only the entry and one separator, never "its line", or the whole file vanishes — Task 5, `inline_container_glue_is_comma_space_and_removal_keeps_the_line`.
2. **Keys that contain dots** (VS Code's `"editor.fontSize"` is ONE key): the path must not read like nesting — Task 4, `paths_quote_keys_that_would_read_as_nesting`.
3. **Delete on disk, modify in source** of the same key: a conflict whose disk side is absent; Disk removes, Source keeps the new value, Both is not offered — Task 4, `delete_versus_modify_is_a_conflict_without_both`.
4. **Empty file on disk** against a full source: every entry source-only, auto source; picking Disk on all of them leaves `{}` with the braces, never an empty file — Task 5, `removing_every_entry_leaves_the_braces`.
5. **The same key added on both sides with different values**: a conflict with no base entry, so no last-written button — Task 4, `a_key_added_on_both_sides_with_different_values_conflicts_without_last_written`.

---

### Task 1: Lossless JSONC parser

**Files:**
- Create: `crates/core/src/structured/mod.rs`, `crates/core/src/structured/jsonc.rs`
- Modify: `crates/core/src/lib.rs` (add `pub mod structured;` after `pub mod scanner;`)
- Test: unit tests at the end of `jsonc.rs`

**Interfaces:**
- Produces, in `tomte_core::structured::jsonc`:
  ```rust
  pub type NodeId = usize;
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct ParseError { pub offset: usize, pub line: usize, pub message: String }  // Display: "{message} at line {line}"
  #[derive(Debug)]
  pub struct Document {
      pub text: String,
      pub nodes: Vec<Node>,
      pub root: Option<NodeId>,          // None = empty document (only whitespace/comments)
      pub bom: bool,
      pub crlf: bool,                     // the first line break is "\r\n"
      pub duplicates: Vec<Duplicate>,
  }
  #[derive(Debug, Clone)]
  pub struct Node { pub range: Range<usize>, pub kind: NodeKind }
  #[derive(Debug, Clone)]
  pub enum NodeKind {
      Object { entries: Vec<Entry> },
      Array { elements: Vec<Element> },
      String(String),                     // unescaped
      Number(String),                     // raw literal
      Bool(bool),
      Null,
  }
  #[derive(Debug, Clone)]
  pub struct Entry {
      pub key: String,                    // unescaped
      pub key_range: Range<usize>,        // quotes included
      pub value: NodeId,
      pub range: Range<usize>,            // key_range.start .. end of value, or of the comma if any
      pub comma: Option<Range<usize>>,
      pub leading: Range<usize>,          // the contiguous comment lines directly above; empty (at range.start) if none
      pub trailing_comment: Option<Range<usize>>,  // a same-line comment after the value/comma
  }
  #[derive(Debug, Clone)]
  pub struct Element { pub value: NodeId, pub range: Range<usize>, pub comma: Option<Range<usize>>, pub leading: Range<usize>, pub trailing_comment: Option<Range<usize>> }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct Duplicate { pub object: NodeId, pub key: String, pub entries: Vec<usize> }  // indices into that object's entries, in order
  pub fn parse(text: &str) -> Result<Document, ParseError>;
  impl Document {
      pub fn is_empty(&self) -> bool;          // root.is_none()
      pub fn node(&self, id: NodeId) -> &Node;
      pub fn slice(&self, range: &Range<usize>) -> &str;
      pub fn line_of(&self, offset: usize) -> usize;   // 1-based
  }
  ```
- `mod.rs` declares `pub mod jsonc;` only in this task; Tasks 4 and 5 add the rest.

- [ ] **Step 1: Write the failing tests** at the end of `jsonc.rs` in `#[cfg(test)] mod tests`:

```rust
// Mutation: drop a comment's range from the tiling → fails.
#[test] fn ranges_tile_the_input_byte_for_byte()
//   input: "{\n  // lead\n  \"a\": 1, // trail\n  \"b\": [1, 2,],\n}\n"
//   collect every entry.leading, key_range, value node range, comma, trailing_comment,
//   element ranges and the whitespace between them; assert the sorted union
//   covers 0..text.len() with no gaps and no overlaps, and that
//   doc.slice(&entry.leading) == "// lead\n" for entry "a".

#[test] fn trailing_commas_and_both_comment_styles_parse()
//   "{ \"a\": 1, /* c */ \"b\": 2, }" → two entries; entries[1].comma == Some(..); root range == 0..len

#[test] fn strings_are_unescaped_numbers_kept_raw()
//   "{\"caf\\u00e9\": \"a\\nb\", \"n\": 1.50e2}" → key "café", String("a\nb"), Number("1.50e2")

// Mutation: swallow a parse error into an empty document → fails.
#[test] fn errors_carry_offset_and_line()
//   "{\n  \"a\": \n}" → Err(ParseError { line: 3, .. }); "{\"a\": 1" → Err(line 1); "tru" → Err

#[test] fn duplicate_keys_are_kept_and_listed()
//   "{\"a\": 1, \"a\": 2}" → entries.len() == 2; duplicates == [Duplicate { key: "a", entries: [0, 1], .. }]

#[test] fn empty_and_comment_only_documents_have_no_root()
//   "", "  \n", "// nothing\n" → Ok(doc) with doc.is_empty()

#[test] fn bom_and_crlf_are_recorded()
//   "\u{feff}{\r\n\"a\": 1\r\n}" → bom == true, crlf == true, root range starts at 3

// Mutation: recursive descent → stack overflow at 10_000 levels.
#[test] fn ten_thousand_nested_arrays_parse()
//   "[".repeat(10_000) + &"]".repeat(10_000) → Ok

#[test] fn single_quotes_and_unquoted_keys_are_errors()
//   "{a: 1}" → Err; "{'a': 1}" → Err
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p tomte-core --lib structured::jsonc`
Expected: compile error (module missing) or failures.

- [ ] **Step 3: Implement `parse` in `jsonc.rs`**

A tokenizer over bytes (whitespace, comments, punctuation, strings with JSON escapes, numbers per the JSON grammar, `true`/`false`/`null`) feeding an explicit-stack builder: a stack of open containers each holding the entries or elements collected so far plus the pending leading-trivia range and the pending key. Comment lines directly above an entry (no blank line between) form `leading`; a comment on the same line after the value or comma is `trailing_comment`. Line 1-based from counting `\n` before the offset.

- [ ] **Step 4: Run the tests until they pass**

Run: `cargo test -p tomte-core --lib structured::jsonc`
Expected: all PASS.

- [ ] **Step 5: Run the two named mutations, confirm the failures, revert from backups**

- [ ] **Step 6: Gate and commit**

```bash
git add crates/core/src/lib.rs crates/core/src/structured
git commit -m "feat(core): lossless iterative JSONC parser for the structured merge"
```

---

### Task 2: Value equality and the fixture corpus

**Files:**
- Modify: `crates/core/src/structured/jsonc.rs`
- Create: `crates/core/tests/fixtures/jsonc/zed-settings.jsonc`, `vscode-settings.jsonc`, `claude-settings.json`, `opencode.jsonc`, `skill-lock.json` (hand-written, representative: comments and trailing commas in the two JSONC editor files, nested objects, arrays of strings and of objects, a dotted VS Code key, a `$schema` key)
- Create: `crates/core/tests/jsonc_corpus.rs`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq)]
  pub enum Num { Float(f64), Text(String) }   // Float when the literal has ≤ 15 significant digits, else Text(raw)
  #[derive(Debug, Clone, PartialEq)]
  pub enum Value { Null, Bool(bool), Number(Num), String(String), Array(Vec<Value>), Object(BTreeMap<String, Value>) }  // Object: last duplicate wins
  impl Document { pub fn value(&self, id: NodeId) -> Value; }
  pub fn canonical(value: &Value) -> String;   // deterministic text, used as the array element token (Task 6)
  ```

- [ ] **Step 1: Write the failing tests**

In `jsonc.rs` tests:
```rust
#[test] fn values_compare_by_content_not_text()
//   value("{\"b\":1,\"a\":[1.0, \"x\"]}") == value("{ \"a\" : [ 1 , \"x\" ] , \"b\" : 1.00 }")
//   value("\"caf\\u00e9\"") == value("\"café\"")
//   value("12345678901234567890") != value("12345678901234567891")   // Text comparison
//   value("1e2") == value("100")
#[test] fn object_value_takes_the_last_duplicate()
//   value("{\"a\":1,\"a\":2}") == Object{a: 2}
#[test] fn canonical_is_stable_across_formatting()
//   canonical(value(A)) == canonical(value(B)) for the first pair above
```

In `crates/core/tests/jsonc_corpus.rs` (same `fixture` helper shape as `template_roundtrip.rs`, directory `tests/fixtures/jsonc`):
```rust
// Mutation: drop a comment's range from the tiling → fails on the two JSONC files.
#[test] fn every_corpus_file_parses_and_tiles()
//   for each file: parse ok; ranges tile 0..len — a `tiles(&Document) -> bool` helper of about 15 lines
//   written in this integration test (Task 1's unit-test helper is cfg(test) and not reachable here).
#[test] fn corpus_values_round_trip_through_serde_json()
//   for the two strict .json files: serde_json::from_str::<serde_json::Value>(text) succeeds and
//   a `to_serde(&Value)` helper in the test produces an equal serde value.
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tomte-core --lib structured::jsonc && cargo test -p tomte-core --test jsonc_corpus`

- [ ] **Step 3: Implement `Document::value`, `Num`, `Value`, `canonical`**

`canonical`: `null`, `true`/`false`, numbers as `Float` → `format!("{}", f)` and `Text` → raw, strings JSON-escaped, arrays `[a,b]`, objects `{"k":v,...}` with keys sorted.

- [ ] **Step 4: Run to verify pass**

- [ ] **Step 5: Gate and commit**

```bash
git add crates/core/src/structured/jsonc.rs crates/core/tests/jsonc_corpus.rs crates/core/tests/fixtures/jsonc
git commit -m "feat(core): JSONC values compare by content; settings corpus parses and tiles"
```

---

### Task 3: Factor `three_way` out of the line engine

**Files:**
- Modify: `crates/core/src/merge.rs` (lines 55–130: `side_hunks`, `touches`, `Cluster`, `clusters`, `map_pos`; `MergeDocument::compute` lines 140–230)

**Interfaces:**
- Produces:
  ```rust
  /// Three-way region clustering over any token sequence (lines today, array
  /// elements for the structured merge). Regions partition each side in order.
  pub fn three_way<T: Hash + Eq>(base: &[T], ours: &[T], theirs: &[T], same: impl Fn(&[T], &[T]) -> bool) -> Vec<MergeRegion>;
  ```
  `MergeDocument::compute` becomes `three_way(&b, &o, &t, |a, b| eq_lines_opts(a, b, opts))` plus the owning of lines. `SliceTokens` generalizes to `SliceTokens<'a, T>(pub &'a [T])` with `T: Hash + Eq + Clone`; `split_lines` and `line_hunks` unchanged.

- [ ] **Step 1: Write the failing test** in `merge.rs` tests:

```rust
// Mutation: three_way ignores `theirs` → the TheirsOnly region disappears.
#[test] fn three_way_over_integers_matches_the_line_engine_shapes()
//   base [1,2,3,4,5], ours [1,9,3,4,5], theirs [1,2,3,8,5] →
//   kinds [Unchanged, OursOnly, Unchanged, TheirsOnly, Unchanged]; regions[1].ours == 1..2
//   base [1], ours [2], theirs [3] → [Conflict]; ours [2], theirs [2] → [BothSame]
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p tomte-core --lib merge::`

- [ ] **Step 3: Implement `three_way`**, make `compute` call it. `imara_diff::TokenSource` needs `Token: Hash + Eq + Clone`; implement for `SliceTokens<'a, T>` returning `&'a T`.

- [ ] **Step 4: Run the whole core suite** — `cargo test -p tomte-core`. Every existing merge, writeback and template test stays green (the factoring must not change a single region).

- [ ] **Step 5: Gate and commit**

```bash
git add crates/core/src/merge.rs
git commit -m "refactor(core): factor three_way clustering out of the line merge"
```

---

### Task 4: Decisions over objects (`StructuredDocument::compute`, no arrays, no locking)

**Files:**
- Create: `crates/core/src/structured/merge.rs`
- Modify: `crates/core/src/structured/mod.rs`
- Test: unit tests at the end of `structured/merge.rs`

**Interfaces:**
- Produces, in `tomte_core::structured` (re-exported from `mod.rs`):
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum Format { Jsonc }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum NotStructured {
      Parse { side: Side, line: usize },   // "disk is not valid JSON (line N): line merge"
      ShapeMismatch,                       // roots differ in type
      ScalarRoot,
      BothEmpty,
  }
  pub fn detect(ours: &str, theirs: &str) -> Result<Format, NotStructured>;
  /// Whether a note about `NotStructured` is worth showing (spec §4): a .json/.jsonc
  /// target, or one side that did parse as an object or array.
  pub fn looked_like_json(target: &Path, ours: &str, theirs: &str) -> bool;

  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum Side { Base, Ours, Theirs }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct SideText { pub range: Range<usize>, pub text: String }   // entry range for Added/Removed/TypeChanged/Elements, value range for Value
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum Change { Value, Added, Removed, TypeChanged, Elements }
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct Lock { pub span: usize }
  #[derive(Debug, Clone)]
  pub struct Decision {
      pub id: usize,
      pub path: String,
      pub kind: RegionKind,
      pub what: Change,
      pub base: Option<SideText>,
      pub ours: Option<SideText>,
      pub theirs: Option<SideText>,
      pub duplicated: Option<Side>,
      pub both_allowed: bool,
      pub locked: Option<Lock>,
      pub(crate) site: Site,      // where it lives in source: container node + entry index or insertion index (Task 5 consumes)
  }
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum Pick { Disk, Source, LastWritten, Both }
  #[derive(Debug, Default)]
  pub struct Resolution { .. }   // set(id, Pick) / get(id) -> Option<Pick> / unset(id)
  #[derive(Debug, thiserror::Error)]
  pub enum StructuredError { #[error(transparent)] NotStructured(NotStructured), #[error("base is not valid JSON (line {line})")] Base { line: usize } }  // Base is informational: compute degrades, never returns it
  #[derive(Debug)]
  pub struct StructuredDocument { pub decisions: Vec<Decision>, pub degraded_base: bool, .. }
  impl StructuredDocument {
      pub fn compute(base: Option<&str>, ours: &str, theirs: &str, span_map: Option<&SpanMap>) -> Result<Self, StructuredError>;
      pub fn auto_pick(&self, id: usize) -> Option<Pick>;        // None for an unlocked conflict
      pub fn required_decisions(&self) -> Vec<usize>;             // unlocked conflicts, in order
      pub fn has_base(&self, id: usize) -> bool;                  // base entry exists → LastWritten offered
      pub fn side_text(&self, side: Side) -> &str;
  }
  ```
  `path` rules: segments joined by `.`; a key is quoted with its JSON quotes when it contains any character outside `[A-Za-z0-9_-]`; array indices as `[n]`; a root array is `[n]`.
  `kind` and `what` per spec §5.2. Decision order: source order, disk-only keys after the entry that precedes them on disk when that entry exists in the result, else at the end of the container.
- Arrays are compared whole in this task (a differing array is `Change::Value`); Task 6 replaces that.
- `span_map` is accepted and ignored until Task 7.

- [ ] **Step 1: Write the failing tests**

```rust
fn doc(base: Option<&str>, ours: &str, theirs: &str) -> StructuredDocument  // helper
fn kinds(d) -> Vec<(String, RegionKind, Change)>

#[test] fn detect_accepts_objects_and_arrays_and_names_the_failure()
//   detect("{}", "{\"a\":1}") == Ok(Jsonc); detect("[]", "[1]") == Ok(Jsonc); detect("", "{}") == Ok
//   detect("{", "{}") == Err(Parse { side: Ours, line: 1 }); detect("{}", "[]") == Err(ShapeMismatch)
//   detect("1", "2") == Err(ScalarRoot); detect("", " ") == Err(BothEmpty)
#[test] fn looked_like_json_is_extension_or_one_parsed_side()
//   (".zshrc", "x", "y") false; ("settings.json", "x", "y") true; (".rc", "{}", "y") true

// Mutation: compare values by text → the reformatted case produces a decision.
#[test] fn equal_values_in_different_formatting_produce_no_decision()
//   base None; ours "{ \"a\" : 1 }"; theirs "{\"a\":1}" → decisions empty

#[test] fn one_sided_scalar_changes_classify_by_side_and_auto_resolve()
//   base {"a":1,"b":1}; ours {"a":2,"b":1}; theirs {"a":1,"b":2} →
//   [("a", OursOnly, Value), ("b", TheirsOnly, Value)]; auto_pick(0) == Some(Disk); auto_pick(1) == Some(Source)
#[test] fn adds_and_removes_are_entry_decisions_with_absent_sides()
//   base {"a":1}; ours {"a":1,"n":2}; theirs {} →
//   [("a", TheirsOnly, Removed) with theirs None, ("n", OursOnly, Added) with theirs None and base None]
#[test] fn same_change_on_both_sides_is_both_same()  // → BothSame, auto Source
#[test] fn different_changes_conflict_and_wait()     // → Conflict; auto_pick None; required_decisions == [0]
#[test] fn nested_objects_recurse_and_paths_join()
//   base {"e":{"f":1,"g":1}}; ours {"e":{"f":2,"g":1}}; theirs {"e":{"f":1,"g":2,"h":3}} →
//   paths ["e.f", "e.g", "e.h"]
#[test] fn type_change_is_one_decision_for_the_entry()
//   base {"a":{"x":1}}; ours {"a":5}; theirs same as base → [("a", OursOnly, TypeChanged)]
// Review Focus 2
#[test] fn paths_quote_keys_that_would_read_as_nesting()
//   ours {"editor.fontSize": 12, "a b": 1}, theirs {} (base None) → paths ["\"editor.fontSize\"", "\"a b\""]
// Review Focus 3
#[test] fn delete_versus_modify_is_a_conflict_without_both()
//   base {"a":1}; ours {}; theirs {"a":2} → Conflict, ours None, both_allowed false
// Review Focus 5
#[test] fn a_key_added_on_both_sides_with_different_values_conflicts_without_last_written()
//   base {}; ours {"a":1}; theirs {"a":2} → Conflict; has_base(0) == false
#[test] fn two_way_makes_every_difference_disk_sided()
//   base None; ours {"a":2,"n":1}; theirs {"a":1} → all OursOnly, degraded_base true
#[test] fn an_unparseable_base_degrades_to_two_way()
//   base Some("{") → Ok, degraded_base true
// Mutation: ignore `duplicates` → the source case produces no decision.
#[test] fn duplicates_flag_the_row_and_use_the_last_value()
//   The result starts from source, so a key duplicated on DISK gets a row only when its effective (last) value
//   differs; a key duplicated in SOURCE always gets a row (its splice collapses the copies).
//   base None; ours {"a":1,"a":2}; theirs {"a":2} → decisions empty
//   base None; ours {"a":1,"a":3}; theirs {"a":2} → [("a", OursOnly, Value)] with duplicated == Some(Ours), ours.text == "3"
//   base None; ours {"a":2}; theirs {"a":1,"a":2} → [("a", BothSame, Value)] with duplicated == Some(Theirs), auto_pick Source
#[test] fn disk_only_keys_sit_after_their_disk_predecessor()
//   base None; ours {"a":1,"n":9,"b":2}; theirs {"a":1,"b":2} → decisions [("n", OursOnly, Added)] and
//   site says: container root, after entry "a" (assert through a pub(crate) accessor or by Task 5's assembly; here assert `decisions[0].site.after == Some(0)`).
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p tomte-core --lib structured::merge`

- [ ] **Step 3: Implement `detect`, `looked_like_json`, the types, and `compute`**

`compute` parses the three texts (an empty text is an empty document; a base parse error sets `degraded_base` and uses source as base), checks `detect`, then walks: `fn walk_object(&mut self, path: &str, base: Option<NodeId>, ours: Option<NodeId>, theirs: Option<NodeId>)` building the key union in the order rule, classifying each key with `Value` equality, recursing when both non-base sides are objects. `Site { container: NodeId /* in source */, entry: Option<usize> /* source entry index */, after: Option<usize> /* for Added: source entry index to insert after, None = end */ }`.

- [ ] **Step 4: Run to verify pass**

- [ ] **Step 5: Run the named mutation; revert**

- [ ] **Step 6: Gate and commit**

```bash
git add crates/core/src/structured
git commit -m "feat(core): structured decisions over JSON objects with detect and value equality"
```

---

### Task 5: Assembly by container rebuild (`splice.rs`)

**Files:**
- Create: `crates/core/src/structured/splice.rs`
- Modify: `crates/core/src/structured/mod.rs`, `crates/core/src/structured/merge.rs` (expose `Site` fields to the splicer)
- Test: unit tests at the end of `splice.rs`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, thiserror::Error)]
  pub enum AssembleError {
      #[error("decision {id} ({path}) is a conflict with no pick")] Unresolved { id: usize, path: String },
      #[error("the merged JSON does not parse: {error}")] Invalid { error: ParseError },
  }
  impl StructuredDocument {
      /// The result text; errors on an unresolved unlocked conflict or when the result does not re-parse.
      pub fn assemble(&self, res: &Resolution) -> Result<String, AssembleError>;
      /// Like assemble, but an unresolved conflict takes Source so the text is always valid (the editor seed).
      pub fn seed(&self, res: &Resolution) -> String;
      /// For each decision in order, its byte range in `seed(res)`; empty at the insertion point when absent from the result.
      pub fn result_ranges(&self, res: &Resolution) -> Vec<Range<usize>>;
      /// Lines of SOURCE that the pick's splice touches (byte ranges of whole lines) — Task 7's lock input.
      pub fn touched_lines(&self, id: usize, pick: Pick) -> Vec<Range<usize>>;
  }
  ```
  Algorithm (pins spec §5.4): group decisions by `site.container`. A container with no non-source resolution is copied verbatim. Otherwise its body is rebuilt from an ordered list of items: kept source entries (their text from `leading.start` to the value's end, comma excluded), replaced entries (source key text, chosen side's value text re-indented), inserted entries (chosen side's entry text re-indented), each pair separated by a separator. The separator between two items is the ORIGINAL text between them when both are original source neighbours and still adjacent; otherwise synthesized: `,` + newline + the container indent for a multiline container, `, ` for an inline container (no newline between the open bracket and the first entry). The last item gets a trailing comma only if the source's last entry had one. Same-line trailing comments stay with their item, after the comma. An emptied container keeps its brackets and the newline between them if it had one. Nested containers rebuilt inside a rebuilt parent are handled by rebuilding innermost first and substituting text ranges. Indent unit per spec §5.4. CRLF: every synthesized newline is `\r\n` when `doc.crlf`.

- [ ] **Step 1: Write the failing tests**

```rust
// Property. Mutation: splice an unchanged entry → the Source-everywhere result differs from source.
#[test] fn all_source_picks_reproduce_source_byte_for_byte()
//   for the Zed and VS Code corpus files as `theirs`, with `ours` a value-edited copy: pick Source on every decision → assemble == theirs
#[test] fn all_disk_picks_two_way_are_value_equal_to_disk()
//   base None, ours = corpus with edits, theirs = corpus → assemble with Disk everywhere: value(result) == value(ours)

#[test] fn a_changed_value_is_replaced_in_place()
//   theirs "{\n  \"a\": 1,\n  \"b\": 2\n}\n", ours same with "a": 10 → Disk → "{\n  \"a\": 10,\n  \"b\": 2\n}\n"
#[test] fn a_multi_line_value_is_reindented_to_the_destination()
//   ours value for "a" is "{\n      \"x\": 1\n    }" at indent 4; source indent 2 → inner line at 4 spaces, closing brace at 2
// Mutation: always append a comma → every case below fails.
#[test] fn added_entries_get_commas_in_every_position()
//   middle, last (no trailing comma in source), last (source uses trailing commas → new last keeps one), into an empty object "{}", first
#[test] fn removed_entries_fix_the_previous_comma_only_when_last()
//   remove middle → previous keeps its comma; remove last (no trailing-comma style) → previous comma removed;
//   remove last (trailing-comma style) → previous keeps its comma
#[test] fn leading_comments_and_same_line_comments_travel_with_their_entry()
//   remove an entry with "// why" above and "// note" after → both gone, blank line above the comment kept
//   add an entry whose disk text carries "// why" above → inserted with the comment
// Review Focus 1
#[test] fn inline_container_glue_is_comma_space_and_removal_keeps_the_line()
//   theirs "{\"a\": 1, \"b\": 2}\n"; ours "{\"a\": 1, \"b\": 2, \"c\": 3}\n" → Disk on c → "{\"a\": 1, \"b\": 2, \"c\": 3}\n"
//   ours "{\"b\": 2}\n" → Disk on a (remove) → "{\"b\": 2}\n"
// Review Focus 4
#[test] fn removing_every_entry_leaves_the_braces()
//   theirs "{\n  \"a\": 1\n}\n", ours "" → Disk on a → "{\n}\n"
#[test] fn crlf_and_bom_follow_source()
#[test] fn source_duplicates_are_collapsed_to_the_last_copy_whatever_the_pick()
//   theirs "{\"a\":1,\n\"a\":2}" ours "{\"a\":2}" → Source → "{\"a\":2}"
#[test] fn seed_takes_source_for_an_unresolved_conflict_and_assemble_refuses()
#[test] fn result_ranges_cover_each_decision_and_are_empty_for_removed_entries()
#[test] fn touched_lines_include_the_previous_line_for_a_last_entry_removal()
// Mutation: skip the re-parse → `assemble` returns text. The hook below exists for this test and for
// Task 10's view test: `#[cfg(any(test, feature = "test-support"))] pub fn corrupt_source_for_tests(&mut self)`
// on StructuredDocument drops the source text's final closing bracket, so any rebuilt output is invalid.
#[test] fn assemble_refuses_a_result_that_does_not_parse()
//   doc with one OursOnly decision; doc.corrupt_source_for_tests(); pick Disk → Err(AssembleError::Invalid { .. })
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p tomte-core --lib structured::splice`

- [ ] **Step 3: Implement `splice.rs`** per the algorithm above, plus `indent_of(text, offset) -> &str`, `indent_unit(doc, container) -> String`, `is_inline(doc, container) -> bool`, `reindent(text, from: &str, to: &str) -> String`.

- [ ] **Step 4: Run to verify pass**; run the two named mutations; revert.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/core/src/structured
git commit -m "feat(core): assemble the structured result by rebuilding only the containers that changed"
```

---

### Task 6: Arrays merge element-wise

**Files:**
- Modify: `crates/core/src/structured/merge.rs` (replace the whole-array `Value` comparison), `crates/core/src/structured/splice.rs` (element items)
- Test: unit tests in both files

**Interfaces:**
- Consumes: `tomte_core::merge::three_way` (Task 3), `jsonc::canonical` (Task 2).
- Produces: `Change::Elements` decisions carry `ours`/`theirs` as the run's text (first element start to last element end) and `site.elements: Range<usize>` over source element indices; `both_allowed == true` only for `Change::Elements` conflicts. One-sided regions split per element into `Added`/`Removed` decisions with paths `plugins[2]`; a conflict region is one `Elements` decision with the path of its first source index (`plugins[2..4]` when the run covers more than one).

- [ ] **Step 1: Write the failing tests**

```rust
// merge.rs
// Mutation: compare arrays whole → one Value decision instead of per-element rows.
#[test] fn one_sided_array_changes_split_into_one_row_per_element()
//   base {"p":["a","b"]}; ours {"p":["a","x","b"]}; theirs {"p":["b"]} →
//   result order: a removed element keeps its place, an added one follows its predecessor →
//   [("p[0]", TheirsOnly, Removed), ("p[1]", OursOnly, Added)]; both_allowed false on both
#[test] fn an_edited_element_is_a_removed_row_and_an_added_row()
//   base ["a"]; ours ["A"]; theirs ["a"] → [Removed "a", Added "A"]
#[test] fn conflicting_element_runs_are_one_row_with_both()
//   base ["a"]; ours ["x"]; theirs ["y"] → one Elements Conflict, both_allowed true, path "p[0]"
#[test] fn array_elements_compare_by_value_and_objects_do_not_recurse()
//   base [{"k":1}]; ours [{ "k" : 1 }]; theirs [{"k":2}] → one decision, TheirsOnly, Removed+Added (not "k" nested)
#[test] fn a_root_array_file_has_bracket_paths()   // "[0]"
// splice.rs
#[test] fn inserted_elements_land_where_the_diff_put_them_with_commas()
//   theirs "[\n  \"a\",\n  \"b\"\n]\n"; ours adds "x" between → Disk → "[\n  \"a\",\n  \"x\",\n  \"b\"\n]\n"
#[test] fn both_on_an_element_conflict_emits_disk_run_then_source_run()
//   → "[\n  \"x\",\n  \"y\"\n]\n"
#[test] fn removing_the_last_element_fixes_the_previous_comma()
```

- [ ] **Step 2: Run to verify failure**

- [ ] **Step 3: Implement**: in `walk`, when both non-base sides are arrays, tokenize each side's elements by `canonical(value)`, call `three_way(base_tokens, ours_tokens, theirs_tokens, |a, b| a == b)`, map regions to decisions; in `splice.rs`, treat elements as items with the same separator rules (Task 5's rebuild works on `Element` as on `Entry`; factor an `Item` view over both).

- [ ] **Step 4: Run to verify pass**; mutation; revert.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/core/src/structured
git commit -m "feat(core): arrays merge element-wise through the shared three_way clustering"
```

---

### Task 7: Locking against the template span map

**Files:**
- Modify: `crates/core/src/structured/merge.rs` (lock computation at the end of `compute`)
- Create: `crates/core/tests/structured_writeback.rs` (real `chezmoi execute-template`, the `scratch_chezmoi` helper copied from `template_roundtrip.rs`)

**Interfaces:**
- Consumes: `StructuredDocument::touched_lines` (Task 5), `tomte_core::template::anchor::{SpanMap, SpanOrigin}`, `write_back`, `verify_write_back`.
- Produces: `Decision::locked = Some(Lock { span })` where `span` indexes `span_map.spans`; `auto_pick` returns `Some(Source)` for a locked decision; `required_decisions` excludes locked ones.

- [ ] **Step 1: Write the failing tests**

In `merge.rs` tests, with `map_for(tmpl, rendered)` = `anchor(tmpl, &lex(tmpl).unwrap(), rendered)`:
```rust
// Mutation: lock only the value's own line, not every touched line → the removal case stays unlocked.
#[test] fn entries_whose_splice_touches_a_template_line_are_locked()
//   tmpl  "{\n  \"email\": \"{{ .email }}\",\n  \"editor\": \"hx\"\n}\n"
//   theirs "{\n  \"email\": \"me@x.y\",\n  \"editor\": \"hx\"\n}\n"
//   (a) ours changes email → decision "email" locked, Lock.span points at an Action span; auto_pick == Some(Source); required_decisions empty
//   (b) ours changes editor → unlocked
//   (c) same template (no trailing comma): ours removes "editor", the last entry; the comma fix lands on the
//       templated "email" line → the "editor" decision is locked
//   (d) templated KEY: tmpl "{\n  \"{{ .k }}\": 1\n}\n" → locked
#[test] fn without_a_span_map_nothing_is_locked()
```

In `crates/core/tests/structured_writeback.rs`:
```rust
// The lock rule and the write-back agree. Mutation: make touched_lines return only the value's range → (c) above passes
// write_back here with ProtectedSpanTouched, failing this test.
#[test] fn every_unlocked_disk_pick_writes_back_and_re_renders()
//   template "{\n  \"name\": \"{{ .name }}\",\n  \"editor\": \"hx\",\n  \"tabs\": 2\n}\n" rendered via the scratch chezmoi;
//   ours: editor → "zed", tabs → 4, name → "X"; compute with the span map; assert "name" locked;
//   pick Disk on every unlocked decision; assemble; write_back(template, &map, rendered, &result).unwrap();
//   verify_write_back(&chezmoi, &new_template, &result).unwrap()
```

- [ ] **Step 2: Run to verify failure**

- [ ] **Step 3: Implement** the lock pass: for each decision and each non-Source pick it allows, `touched_lines(id, pick)`; a decision is locked if any touched line overlaps any span whose origin is not `Literal { repeated: false }`; record the first such span's index.

- [ ] **Step 4: Run to verify pass**; mutation; revert.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/core/src/structured crates/core/tests/structured_writeback.rs
git commit -m "feat(core): templated entries lock to source from the span map; write-back agrees"
```

---

### Task 8: Decision strip gains a path label and a locked state

**Files:**
- Modify: `crates/ui/src/components/merge_region.rs` (`StripState`, `decision_strip`), `crates/ui/src/components/mod.rs` if re-exports need it, the `comp:merge-region` preview in `crates/app/src/views/fixtures.rs` (the component previews near "Component previews: bare Shell routed at the preview", with `preview_document()` above them)
- Test: `cargo test -p tomte-ui` (any pure helpers) and the preview screenshot

**Interfaces:**
- Produces:
  ```rust
  pub enum StripState {
      Deciding { has_base: bool, current: Option<ChoiceKind>, focused: bool, origin: Option<SharedString> },
      Decided { choice: ChoiceKind, focused: bool },
      /// Follows source; no buttons. `reason` is the hover text (the template line).
      Locked { reason: SharedString, focused: bool },
  }
  /// `label`: leading text before the state words (the structured path, e.g. `editor.fontSize`); `None` keeps today's layout.
  /// `detail`: the change in words ("added on disk"); `None` keeps today's "conflict"/"auto" wording.
  pub fn decision_strip(theme, region_ix, state, label: Option<SharedString>, detail: Option<SharedString>, on_pick, on_edit, on_revisit) -> Div;
  ```
  Locked renders the label, the detail, a lock glyph, and "templated · follows source"; `both` is rendered only when the caller passes it through `Deciding`'s existing button set (add `both: bool` to `Deciding`, default `true` for the line editor's callers).

- [ ] **Step 1: Update every caller** (`crates/app/src/views/merge.rs` `strip_block`, the preview) to pass `None, None` and `both: true` so the workspace compiles unchanged in behaviour.

- [ ] **Step 2: Add `Locked` and the label/detail rendering**; the preview gains a locked strip and a labelled strip.

- [ ] **Step 3: Shoot and read the preview**: `./scripts/shoot.sh comp:merge-region dark` (unsandboxed), open the PNG, confirm the label, detail and lock render without clipping.

- [ ] **Step 4: Gate and commit**

```bash
git add crates/ui crates/app/src/views/merge.rs
git commit -m "feat(ui): decision strip takes a path label, a detail line and a locked state"
```

---

### Task 9: Pure structured editor state

**Files:**
- Create: `crates/app/src/structured_state.rs`
- Modify: `crates/app/src/lib.rs` (`pub mod structured_state;`)
- Test: unit tests in the file, modelled on `merge_state.rs`

**Interfaces:**
- Consumes: `tomte_core::structured::{StructuredDocument, Resolution, Pick, Decision, AssembleError}`.
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum Stage { Structure, Text }
  #[derive(Debug)]
  pub struct StructuredState { pub doc: StructuredDocument, pub resolution: Resolution, pub cursor: Option<usize>, pub degraded_base: bool, pub stage: Stage }
  impl StructuredState {
      pub fn new(doc: StructuredDocument) -> Self;          // cursor on the first required decision
      pub fn conflicts(&self) -> Vec<usize>;                 // doc.required_decisions()
      pub fn unresolved(&self) -> Vec<usize>;
      pub fn progress(&self) -> (usize, usize);
      pub fn pick(&mut self, id: usize, pick: Pick);         // ignored on a locked decision; advances the cursor as MergeState::pick does
      pub fn next_unresolved(&mut self) -> Option<usize>;
      pub fn changed(&self) -> Vec<usize>;                   // every unlocked decision
      pub fn next_changed(&mut self) -> Option<usize>;
      /// Ok(None) while a conflict is unresolved; Err only for AssembleError::Invalid (never swallowed).
      pub fn assembled(&self) -> Result<Option<String>, AssembleError>;
      pub fn seed(&self) -> String;                          // doc.seed(&resolution)
      pub fn result_lines(&self) -> Vec<usize>;              // first result LINE of each decision (from doc.result_ranges)
      pub fn nothing_to_decide(&self) -> bool;               // decisions.is_empty()
  }
  ```

- [ ] **Step 1: Write the failing tests** (JSON inputs through `StructuredDocument::compute`):

```rust
#[test] fn conflict_bookkeeping_tracks_decisions()      // mirrors merge_state's
#[test] fn pick_on_a_locked_decision_is_ignored()       // Mutation: drop the locked check → resolution.get(id) is Some
#[test] fn pick_on_the_cursor_advances_it_elsewhere_it_stays()
#[test] fn next_unresolved_wraps_around()
#[test] fn assembled_gates_on_full_resolution_then_matches_doc_assemble()
// Mutation: map the Invalid error to Ok(None) → this fails.
#[test] fn assembled_surfaces_an_invalid_result_as_an_error()   // doc.corrupt_source_for_tests(); pick → Err(Invalid)
#[test] fn two_way_without_a_base_has_no_conflicts_and_offers_no_last_written()   // degraded_base true; doc.has_base(id) false for all
#[test] fn result_lines_follow_the_seed()               // the second decision's line equals the line of its key in seed()
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p tomte-app --lib structured_state`

- [ ] **Step 3: Implement**

- [ ] **Step 4: Run to verify pass**; mutation; revert.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/app/src/lib.rs crates/app/src/structured_state.rs
git commit -m "feat(app): pure structured editor state beside the line merge state"
```

---

### Task 10: Structure mode in the merge editor — load, mode, strips, picks, keys, Save

**Files:**
- Create: `crates/app/src/views/merge_structured.rs`
- Modify: `crates/app/src/views/merge.rs` (`MergeLoad`, `LoadedMerge`, `MergeView` fields, `load`, `seed_editor`, `apply_many`/`undo`/`redo`, `pick_at_cursor`, `next`, `install_result_decorator`, `header`, `save`), `crates/app/src/views/mod.rs` (`mod merge_structured;` and tests)
- Test: `#[gpui::test]`s in `views/mod.rs`

**Interfaces:**
- Consumes: Tasks 4–9.
- Produces (in `merge.rs`):
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub(super) enum MergeMode { Lines, Structure }
  pub(super) struct LoadedMerge { .., pub(super) structured: Option<StructuredState>, pub(super) mode: MergeMode, .. }
  impl LoadedMerge { pub(super) fn new(inputs: Arc<MergeInputs>) -> Self }   // runs detect + compute; mode = Structure when Some
  impl MergeView {
      pub(super) fn set_mode(&mut self, mode: MergeMode, cx: &mut Context<Self>);   // discards the other mode's picks; the confirmation is an armed second press like `cancel`: field `mode_switch_armed: Option<MergeMode>`
      pub(super) fn apply_pick(&mut self, id: usize, pick: Pick, cx: &mut Context<Self>);   // Structure-mode twin of `apply`: one undo entry
  }
  ```
  In `merge_structured.rs`: `pub(super) fn strip_specs(state: &StructuredState, revisiting: &HashSet<usize>, template: Option<&str>, span_map: Option<&SpanMap>) -> Vec<StructuredStrip>`, `pub(super) fn strip_block(spec: &StructuredStrip, theme, view: WeakEntity<MergeView>) -> ui::text_area::Block`, `pub(super) fn detail_words(d: &Decision) -> SharedString` (the spec's copy), `pub(super) fn lock_reason(d: &Decision, template: Option<&str>, span_map: Option<&SpanMap>) -> SharedString` (uses `template_line_at` on the span's segment, made `pub(super)` in `merge.rs`), `pub(super) fn mode_toggle(theme, mode, cx) -> Div` (two `ui::button`s, `ButtonVariant::Tonal(theme.accent)` on the selected one, `Ghost` on the other, ids `merge-mode-structure` / `merge-mode-lines`, debug selectors the same).
  Undo entries: `UndoEntry` gains a `structured: Vec<(usize, Option<Pick>, Pick)>` side so one stack serves both modes (entries of the inactive mode are cleared on `set_mode`).
  Keys: `PickDisk`/`PickSource`/`PickBase`/`PickBoth`/`NextConflict`/`UndoChoice`/`RedoChoice` route to the structured state when `mode == Structure`; `PickBoth` is ignored on a decision with `both_allowed == false`.
  Save in Structure: `loaded.structured.assembled()` → `Ok(Some(text))` enqueues `Decision::Merged` exactly as today; `Ok(None)` is unreachable (the Save button is disabled with "N conflicts left" through the existing `save_button` over structured progress); `Err(Invalid)` logs with `log_error!("merge", ..)`, sets `self.error` to the spec's message-box state ("the merged JSON does not parse: …") while keeping the picks, and the header still offers the Lines toggle (spec §7). Header progress text: "N conflicts, M resolved" over structured progress; `nothing_to_decide` → "same settings, formatting differs · Save keeps the source layout"; degraded base → the existing degraded note; `inputs.origin.is_some()` → "origin differs · see Lines" chip.

- [ ] **Step 1: Write the failing tests** in `views/mod.rs` (fixtures from Task 12 can be stubbed here with a local `json_inputs()` helper; Task 12 replaces it with the pose):

```rust
fn json_inputs() -> MergeInputs  // target home(".config/zed/settings.json"), base Some, ours/theirs JSONC with: a conflict on "theme", "font_size" changed on disk, "vim_mode" added on disk, "plugins" array with one element added in source

// Mutation: LoadedMerge::new never runs detect → mode stays Lines.
#[gpui::test] fn json_inputs_open_in_structure_mode_with_one_strip_per_decision(cx)
//   posed_merge(json_inputs) → mode == Structure; debug_selector "merge-strip:<id>" present for every decision id;
//   strips carry the path text (assert the rendered text of "merge-strip:0" contains "theme")
// Mutation: apply_pick writes to the LINE resolution → result text unchanged.
#[gpui::test] fn a_strip_pick_changes_the_result_text(cx)
//   click "merge-strip-pick:0:disk" → result_editor text contains the disk theme value; undo (cmd-z action) restores the source value
#[gpui::test] fn save_in_structure_mode_enqueues_the_assembled_json(cx)
//   pick every conflict; view.save(cx); run_until_parked; the queue's Decision::Merged resolved text parses and == structured.assembled()
#[gpui::test] fn the_lines_toggle_shows_the_line_editor_and_arms_a_confirmation_when_picks_exist(cx)
//   click "merge-mode-lines" with no picks → mode Lines and no "merge-strip:*" selector (the line editor's strips carry none today);
//   with a pick → first click arms (banner text starts with "Switching to Lines discards"), second click switches
// Mutation: map Err(Invalid) to a disabled Save → no message box, nothing logged.
#[gpui::test] fn an_invalid_assembly_is_a_message_box_that_keeps_the_picks_and_offers_lines(cx)
//   loaded.structured.doc.corrupt_source_for_tests() (the app's dev-dependency on tomte-core already enables `test-support`);
//   pick Disk on 0; view.save(cx) → view.error contains "does not parse"; resolution.get(0) == Some(Disk); "merge-mode-lines" present
#[gpui::test] fn a_locked_strip_has_no_buttons(cx)
//   templated JSON inputs (span map from lex+anchor) → "merge-strip:<locked id>" present, "merge-strip-pick:<id>:disk" absent at first render
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p tomte-app --bin tomte <name>`

- [ ] **Step 3: Implement** the load wiring (compute inside `LoadedMerge::new`, which already runs on the background executor through `load`), the mode field and toggle, the Structure-mode seed (`structured.seed()`) in `seed_editor`, strip specs and blocks (`above_line` = `result_lines()[i]`), `apply_pick` with undo, key routing, header text and Save.

- [ ] **Step 4: Run to verify pass**; run the two named mutations; revert.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/app/src/views
git commit -m "feat(merge): Structure mode — per-key strips, picks, undo, keys and Save on JSON files"
```

---

### Task 11: Input panes in Structure mode — tints and jump

**Files:**
- Modify: `crates/app/src/views/merge_structured.rs`, `crates/app/src/views/merge.rs` (`LoadedMerge::new` pane rows, `sync_from`, `next`, strip focus)
- Test: pure tests in `merge_structured.rs`; one `#[gpui::test]`

**Interfaces:**
- Produces: `pub(super) fn structured_pane_decors(doc: &StructuredDocument, side: Side, theme) -> Vec<LineDecor>` (each line of that side's full text gets `row_bg(kind, pane_side, theme)` of the decision covering it; a decision's side range comes from `Decision::{base,ours,theirs}`); `pub(super) fn jump_lines(doc, id) -> [Option<usize>; 3]` (first line of the decision on each side, `None` when absent). In Structure mode `sync_from` returns early (panes scroll independently), and `next`/a strip click calls `jump_to(id)`, which sets each pane's scroll offset to its jump line (`PANE_ROW_H` per line) and the result editor's to the strip row.

- [ ] **Step 1: Write the failing tests**

```rust
// Mutation: tint by decision index instead of byte range → the wrong lines wash.
#[test] fn pane_decors_tint_exactly_the_lines_a_decision_covers()
//   ours text where "font_size" sits on line 3 → decors[2].bg == row_bg(OursOnly, Ours); decors[1].bg == None
#[test] fn jump_lines_name_each_sides_first_line_or_none_when_absent()
#[gpui::test] fn next_in_structure_mode_moves_every_pane_to_the_decision(cx)
//   after view.next(cx), pane_scrolls[OURS_COL].offset().y == -(line * PANE_ROW_H)
```

- [ ] **Step 2: Run to verify failure**

- [ ] **Step 3: Implement**; the pane editors in Structure mode are seeded with each side's full text (not `pane_lines`), decorated by `structured_pane_decors`; the theirs pane keeps the protected `{}` gutter from `protected_line_info`.

- [ ] **Step 4: Run to verify pass**; mutation; revert.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/app/src/views
git commit -m "feat(merge): Structure mode tints the panes per decision and jumps them together"
```

---

### Task 12: Text stage — Edit, Back to structure, the invalid-JSON chip

**Files:**
- Modify: `crates/app/src/views/merge.rs` (`start_edit`, `commit_edit`, `cancel_edit`, `header`, `save`), `crates/app/src/views/merge_structured.rs`
- Test: `#[gpui::test]`s in `views/mod.rs`

**Interfaces:**
- Produces: in Structure mode, `start_edit(id)` sets `structured.stage = Text`, flips the editor editable at the decision's line, hides strips (`editing = true` as today). The header in Text stage shows `merge-save` and `merge-back-to-structure` (label "Back to structure"); `ApplyEdit`/`merge-edit-apply` are not shown in Structure mode. `back_to_structure(cx)`: if the editor text != `structured.seed()`, arm (`text_discard_armed: bool`, banner "Discard the text edits and return to the structure picks?" with the button reading "Discard edits"), second press re-seeds read-only with the picks intact. `invalid_json_line(text) -> Option<usize>` (via `jsonc::parse`) drives the chip "not valid JSON · line N" (id `merge-json-chip`) in the header while in Text stage; Save in Text stage enqueues the editor text verbatim (`Decision::Merged`), chip or not.

- [ ] **Step 1: Write the failing tests**

```rust
// Mutation: Save in Text stage uses structured.assembled() → the typed text is lost.
#[gpui::test] fn edit_then_save_enqueues_the_editor_text_verbatim(cx)
//   click "merge-strip-edit:0" → editing; editor.set_content(seed + "\n// hand\n")… use `editor.update(cx, |a, _| a.set_content(..))`;
//   view.save(cx); run_until_parked; the queued Decision::Merged.resolved ends with "// hand\n"
#[gpui::test] fn back_to_structure_restores_the_strips_and_keeps_the_picks(cx)
//   pick Disk on 0; Edit; Back (text unchanged → no arming); strips present; resolution.get(0) == Some(Disk)
#[gpui::test] fn back_to_structure_arms_when_the_text_changed(cx)
//   Edit; change text; Back → still editing, banner present; Back again → structure, text == seed
#[gpui::test] fn the_invalid_json_chip_names_the_line_and_save_stays_enabled(cx)
//   Edit; set_content("{\n  \"a\": \n}") → "merge-json-chip" text == "not valid JSON · line 3"; "merge-save" is enabled (not the disabled variant)
```

- [ ] **Step 2: Run to verify failure**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Run to verify pass**; mutation; revert.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/app/src/views
git commit -m "feat(merge): Text stage — refine the assembled JSON, go back to the picks, or save as typed"
```

---

### Task 13: Notes for files that are not structured, gallery poses, docs

**Files:**
- Modify: `crates/app/src/views/merge.rs` (header note), `crates/app/src/views/fixtures.rs` (`STATES`, three poses, `json_inputs`/`json_templated_inputs` helpers moved here from Task 10's test), `crates/app/src/views/mod.rs` (`merge_view_renders_all_states_without_panicking` covers the new poses; Task 10's tests use the fixtures), `AGENTS.md` (workspace map: `structured` in core, `structured_state.rs`/`merge_structured.rs` in app; pose list; "This branch adds"), `CHANGELOG.md` (`## Unreleased` → `### Added` entry "Settings files merge per key")

**Interfaces:**
- Produces: poses `merge-structured` ("JSON settings in Structure mode: a conflict, a key added on disk, one removed, array rows"), `merge-structured-locked` ("templated JSON source: a locked row that follows source"), `merge-structured-text` ("Text stage after picks, with the not-valid-JSON chip"). The header note for a non-structured JSON-looking file: `NotStructured::Parse { side, line }` → "disk is not valid JSON (line N): line merge"; `ShapeMismatch` → "disk and source are different JSON shapes: line merge"; shown only when `looked_like_json(target, ours, theirs)`; `LoadedMerge` keeps `not_structured: Option<NotStructured>` for it.

- [ ] **Step 1: Write the failing tests**

```rust
// Mutation: show the note for every NotStructured → the .zshrc case renders it.
#[gpui::test] fn a_json_looking_file_that_does_not_parse_says_so_and_a_shell_file_does_not(cx)
//   inputs target "settings.json", ours "{" → header contains "disk is not valid JSON (line 1): line merge";
//   target ".zshrc", ours "x", theirs "y" → no "line merge" text in the header
```
Add the three pose names to `STATES` and `build`, and assert in the render-smoke test that each builds.

- [ ] **Step 2: Run to verify failure**

- [ ] **Step 3: Implement the note, the poses, the docs**

- [ ] **Step 4: Shoot and READ**: `./scripts/shoot.sh merge-structured dark`, `merge-structured light`, `merge-structured-locked dark`, `merge-structured-text dark` (unsandboxed). Open each PNG. Check: path labels not clipped (`line_clamp(1)` in a fixed width if they are), the locked row reads "templated · follows source", the chip shows the line, no `.truncate()` anywhere.

- [ ] **Step 5: Gate and commit**

```bash
git add crates/app/src/views AGENTS.md CHANGELOG.md
git commit -m "feat(merge): poses and notes for Structure mode; docs"
```

---

### Task 14: End-to-end stories through the real engine

**Files:**
- Modify: `crates/app/tests/resolve_e2e.rs`

**Interfaces:**
- Consumes: `DriftLab`, `templatize_and_drift`'s pattern, `StructuredDocument::compute`, `StructuredState`, `ResolveEngine::resolve_merged`.

- [ ] **Step 1: Write the failing tests**

```rust
const JSON_BASE: &[u8]   = b"{\n  \"theme\": \"one\",\n  \"tabs\": 2\n}\n";
const JSON_SOURCE: &[u8] = b"{\n  \"theme\": \"two\",\n  \"tabs\": 2\n}\n";
const JSON_DISK: &[u8]   = b"{\n  \"theme\": \"one\",\n  \"tabs\": 2,\n  \"vim\": true\n}\n";
const JSON_MERGED: &[u8] = b"{\n  \"theme\": \"two\",\n  \"tabs\": 2,\n  \"vim\": true\n}\n";

// Mutation: assemble drops the Added splice → the file on disk lacks "vim".
#[test] fn story_x_a_structured_save_lands_a_disk_added_key_and_a_source_change()
//   like story_f: write JSON_BASE as the managed file (replace dot_testrc's content, commit, apply, settle);
//   source → JSON_SOURCE (commit); rescan_and_settle (snapshots the base); disk → JSON_DISK; rescan_and_settle;
//   inputs = lab.load_merge_inputs(); doc = StructuredDocument::compute(inputs.base.as_deref(), &inputs.ours, &inputs.theirs, None).unwrap();
//   assert decisions: ["theme" TheirsOnly, "vim" OursOnly]; state = StructuredState::new(doc); resolved = state.assembled().unwrap().unwrap();
//   assert_eq!(resolved.as_bytes(), JSON_MERGED); outcome = resolve_merged → Done { committed: true, pushed: true };
//   disk and source both == JSON_MERGED; undo_last → disk == JSON_DISK

// Mutation: skip locking → "name" is picked Disk and write_back rejects (ProtectedSpan outcome).
#[test] fn story_y_a_structured_save_on_a_templated_json_source_writes_back_the_literal_key()
//   templatize with "{\n  \"name\": \"{{ .testvalue }}\",\n  \"tabs\": 2\n}\n" (config data testvalue = "tpl");
//   drift disk to tabs 4 AND name "X"; inputs = load_merge_inputs(); compute with inputs.span_map.as_ref();
//   assert "name" locked, "tabs" unlocked; Disk on tabs; resolved = assembled; resolve_merged → Done;
//   source template == "{\n  \"name\": \"{{ .testvalue }}\",\n  \"tabs\": 4\n}\n"; disk == rendered with tabs 4 and name "tpl"
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p tomte-app --test resolve_e2e story_x story_y`

- [ ] **Step 3: Implement** only test helpers (a `set_managed_content(lab, bytes)` that writes, commits, applies and settles).

- [ ] **Step 4: Run to verify pass**; run both mutations; revert.

- [ ] **Step 5: Full gate (both test runs), then commit**

```bash
git add crates/app/tests/resolve_e2e.rs
git commit -m "test(e2e): structured saves land through the real engine, plain and templated"
```
