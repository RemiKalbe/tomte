# Type changes: deferred follow-ups

Recorded at the end of the feat/type-changes run (2026-10-29). Spec: .agents/artifacts/specs/2026-10-29-type-changes-design.md. Every item below was judged safe to merge without; the final review's triage is copied after the list.

## Rulings made during implementation

- Ruling R1: type_change((Folder, Absent)) ⇒ None — a deleted folder is out of the spec's cases; its files keep today's per-file deleted rows (Keep disk forgets them) — cost if wrong: a deleted folder shows N rows instead of one.
- Ruling R2: two detection routes (scanner: managed_kinds; engine: source path naming) stay — the engine handles one target and must not run two whole-home managed calls per decision — cost if wrong: a divergence between routes shows as a decision that refuses; covered by e2e tests that drive both.
- Ruling R3: folder_comparison and source_version are free `pub fn`s in resolve.rs taking `&ChezmoiClient` (engine code calls them with self.chezmoi; the view loader with app_chezmoi_client()) — cost if wrong: a later move of two functions.
- Ruling R4: Task 6 builds BatchItem { copies } in Shell where items are assembled from queued actions (Decision::KeepLink.copies), as_batch stays BatchDecision-only — cost if wrong: small refactor in Shell::pump.
- Ruling R5: batch re-add/add/forget run through a ChezmoiClient derived with git automation off (dump-config → autoCommit/autoPush false → --config tmp), so commit_batch makes the one commit and push; verify the roundtrip first; single-file paths unchanged — why: spec §Batches "one commit, one push" and the user's machine has autoPush on — cost if wrong: a derived config that drops a setting would break template rendering in batches (verification step guards it).
- Ruling R6: re-review's Minor "derived config written 0755/0644 in tempdir" raised to Important and sent as fix round 2 — why: the copy holds the user's full rendered config (data, 1Password settings), world-readable if TMPDIR falls back to /tmp — cost if wrong: one extra small round.
- Ruling R7: footer_text(tc, target) replaces the plan's footer_text(tc) — the folder→file / link→folder footers name the entry — cost if wrong: none.
- Ruling R8: the folder→link footer shows the link text as written (../../.agents/skills/humanizer), not the spec mockup's ".agents/skills/humanizer" — it is the exact text the user wrote and the header line shows the same — cost if wrong: one string change; Task 8 updates the spec mockup to match.
- Ruling R9: timeline/history rows keep their journaled wording; only rows without one use the live type-change wording — history says what happened then — cost if wrong: an old "modified on disk" row stays in the history.
- Ruling R10: the open Differs diff stays below the virtualized list as the existing diff view (its own scroller) instead of a list item — nested scrollers under one wheel would be worse, and the diff view is already the virtualized line renderer — cost if wrong: a layout change in one view.
- Ruling R11: C1 — Keep source (and its batch path) refuses up front when anything it would delete from disk has a file over the journal's blob cap, naming the file and its size; Undo also checks every needed blob exists BEFORE removing anything — why: keeps the spec's promise "Undo brings it back" without growing the journal or adding a trash store late in the branch — cost if wrong: a user with a large file in such a folder must move it by hand first.

## Deferred minors

- Task 1: minor (deferred): compare_folder hard-errs on IsADirectory/NotADirectory for one odd entry (type_change.rs:475-484)
- Task 1: minor (deferred): OnlyOnDisk hides a nested disk-only file under a folder that also exists in source (sub/y beside source sub/x)
- Task 1: minor (deferred): is_ok_and swallows a metadata error when marking a folder '/' (type_change.rs:500)
- Task 1: minor (deferred): untested compare_folder branches: disk_root symlink followed; disk_root missing
- Task 1: minor (deferred): status_of decodes lossy while status uses run_utf8 (chezmoi.rs:165)
- Task 1: minor (deferred): folder '/' marker lives inside PathBuf (lost by equality/components)
- Task 1: minor (deferred): no type_change table row for (Link, Link, None)
- Task 2: minor (deferred): detection io error → unprobed arm has no test (old unreadable-target test rewritten)
- Task 2: minor (deferred): remove_ entries/scripts read as File sources; limit managed to --include files,dirs,symlinks
- Task 2: minor (deferred): scan cost: 2 managed + 1 cat per link per scan; managed_kinds per probe_one
- Task 2: minor (deferred): a link changed only on origin counts as in sync while also a step (b) candidate
- Task 2: minor (deferred): probe_one assumes a managed target; unmanaged folder would read file→folder
- Task 3: minor (deferred): type_changed recurrence suppressed by the events_for(target,10) window (same as dest side)
- Task 3: minor (deferred): dedupe ignores disk_moved
- Task 3: minor (deferred): to_value(..).unwrap_or_default() swallows an (impossible) error in the TypeChanged arm
- Task 3: minor (deferred): class TypeChanged with type_change None journals nothing and logs nothing
- Task 3: minor (deferred): type_changed_label vs type_change_label naming
- Task 4: minor (deferred): keep_source looks up source/change before ensure_merge_started (pre-join kind)
- Task 4: minor (deferred): copies not validated as relative, no ParentDir (target.join(rel) escape)
- Task 4: minor (deferred): keep_source mid-merge type-change refusal untested; message names repo path not target
- Task 4: minor (deferred): no test for folder↔file through chezmoi add
- Task 4: minor (deferred): undo of restored folder: removals not passed to expect_changes; empty subfolders not restored
- Task 4: minor (deferred): snapshot_entry unwrap_or_default would journal "" (guarded by count check)
- Task 4: minor (deferred): tests written after code (mutation runs stand in); dangling-link test uses a different mutation than the brief
- Task 5: minor (deferred): keep_source_type_change runs managed_under after session_start (empty open session on failure)
- Task 5: minor (deferred): no test for Phase 2b copy-write failure; none for batched keep-disk on a removed link
- Task 5: minor (deferred): RestoreMode hard-codes 0o644 without saying why
- Task 5: minor (deferred): persistentState pin assumes chezmoi's default state location (e2e catches a change)
- Task 5: minor (deferred): a crash mid-batch leaves the derived config copy in $TMPDIR
- Task 6: minor (deferred): fn sub_line sits between sidebar_rows' doc comment and the fn (review.rs:1275)
- Task 6: minor (deferred): sub_line mutation covers only the safe-row arm
- Task 7: minor (deferred): link-side read error other than NotFound falls back to "dangling" silently (review.rs side())
- Task 7: minor (deferred): load_compare_diff_blocking / load_link_diff_blocking return Error without log_error!
- Task 7: minor (deferred): open_compare_diff toggle and toggle_copy_pick untested
- Task 7: minor (deferred): empty deletes list renders a label with nothing under it
- Task 7: minor (deferred): bulk Keep disk on a ticked type-change row queues plain "Keep disk" (header buttons carry the words)
- Task 7: minor (deferred): wordings chosen by implementer: "On disk · deleted if you restore the link", "removed" for absent disk side
- Task 7: minor (deferred): compare_group has no comp: gallery preview
- Task 7: minor (deferred, recommend fixing in final wave): the footer (what Restore does to disk) became the list's last item and scrolls below the fold with a diff open or a long folder; keep it pinned as a flex_none sibling
- Task 8: minor (deferred): AGENTS.md Scratch sentence reflow; pose list suffix shorthand
- Final: minor (deferred): record_expected_apply records an expected apply of an ordinary managed link with no hash (was the followed target's hash)
- Final: minor (deferred): undo_last holds every blob of a restored folder in memory (no folder-size ceiling)
- Final: out of scope (pre-existing): plain Keep source / Undo on a managed file over 4 MB is not refused up front

## Final review (full text)

# Final whole-branch review: type changes (2ec0ace..0a6f969)

Reviewer: final-review. Read in passes: spec, plan, ledger, then the diff by
crate (core → daemon/proto/journal → resolve.rs → queue/Shell → review.rs,
dashboard, fixtures, ui). Spot checks run (read-only): `git rm -r` mid-merge
in a throwaway repo; `chezmoi add --force` over a templated file and over a
folder turned into a file in a throwaway home; `dump-config` round trip on
the user's real config (`chezmoi data` identical except `configFile`/`args`,
`managed` count identical at 1722, `git.autocommit/autopush` true, no
`persistentState` set, so R5's pin is needed and correct). The user's source
has 22 `symlink_` entries under `dot_claude/skills/` and no
`.chezmoiexternal`. No test suite run.

**Verdict: needs fixes.** 1 Critical, 5 Important, 8 Minor.

## Critical

### C1. Restore (Keep source) deletes files over 4 MB that Undo cannot bring back
`crates/daemon/src/core.rs:349-365` (`snapshot_file`) returns the hash of a
file over `BLOB_CAP` (4 MB) WITHOUT storing the blob, and `snapshot_blobs`
(`core.rs:~1070`) passes that hash back as if it were stored. The engine's
`snapshot_files` (`resolve.rs`, count check only) accepts it, so
`snapshot_entry` (`resolve.rs:524`) journals a folder whose large files have
no blob. `apply --force` then deletes the folder. Undo
(`resolve.rs:1476`, `:1496`) calls `remove_entry(target)` first, then writes
the entries and fails at the first missing blob ("undo: blob … is missing
from the journal") with the folder half restored. The large file is gone.

For a single file this gap predates the branch, but there the file is a
managed file with a source copy. This branch extends it to whole folders on
disk that the source never held: "link replaced by a folder" and "file
replaced by a folder" folders are the user's own content (a cloned repo, a
skill with assets), and the spec promises "snapshotted into the journal, so
Undo brings it back". This loses data.

Fix: have the engine refuse a type-change Keep source (single and batch)
whose folder holds a file over the cap, naming it, or store the blob anyway
for this path. Then make undo check every blob exists BEFORE
`remove_entry`. The same folder walk also sends every file of an arbitrarily
large folder through one `SnapshotBlobs` under the core lock, so a file-count
or byte ceiling with a named refusal belongs in the same fix.

## Important

### I1. Mid-merge "Keep the link" fails when origin changed a file inside the folder
`resolve.rs:466` runs `git rm -r -q -- <folder>`. Mid-merge, a file under the
folder that origin changed and git merged cleanly is staged (index ≠ HEAD),
and `git rm` refuses: "error: the following file has changes staged in the
index … use --cached … or -f" (reproduced in a scratch repo). That is the
humanizer case whenever another machine touched the skill and some other
file conflicts. The decision fails after `ensure_merge_started` opened the
merge in this same call and after the copies were written (session left
open, so Undo cannot reach them). `keep_link_mid_merge_*` tests only
conflict on an unmerged path, which `git rm` accepts. Fix: `git rm -r -f`
(the user is replacing the folder; the staged content stays in origin's
commit), with a test where a sibling file under the folder merges cleanly.

### I2. Generic "Keep disk"/"Keep source" still act on type-change rows from the dashboard and the bulk bar
The dashboard's quick actions (`views/dashboard.rs:1008-1009`) and the bulk
selection bar (`views/review.rs:502`, `bulk_decision`) queue plain
`Decision::KeepDisk` / `KeepSourceLocal` for a type-change row. The engine
re-detects and runs `add` / `apply --force`, so a hover-click on "Keep
source" on a "replaced by a folder" dashboard row deletes that folder, with
nothing on screen naming what goes. That breaks the spec's "button names
come from was/now, never Keep disk/Keep source alone" and "anything Keep
source would delete from disk is named on the row before the click".
Ledger Task 7's deferred "bulk Keep disk queues plain Keep disk" is the same
seam. Fix: no quick actions on dashboard type-change rows (send the user to
Review), and either exclude type-change rows from the bulk Keep source or
show the deletion count in the bar.

### I3. Type-change rows draw the in-sync colour
`crates/ui/src/theme.rs:71`: `class_color` has no `type_changed` arm and
falls to `_ => self.ok`. Every type-change row's sidebar dot and its History
chip are green, the colour of "in sync"
(`shots/review-type-folder-link-dark.png`: humanizer/, nvim, zsh/ and the
"replaced by a link" chip are all green). A drifted row that looks in sync
is the kind of state the product says it never shows. Fix: map
`type_changed` to `self.drift` (or its own token) and add it to
`class_colors_are_distinct_per_severity`.

### I4. Keep disk does not name the source files it removes; templates and encrypted files are not marked
The spec (§Resolving, "Source content lost by Keep disk") requires the row to
name the source files a folder → link or folder → file Keep disk removes,
"with a mark for templates and encrypted files". The plan dropped this
without a ruling. Folder → link lists the files (comparison) but marks none
as template or encrypted. Folder → file ("replaced by a file", body
`Deletes`) lists only what Restore deletes on disk (the file), so "Keep the
file" removes the whole source folder (templates, encrypted files) with
nothing on screen naming it. Git history keeps them, but the click is
unannounced. Fix: list the source folder's managed files under the
folder → file header, and mark `.tmpl` / `encrypted_` sources in both lists.
Otherwise record a ruling that defers it.

### I5. The per-event path errors on an in-sync folder and drops the rest of the event batch
`scanner.rs:444` returns early for an in-sync LINK, but an in-sync FOLDER
falls through to `probe_file` (`:453`), and `hash_file` → `fs::read(dir)` is
`EISDIR` → `ScanError::Io`. Likewise `record_expected_apply`
(`daemon/src/core.rs:506`) → `snapshot_file(dir)` errors when the expected
path is the folder that Restore the folder / Undo just wrote (both announce
the target in `expect_changes`). `handle_paths_changed` stops at the first
`?`, so the rest of that debounced batch is not probed. When the user puts
the folder back by hand, the "replaced by a link" row stays until the next
full scan, and pressing "Keep the link" on it goes down the ordinary
keep-disk path and fails on the 1-of-2 snapshot. Folder paths are in
`self.managed` (default `chezmoi managed` lists dirs) and in the watch set.
The error predates the branch for plain folders. The branch makes folder
events part of the feature. Fix: `Detected::Same if kinds.folders.contains`
→ `Ok(None)`, and have `record_expected_apply` skip non-files. Add a daemon
test that probes an in-sync folder.

## Minor

- M1. `.chezmoiexternal` rows (spec §Not covered: "Restore only and says the
  entry comes from an external") are not implemented; Keep disk would run
  `chezmoi add` on an external's target. The user has no externals, so this
  can wait, but record a ruling.
- M2. "Only in the link target · not managed" (`review.rs:731`) is false in
  the humanizer case itself, where `~/.agents/skills/humanizer` is managed.
  This is the spec wording the user picked, so the user should decide
  whether to change it.
- M3. Undo writes restored folder files with default mode: an executable
  script in a restored folder loses `+x`. Empty subfolders are not restored
  (already in the ledger).
- M4. `swap_source_entry` (`resolve.rs:447`) adds into a scratch source
  under an EMPTY config, so the user's `umask`, `add.*` settings and
  `.chezmoiignore` do not apply mid-merge, while they do outside a merge.
  The two paths can encode the same entry differently.
- M5. `swap_source_entry`'s `remove_entry(source)` after `git rm` deletes
  untracked or ignored files left in the old source folder without a
  snapshot. Outside a merge `chezmoi add` does the same, but say so in the
  doc comment.
- M6. A failed `chezmoi add` after copies were written leaves the copies on
  disk and the session open, so Undo cannot remove them (single path). The
  batch path says so; the single path does not.
- M7. CHANGELOG says files "that exist only in the source can be ticked";
  Differs files can be ticked too. "A folder turned into a file no longer
  breaks the scan" is a user-visible fix with no Fixed entry.
- M8. Scan cost on the user's machine: 22 `chezmoi cat` per full scan (one
  per link) plus 2 `managed` calls. `probe_one` adds 2 `managed` calls per
  event UNDER the core lock. Measure from the user-run daemon log before
  the next release.

## Deferred-minor triage (ledger)

Fix before merge:
- Task 7: footer is the list's last item and scrolls out of view with a diff
  open or a long folder. Pin it as a `flex_none` sibling. It is the line
  saying what Restore deletes.
- Task 7: `load_compare_diff_blocking` / `load_link_diff_blocking`
  (`review.rs:396`, `:364`) return `PreviewState::Error` without
  `log_error!`, against the AGENTS.md rule that failures are logged. Two
  lines.
- Task 7: link-side read error other than NotFound silently shown as the
  link text (`load_link_diff_blocking`'s `side`). Same rule: log it, and show
  the text only for NotFound.
- Task 6: `fn sub_line` (`review.rs:1574`) now sits between `sidebar_rows`'
  doc comment ("Plan the sidebar…", `:1569`) and the fn, so rustdoc attaches
  that paragraph to `sub_line`. Move it.
- Task 7: bulk Keep disk queues plain "Keep disk". Fold into I2.

Can wait (record in the ledger as open):
- Task 1: all seven: compare_folder hard error on one odd entry (the pane
  shows "could not compare", the buttons still work), nested disk-only file
  hidden, `is_ok_and` on the `/` marker, untested branches, `status_of`
  lossy decode, `/` inside `PathBuf`, the missing (Link, Link, None) row.
- Task 2: unprobed-arm test; `remove_`/scripts read as File (no
  `remove_` folders in the user's source); scan cost (see M8); a link
  changed only on origin counts as in sync and never shows a row (a real
  sync-axis gap, but `update`/auto-join applies it); `probe_one` on an
  unmanaged path.
- Task 3: all five (recurrence window, `disk_moved` dedupe,
  `unwrap_or_default` on an infallible `to_value`, unreachable None arm,
  label naming).
- Task 4: change looked up before the join; copies not validated as
  relative (input only comes from Tomte's own comparison; a one-line
  `Component::Normal` check is cheap if someone is in the file); untested
  mid-merge keep-source refusal; folder↔file through `add` (I checked it by
  hand: `dot_d/` → `empty_dot_d`); undo expect_changes/empty folders (see
  M3); `snapshot_entry` `unwrap_or_default` (guarded); process note;
  `tests/support/mod.rs` writing `root/chezmoi.toml`.
- Task 5: `managed_under` after `session_start`; Phase 2b and removed-link
  batch tests; `0o644` comment; `persistentState` pin (I confirmed the
  real config needs it and round-trips); a crash leaving the 0600 derived
  config in `$TMPDIR`.
- Task 6: sub_line mutation covers only the safe arm.
- Task 7: toggle tests, empty deletes label, implementer wordings (user to
  see), `comp:` preview.
- Task 8: AGENTS.md reflow.

## Checked and found sound

- R5 batch path: one commit, one push with autoCommit/autoPush on; the
  derived config is private (0700/0600) and round-trips the user's real
  config (see the header).
- Scanner: child suppression by literal path (a link's target keeps its own
  rows, Review Focus 1); dangling links are not followed; a templated-link
  eval failure is `eval_failed`; scoped status with `--recursive=false`;
  1722 targets as argv are about 120 KB, well under macOS ARG_MAX.
- Undo: `replace` + `refuse_foreign_folder` refuses before touching
  anything; a link on disk is removed, never followed; keep-disk type
  changes journal no disk entry, and the session still counts as landed.
- Queue ⇄ engine: `Decision::TypeChange` keeps its side in `as_batch`;
  copies travel on `BatchItem`; the single-run fallback routes to
  `keep_disk_with` / `keep_source`; the stranded-merge probe test includes
  `.keep_disk_with(`.
- One latch: every new engine mutation is reached only from `Shell::pump`
  through `run_batch_inner`; nothing new runs git outside it.
- Wire: every new field is `#[serde(default)]`; old lines parse.

## Declined to judge

- Whether `ensure_merge_started` should run at all on a type-change
  decision (it opens a merge on Tomte's initiative when behind origin).
  This design predates the branch and every keep-disk/keep-source shares it.
  I1 covers the new failure inside it.
- Bulk-selecting a destructive Restore at all (vs. excluding type-change
  rows from SAFE TO RESOLVE): the spec chose to include them, so I2 only
  asks for honest words or exclusion of Keep source.
- The "source is now a …" (`disk_moved: false`) button wording ("Restore the
  link" for something new from the source): the planner accepted it in
  pre-flight (T6 self-check).
- Visual layout of the poses beyond the colour in I3: approved by the user
  from mockups; I read one PNG only.
