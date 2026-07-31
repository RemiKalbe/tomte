# Merge Editor v2 — every region a decision, real hand-editing

**Problem (user report, 2026-07-30):** the merge editor auto-resolves non-overlapping changes and declares "No conflicts to resolve" — no region visibility, no override, no manual editing. Auto-merge ("keep both") isn't offered as a quick action either.

**Grounding:** Zed deep-dive (workflow wf_83381604-e80, brief at scratchpad/zed-brief.json + full per-area findings in its journal; conflict_view.rs also read directly). Zed's model is buffer-text-is-truth with destructive dissolve-on-pick; ours is Resolution-is-truth with an immutable document — simpler and strictly more reversible (every pick revisitable). We copy Zed's correctness patterns, not its machinery.

## Copy faithfully (Zed receipts in brief)
- One `apply()` reconcile funnel for pick/edit/undo; UI fully derived from Resolution.
- Always-visible decision strip above every changed region: `Ours / Theirs / Base / Both / Edit`, small labeled buttons (conflict_view.rs:325-445); "Use Both" = ours-then-theirs (disk first).
- Full `EntityInputHandler` incl. marked text — IME correctness is non-negotiable.
- Zed undo policy: 300ms grouping, drop-empty, clear-redo-on-edit, finalize-before-programmatic, IME composition = one undo step.
- Small-editor embedding kit: wrapper-owns-chrome, commit-on-focus-out, key-context switch ("MergeEditor" / +"RegionEditor"), stop_propagation shields, fixed line budget (never flex-grow).
- Structural read-only: provenance rows are divs, not clamped keystrokes.

## Deliberately skip
Rope/CRDT/anchors (region-sized `String` + two-branch cursor fixup), BlockMap (we interleave composed elements), marker re-parsing (regions come from the engine), multibuffer/multi-cursor/LSP/minimap/scroll anchors.

## Components
1. `czui_core::merge::Choice::Both` — first-class; `assemble()` emits ours then theirs. NOT `Edited(concat)`: preserves provenance and per-half tinting.
2. czui-ui: `MergeTints` theme tokens (**ours/disk = drift amber, theirs/source = accent blue** — our established mapping, deviating from the brief's GitHub purple), `decision_strip` (StripState: Undecided/Decided{choice}/Editing), `provenance_rows` (tinted blocks + protected glyphs), `region_frame`. All in `comp:` previews.
3. `MergeEditorView` (merge.rs rework): derived row walk over `doc.regions` in a plain scrollable column (uniform_list dropped; dotfile scale). Undecided conflict: strip + both sides tinted. Auto-resolved / decided: materialized lines + "decided: X · revisit". Document undo stack `{region, prev, next, cursor}` (cursor restored). gpui actions + key bindings under "MergeEditor" context: 1/2/3/b pick, e edit, n/p conflict nav, cmd-z/shift-cmd-z — retiring the deferred fake-hints item honestly.
4. `czui_ui::text_area` (~600–800 lines, isolated `comp:` first): String buffer, byte-offset cursor/selection, custom Element (shape_line + PositionMap, selection quads → glyphs → bar cursor), blink entity, full EntityInputHandler in UTF-16, TransactionHistory. 0.2.2 signature drifts noted in brief (focus() arity, 4-arg ShapedLine::paint, no pixel_snap). Manual acceptance: Option-e, press-and-hold, CJK source, undo through composition.
5. Protected spans: advisory in-editor clamp + warning border + gutter glyphs (mapped from SpanMap into region byte space); save-time re-render check REMAINS the authority. Policy: `Both` on a protected region allowed, verifier must tolerate duplicated spans — else strip flags it (decide at step 8 with a test).
6. Review pane "Keep both" quick action: detail load dry-runs the 3-way merge; zero conflicts and result differs from both sides → third header button writing assembled output through resolve_merged (snapshot/undo pipeline). Merge-editor button carries accent when the dry-run has real conflicts.

## Undo composition (approved)
Region editor focused → ⌘Z hits TextArea history. commit_edit finalizes text history, pushes ONE document entry. Document-undo of Edited discards stale editor history; reopening reseeds.

## Build order (9 steps, value ships at 3)
Per brief build_order: 1 engine Both → 2 tints+builders+previews → 3 MergeEditorView (usable pick-anything editor) → 4 TextArea skeleton → 5 IME → 6 TransactionHistory → 7 integration → 8 protected spans → 9 hardening. Screenshot-verify per step (gallery states per new sub-state); tests per step; commit per step.
