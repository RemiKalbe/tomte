# chezmoi-ui v0 — Plan 7: Merge Editor & Packaging

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The approved mockup-A merge editor — three panes (theirs = chezmoi's rendered state / base = last-written / ours = disk) over a result pane assembled from per-region choices, with template-protected write-backs — plus packaging: `Chezmoi UI.app` (LSUIElement) with a LaunchAgent for `chezmoid`. This completes v0.

**Standing on (all tested since Plan 2, `czui-core`):** `MergeDocument::compute/regions/required_decisions/assemble`, `Choice/Resolution`, `word_diff`, template `lex/anchor/write_back/verify_write_back` with protected spans. Plan 6's `ResolveEngine` provides sessions/snapshots/expect/commit plumbing to extend.

**Scope decisions (state honestly in UI where relevant):**
- Region resolution is **choice-based** (take ours / take theirs / take base). Free-text region editing needs a text-input widget gpui doesn't provide — deferred (the "open in editor" escape hatch covers it).
- The editor resolves **destination ↔ rendered-source** drift (incl. Conflict class) — Remi's daily pain. True git-diverged repos (machine A vs B committed independently) stay deferred: `sync_all` covers fast-forward; diverged repos get an honest error directing to the terminal (v0 spec §5 stage-1 is Plan 8 material).
- Templated files: resolution goes through `write_back` + `verify_write_back`; protected-span violations surface as "this change touches a templated value — open in editor" (assisted-manual live-rerender UI needs text input too; deferred).
- **Base pane content**: last-written content = journal blob for the state-dump `contentsSHA256` when available (daemon has been snapshotting since Plan 4); otherwise the pane shows "(no snapshot of last-written state)" and merge degrades to 2-way (base := rendered).

## Global Constraints

Plan 1's gate (fmt/clippy `-D warnings` all-features/tests) before every commit; no unwrap/expect in lib code; no new dependencies except in packaging scripts; all engine calls off the main thread; 1Password commit protocol as before. UI: Theme tokens only; Zed-idiom density; render smoke tests must cover the new view.

## File Structure

```
crates/app/src/merge_inputs.rs     # load ours/base/theirs + SpanMap for a target
crates/app/src/resolve.rs          # + resolve_merged(target, resolved_text)
crates/app/src/views/merge.rs      # MergeView (three panes + result + controls)
crates/app/src/views/mod.rs        # Route::Merge wiring
crates/app/src/views/review.rs     # "Open merge editor" goes live
crates/app/tests/resolve_e2e.rs    # + stories F/G/H (merged save, template write-back, protected rejection)
scripts/bundle.sh                  # release build → Chezmoi UI.app
scripts/install-launchagent.sh     # ~/Library/LaunchAgents plist + bootstrap
bundle/Info.plist                  # LSUIElement=true, bundle id com.remikalbe.chezmoi-ui
```

---

### Task 1: Merge inputs + engine save path

**Files:** create `crates/app/src/merge_inputs.rs` (+ `pub mod merge_inputs;` in lib.rs); modify `crates/app/src/resolve.rs`.

**Interfaces:**
```rust
// merge_inputs.rs — all blocking; callers use the background executor.
pub struct MergeInputs {
    pub target: PathBuf,
    pub ours: String,            // destination file (lossy UTF-8; binary → Err)
    pub theirs: String,          // chezmoi cat (rendered source state)
    pub base: Option<String>,    // journal blob of last-written, if snapshotted
    pub source_path: PathBuf,
    pub templated: bool,         // source ends in .tmpl
    /// Present when templated: protected spans over `theirs` (rendered).
    pub span_map: Option<czui_core::template::anchor::SpanMap>,
}
pub enum MergeInputsError { Binary, Chezmoi(..), Journal(..), Io(..), Lex(..) } // thiserror, #[from]s
pub fn load(chezmoi: &ChezmoiClient, journal_path: &Path, target: &Path)
    -> Result<MergeInputs, MergeInputsError>;
```
- `base`: `state_dump()` → entry `contentsSHA256` → `Journal::open_read_only(journal_path).get_blob(hash)`; any miss → `None`.
- `span_map`: templated only — read source file, `lex` + `anchor(template, segments, &theirs)`.

```rust
// resolve.rs — extends ResolveEngine
impl ResolveEngine {
    /// Persist a merge-editor resolution: write the resolved text into the
    /// source (plain file: verbatim; templated: write_back + verify), then
    /// apply the target so all states converge. Full session/undo plumbing.
    pub fn resolve_merged(&self, inputs: &MergeInputs, resolved: &str)
        -> Result<ResolveOutcome, ResolveError>;
}
```
Behavior: SessionStart → SnapshotBlobs([target, source_path]) → decision `{action:"merge", target, blobs}` → ExpectChanges both paths →
- plain: `std::fs::write(source_path, resolved)`;
- templated: `czui_core::template::writeback::write_back(template, span_map, theirs, resolved)`; on `ProtectedSpanTouched`/`RepeatedLiteral` → `Ok(ResolveOutcome::NeedsMergeEditor)`? NO — new outcome variant `ResolveOutcome::ProtectedSpan { detail: String }` (add it; UI copy: "this change touches a templated value — open in editor"); on success `verify_write_back(chezmoi, &new_template, resolved)` — mismatch → same `ProtectedSpan` outcome with the verify note, and the source file is NOT modified (write only after verify passes: write to a temp String first, verify via `execute_template`, then persist);
- then `chezmoi apply <target>` (has `--force`), commit phase (reuse), SessionEnd, Rescan.

TDD: unit tests for the templated/plain branching with FakeRunner where cheap; full coverage in Task 4 stories. Commit: `feat(app): merge inputs loader and resolve_merged engine path`.

- [x] Failing tests → implement → gate → commit.

---

### Task 2: MergeState (pure editor model)

**Files:** create `crates/app/src/merge_state.rs` (+ lib.rs export).

```rust
pub struct MergeState {
    pub doc: czui_core::merge::MergeDocument,   // compute(base_or_theirs, ours, theirs)
    pub resolution: czui_core::merge::Resolution,
    pub cursor: Option<usize>,                  // focused region index
    pub degraded_base: bool,                    // base was None → 2-way
}
impl MergeState {
    pub fn new(inputs: &MergeInputs) -> Self;   // base.unwrap_or(theirs) as base
    pub fn conflicts(&self) -> Vec<usize>;      // doc.required_decisions()
    pub fn unresolved(&self) -> Vec<usize>;     // conflicts without a choice
    pub fn progress(&self) -> (usize, usize);   // (decided, total conflicts)
    pub fn pick(&mut self, region: usize, choice: czui_core::merge::Choice);
    pub fn next_unresolved(&mut self) -> Option<usize>; // advances cursor
    pub fn assembled(&self) -> Option<String>;  // Some when fully resolved
}
```
Unit tests: conflict bookkeeping, pick/override, next_unresolved wrap-around, assembled None-until-done then equals `doc.assemble`. Commit: `feat(app): pure MergeState for the merge editor`.

- [x] TDD → gate → commit.

---

### Task 3: Merge editor view

**Files:** create `crates/app/src/views/merge.rs`; modify `views/mod.rs`, `views/review.rs`, `main.rs` (route only if needed).

**Behavior (mockup A, full-window takeover):**
- `Route::Merge` renders `MergeView` (an `Entity`, lazily created like Review, holding `Option<LoadedMerge>`: inputs + MergeState). Entered via Review's "Open merge editor" (now live, enabled when a target is selected and the engine global exists): spawns `merge_inputs::load` on the background executor → entity update.
- **Header bar**: file name + muted path; provenance chips ("source (rendered)" / "last written" / "on disk"); progress "N of M regions need you" (ok tint when 0); `next ↓` button (next_unresolved); Cancel (back to Review) and **Save** (enabled when `assembled().is_some()`).
- **Panes row** (three equal columns, min_w_0, bordered): theirs / base / ours, each a read-only line-rendered pane (uniform_list of pre-split lines, `text_sm` monospace via `.font_family("Menlo")` — check the exact gpui text API in local source; fall back to default font if family setting is awkward). Region backgrounds: conflict rows tinted `wash(conflict, 0.10)`, ours-only `wash(drift,0.08)`, theirs-only `wash(accent,0.08)`; base pane shows "(no snapshot of last-written state)" watermark when degraded.
- **Result pane** (below, taller): assembled-so-far rendering: resolved regions show their chosen text; unresolved conflicts render a placeholder row `‹pick one: ours | theirs›` with two inline buttons (+ `base` when not degraded); protected spans (templated files) render with a 🔒-prefixed amber underlay in the THEIRS pane rows they map to (span ranges are over `theirs`; approximate to full lines containing a protected byte — precision polish deferred).
- Choosing updates MergeState via the entity; Save → `resolve_merged` on background executor → outcome banner (reuse Review's `OutcomeBanner` — move it to a shared spot or re-export) → on success route back to Review with the banner shown there.
- Render smoke test: MergeView with a synthetic conflict document renders (no subprocess).

Commit: `feat(app): three-pane merge editor with per-region choices`.

- [x] Implement per contract → smoke test → gate → commit.

---

### Task 4: E2E stories F/G/H

**Files:** modify `crates/app/tests/resolve_e2e.rs` (reuse `DriftLab`).

- [x] **F merged save (plain)**: drift `.testrc` so dest and source BOTH changed (edit source file + dest differently → Conflict class); `merge_inputs::load` → MergeState → pick ours for the conflict → `resolve_merged` → source == resolved == dest after apply; committed; session journaled; in-sync after.
- [x] **G templated write-back**: `dot_testrc.tmpl` with a `{{ .hostname }}`-style value (scratch config data provides it); drift dest in a LITERAL region; resolved keeps the templated value → `resolve_merged` → the `.tmpl` gains the literal edit, `{{ }}` survives verbatim, verify passed, dest applied; in-sync.
- [x] **H protected rejection**: same template, resolved text alters the templated VALUE region → outcome `ProtectedSpan`, `.tmpl` byte-identical, dest untouched, no partial state.

Commit: `test(app): merge-resolution e2e stories incl. template write-back`.

---

### Task 5: Packaging

**Files:** create `scripts/bundle.sh`, `scripts/install-launchagent.sh`, `bundle/Info.plist`.

- [x] `bundle/Info.plist`: CFBundleIdentifier `com.remikalbe.chezmoi-ui`, CFBundleName "Chezmoi UI", CFBundleExecutable `chezmoi-ui`, **LSUIElement true** (accessory — no Dock), CFBundleShortVersionString 0.1.0, LSMinimumSystemVersion 13.0.
- [x] `scripts/bundle.sh`: `cargo build --release -p czui-app -p czui-daemon` → assemble `target/bundle/Chezmoi UI.app/Contents/{MacOS/{chezmoi-ui,chezmoid},Info.plist}` → `codesign --force --deep -s - "…app"` (ad-hoc) → print the path. Idempotent; `set -euo pipefail`.
- [x] `scripts/install-launchagent.sh`: writes `~/Library/LaunchAgents/com.remikalbe.chezmoid.plist` (ProgramArguments → the BUNDLED chezmoid, RunAtLoad, KeepAlive true, StandardOut/ErrPath → `~/Library/Application Support/ChezmoiUI/chezmoid.launchd.log`) then `launchctl bootstrap gui/$UID` (with bootout-first for idempotence). Prints how to uninstall. DOES NOT run automatically — user-invoked.
- [x] Smoke (agent-safe): run bundle.sh, assert bundle structure + codesign verifies + `"…/Contents/MacOS/chezmoid" --once` works with CZUI_JOURNAL in a temp dir. Do NOT install the LaunchAgent and do NOT launch the .app GUI (user-attended).
- [x] Commit: `feat: bundle script, Info.plist, and LaunchAgent installer`.

---

### Task 6: Close-out (user-attended)

- [x] Full gate; tick plan checkboxes; update project-state memory.
- [x] Hand off to Remi: (1) relaunch dev app → Review → pick the remaining conflict-y file → Open merge editor → resolve region-by-region → Save → verify convergence + Undo; (2) optionally `./scripts/bundle.sh` + open the .app + `./scripts/install-launchagent.sh` for the daemon-at-login experience. This is v0's acceptance.

---

## Self-Review Notes
- Spec coverage: §7.3 mockup-A editor (choice-based; free-text + assisted-manual deferred with honest UI); §6.2 write-back + mandatory verify with no-partial-writes ✓; §6.3 sessions/undo reused ✓; §12 packaging + LaunchAgent (SMAppService swapped for launchctl scripts — simpler, user-invoked; revisit for App Store-grade later).
- Known deferrals (Plan 8 / vNext): git-diverged repo merges, free-text region editing, assisted-manual template mode, source-side undo, styled menubar popup, journal history pruning.
- Type consistency: `MergeInputs`/`MergeState`/`ResolveOutcome::ProtectedSpan` names shared across Tasks 1–4; `OutcomeBanner` reuse noted in Task 3.
