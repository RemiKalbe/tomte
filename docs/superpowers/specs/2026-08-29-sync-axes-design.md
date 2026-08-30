# Origin as a content state: making "source" honest

**Status:** decided. All of §6 is answered; implemented by
`docs/superpowers/plans/2026-08-30-origin-as-content-state.md`.
**Supersedes:** the first draft of this file (2026-08-29), which modelled
origin as a separate "history axis" with its own surfaces. Rejected —
correctly. This version follows the locked domain model instead.

> "It should be transparent. To me *source* represents origin | my local
> chezmoi. Meaning, if there is a divergence there too, then I should be able
> [to pick either one, everywhere]."

---

## 1. The model was always four states

Design Section 2 (locked, July 2026) defines **four** content states per
managed file:

| # | state | where it comes from | shown today |
|---|---|---|---|
| 1 | **remote source** | `origin/<branch>:<source path>` | ❌ never |
| 2 | **local source** | `chezmoi cat` (this machine's repo, rendered) | ✅ "source (rendered)" |
| 3 | **rendered target** | journal snapshot of last apply | ✅ "last written" |
| 4 | **destination** | the file on disk | ✅ "on disk" |

The implementation only ever built three. Everything wrong here follows from
that omission: because state 1 had no home in the content model, repo
divergence got exiled to a *history* concern with its own banner, its own
button, and its own flow — which is the thing that felt wrong.

**"Source" is not one state.** It is `origin | local`, and when those two
disagree the disagreement belongs in the same places every other
disagreement already lives: the review actions, the diff, and the merge
editor. Not in a banner.

## 2. What this means concretely

**2.1 Review actions split.** "Keep source" becomes two actions when — and
only when — origin's source differs from local's:

- **Source · this machine**
- **Source · origin**

Unchanged (single "Keep source") when they agree, which is the common case.

**2.2 The review diff gains a comparison selector.** A segmented control
choosing which pair the diff shows:

- `on disk ↔ source · this machine` (default, today's behaviour)
- `on disk ↔ source · origin`
- `source · this machine ↔ source · origin`

Shown only when origin differs; otherwise the diff is what it is today.

**2.3 The merge editor gains a fourth column: origin.** Conditional on origin
differing for that file. Its decision strips gain an **origin** pick beside
disk / source / last written / both.

**2.4 The banner dies.** Divergence stops being an event to announce. It
becomes a property of individual files, visible where those files are.

## 3. The engine constraint, and how to satisfy it

`MergeDocument::compute(base, ours, theirs)` is strictly 3-way
(`crates/core/src/merge.rs:171`). A fourth *pickable* side needs a decision.

**Option A — two merges.** Merge the source axis first (base = merge base of
the two source commits, ours = local source, theirs = origin source), then
merge the result against disk. Two region systems that do not align, in one
view. Rejected: complex, and it forces a two-step interaction the user
explicitly did not ask for.

**Option B — origin as a text provider (recommended).** Keep the 3-way merge
exactly as it is. Additionally compute origin's rendered content, line-diff
it against local source, and project origin's lines onto the existing merge
regions. A region that origin differs on offers an **origin** pick whose
payload is origin's text for that region.

Option B needs no engine rework, keeps one region system, and the projection
machinery **already exists and is tested** — `line_hunks` + `map_line` +
`mapped_boundaries` in `crates/app/src/views/merge.rs`, built for whole-file
editing in 0.1.13/0.1.14. This is the same problem: map text from one diff's
coordinates onto another's regions.

The engine gains one variant beside `Edited(String)`:

```rust
pub enum Choice {
    Ours, Theirs, Base, Both,
    Edited(String),
    /// Origin's version of this region (payload precomputed at load).
    /// Distinct from `Edited` so provenance survives into the strip label
    /// and the result-row tint.
    Origin(String),
}
```

`assemble()` treats it exactly like `Edited` — the difference is entirely
provenance, which is the point.

## 4. Rendering origin

Origin's source is raw template text; local's "source (rendered)" is
rendered. Comparing them directly would be comparing different kinds of
thing.

`ChezmoiClient::execute_template` (`chezmoi.rs:341`) already exists. So:

```
blob_at("origin/<branch>", source_rel)   →  raw bytes
    if templated: execute_template(bytes) →  rendered on THIS machine
```

That yields the honest question — *what would origin's source produce here* —
in the same space as every other pane. For non-templated files (most of
them) raw and rendered are identical and the render is skipped.

**Cost control.** Rendering is a subprocess; doing it per file during a scan
would be unacceptable. Split cheap detection from expensive content:

- **Detection (repo-wide, one command):** `git diff --name-only HEAD..origin/<branch>`
  gives every source file origin differs on. That is all the scan needs to
  decide whether a file gets the fourth column and the split actions.
- **Content (on demand):** `blob_at` + `execute_template` run only when the
  user selects that file or opens its merge editor.

## 5. What happens to history joining

Joining is still required to push, but it stops being a user-facing step.

- **Clean join:** when `git merge-tree --write-tree` (git ≥ 2.38; 2.50.1
  present) reports no conflicts, perform the merge automatically at fetch
  time and log a timeline event. No banner, no button. Tomte never leaves
  the repo mid-merge on its own initiative — the virtual merge proves it
  clean *before* anything is written.
- **Conflicting join:** the affected files already surface as
  `LocalSourceDiverged` / `Conflict` rows (`crates/core/src/drift.rs:65`,
  and both are in `NEEDS_YOU_CLASSES`). With §2 in place those rows now
  carry the tools to answer them.

## 6. Decisions

**6.1 A per-file origin decision also resolves the git merge. — DECIDED: yes.**
On the first per-file decision while diverged, Tomte starts the real
`git merge`; each decision writes the source file and `git add`s it; when the
last conflict is answered the merge commits. The existing engine calls
(`start_repo_reconcile`, `resolve_repo_conflict`, `finish_repo_reconcile`)
become the handlers behind ordinary queue rows rather than a separate flow.
Nothing is ever answered twice. The merge state must be visible as a quiet
fact somewhere — not a banner.

**6.2 Origin's colour. — DECIDED: pastel purple.** A new `remote` theme
token, alongside `accent` / `drift` / `ok` / `conflict`. It needs its own
value per theme: a pastel that reads on the dark background will be too pale
on the light one, so the light variant carries more chroma and less
lightness. Same treatment as every other tint — 2px rail, ~7% field wash,
pill — so it slots into the existing vocabulary without new rules.

**6.3 Four columns at the 840px minimum. — DECIDED: acceptable.** Panes are
resizable (0.1.14) and the column is conditional. A gallery state still gets
built first so the four-column case is inspected before real wiring.

**6.4 Scope of "differs from origin". — DECIDED: show it whenever origin's content differs, without distinguishing why.**

Origin's source can differ from local's for two quite different reasons:

*Behind only.* Origin has commits you don't; you have none of your own on
that file. There is no disagreement — you are simply out of date, and the
automatic fast-forward (§5) makes the difference disappear on its own.

*Truly diverged.* You committed something and origin committed something.
The two genuinely disagree and a human must choose.

The question is whether the fourth column and the split actions appear in
both cases or only the second. Note that with §5 in place the first case is
usually transient — except when the fast-forward is blocked, which today
happens whenever the source tree is dirty
(`crates/daemon/src/core.rs:563` requires `dirty_files().is_empty()`). In
that state "behind only" can persist indefinitely, and seeing origin's
version is genuinely useful.

One rule, no extra condition, and no state where Tomte knows something about
origin and declines to show it. The "behind only" case is the one where the
answer is hardest to get by other means, and restricting the column to true
divergence would hide origin precisely when the user is stuck behind it.

Implementation consequence: detection is `git diff --name-only
HEAD..origin/<branch>`, which does not distinguish the two cases — so this
decision is also the cheapest one to implement. The scan already runs that
exact command (`crates/core/src/scanner.rs:116`) and discards the
distinction; the plan reuses it rather than adding a second call.

## 7. Provisional build order

For sizing only — not the plan.

1. `GitClient::merge_tree_conflicts(upstream)` — virtual merge, no mutation.
2. Auto-join at fetch when merge-tree is clean; timeline event; delete the
   banner, the `repo_diverged` field, and the 60s divergence probe.
3. `origin_differs: BTreeSet<PathBuf>` in the scan report from one
   `git diff --name-only HEAD..origin/<branch>`; carried per file to the app.
4. `MergeInputs::origin: Option<String>` — `blob_at` + conditional
   `execute_template`, loaded on demand.
5. `Choice::Origin(String)` in the engine + assemble + provenance labels.
6. Merge editor: conditional fourth pane, origin pick in the decision strip,
   origin text projected onto regions via the existing `map_line` machinery.
7. Review: split actions and the diff comparison selector.
8. Whichever of 6.1 wires decisions to the git merge.
