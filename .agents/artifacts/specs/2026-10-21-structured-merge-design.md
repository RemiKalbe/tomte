# Structured merge: JSON/JSONC settings merged per key, not per line

**Status:** decided (conversation, 2026-10-17 to 2026-10-21). Approach,
engine, UI, error handling and testing were presented in chat section by
section and approved. Recorded here so they can be overruled before or
during implementation.
**Implemented by:** `.agents/artifacts/plans/2026-10-21-structured-merge.md`

> "files that are not code, but structured settings like json, toml and
> everything else, should have an alternative path where the merge would
> understand when keys are duplicated, when and where to put commas when
> needed, and whatnot, with a ui where you can pick and choose how the new
> json is merged" · "The main thing that bites is json stuff, editor
> settings, agent settings... but I do need to support templates" · "why
> can't, given two arrays (one disk one source), we pick elements instead
> of the whole thing" · "merge editor only, and also there should be a way
> once we have selected what we want to apply those changes and edit the
> lines directly if we want to refine something before saving, or we can
> also choose to save right away"

## 1. What happens today

The merge engine (`crates/core/src/merge.rs`) is a line-based three-way
merge: base, disk ("ours") and rendered source ("theirs") are split into
lines, diffed with imara-diff, and clustered into regions of kind
Unchanged, OursOnly, TheirsOnly, BothSame or Conflict. The merge editor
(`crates/app/src/views/merge.rs`) shows the regions in a result pane with a
decision strip above each changed one, and a free-text stage for hand
edits. For a settings file this is the wrong unit:

- A key added on disk and a different key added in source at the same spot
  cluster into one conflict, and picking one side drops the other.
- Taking an added entry from one side leaves a missing or doubled comma,
  so the saved file is invalid JSON until the user fixes it by hand.
- A key that both sides reordered shows as two unrelated hunks.
- An array (a plugin list, a list of paths) is one blob: an element added
  on each side is a conflict over the whole array.

The files that bite are Zed and VS Code `settings.json` (JSONC: comments and
trailing commas), Claude Code `settings.json` (strict JSON) and opencode
`opencode.json` (JSONC). Several of these are templated (`.tmpl` sources
with `{{ }}` actions), so any structural path must respect the template
span pipeline (`crates/core/src/template/`) that keeps protected spans out
of a save.

## 2. Decisions

Asked one at a time and answered:

1. **JSON/JSONC first.** Other formats later behind the same seam; TOML is
   the expected next one and would ride a lossless TOML editor crate.
2. **Templated sources are supported.** A templated key is locked to the
   source side: the row shows disk's value for information, offers no
   choice, and says why.
3. **Arrays merge element-wise, always.** A sequence diff over the elements
   compared by value; every inserted, removed or changed element is a row
   with the same choice strip; auto-resolution with a base; order
   preserved; an edited element shows as two neighbouring rows (removed,
   added).
4. **Merge editor only.** Review's diff preview keeps its line diff.
5. **Two stages.** After the structural picks the user either saves right
   away or drops into direct line editing of the assembled result to refine
   it, then saves from there.
6. **Approach B, own parser** (see §3): a lossless syntax tree with text
   splicing, parsed by a hand-written JSONC parser in `tomte-core`. No new
   dependency.

## 3. Approaches considered

**A. Tree merge, re-serialize.** Parse each side into values, merge per
path, print with one formatter. Easiest. Drops comments, blank lines, the
file's own formatting, and reorders untouched keys. Zed and VS Code
settings carry comments, so this rewrites files the user never touched.
Rejected.

**B. Lossless syntax tree, text splice. Chosen.** Parse each side into a
concrete tree where every key, value, comma and comment keeps its byte
range. Merge per path. Build the result by walking the source text and
splicing in the chosen side's exact text for each changed entry,
synthesizing only the glue (a comma, an indent, a trailing-comma fix).
Untouched bytes are identical to source. Templated keys are the ones whose
splice would touch a non-literal template span, which the span map already
gives us. Arrays merge as element sequences through the same clustering the
line engine uses.

**C. Line merge plus a structural repair pass.** Keep the line engine, then
parse the merged text and fix duplicate keys and commas. Cheap, but no
per-key picks, and a merge that interleaved braces is unrepairable.
Rejected.

Parser: own (JSONC only, spans exactly as the pipeline needs, no dependency
to vet) over dprint's `jsonc-parser` crate. Chosen: own.

## 4. Architecture

- **Engine in `tomte-core`, new module `structured`**, three files:
  `jsonc.rs` (lossless parser), `merge.rs` (tree differ producing a flat
  list of decisions), `splice.rs` (assembly by editing the source text in
  place). Pure Rust, no gpui, no subprocess.
- **Format seam.** `structured::detect(ours: &str, theirs: &str) ->
  Option<Format>` with `enum Format { Jsonc }`. Detection does not consult
  the file name: both texts must parse as JSONC, an empty or
  whitespace-only text counting as an empty document, and the non-empty
  roots must both be objects or both be arrays. At least one side must be
  non-empty. A later format adds a variant and its own parser; the decision
  model, splicer contract, state and UI do not change.
- **Three-way when the journal has a base, two-way otherwise.** The base
  is `MergeInputs::base` (the last-written blob). A missing base or a base
  that does not parse degrades to two-way with base := source, the same
  rule as `MergeState::new`, and the view reports it the same way.
- **Offered, not forced.** The merge editor opens in Structure mode when
  `detect` returns `Some`, with a header toggle to the existing line
  editor. Otherwise the line editor opens as today, with a one-line reason
  when the file looked like JSON but did not qualify: the target path ends
  in `.json` or `.jsonc`, or one side parsed as a JSONC object or array
  (§7). A shell rc file that parses as nothing gets no note.
- **Two stages inside the editor.** Structure (pick per row; strips in the
  result pane; the same keys and document undo as today) and Text (the
  assembled result in the editable text area). Save works from both.
- **Save path unchanged.** Both stages produce one string that is enqueued
  as the existing `Decision::Merged { resolved, inputs }` and runs through
  `ResolveEngine::resolve_merged`: template write-back and re-render
  verification, snapshot, `chezmoi apply`, commit, push, rescan, undo. The
  journal entry keeps the `merge` action name. The lock rule (§5.3) is
  computed from the same span map the write-back reads, so a structural
  save should never reach a protected-span refusal; the verifier stays the
  authority.

## 5. Engine

### 5.1 Parser (`structured/jsonc.rs`)

Hand-written, iterative (an explicit stack, never recursion), lossless.

- Accepts JSON plus `//` and `/* */` comments and trailing commas in
  objects and arrays. Does not accept single-quoted strings, unquoted keys
  or other JSON5 forms.
- Produces an arena of nodes. Every node records its byte range. An object
  entry records its key range (quotes included), its unescaped key text,
  its value node, the range of its trailing comma if any, and its leading
  trivia: the contiguous comment lines directly above it (a blank line
  ends the block and stays outside it). A same-line trailing comment after
  the value or comma belongs to the entry. Array elements record the same
  minus the key.
- Duplicate keys inside one object are kept in the tree and listed by the
  parser (`duplicates: Vec<(key, Vec<entry index>)>`).
- Errors carry the byte offset and the 1-based line.
- Round-trip invariant: the ranges of all tokens and trivia tile the input
  exactly; `concat` of them equals the input byte for byte.
- A BOM and CRLF line endings are recorded on the document so the splicer
  can follow them.

### 5.2 Decisions (`structured/merge.rs`)

`StructuredDocument::compute(base: Option<&str>, ours: &str, theirs: &str,
span_map: Option<&SpanMap>) -> Result<StructuredDocument, StructuredError>`
walks the three trees together and emits a flat `Vec<Decision>` in result
order. The document keeps the three texts and trees.

```
struct Decision {
    id: usize,                    // index, stable for undo and tests
    path: String,                 // "editor.fontSize", "plugins[2]", "[0].name"
    kind: RegionKind,             // OursOnly | TheirsOnly | BothSame | Conflict
    what: Change,                 // Value | Added | Removed | TypeChanged | Elements
                                  // (Elements = an array conflict carrying both runs)
    base: Option<SideText>,       // text + byte range in that side, None = absent
    ours: Option<SideText>,
    theirs: Option<SideText>,
    duplicated: Option<Side>,     // the key appears more than once on that side
    both_allowed: bool,           // array conflict rows only
    locked: Option<Lock>,         // Some => follows source; Lock { span: usize }
                                  // indexes the span map; the view names the
                                  // template line from it
}
```

Rules:

- **Equality is by value, never by text.** Strings compare unescaped;
  numbers by numeric value, with integers beyond f64 precision compared by
  text; objects by key set and values regardless of key order; arrays by
  element sequence. A reformatted but equal value is not a change and
  produces no decision.
- **Objects recurse.** For each key in the union of the three sides, in
  source order with disk-only keys placed after the entry that precedes
  them on disk when that entry exists in the result, else at the end of
  the container: equal on all sides → nothing; both sides objects → recurse;
  both sides arrays → §5.2 arrays; otherwise one decision for the whole
  entry. A key missing on one side is one decision of kind Added or Removed
  for that side. A type change (object on one side, scalar or array on the
  other) is one decision for the whole entry.
- **Kinds** reuse `tomte_core::merge::RegionKind`: OursOnly = disk changed
  (edited, added or removed on disk), TheirsOnly = source changed, BothSame
  = same change on both sides, Conflict = different changes. Unchanged is
  never emitted. A key removed on both sides emits nothing.
- **Arrays.** The elements of each side are encoded canonically and run
  through the line engine's clustering (`three_way` is factored out of
  `merge.rs` so the line merge and the element merge share it; the line
  engine's behaviour does not change). A one-sided region splits into one
  row per element: removed elements first, then added ones, each with its
  own strip. A conflict region stays one row carrying both sides' element
  runs, with Both allowed (disk run, then source run). Elements are
  compared whole; an element that is itself an object does not recurse.
- **Auto-resolution** follows the line engine: OursOnly → disk, TheirsOnly
  → source, BothSame → either (source text), Conflict → waits. Last written
  is offered on a row only when the base had the entry.
- **Two-way** (no base): base := source, so every difference is OursOnly
  and auto-resolves to disk, exactly as the line editor does today.
- **Duplicate keys inside one side** set `duplicated` on the key's row.
  The last occurrence is the effective value (what every JSON reader does)
  and the one whose text a pick splices; the row says "appears twice on
  disk · last value used". A key duplicated in source gets a row even when
  its value equals disk's, because the result starts from source: that
  row's splice removes the earlier copies whatever the pick, so the
  assembled result carries one copy. Under §5.3 that removal can be locked,
  in which case the copies stay as source has them and the row says so.
- **Picks.** `enum Pick { Disk, Source, LastWritten, Both }`;
  `Resolution` maps decision id → Pick with `set`, `get`, `unset`.
  `required_decisions()` lists unlocked conflicts; `assemble` errors on an
  unresolved one.

### 5.3 Locking (templated sources)

For each decision, the engine computes the splice (§5.4) that every
non-source pick would make, including glue on neighbouring entries. The
write-back maps edits at line granularity, so the rule is at line
granularity: a pick is offered only if every line of the source text that
the splice touches lies entirely inside unrepeated literal spans
(`SpanOrigin::Literal { repeated: false }`) of the span map. Otherwise the
decision is `locked`, resolves to source, counts as neither a conflict nor
a decision owed, and records the offending span's index. The view turns
that into the template line with the lookup the line editor already uses
for protected rows (`template_line_at` over `inputs.template`), so the row
can say which `{{ }}` it follows. Without a span map nothing is locked.

### 5.4 Assembly (`structured/splice.rs`)

`assemble(doc, resolution) -> Result<String, AssembleError>` starts from
the source text and applies one splice per decision that resolves away
from source, back to front so offsets stay valid. Untouched bytes are
identical to source. Rules:

- **Changed value.** Replace the value's byte range with the chosen side's
  exact value text. A multi-line value is re-indented: each inner line's
  leading whitespace has the chosen side's entry indent replaced by the
  destination entry's indent; a line that does not start with that prefix
  is left alone.
- **Added entry** (disk-only key or element, pick Disk). Insert after the
  entry that precedes it on disk when that entry exists in the result,
  else at the end of the container. The inserted text is the disk entry's
  exact text (key, value, leading comment block and same-line comment)
  re-indented to the container's indent. A separating comma is inserted
  immediately after the previous entry's value, before any same-line
  comment. The new last entry gets a trailing comma only if the container
  already ends its last entry with one. An empty container is opened with
  a newline and the indent of the container's siblings.
- **Removed entry** (pick Disk on a key removed on disk, or Source on a key
  added on disk). Remove from the start of its leading comment block
  through the end of its line, newline included. If it was the last entry
  and the container does not use trailing commas, the previous entry's
  comma is removed too, and that touch is part of the splice for §5.3.
  Braces always stay.
- **Both** (array conflicts only). The result holds the disk run then the
  source run, each element with the glue above.
- **Array rows** use the same insert, remove and replace rules per element;
  an inserted element goes where the sequence diff places it, after the
  result element that corresponds to the hunk's base position.
- **Indent unit.** The indent of the container's existing entries; for an
  empty container, the parent entry's indent plus the file's unit, detected
  from the first indented line, defaulting to two spaces.
- **Line endings and BOM** follow source.
- **Unresolved conflicts.** `assemble` errors. For display, the seed text
  (§6) uses the source value for an unresolved row so the pane is always
  valid JSON.
- **Guard.** The result is re-parsed before it leaves the engine; a parse
  failure is `AssembleError::Invalid { error }`, never text.

## 6. UI and data flow

**Files.** Pure state in a new `crates/app/src/structured_state.rs`
(`StructuredState { doc, resolution, cursor, degraded_base, stage }`,
with `pick`, `unresolved`, `progress`, `next_unresolved`, `next_changed`,
`assembled`, mirroring `MergeState`). Rendering in a new
`crates/app/src/views/merge_structured.rs` (strip specs, pane tints,
header controls, stage buttons), so `merge.rs` does not grow. The decision
strip in `crates/ui/src/components/merge_region.rs` gains a path label and
a `StripState::Locked { reason }` variant with no buttons.

**Load.** `merge_inputs::load` is unchanged. The background load step that
runs it also runs `structured::detect` and `StructuredDocument::compute`,
so `MergeLoad::Ready` carries the line document and, when available, the
structured one. Origin is not offered in Structure mode in this round: the
Origin pick and fourth pane stay with the line editor, and when
`inputs.origin` is `Some` the Structure header says "origin differs · see
Lines".

**Mode.** A segmented "Structure | Lines" control in the header, shown
only when both exist; Structure is the default when available. Switching
discards the current mode's picks, after a confirmation when any were
made. The choice lives for the editor session only.

**Structure stage.** The result pane is the existing read-only
`ui::TextArea` seeded with the assembled text, one strip block above each
decision's first line in the result. A decision that resolves to "absent"
(a removed entry) keeps its strip at the line where the entry would be. A
strip shows the path, the change in words ("added on disk", "removed on
disk", "changed in source", "conflict", "appears twice on disk", "type
changed") and the buttons: auto, disk, source, last written (when the base
had the entry), both (array conflict rows only), edit. A conflict shows
both sides' text as tinted provenance rows (amber = disk, blue = source) as
today. A locked row has no buttons and reads "templated · follows source",
with the template line on hover. The three input panes show each side's
full text with lines tinted by the decision that covers them; the panes
scroll independently in this mode, and Next or a strip click jumps all
three to that decision's line on their side. Keys 1, 2, 3, b, n and ⌘Z
route to the structured state; document undo entries are per pick with
the cursor restored, as in the line editor.

**Text stage.** Edit, from the header or any strip's edit button, flips
the editor editable with the assembled text and hides the strips, as the
line editor does. The header offers Save and "Back to structure", which
restores the strips with the picks intact and discards text edits, after a
confirmation when the text differs from the seed. ⌘Z in this stage is the
text area's own history. A chip reads "not valid JSON · line N" when the
editor text fails to parse; Save stays allowed.

**Save.** Both stages produce one string. Structure saves
`state.assembled()`; Text saves the editor text verbatim. Either enqueues
`Decision::Merged { resolved, inputs }` as today; the engine, the journal
(`merge`), the banner and undo do not change.

**Gallery.** Three poses in `fixtures.rs`: `merge-structured` (a conflict,
an added key, a removed key, array rows), `merge-structured-locked` (a
templated source with a locked row), `merge-structured-text` (the text
stage with the invalid-JSON chip).

## 7. Error handling

Every case names what the user sees.

- **Disk or source does not parse.** The line editor opens with a muted
  header note "disk is not valid JSON (line 12): line merge" (or "source
  …"), shown only when the file looked like JSON (§4). No Structure
  toggle. An empty side parses as an empty document, so
  a deleted or not-yet-created file still merges structurally with every
  entry one-sided.
- **Base missing or not parseable.** Two-way against source, the existing
  "no last-written snapshot" degraded note, and no last-written button on
  any row.
- **Root is a scalar, or the roots differ in type.** Line editor with the
  note "disk and source are different JSON shapes: line merge".
- **Assembled result fails to re-parse.** An engine bug, never user error.
  The view shows the existing message-box state with the parse error,
  logs it with `log_error!`, keeps the picks, and offers Lines. Save is
  unreachable from a failed assembly.
- **Locked rows and the write-back.** A structural save should never reach
  a protected-span refusal. If one does, the existing banner and journal
  rejection fire unchanged and name the rendered range; that is a lock-rule
  bug to fix, not a state to live with.
- **Text stage, invalid JSON.** The warning chip only; Save is the user's
  call.
- **Conflicts without a pick.** Save disabled with "N conflicts left", as
  today. Locked rows never count.
- **Duplicate keys.** Never an error (§5.2).
- **Oversize or deep input.** The parser is iterative, so nesting depth
  cannot overflow the stack. No size cap beyond the existing text-only rule
  (`MergeInputsError::Binary`).
- **Nothing to decide.** Both sides parse and differ only in formatting:
  zero decisions, the result equals source, and the header says "same
  settings, formatting differs · Save keeps the source layout".

## 8. Testing

Each test names, in a comment, the mutation that fails it, and the
mutation is run (house rule).

**Parser** (unit tests beside `jsonc.rs`)
- Round trip over a fixture corpus of real settings files checked in under
  `crates/core/tests/fixtures/jsonc/` (Zed, VS Code, Claude Code, opencode,
  each with comments and trailing commas where the tool allows them): the
  ranges tile the input byte for byte. Mutation: drop a comment's range.
- Comments in every position, trailing commas, string escapes, nested
  empty containers, duplicate keys listed, errors with byte and line.
  Mutation: swallow a parse error into an empty document.
- Nesting 10,000 levels deep parses. Mutation: recursive descent.

**Engine** (table-driven, beside `merge.rs` and `splice.rs`)
- One case per decision kind, per side, per container: scalar change, add,
  remove, type change, nested object, array insert, delete, edit, same
  change on both sides, conflict. Each asserts the decision list and the
  assembled text for every allowed pick.
- Comma glue in every position: first, middle, last, only entry, empty
  container, trailing-comma style preserved, same-line comment after the
  previous value. Mutation: always append a comma.
- Comments and blank lines travel with their entry; the indent delta is
  applied to multi-line values; CRLF and BOM preserved.
- Property: all picks Source gives source byte for byte; all picks Disk on
  a two-way merge gives a document value-equal to disk. Mutation: splice an
  unchanged entry.
- Equality by value: reformatted but equal values give zero decisions.
- Duplicate keys: last value wins, one copy in the result.
- Locking against a real span map from `lex` and `anchor`: a templated
  value, a templated key, and a removal whose comma fix would touch a
  templated line, each locked with the template line named; then
  `write_back` on the assembled text succeeds for every unlocked pick.
  Mutation: lock the value range only.
- The re-parse guard returns `AssembleError::Invalid`, never text.
- The factored `three_way` leaves every existing `merge.rs` test green.

**App state** (`structured_state.rs`)
- Pick, cursor advance, progress, undo entries, stage transitions, "back
  to structure" keeping picks, two-way without a base offering no last
  written. Same shape as `merge_state.rs`'s tests.

**View** (`#[gpui::test]` in `views/mod.rs`)
- JSON inputs open in Structure with one strip per decision; a strip click
  changes the result text; Edit enters the text stage and Save enqueues the
  editor text verbatim; Back to structure restores the strips; a locked
  strip has no buttons; the Lines toggle shows the line editor. Mutation
  per test.

**End to end** (`crates/app/tests/resolve_e2e.rs`)
- A JSON target with a disk-added key and a source-changed key: the
  structured save lands both on disk and in source, commits, and undo
  restores the file. A templated JSON source: a disk pick on a literal key
  lands through the write-back, and chezmoi re-renders to the saved text.

**Gallery.** The three poses render in the smoke test and are shot and
read in dark and light.

## 9. Out of scope, deliberately

- TOML and other formats: the `Format` seam only.
- Origin in Structure mode (the line editor keeps it).
- Editing one row's value inline; the Text stage covers refinement.
- A structural view in Review's diff preview.
- JSON5 forms (single quotes, unquoted keys, hex numbers).
- Recursing into array elements that are objects; an element is compared
  and picked whole.
