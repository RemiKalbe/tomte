# Type changes: files, folders and links

Status: draft, for review. Plan: not written yet.

## Why

On 2026-10-26 `~/.claude/skills/humanizer/` was replaced on disk by a link to
`../../.agents/skills/humanizer`. The chezmoi source still held the folder.
Tomte showed three unrelated file rows: `README.md` and `SKILL.md` "modified
on disk" (read through the link), `WARP.md` deleted (missing from the link
target). Neither action on those rows did what the user meant. The fix was a
hand edit of the source repo.

That case is one of a family. A managed entry is a file, a folder or a link,
and on disk it can become any of the others, or a link can point somewhere
else. Tomte today handles none of these:

- The scanner skips every state entry that is not a file
  (`scanner.rs:113`, `scanner.rs:244`), so a link entry never drifts: a
  retargeted or deleted skill link shows nothing.
- A file replaced by a link is hashed through the link and shows as "modified
  on disk". Keep disk runs `re-add`, which does nothing for it (verified
  below), so the row comes back after a "successful" decision.
- A folder replaced by a file makes `chezmoi status` fail for the whole home
  (verified below). The scan returns that error (`scanner.rs:183`), so one
  such folder blinds Tomte to every other file.

Goal: every type change shows as one row that says what happened in the
user's terms, and resolves in one click either way, from the Review sidebar,
in batches, mid-merge, with Undo, exactly like file drift does.

## What chezmoi does (verified 2026-10-27 and 2026-10-29, chezmoi 2.70.4)

Scratch home: source `dot_d/one`, `dot_f`, `symlink_dot_l` (`.real`), all
applied.

State dump: `.d` type `dir`, `.d/one` and `.f` type `file`, `.l` type
`symlink` with no link text recorded. The link text Tomte compares against is
the source's (`chezmoi cat` of the link target prints it).

| Change on disk | `chezmoi status` | Keep disk | Keep source (`apply --force <t>`) |
|---|---|---|---|
| folder → link | `MM .d`, plus `MM` per differing file read through the link | `add .d`: `dot_d/` becomes `symlink_dot_d` | link removed, folder written; link target untouched |
| folder → file | fails for the whole home: `lstat .d/one: not a directory`; status on other targets works | `add .d`: `dot_d/` becomes file `dot_d` | file removed, folder written |
| file → link | `MM .f` (and `DA` for a dangling link elsewhere) | `add .f`: `symlink_dot_f`. `re-add .f` does nothing | link replaced by the file |
| link retargeted | `MM .l` | `add .l`: new link text | link text restored |
| link → folder | `MM .l` | `add .l`: folder added | folder AND its files deleted, link restored |
| link → file | `MM .l` | `add .l`: file added | file deleted, link restored |
| link deleted | `DA .l` | `forget .l` (as for a deleted file) | link recreated |

`add` needs no `forget` first in any row. After a swap the state keeps the
old child entries until the next apply; they are no longer managed, so the
scanner's `managed` filter skips them. `chezmoi managed` and `state dump`
keep working while status is broken.

## Detection (tomte-core scanner)

Before step (c), for every managed target with a state entry, compare the
entry's type (`file`, `dir`, `symlink`) with `symlink_metadata(target)`. A
different type, or a `symlink` entry whose `read_link` differs from the
source's link text, is a `TypeChange`:

```
TypeChange {
    target, source_rel,
    was: EntryKind,            // File | Folder | Link(text)
    now: EntryKind,            // what is on disk; Link(text) as read_link returns it
    comparison: Option<FolderComparison>,
}
```

- `FolderComparison` (when a folder becomes a link, or a link becomes a
  folder): per relative path, one of equal / differs / only in source / only
  on disk, from hashes. It names the files; it holds no diff text.
- Every managed target under a type-changed folder leaves `candidates` and the
  in-sync count. The one row stands for them.
- Step (c) runs `chezmoi status` on the managed targets OUTSIDE type-changed
  folders: the smallest set of paths that covers them (a folder's siblings
  replace the folder's ancestors). With no type changes it runs as today, with
  no arguments.
- `probe_one` (the per-event path) runs the same check for the target and for
  its nearest managed ancestor folder, so an event under a link never brings a
  file row back.
- Link entries are now scanned. A link whose text equals the source's is in
  sync and counts toward the in-sync total.

`ScanReport.drifted` carries type changes as `FileDrift` with a new class
`type_changed` and the `TypeChange` alongside, so the daemon, journal, proto
and model stay one list. `class_label` gives the row's wording from
`was`/`now`: "replaced by a link", "replaced by a folder", "replaced by a
file", "link points elsewhere", "link removed".

## Resolving

Two decisions, as for any file:

- Keep disk ("Keep the link", "Keep the folder", "Keep the file", "Keep the
  new link"): `chezmoi add <target>`. A removed link uses `forget`, like a
  deleted file.
- Keep source ("Restore the folder", "Restore the link", …):
  `chezmoi apply --force <target>`.

The button names come from `was`/`now`, never "Keep disk"/"Keep source"
alone.

Folder → link, keep the link: the comparison lists the files the link target
lacks or holds differently. Each such file can be picked "copy the source's
version into the link target" before the swap; by default nothing is copied
and the disk is left as it is. Picks are written first, then `add` runs.

Anything Keep source would delete from disk (the folder a link replaced, the
file a folder replaced) is named on the row before the click, and snapshotted
into the journal, so Undo brings it back.

Source content lost by Keep disk: a folder replaced by a link or a file loses
its source files, templates and encrypted files included. The row names
them, with a mark for templates and encrypted files. The commit removes them;
git history keeps them. No refusal: the user is replacing the folder on
purpose, and the row has said what goes.

### Batches and multi-select

Type-change rows are SAFE TO RESOLVE like any one-click row and join
multi-select and batches. A batch gains an `adds` list beside `re_adds` and
`forgets`, run as one `chezmoi add` invocation; Keep source targets join the
batch's one `apply_many`. One commit, one push, per-row outcomes, as today.
Templated-source rules do not apply: `add` replaces the entry, it does not
write through a template.

### Mid-merge

`chezmoi add` would autoCommit in the middle of a merge and sweep up the
conflicted files, as `re-add` would. Mid-merge, Keep disk does the swap by
hand, the way Keep disk on a file does: `git rm -r` the old source entry,
write the new one (`symlink_<name>` with the link text, the file's bytes, or
the folder's files), stage, conclude. Keep source runs `apply` as usual; it is
refused only when an entry under the target is still conflicted, because
apply would write the conflict markers to disk, and the refusal says which.

### Undo

Undo reverts the disk, as it does for files. The journal's decision gains a
`dest_entry` that can be a file blob, absent, a link text, or a folder (a list
of relative paths with blobs and link texts). `UndoRestore` gains `Link(text)`
and `Folder(entries)`. Undo removes whatever is at the target now and writes
the recorded entry back. Keep disk changes nothing on disk, so its Undo
restores nothing, as for a file.

## The Review screen

One row per type change in the sidebar, in the folder tree at the target's
place, with the class wording and the link text where there is one. Opening
it shows `was` and `now`, the folder comparison when there is one, the per-file
copy picks, and what each button deletes. A file in the comparison opens a
read-only line diff of its two versions.

Layout is not decided: mockups on the humanizer case first (house rule), then
they go in this section.

## Not covered, and why

- A target managed by `.chezmoiexternal`: Keep disk would mean editing that
  file's TOML. The row shows with Restore only and says the entry comes from
  an external.
- Files on disk that are not managed and sit in a managed folder (the `three`
  case) are listed in the comparison as "only on disk" and are never written
  to the source except through a copy pick.

## Tests (each names its mutation)

core (scratch home):
- Each row of the table above yields one `TypeChange` with the right
  `was`/`now`. MUTATION: compare type only, not link text, so "link points
  elsewhere" is lost.
- Folder → link: no file drift for children; in-sync count excludes them.
  MUTATION: drop child suppression.
- Folder → file: the scan succeeds and still reports an unrelated edited
  file. MUTATION: run status without the scoped targets.
- Link in sync counts toward in-sync. MUTATION: keep skipping symlink
  entries.
- `probe_one` under a linked folder returns nothing. MUTATION: skip the
  ancestor check.

app e2e (`resolve_e2e.rs`):
- Keep disk for folder → link, file → link, link retargeted: the source holds
  the new entry, one commit pushed, status clean. MUTATION: `re_add` instead
  of `add`.
- A batch mixing a type change, a file keep-disk and a deleted file: one
  commit. MUTATION: run `adds` outside the batch commit.
- Copy picks: a picked source-only file appears in the link target before the
  swap. MUTATION: write picks after `add`.
- Keep source for link → folder deletes the folder; Undo restores it with its
  files. MUTATION: drop `UndoRestore::Folder`.
- Keep source for folder → link, then Undo: the link is back. MUTATION: drop
  `UndoRestore::Link`.
- Mid-merge Keep disk for folder → link: merge concluded, no conflicted file
  committed. MUTATION: call `chezmoi add` mid-merge.

gallery: a pose per wording, plus the humanizer folder comparison with picks;
the render smoke test builds them.
