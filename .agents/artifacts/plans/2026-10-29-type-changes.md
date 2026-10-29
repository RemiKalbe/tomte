# Type Changes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A managed entry whose type differs between the chezmoi source and the disk (file, folder, link, or a link pointing elsewhere) shows as one Review row and resolves either way, alone or in a batch, mid-merge, with Undo.

**Architecture:** tomte-core detects type changes before `chezmoi status` runs (source kinds from `chezmoi managed --include`, disk kinds from `symlink_metadata`), suppresses the per-file rows under a changed folder, and reports each as a `FileDrift` of class `TypeChanged` carrying a `TypeChange`. The wire carries it as `DriftSummary.type_change`. The engine's existing `keep_disk` / `keep_source` / `resolve_batch` branch on it: Keep disk is `chezmoi add` (or `forget` for a removed link), Keep source is `apply --force`; Undo learns to restore a link and a folder. Review draws a two-line row and a "file list" detail pane with copy picks.

**Tech Stack:** Rust 2024, GPUI 0.2.2, chezmoi 2.70.4 (CI: brew), serde, rusqlite journal.

**Spec:** `.agents/artifacts/specs/2026-10-29-type-changes-design.md`

## Global Constraints

- Every task runs the AGENTS.md gate in the foreground before its commit: `cargo fmt --all --check`; clippy `-D warnings` exit 0 AND an empty `rg -n '^(error|warning)' /tmp/clippy.log | rg -v 'block v0\.1\.6|proc-macro-error2'`; `cargo test --workspace`; the same with `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null`.
- Every new test carries a `/// MUTATION:` line naming the production change that fails it, and the implementer applies that mutation to PRODUCTION code, sees the test fail, and reverts.
- No background cargo runs, no monitors, no sleeps in tests.
- Commits are SSH-signed via 1Password. On `1Password: failed to fill whole buffer`, leave the work staged and report it; never `--no-gpg-sign`, never touch git config. Messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_019Ly84NKVSjcQEhVk14LvJU`.
- Never edit `~/.config` or the user's dotfiles; tests use `tomte_core::testsupport::Scratch` and `resolve_e2e.rs`'s `DriftLab`.
- `chezmoi re-add` / `apply_many` / `forget` / the new `add` with an empty list are hard errors.
- Wording (verbatim, from the spec): class labels "replaced by a link", "replaced by a folder", "replaced by a file", "link points elsewhere", "link removed"; source-moved labels "source is now a link", "source is now a folder", "source is now a file", "source link points elsewhere". Buttons: Keep disk side "Keep the link" / "Keep the folder" / "Keep the file" / "Keep the new link" / "Remove from source" (link removed); Keep source side "Restore the link" / "Restore the folder" / "Restore the file". Comparison group headers: "Only in source · removed from source if you keep the link", "Differs", "Only in the link target · not managed", "Same on both sides". Pick label "copy into the link target".
- Views use Theme tokens only, reusable styling goes in `crates/ui` first, no raw hex; `.truncate()` never ellipsizes (use `line_clamp(1)` in a fixed width); never update an entity inside its own update closure.
- Old daemons/apps interoperate: every new wire field is `#[serde(default)]`.

## Spec refinements made while planning (recorded in the spec by Task 1's commit)

1. "was" is what the SOURCE says, from `chezmoi managed --include dirs|symlinks` and `chezmoi cat` for link text; the state dump is used only to tell which side moved (state kind == disk kind ⇒ the source moved). Comparing state to disk alone misses a type change made in the source on another machine and has no link text for links.
2. Scoped status passes every managed target outside the type-changed folders with `--recursive=false` (verified in Task 2 step 1), not a minimal cover; simpler and the argument list for ~1,000 targets is ~60 KB.
3. The source side of a folder comparison is the raw source file, or `chezmoi cat` for a `.tmpl` / `encrypted_` file.

## Review Focus

1. A folder replaced by a link whose target is ALSO under a managed folder (`~/.claude/skills/x` → `~/.agents/skills/x`, both managed trees): the linked folder's files must not be counted twice or suppressed on the `.agents` side. Pinned in Task 2 (`linked_target_inside_another_managed_tree_keeps_its_own_rows`).
2. A dangling link (target missing) where the source holds a folder: detection must not follow it, the comparison lists everything as "only in source", and Keep the link still works. Pinned in Task 2 and Task 4 (`keep_link_over_a_dangling_link`).
3. A templated link (`symlink_x.tmpl`) whose rendering fails: the row is `eval_failed`, never a type change against an empty text. Pinned in Task 2 (`templated_link_that_fails_to_render_is_eval_failed`).
4. A file event under a linked folder arriving between scans (per-event path) must not resurrect file rows. Pinned in Task 2 (`probe_one_under_a_linked_folder_returns_nothing`).
5. Undo after Restore the folder when the user has since written a new file into the restored folder: Undo refuses with a message naming the file instead of deleting it. Pinned in Task 4 (`undo_restore_folder_refuses_when_the_folder_gained_a_file`).

---

### Task 1: Type-change model and chezmoi calls (tomte-core)

**Files:**
- Create: `crates/core/src/type_change.rs` (module `tomte_core::type_change`, declared in `crates/core/src/lib.rs`)
- Modify: `crates/core/src/chezmoi.rs` (new client methods + unit tests)
- Modify: `.agents/artifacts/specs/2026-10-29-type-changes-design.md` (the three refinements above, under a "Refinements" heading)

**Interfaces:**
- Produces:
  - `pub enum EntryKind { Absent, File, Folder, Link(String) }` — `Debug, Clone, PartialEq, Eq`.
  - `pub struct TypeChange { pub source: EntryKind, pub disk: EntryKind, pub disk_moved: bool }` — `Debug, Clone, PartialEq, Eq`. `disk_moved` false means the source changed and the disk is what chezmoi last wrote.
  - `pub fn disk_kind(path: &Path) -> std::io::Result<EntryKind>` — `symlink_metadata`; NotFound ⇒ `Absent`; symlink ⇒ `Link(read_link as written, lossy)`; dir ⇒ `Folder`; else `File`. Never follows a link.
  - `pub fn type_change(source: &EntryKind, disk: &EntryKind, state_kind: Option<&str>) -> Option<TypeChange>` — `None` when equal, or when `disk` is `Absent` and `source` is `File` (the existing deleted-file path owns that). `disk_moved = state_kind != Some(kind_name(disk))` where `kind_name` maps File→"file", Folder→"dir", Link→"symlink".
  - `pub enum CompareGroup { OnlyInSource, Differs, OnlyOnDisk, Same }` and `pub struct FolderComparison { pub entries: Vec<(PathBuf, CompareGroup)> }` (relative paths, sorted by group in that order, then by path). `OnlyOnDisk` holds top-level names only, folders with a trailing `/` in the path's last component.
  - `pub fn compare_folder(source: &[(PathBuf, ContentHash)], disk_root: &Path) -> std::io::Result<FolderComparison>` — `source` holds relative paths and the hash of the source version; the disk side is read through `disk_root` (a link is followed here on purpose: that is the content the user now has).
  - `ChezmoiClient::managed_kinds(&self) -> Result<ManagedKinds, ChezmoiError>` with `pub struct ManagedKinds { pub folders: BTreeSet<PathBuf>, pub links: BTreeSet<PathBuf> }` — two invocations: `managed --path-style=absolute --include dirs` and `... --include symlinks`.
  - `ChezmoiClient::link_text(&self, target: &Path) -> Result<String, ChezmoiError>` — `cat`, one trailing `\n` stripped; `Eval` errors stay `Eval`.
  - `ChezmoiClient::add(&self, targets: &[PathBuf]) -> Result<(), ChezmoiError>` — `add --force <targets>` in one invocation; empty slice refused like `forget`.
  - `ChezmoiClient::status_of(&self, targets: &[PathBuf]) -> Result<Vec<StatusEntry>, ChezmoiError>` — `status --path-style=absolute --recursive=false <targets>`, same parsing as `status`; empty slice refused.

- [ ] **Step 1: Write the failing tests** in `type_change.rs`:
  - `disk_kind_reports_a_link_without_following_it` (tempdir: a link to a dir ⇒ `Link("real")`; a dangling link ⇒ `Link("gone")`; a dir ⇒ `Folder`; missing ⇒ `Absent`). MUTATION: use `metadata` instead of `symlink_metadata`.
  - `type_change_table`: `(Folder, Link(".real"), Some("dir"))` ⇒ `Some(disk_moved: true)`; `(Link(".real"), Link(".other"), Some("symlink"))` ⇒ `Some(disk_moved: true)` (the state holds no link text, so two links always read as the disk moving); `(Link(".real"), Folder, Some("dir"))` ⇒ `disk_moved: false` (source moved); `(File, Absent, _)` ⇒ `None`; `(Folder, Folder, _)` ⇒ `None`. MUTATION: drop the File/Absent exclusion.
  - `compare_folder_groups_and_orders` (source `one`=h(a), `two`=h(b), `gone`=h(c); disk `one`=a, `two`=b2, `three`, `sub/x`) ⇒ `[gone:OnlyInSource, two:Differs, sub/:OnlyOnDisk, three:OnlyOnDisk, one:Same]`. MUTATION: list nested disk files instead of top-level names.
  In `chezmoi.rs` (FakeRunner): `add_forces_and_refuses_an_empty_list`, `status_of_is_not_recursive`, `link_text_strips_one_newline`, `managed_kinds_reads_dirs_and_symlinks`. Each with its MUTATION (drop `--force`; drop `--recursive=false`; trim all whitespace instead of one `\n`; swap the two include values).
- [ ] **Step 2:** `cargo test -p tomte-core --lib type_change` and `cargo test -p tomte-core --lib chezmoi` — FAIL (unresolved items).
- [ ] **Step 3:** Implement the items above.
- [ ] **Step 4:** Same commands — PASS. Run each MUTATION.
- [ ] **Step 5:** Gate, then commit `feat(core): type-change model, add, scoped status` (with the spec refinements).

### Task 2: The scanner reports type changes

**Files:**
- Modify: `crates/core/src/scanner.rs`, `crates/core/src/drift.rs`
- Test: `crates/core/tests/scanner_integration.rs`

**Interfaces:**
- Consumes: everything Task 1 produces.
- Produces:
  - `DriftClass::TypeChanged` (new variant; `classify` never returns it).
  - `FileDrift.type_change: Option<TypeChange>` (`Some` exactly when `class == TypeChanged`).
  - `DriftScanner::scan` unchanged signature; `probe_one` unchanged signature.

Algorithm (scan, before step (a)):
1. `kinds = chezmoi.managed_kinds()`. For each managed target `t`: `source = Folder` if in `kinds.folders`, `Link(chezmoi.link_text(t))` if in `kinds.links` (an `Eval` error ⇒ push an `EvalFailed` `FileDrift` for `t` and skip), else `File`. `disk = disk_kind(t)` (an io error ⇒ `unprobed`). `tc = type_change(&source, &disk, state kind)`.
2. Each `tc` becomes a `FileDrift { class: TypeChanged, type_change: Some(tc), probe: StateProbe { destination: None, rendered: Ok(None), last_written: None, git: default }, source_rel }`.
3. `changed_roots` = the targets with a `tc` whose `source` or `disk` is `Folder` or `Link`. Every managed target strictly under a root leaves `candidates`, `counted` and `managed_file_count`, and is never probed.
4. Step (c): when `changed_roots` is empty, `status()` as today; else `status_of(managed targets not under and not equal to any root)`.
5. A link target with no `tc` counts toward `in_sync_count`.
`probe_one(target)`: if `target` or any managed ancestor of it has a type change (same detection for that one ancestor chain), return the ancestor's `FileDrift` when the ancestor IS `target`, else `Ok(None)`.

- [ ] **Step 1: Verify the status flag.** In a Scratch home with `dot_d/one` and an edited `.f`, replace `~/.d` by a file; run `chezmoi status --recursive=false <.f>`; expect `MM .f` and exit 0. If the flag is rejected, stop and report (`NEEDS_CONTEXT`).
- [ ] **Step 2: Write the failing tests** (Scratch homes; one per case):
  - `folder_replaced_by_a_link_is_one_type_change` — `TypeChange { source: Folder, disk: Link(".real"), disk_moved: true }`, no drift for `.d/*`, and an unrelated edited `.f` still drifts. MUTATION: skip step 3 (child suppression).
  - `folder_replaced_by_a_file_does_not_break_the_scan` — scan is `Ok`, reports the type change and the edited `.f`. MUTATION: always call `status()`.
  - `link_retargeted_and_link_removed` — `.l` → `.other` ⇒ `Link(".real")`→`Link(".other")`; `.l` deleted ⇒ disk `Absent`. MUTATION: compare kinds only, not link text.
  - `file_replaced_by_a_link` — `.f` ⇒ `File`→`Link(...)`, no `destination_drift` row for `.f`. MUTATION: run detection after step (a).
  - `source_changed_type_is_not_disk_moved` — commit `dot_d/` → `symlink_dot_d` in the source without applying ⇒ `disk_moved: false`. MUTATION: hardcode `disk_moved: true`.
  - `link_in_sync_counts_toward_in_sync` — MUTATION: keep skipping symlink entries in the tally.
  - `linked_target_inside_another_managed_tree_keeps_its_own_rows` (Review Focus 1) — source manages `dot_agents/skills/h/SKILL.md` and `dot_claude/skills/h/SKILL.md`; disk `~/.claude/skills/h` → `../../.agents/skills/h`; edit `~/.agents/skills/h/SKILL.md` ⇒ one type change for `.claude/skills/h` AND a `destination_drift` for `.agents/skills/h/SKILL.md`. MUTATION: suppress by canonicalized path.
  - `dangling_link_over_a_folder` (Review Focus 2). MUTATION: `disk_kind` follows the link.
  - `templated_link_that_fails_to_render_is_eval_failed` (Review Focus 3) — `symlink_dot_t.tmpl` with `{{ fail "x" }}`. MUTATION: map an `Eval` error to `Link(String::new())`.
  - `probe_one_under_a_linked_folder_returns_nothing` (Review Focus 4). MUTATION: drop the ancestor check.
- [ ] **Step 3:** `cargo test -p tomte-core --test scanner_integration` — FAIL.
- [ ] **Step 4:** Implement.
- [ ] **Step 5:** PASS; run each MUTATION; gate; commit `feat(scanner): detect type changes; a folder turned into a file no longer breaks the scan`.

### Task 3: Wire, daemon, journal, model

**Files:**
- Modify: `crates/proto/src/lib.rs`, `crates/daemon/src/core.rs`, `crates/journal/src/lib.rs` (EventKind), `crates/app/src/model.rs`, every `DriftSummary { … }` literal (`rg -n 'DriftSummary \{' crates`)

**Interfaces:**
- Produces (proto, all `Serialize, Deserialize, Debug, Clone, PartialEq, Eq`):
  - `#[serde(tag = "kind", rename_all = "snake_case")] pub enum WireEntryKind { Absent, File, Folder, Link { text: String } }`
  - `pub struct WireTypeChange { pub source: WireEntryKind, pub disk: WireEntryKind, pub disk_moved: bool }`
  - `DriftSummary.type_change: Option<WireTypeChange>` and `Event::Drift.type_change: Option<WireTypeChange>`, both `#[serde(default)]`.
- daemon: `class_str(TypeChanged) = "type_changed"`; `fn wire(tc: &TypeChange) -> WireTypeChange`; `drift_state` value gains the `Option<WireTypeChange>`; `journal_probe` for a type change records ONE `EventKind::TypeChanged` (`as_str` "type_changed") per distinct `(source, disk)` with meta `{"class": "type_changed", "label": <type_change_label>, "source": …, "disk": …}`, and none of the dest/source/remote events.
- model: `pub fn type_change_label(tc: &WireTypeChange) -> &'static str` (wording table in Global Constraints; `disk_moved: false` ⇒ the "source is now …" set); `class_label("type_changed") = "type changed"` (fallback only); `kind_label("type_changed") = "type changed"`; `upsert_drift` stores `type_change` and treats a changed `type_change` as new information; `TimelineRow`/`ProvRow` prefer `meta.label` for `type_changed` rows.

- [ ] **Step 1: Failing tests:**
  - proto `drift_summary_without_type_change_still_parses` (an old JSON line) and `type_change_roundtrips`. MUTATION: drop `#[serde(default)]`.
  - model `type_change_label_table` — all nine wordings. MUTATION: swap two arms.
  - model `upsert_drift_updates_a_changed_type_change`. MUTATION: compare class only.
  - daemon (`crates/daemon/tests`, scratch home) `type_change_is_journaled_once_and_published` — two scans, one `type_changed` event with meta label "replaced by a link"; `status_snapshot` carries it. MUTATION: record on every scan.
- [ ] **Step 2–4:** run (FAIL), implement, run (PASS), mutations.
- [ ] **Step 5:** Gate; commit `feat(daemon): publish and journal type changes`.

### Task 4: The engine resolves one type change

**Files:**
- Modify: `crates/app/src/resolve.rs`
- Test: `crates/app/tests/resolve_e2e.rs`, `resolve.rs` unit tests

**Interfaces:**
- Consumes: Task 1 (`disk_kind`, `add`, `forget`, `compare_folder`), Task 3 (none at runtime).
- Produces:
  - `pub fn keep_disk_with(&self, target: &Path, copies: &[PathBuf]) -> Result<ResolveOutcome, ResolveError>`; `keep_disk(t)` = `keep_disk_with(t, &[])`. `copies` are paths relative to `target`, allowed only for a folder→link change; anything else with non-empty `copies` is `ResolveError::Failed("copy picks apply only to a folder replaced by a link")`.
  - `pub fn type_change_of(&self, target: &Path) -> Result<Option<TypeChange>, ResolveError>` — same detection as the scanner for one target (source kind from the source path: `symlink_` prefix ⇒ `Link(link_text)`, a directory ⇒ `Folder`, else `File`).
  - `pub fn source_version(&self, source_file: &Path, target_file: &Path) -> Result<Vec<u8>, ResolveError>` — raw bytes, or `chezmoi cat` for `.tmpl` / `encrypted_`.
  - `pub fn folder_comparison(&self, target: &Path) -> Result<FolderComparison, ResolveError>` — managed files under `target` → `source_paths` → `source_version` hashes → `compare_folder(…, target)`.
  - Journal: decisions gain `"dest_entry"`: `{"kind":"link","text":…}`, `{"kind":"folder","entries":[{"path":rel,"blob":h}|{"path":rel,"link":text}]}`, `{"kind":"file","blob":h}`, `{"kind":"absent"}`; `"dest_blob": null` is still written so `session_landed` counts the decision. `UndoRestore` gains `Link(String)` and `Folder(Vec<FolderEntry>)`; `parse_undo_restores` prefers `dest_entry`.
- Behaviour:
  - keep disk, not merging: snapshot copies' current disk content (one `copy_into_link` decision each, `dest_blob` or null), write copies, then `add([target])` (`forget` when disk is `Absent`), `commit_phase("keep_disk", target)`.
  - keep disk, merging: `chezmoi add` would autoCommit and sweep up the conflicted files, so the new entry is produced by `chezmoi add` into a SCRATCH source instead: a tempdir source with an empty config (no git), same `--destination`, `add <target>`; chezmoi writes the parents and the correctly encoded leaf (`symlink_dot_d`, `dot_d/…`, attributes and all). Then: write copies; remove the old source entry (`git rm -r -q` on it); move the scratch leaf into the old entry's parent directory; `git add` exactly the new leaf path; `conclude_merge`. A removed link: `git rm` the entry and conclude, as `keep_deletion` does.
  - keep source: refuse if an entry under the target is mid-merge conflicted (message names it); journal `dest_entry` from the CURRENT disk (folder walked without following links); `apply(Some(target))`.
  - undo: remove what is at the target (a folder: refuse with "…/<rel> was added after the decision; move it away and undo again" if it holds a path not in the folder Tomte's own apply wrote, i.e. not in the source's managed list recorded at decision time — record that list as `"applied": [rel…]` in the keep_source decision), then write the recorded entry back.
- [ ] **Step 0: Verify the scratch-source add.** In a Scratch home with `dot_claude/skills/h/` replaced on disk by a link, run `chezmoi --config <empty> --source <tmp> --destination <home> add <home>/.claude/skills/h`; expect `<tmp>/dot_claude/skills/symlink_h` holding the link text and no git activity. If it differs, stop and report (`NEEDS_CONTEXT`).
- [ ] **Step 1: Failing e2e tests** (`DriftLab`, autoCommit on as `chezmoi_commits_and_pushes_for_itself` does):
  - `keep_link_swaps_the_source_folder_for_a_link` — `symlink_dot_d` holds `.real\n`, no `dot_d/`, one commit pushed, rescan clean. MUTATION: `re_add` instead of `add`.
  - `keep_link_with_copies_writes_them_first_and_undo_removes_them` — copy `gone` ⇒ `~/.real/gone` exists before add; Undo removes it. MUTATION: write copies after `add`.
  - `keep_link_over_a_dangling_link` (Review Focus 2). MUTATION: compare the folder before checking `disk_kind`.
  - `keep_disk_file_replaced_by_link_and_link_retargeted`. MUTATION: route a file→link through `re_add`.
  - `restore_folder_over_a_link_then_undo_restores_the_link`. MUTATION: drop `UndoRestore::Link`.
  - `restore_link_over_a_folder_then_undo_restores_the_folder_and_its_files`. MUTATION: drop `UndoRestore::Folder`.
  - `undo_restore_folder_refuses_when_the_folder_gained_a_file` (Review Focus 5). MUTATION: drop the `applied` check.
  - `keep_link_mid_merge_concludes_without_sweeping_conflicts` — another source file conflicted with origin; Keep the link; the other file stays conflicted, the merge does not conclude. MUTATION: call `add` mid-merge.
  - unit `decision_json_roundtrips_link_and_folder_entries`. MUTATION: ignore `dest_entry`.
- [ ] **Step 2–4:** `cargo test -p tomte-app --test resolve_e2e keep_link` etc. FAIL → implement → PASS → mutations.
- [ ] **Step 5:** Gate; commit `feat(resolve): keep or restore a type change, with Undo`.

### Task 5: Type changes in batches

**Files:** `crates/app/src/resolve.rs`, `crates/app/tests/resolve_e2e.rs`

**Interfaces:**
- Produces: `BatchItem.copies: Vec<PathBuf>` (default empty; every literal updated). `Candidate.type_change: Option<TypeChange>` set in pre-flight via `type_change_of`. Keep-disk type changes join a new `adds` list → one `chezmoi.add(&adds)`; a removed link joins `forgets`; Keep-source type changes join `applies`. `open_batch_session` journals `dest_entry` exactly as Task 4. Copies are written in Phase 2 before `add`. Phase 3 reports type-change keep-disks from `added` ("the batch add failed: …").
- [ ] **Step 1: Failing tests:**
  - `batch_mixes_a_type_change_a_file_and_a_deletion_in_one_commit`. MUTATION: commit `adds` separately.
  - `batch_restore_folder_rides_the_one_apply`. MUTATION: apply type changes one by one (assert via the recording runner's apply count = 1).
  - `batch_add_failure_is_charged_to_type_changes_only` — a folder replaced by a file whose mode is `000` (so `chezmoi add` cannot read it) batched with an edited plain file: the type-change row fails with "the batch add failed: …", the plain file lands and is committed. Restore the mode in the test's cleanup. MUTATION: report `added` errors on re-add rows.
- [ ] **Step 2–5:** run, implement, PASS, mutations, gate, commit `feat(resolve): type changes share the batch's add, apply and commit`.

### Task 6: Queue decisions and the Review plan (pure)

**Files:** `crates/app/src/action_queue.rs`, `crates/app/src/views/review.rs` (pure functions and their tests only)

**Interfaces:**
- Produces:
  - `Decision::KeepLink { copies: Arc<[PathBuf]> }` — `label()` "Keep the link", batchable as `BatchDecision::KeepDisk` with `copies`; `Shell::pump` routes a single one to `keep_disk_with`.
  - `pub(super) fn type_change_buttons(tc: &WireTypeChange) -> (&'static str, &'static str)` — (keep-disk label, keep-source label) from the Global Constraints table.
  - `pub(super) struct ComparisonPlan { pub groups: Vec<(&'static str, Vec<ComparisonRow>)> }`, `pub(super) struct ComparisonRow { pub rel: PathBuf, pub pickable: bool, pub picked: bool, pub opens_diff: bool }`, `pub(super) fn comparison_plan(cmp: &FolderComparison, picks: &BTreeSet<PathBuf>) -> ComparisonPlan` — empty groups dropped; `pickable` for OnlyInSource and Differs; `opens_diff` for Differs.
  - `pub(super) fn footer_text(tc: &WireTypeChange) -> String` — folder→link: "Restore the folder replaces the link. Files in <link text, ~-relative> stay where they are."; link retargeted: "Restore the link points it back at <source text>."; others per spec §The Review screen.
  - `SidebarRow::Target` gains `sub_line: Option<&'static str>` = `type_change_label` for type changes (derived in `sidebar_rows`, read verbatim by the row like `locked`); `RowKey::Target` gains it.
- [ ] **Step 1: Failing tests:** `buttons_table` (MUTATION swap), `comparison_plan_drops_empty_groups_and_marks_picks` (MUTATION: keep empty groups), `footer_names_the_link_target` (MUTATION: absolute path), `sidebar_type_change_row_has_its_sub_line` (MUTATION: compute sub_line at render only), `keep_link_with_picks_is_batched_with_copies` (queue test; MUTATION: drop copies in `as_batch`).
- [ ] **Step 2–5:** run, implement, PASS, mutations, gate, commit `feat(review): plan type-change rows, buttons and comparison`.

### Task 7: Review renders type changes

**Files:** `crates/app/src/views/review.rs` (render), `crates/ui/src/components/` (a `compare_group` component if the list styling is new: group header + rows with optional checkbox), `crates/app/src/views/fixtures.rs`, `crates/app/src/views/mod.rs` (render-smoke covers new poses)

**Interfaces:**
- Consumes: Task 6's plan functions; Task 4's `folder_comparison` / `source_version` (called from the detail loader on the background executor; `LoadedDetail` gains `type_change: Option<TypeChangeDetail>` with `comparison: Option<FolderComparison>`).
- Layout: exactly the spec's §The Review screen (picked 2026-10-29): header (name with `/` for a folder, path, `source → disk` line with link text, the two named buttons), History, the comparison groups, footer. Clicking a Differs file shows its line diff below via the existing `PreviewState::Ready` renderer with `rendered = source_version`, `destination = disk file`.
- Poses (add to `STATES` with descriptions): `review-type-folder-link` (the humanizer data: `WARP.md` only in source; `.claude-plugin/ .cursor-plugin/ .github/ agents/ AGENTS.md CHANGELOG.md LICENSE scripts/` only on disk; `README.md SKILL.md` same), `review-type-folder-link-picked` (WARP.md picked), `review-type-link-retargeted`, `review-type-file-link` (line diff), `review-type-link-folder` (lists what Restore deletes), `review-type-sidebar` (sidebar with a type-change row among files).
- [ ] **Step 1:** Add the poses; `cargo test -p tomte-app --bin tomte render_smoke` FAILS until they render.
- [ ] **Step 2:** Implement the render path.
- [ ] **Step 3:** `./scripts/shoot.sh review-type-folder-link dark` and `light`, and each other pose (unsandboxed); READ every PNG; check no clipped text, Theme tokens only, the picked layout.
- [ ] **Step 4:** Gate; commit `feat(review): type-change rows and the folder comparison`.

### Task 8: Docs

**Files:** `AGENTS.md` (workspace map line for `type_change.rs`; poses list; "Current state" paragraph), `CHANGELOG.md` (`## Unreleased` entry in the user's terms).
- [ ] **Step 1:** Write both; `rg -n 'type_change' AGENTS.md` shows the map line.
- [ ] **Step 2:** Gate (docs-only still runs fmt/clippy/tests); commit `docs: type changes`.
