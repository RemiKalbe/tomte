# Origin as a Content State — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make origin's version of a dotfile a first-class content state,
visible and pickable everywhere local source already is, and delete the
diverged-repo banner and its separate flow.

**Architecture:** The 3-way merge engine is untouched. Origin is computed
separately (`git show origin/<branch>:<src>`, rendered through
`chezmoi execute-template` when templated) and its lines are *projected* onto
the existing merge regions using the line-diff machinery already built for
whole-file editing. Repo divergence stops being an event: joining histories
happens automatically when a virtual `git merge-tree` proves it clean, and
otherwise resolves as a side effect of the per-file decisions the user was
already making.

**Tech Stack:** Rust 2024, gpui 0.2.2, imara-diff, chezmoi + git subprocesses.

**Spec:** `docs/superpowers/specs/2026-08-29-sync-axes-design.md`

## Global Constraints

- **Never swallow an error.** A failure is surfaced (typed error, log line,
  or a visible state) — never a silent `unwrap_or_default`. This project has
  been bitten twice.
- **Never abort a whole scan for one target.** Per-target failures are
  recorded in `ScanReport::unprobed` and excluded from `in_sync_count`.
- **Tomte never leaves the git repo mid-merge on its own initiative.** A real
  merge starts only after `merge_tree_conflicts` proves it clean, or in
  response to an explicit user decision.
- **Commits are SSH-signed via 1Password.** If signing fails, leave the work
  staged and report it. Never pass `--no-gpg-sign`, never change git config.
- **Verification is a gate, not a vibe:** `cargo fmt --check`,
  `cargo clippy --workspace --all-targets` (clean), `cargo test --workspace`
  (26 suites) before every commit.
- **Screenshots for UI work:** `./scripts/shoot.sh <state> dark|light`, and
  look at the result. A gallery state is the deliverable for UI tasks.
- **Colour tokens:** origin is `theme.remote` — dark `#d2a8ff`, light
  `#8250df`. Same treatment as every other tint (2px rail, `wash(tint, 0.07)`
  field, pill), never a bespoke rule.
- **Releases go only through `./scripts/release.sh X.Y.Z`.**

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/ui/src/theme.rs` | semantic colour tokens | add `remote` |
| `crates/core/src/git.rs` | git subprocess wrapper | add `merge_tree_conflicts` |
| `crates/core/src/scanner.rs` | `DriftScanner` | add `origin_differs` to `ScanReport` |
| `crates/core/src/merge.rs` | pure 3-way engine | add `Choice::Origin` |
| `crates/proto/src/lib.rs` | daemon↔app wire types | `DriftSummary.origin_differs`, bump `PROTOCOL_VERSION` |
| `crates/daemon/src/core.rs` | daemon state | auto-join; carry `origin_differs` |
| `crates/app/src/model.rs` | app-side model | carry `origin_differs`; drop `repo_diverged` |
| `crates/app/src/merge_inputs.rs` | merge editor inputs | add `origin: Option<String>` |
| `crates/app/src/views/merge.rs` | merge editor | fourth pane, origin pick, projection |
| `crates/app/src/views/review.rs` | review queue + diff | split actions, comparison selector |
| `crates/app/src/views/dashboard.rs` | dashboard | delete banner |
| `crates/app/src/main.rs` | wiring | delete divergence probe |

---

### Task 1: The `remote` theme token

**Files:**
- Modify: `crates/ui/src/theme.rs`
- Test: `crates/ui/src/theme.rs` (existing `mod tests`)

**Interfaces:**
- Produces: `Theme::remote: Rgba` — origin's tint, used by every later task.

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` in `crates/ui/src/theme.rs`:

```rust
#[test]
fn remote_is_distinct_from_every_other_class_tint() {
    // Origin is its own provenance and must never be confused with the
    // local source (accent), disk (drift), or a resolved region (ok).
    for t in [Theme::dark(), Theme::light()] {
        for other in [t.accent, t.drift, t.ok, t.conflict, t.text_muted] {
            assert_ne!(
                (t.remote.r, t.remote.g, t.remote.b),
                (other.r, other.g, other.b),
                "remote must be visually distinct from every other tint"
            );
        }
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p tomte-ui remote_is_distinct`
Expected: FAIL — `no field 'remote' on type 'Theme'`

- [ ] **Step 3: Add the token**

In `crates/ui/src/theme.rs`, add to the struct after `conflict`:

```rust
    /// Origin's version of a source file — the fourth content state.
    /// Pastel purple: a member of no existing family, because origin is
    /// neither disk (amber) nor local source (blue).
    pub remote: Rgba,
```

In `dark()`: `remote: rgb(0xd2a8ff),`
In `light()`: `remote: rgb(0x8250df),`

(Light carries more chroma and less lightness — a pastel that reads on the
dark background washes out on white.)

- [ ] **Step 4: Run the test**

Run: `cargo test -p tomte-ui remote_is_distinct`
Expected: PASS

- [ ] **Step 5: Prove it in the gallery**

Add `remote` to the `comp:buttons` shipped section in
`crates/ui/src/preview/buttons.rs` so the tint is visible in both themes:
in `shipped()`, add a fourth button to the `trio` closure calls using
`theme.remote` labelled `"origin"`.

Run: `./scripts/shoot.sh comp:buttons dark && ./scripts/shoot.sh comp:buttons light`
Then READ both PNGs. The purple must be legible against `bg` in both.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/theme.rs crates/ui/src/preview/buttons.rs
git commit -m "feat(ui): add the remote tint for origin's content state"
```

---

### Task 2: `GitClient::merge_tree_conflicts` — a virtual merge

**Files:**
- Modify: `crates/core/src/git.rs`
- Test: `crates/core/tests/git_sync.rs`

**Interfaces:**
- Produces:
  ```rust
  /// SHIPPED (Task 2, amended in fix round 1): exit status is authoritative
  /// over the path list, because a merge can conflict without any individual
  /// file conflicting (git-merge-tree(1), MISTAKES TO AVOID).
  pub enum MergeTreeOutcome {
      Clean,                    // git exited 0
      Conflicts(Vec<PathBuf>),  // git exited 1; the Vec MAY be empty
      Unknown,                  // git too old, or refused outright
  }
  pub fn merge_tree_conflicts(&self, upstream: &str)
      -> Result<MergeTreeOutcome, GitError>
  ```

- [ ] **Step 1: Write the failing tests**

Add to `crates/core/tests/git_sync.rs` (this file already builds real repos;
follow its `unsigned(repo)` helper, which sets repo-local user and
`gpgsign=false` — production `GitClient` inherits global 1Password signing
config, which hangs headless):

```rust
#[test]
fn merge_tree_reports_clean_and_conflicting_merges_without_touching_the_tree() {
    let scratch = Scratch::new();
    unsigned(&scratch.source);
    let git = GitClient::new(Arc::new(SystemRunner), scratch.source.clone());

    // Two branches touching DIFFERENT files: a clean merge.
    sh(&scratch.source, "git", &["checkout", "-b", "other"]);
    write(&scratch.source.join("only-other"), "remote\n");
    sh(&scratch.source, "git", &["add", "-A"]);
    sh(&scratch.source, "git", &["commit", "-m", "other"]);
    sh(&scratch.source, "git", &["checkout", "main"]);
    write(&scratch.source.join("only-main"), "local\n");
    sh(&scratch.source, "git", &["add", "-A"]);
    sh(&scratch.source, "git", &["commit", "-m", "main"]);

    let head_before = git.head_sha().unwrap();
    assert_eq!(
        git.merge_tree_conflicts("other").unwrap(),
        Some(vec![]),
        "different files merge cleanly"
    );
    assert_eq!(
        git.head_sha().unwrap(),
        head_before,
        "a virtual merge must not move HEAD"
    );
    assert!(!git.merge_in_progress(), "and must not start a merge");

    // Now make the SAME file conflict on both branches.
    sh(&scratch.source, "git", &["checkout", "other"]);
    write(&scratch.source.join("dot_testrc"), "a=other\n");
    sh(&scratch.source, "git", &["add", "-A"]);
    sh(&scratch.source, "git", &["commit", "-m", "other testrc"]);
    sh(&scratch.source, "git", &["checkout", "main"]);
    write(&scratch.source.join("dot_testrc"), "a=main\n");
    sh(&scratch.source, "git", &["add", "-A"]);
    sh(&scratch.source, "git", &["commit", "-m", "main testrc"]);

    let conflicts = git.merge_tree_conflicts("other").unwrap().unwrap();
    assert_eq!(conflicts, vec![PathBuf::from("dot_testrc")]);
    assert!(!git.merge_in_progress(), "still no real merge started");
}
```

`git_sync.rs` today imports `use std::path::Path;` — add `PathBuf` to that
import. Its local helpers are `write(full_path, text)` (TWO args, a full
path — not `(repo, name)`), `unsigned(repo)`, `other_clone(&scratch)`, and
`sh(cwd, program, args)` from `support`. `Scratch` exposes `root`, `home`,
`source`, and `bare` — there is **no** `repo` field and **no**
`new_with_origin()`; `Scratch::new()` already creates the bare origin and
pushes `main` to it.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p tomte-core --test git_sync merge_tree_reports`
Expected: FAIL — method not found.

- [ ] **Step 3: Implement**

In `crates/core/src/git.rs`:

```rust
/// Conflicting paths a merge with `upstream` WOULD produce, computed
/// entirely in the object database — the working tree and index are never
/// touched. `Some(vec![])` = clean. `None` = git predates `--write-tree`
/// (2.38), in which case the caller must fall back to not auto-joining
/// rather than guessing.
pub fn merge_tree_conflicts(&self, upstream: &str) -> Result<Option<Vec<PathBuf>>, GitError> {
    let out = self.run(&["merge-tree", "--write-tree", "--name-only", "HEAD", upstream])?;
    // Exit 0 = clean, 1 = conflicts, >1 = usage/old git.
    if out.exit_code > 1 {
        return Ok(None);
    }
    if out.success() {
        return Ok(Some(Vec::new()));
    }
    // With --name-only the stdout is: <tree-oid>\n<conflicted path>\n...
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(Some(
        text.lines().skip(1).filter(|l| !l.is_empty()).map(PathBuf::from).collect(),
    ))
}
```

Match the file's existing private `run`/`run_utf8` helper style; if `run`
returns `Result<CommandOutput, GitError>` only on spawn failure, use it
directly as above, otherwise add a raw variant that does not treat a
non-zero exit as an error — a conflicting merge-tree is an ANSWER, not a
failure, and must not be logged as an error (use
`CommandRequest::failure_is_expected()`).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tomte-core --test git_sync merge_tree_reports`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/git.rs crates/core/tests/git_sync.rs
git commit -m "feat(core): virtual merge conflict probe via git merge-tree"
```

---

### Task 3: Auto-join histories when the virtual merge is clean

**Files:**
- Modify: `crates/daemon/src/core.rs:555-583` (the `handle_fetch` integration block)
- Test: `crates/core/tests/git_sync.rs`

**Interfaces:**
- Consumes: `GitClient::merge_tree_conflicts` (Task 2).
- Produces: `Event::FetchDone` unchanged; a new log line `"joined N commit(s) from {upstream}"`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_diverged_but_non_conflicting_repo_joins_automatically() {
    // Both sides moved, on different files. Today this is left alone and the
    // user gets a red banner; it must now join itself.
    let scratch = Scratch::new();
    unsigned(&scratch.source);
    let git = GitClient::new(Arc::new(SystemRunner), scratch.source.clone());

    // Origin moves…
    let other = other_clone(&scratch);
    write(&other.join("remote-only"), "x\n");
    sh(&other, "git", &["add", "-A"]);
    sh(&other, "git", &["commit", "-m", "remote"]);
    sh(&other, "git", &["push", "origin", "main"]);
    // …while we commit a DIFFERENT file locally.
    write(&scratch.source.join("local-only"), "y\n");
    sh(&scratch.source, "git", &["add", "-A"]);
    sh(&scratch.source, "git", &["commit", "-m", "local"]);

    git.fetch("origin").unwrap();
    let div = git.divergence("origin/main").unwrap();
    assert!(div.ahead > 0 && div.behind > 0, "precondition: diverged");

    // The virtual merge proves it clean BEFORE anything is written.
    assert_eq!(
        git.merge_tree_conflicts("origin/main").unwrap(),
        MergeTreeOutcome::Clean
    );
    assert!(matches!(git.merge("origin/main").unwrap(), MergeStart::Clean));

    let after = git.divergence("origin/main").unwrap();
    assert_eq!(after.behind, 0, "origin's commits are now local");
    assert!(!git.merge_in_progress());
}
```

`other_clone(&scratch)` already exists in `git_sync.rs` (it clones
`scratch.bare` and sets repo-local unsigned identity) — use it rather than
writing a new clone helper.

- [ ] **Step 2: Run it and watch it fail (or pass trivially — then fix the assertions to bite)**

Run: `cargo test -p tomte-core --test git_sync a_diverged_but_non_conflicting`

- [ ] **Step 3: Extend `handle_fetch`**

Replace the `if behind > 0 && ahead == 0 && clean` block in
`crates/daemon/src/core.rs` with:

```rust
let clean_tree = self.git.dirty_files().map(|d| d.is_empty()).unwrap_or(false);
if behind > 0 && clean_tree {
    if ahead == 0 {
        // Lossless: nothing local to preserve.
        match self.git.ff_merge(&self.remote_ref) { /* unchanged */ }
    } else {
        // Diverged. Join ONLY if a virtual merge proves it clean —
        // Tomte never leaves the repo mid-merge on its own initiative.
        match self.git.merge_tree_conflicts(&self.remote_ref) {
            Ok(MergeTreeOutcome::Clean) => {
                // `merge` returns `MergeStart`. A `Conflicts` here would mean
                // the repo is NOW mid-merge — that must never be logged as a
                // success, and must never be left for the user to discover.
                match self.git.merge(&self.remote_ref) {
                    Ok(MergeStart::Clean) => tomte_core::log_info!(
                        "fetch", "joined {behind} commit(s) from {}", self.remote_ref
                    ),
                    Ok(MergeStart::Conflicts(paths)) => {
                        tomte_core::log_warn!(
                            "fetch",
                            "merge-tree said clean but {} path(s) conflicted; aborting",
                            paths.len()
                        );
                        self.git.merge_abort();
                    }
                    Err(e) => tomte_core::log_warn!("fetch", "join failed: {e}"),
                }
            }
            // NOTE the empty case: `Conflicts(vec![])` means conflicted with
            // no individually-listed file (a directory rename conflict, say).
            // It must NOT be treated as clean — never match on `.is_empty()`.
            Ok(MergeTreeOutcome::Conflicts(paths)) => tomte_core::log_info!(
                "fetch",
                "{} conflict(s) with {} — left for review",
                paths.len(), self.remote_ref
            ),
            Ok(MergeTreeOutcome::Unknown) => tomte_core::log_warn!(
                "fetch", "merge-tree gave no answer; not auto-joining"
            ),
            Err(e) => tomte_core::log_warn!("fetch", "merge probe failed: {e}"),
        }
    }
}
```

- [ ] **Step 4: Run the whole suite**

Run: `cargo test --workspace`
Expected: 26 suites pass.

- [ ] **Step 5: Commit**

```bash
git add crates/daemon/src/core.rs crates/core/tests/git_sync.rs
git commit -m "feat(daemon): auto-join diverged history when a virtual merge is clean"
```

---

### Task 4: Delete the banner, the probe, and `repo_diverged`

**Files:**
- Modify: `crates/app/src/views/dashboard.rs` (the `when_some(repo_diverged, …)` block)
- Modify: `crates/app/src/main.rs` (`spawn_divergence_loop` and its call site)
- Modify: `crates/app/src/model.rs` (`repo_diverged` field + `hydrate_status` arg)
- Modify: `crates/app/src/views/fixtures.rs` (`dashboard-diverged` state)

**Interfaces:**
- Produces: `SyncModel` without `repo_diverged`; `dashboard-diverged` gallery
  state removed from `STATES`.

- [ ] **Step 1: Delete the probe**

Remove `fn spawn_divergence_loop` from `crates/app/src/main.rs` and its call.

- [ ] **Step 2: Delete the banner**

Remove the `.when_some(repo_diverged, |el, (ahead, behind)| { … })` block
from `crates/app/src/views/dashboard.rs` and the `repo_diverged` binding.

- [ ] **Step 3: Delete the model field**

Remove `pub repo_diverged: Option<(u32, u32)>` from `crates/app/src/model.rs`
and every write to it. Remove the `dashboard-diverged` entry from
`fixtures::STATES` and its `build` arm.

- [ ] **Step 4: Verify nothing references it**

Run: `grep -rn 'repo_diverged\|spawn_divergence_loop' crates/`
Expected: no output.

- [ ] **Step 5: Gate + screenshot**

Run: `cargo test --workspace && ./scripts/shoot.sh dashboard dark`
READ the PNG: the dashboard must be banner-free and otherwise unchanged.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor: delete the diverged banner, its probe, and repo_diverged"
```

---

### Task 5: Detect which files origin differs on

**Files:**
- Modify: `crates/core/src/scanner.rs` (`ScanReport`, `scan`)
- Modify: `crates/proto/src/lib.rs` (`DriftSummary`, `PROTOCOL_VERSION`)
- Modify: `crates/daemon/src/core.rs` (carry it into the status snapshot)
- Modify: `crates/app/src/model.rs` (carry it per row)
- Test: `crates/core/tests/scanner_integration.rs`

**Interfaces:**
- Produces:
  - `ScanReport::origin_differs: BTreeSet<PathBuf>` — TARGET paths (not
    source paths) whose origin source differs from local's.
  - `DriftSummary::origin_differs: bool`
  - `PROTOCOL_VERSION = 4`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn origin_differences_are_detected_per_target() {
    let s = Scratch::new();
    // Push a change to origin that the local repo does not have.
    let other = s.root.path().join("other");
    support::sh(s.root.path(), "git",
        &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()]);
    std::fs::write(other.join("dot_testrc"), "a=origin\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "origin edit"]);
    git(&other, &["push"]);
    support::git(&s.source, &["fetch", "origin"]);

    let report = s.scanner().scan().unwrap();
    assert!(
        report.origin_differs.iter().any(|t| t.ends_with(".testrc")),
        "origin's version of .testrc differs: {:?}",
        report.origin_differs
    );
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p tomte-core --test scanner_integration origin_differences`
Expected: FAIL — no field `origin_differs`.

- [ ] **Step 3: Implement in the scanner**

**Add no new subprocess.** `crates/core/src/scanner.rs:116` ALREADY runs
`self.git.changed_files("HEAD", &self.remote_ref)` and already maps those
rels to managed targets — it just discards which half of `source_changed`
came from origin versus the dirty working tree. Split that set and record
the origin half as it maps. Replace the block at `scanner.rs:114-128`:

```rust
// (b) source side via git
//
// NOTE: changed_files runs `git diff --name-only HEAD..<remote>`, which lists
// every file whose CONTENT differs between the two — regardless of which side
// moved. That is deliberately what we want: decision 6.4 is "surface origin
// whenever its content differs, without distinguishing why", so a file only
// this machine changed still has an origin version that differs from ours.
// Do NOT narrow this to an ahead/behind split.
let differs_from_origin: BTreeSet<PathBuf> =
    self.git.changed_files("HEAD", &self.remote_ref)?.into_iter().collect();
let mut source_changed: BTreeSet<PathBuf> = differs_from_origin.clone();
source_changed.extend(self.git.dirty_files()?);

// Which managed TARGETS origin has its own version of. Recorded as the
// existing mapping runs, so the scan costs exactly what it did before.
let mut origin_differs: BTreeSet<PathBuf> = BTreeSet::new();

// Map one source at a time: non-entry files (.chezmoiignore, README, …)
// make `chezmoi target-path` fail, and must not kill the scan.
for rel in &source_changed {
    let abs = source_dir.join(rel);
    let Ok(targets) = self.chezmoi.target_paths(std::slice::from_ref(&abs)) else {
        continue;
    };
    for target in targets {
        if managed.contains(&target) {
            if differs_from_origin.contains(rel) {
                origin_differs.insert(target.clone());
            }
            candidates.insert(target);
        }
    }
}
```

Add `pub origin_differs: BTreeSet<PathBuf>` to `ScanReport` (documenting
that these are TARGET paths, not source paths) and return it.

Leave the existing `?` on `changed_files` alone. It is repo-wide, not
per-target, and turning it non-fatal is a behaviour change that belongs to
its own task — not a side effect of this one.

- [ ] **Step 4: Carry it over the wire**

`crates/proto/src/lib.rs`: add `pub origin_differs: bool` to `DriftSummary`,
bump `PROTOCOL_VERSION` to `4`. The daemon and app ship in the same bundle,
and the existing hello-mismatch takeover handles a stale daemon.

`crates/daemon/src/core.rs`: when building each `DriftSummary`, set
`origin_differs: report_origin_differs.contains(&d.target)`. Store the set on
the core alongside `in_sync_count` so `status_snapshot` can read it.

`crates/app/src/model.rs`: carry the flag through `hydrate_status` onto
whatever per-row struct the views read.

**`handle_fetch` must update it too** (found in review, 2026-08-31).
`handle_fetch` runs on the ~15-minute fetch cadence and already calls
`changed_files("HEAD", &self.remote_ref)`. Wiring only `full_rescan` leaves a
live-event row asserting `origin_differs: false` for up to an hour — until the
hourly full-rescan safety net — while the daemon had the correct answer in
hand. Update the retained set from `handle_fetch` as well.

Mind the ordering trap: `changed` there is computed *before* the Task 3
auto-join, and after a join HEAD contains origin's commits, so those files no
longer differ. Populating `origin_differs` from the pre-join set marks files
as differing exactly when they were merged in. The set must reflect
post-integration state.

- [ ] **Step 5: Run everything**

Run: `cargo test --workspace`
Expected: 26 suites pass (fix `DriftSummary` literals in fixtures/tests).

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: detect and carry which files origin differs on (proto v4)"
```

---

### Task 6: Load origin's content on demand

**Files:**
- Modify: `crates/app/src/merge_inputs.rs`
- Test: `crates/app/src/merge_inputs.rs` (existing `mod tests`)

**Interfaces:**
- Consumes: `GitClient::blob_at`, `ChezmoiClient::execute_template`.
- Produces:
  ```rust
  pub struct MergeInputs { /* … */ pub origin: Option<String> }
  pub fn load(chezmoi: &ChezmoiClient, git: &GitClient, remote_ref: &str,
              journal_path: &Path, target: &Path)
      -> Result<MergeInputs, MergeInputsError>
  ```
  `origin` is `Some` only when it differs from `theirs`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn origin_is_loaded_and_rendered_only_when_it_differs() {
    // Identical origin → None, so the fourth pane never appears for the
    // overwhelmingly common case.
    let f = fixture_with_origin_matching_local();
    assert_eq!(load_inputs(&f).unwrap().origin, None);

    // Different origin → Some, rendered through execute-template when the
    // source is a .tmpl (raw template text is not comparable to a rendered
    // pane).
    let f = fixture_with_origin_differing("a=origin\n");
    assert_eq!(load_inputs(&f).unwrap().origin.as_deref(), Some("a=origin\n"));
}
```

Build the fixtures with the fake `CommandRunner` already used in this file's
tests (`chezmoi.rs` shows the `FakeRunner` idiom): stub `git show
origin/main:dot_testrc` and, for the templated case, `chezmoi
execute-template`.

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p tomte-app origin_is_loaded_and_rendered`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
/// Origin's version of this file, rendered on THIS machine so it is
/// comparable with `theirs`. `None` when origin matches local — the fourth
/// pane exists only when there is something to compare.
fn load_origin(
    chezmoi: &ChezmoiClient,
    git: &GitClient,
    remote_ref: &str,
    source_rel: &Path,
    templated: bool,
    theirs: &str,
) -> Option<String> {
    let raw = git.blob_at(remote_ref, source_rel).ok()??;
    let rendered = if templated {
        chezmoi.execute_template(&raw).ok()?
    } else {
        raw
    };
    let text = String::from_utf8(rendered).ok()?;
    (text != theirs).then_some(text)
}
```

Thread `git` and `remote_ref` through `load`'s signature and update its two
call sites in `crates/app/src/views/merge.rs` and `review.rs`.

- [ ] **Step 4: Run**

Run: `cargo test -p tomte-app merge_inputs`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(app): load origin's rendered content when it differs"
```

---

### Task 7: `Choice::Origin` in the merge engine

**Files:**
- Modify: `crates/core/src/merge.rs`
- Test: `crates/core/src/merge.rs` (existing `mod tests`)

**Interfaces:**
- Produces: `Choice::Origin(String)`, assembled verbatim like `Edited`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn origin_choice_assembles_verbatim_and_keeps_its_provenance() {
    let doc = MergeDocument::compute("x\n", "disk\n", "src\n", MergeOptions::default());
    let mut res = Resolution::new();
    res.set(0, Choice::Origin("from origin\n".into()));
    assert_eq!(doc.assemble(&res).unwrap(), "from origin\n");
    // Provenance is the whole point: it must not be an `Edited`.
    assert!(matches!(res.get(0), Some(Choice::Origin(_))));
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p tomte-core origin_choice_assembles`
Expected: FAIL — no variant `Origin`.

- [ ] **Step 3: Implement**

Add to the enum in `crates/core/src/merge.rs`:

```rust
    /// Origin's version of this region. Assembles exactly like `Edited`;
    /// separate so the strip label and result-row tint can say where the
    /// text came from.
    Origin(String),
```

Add to `assemble`: `(Some(Choice::Origin(text)), _) => out.push_str(text),`

- [ ] **Step 4: Fix every exhaustive match**

Run: `cargo build --workspace` and fix each non-exhaustive match:
`choice_kind` (add `ChoiceKind::Origin`), `choice_lines`,
`region_seed_text`, `region_pattern`. `ChoiceKind::Origin` gets
`label() = "origin"` and `tint() = theme.remote`.

- [ ] **Step 5: Run**

Run: `cargo test --workspace`
Expected: 26 suites pass.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(core): Choice::Origin with its own provenance"
```

---

### Task 8: The fourth pane and the origin pick

**Files:**
- Modify: `crates/app/src/views/merge.rs`
- Modify: `crates/ui/src/components/merge_region.rs` (strip gains an origin button)
- Modify: `crates/app/src/views/fixtures.rs` (new `merge-origin` gallery state)

**Interfaces:**
- Consumes: `MergeInputs::origin` (Task 6), `Choice::Origin` (Task 7),
  `theme.remote` (Task 1).
- Produces: `merge-origin` gallery state; `origin_region_text(region) -> Option<String>`.

- [ ] **Step 1: Write the failing projection test**

Origin's lines must be mapped onto the merge's regions. Reuse the existing
`line_hunks` / `map_line` machinery — this is the same problem solved for
whole-file editing.

```rust
#[test]
fn origin_text_projects_onto_merge_regions() {
    // theirs (local source) vs origin differ on one line; that line sits
    // inside exactly one region, and the origin pick must offer only that
    // region's origin text — not the whole file.
    let state = MergeState::new(&inputs(Some("a\nv = 1\nz\n"), "a\nv = 2\nz\n", "a\nv = 3\nz\n"));
    let origin = "a\nv = 9\nz\n";
    let (seed, starts) = seeded(&state);
    let per_region = origin_region_texts(&seed, &starts, origin);
    let conflict = state.conflicts()[0];
    assert_eq!(per_region[conflict].as_deref(), Some("v = 9\n"));
    // Regions origin agrees on offer nothing — no button, no noise.
    assert_eq!(per_region[0], None);
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p tomte-app origin_text_projects`
Expected: FAIL — `origin_region_texts` not found.

- [ ] **Step 3: Implement the projection**

In `crates/app/src/views/merge.rs`, beside `edited_region_picks`:

```rust
/// Origin's text for each region, or `None` where origin agrees with the
/// seed. Same projection as a free-text edit: line-diff origin against the
/// seed, map each region boundary through the hunks, slice.
fn origin_region_texts(
    seed: &str,
    seed_line_starts: &[usize],
    origin: &str,
) -> Vec<Option<String>> {
    let seed_lines = tomte_core::merge::split_lines(seed);
    let origin_lines = tomte_core::merge::split_lines(origin);
    let mapped = mapped_boundaries(seed, seed_line_starts, origin);
    (0..seed_line_starts.len().saturating_sub(1))
        .map(|r| {
            let old = seed_lines[seed_line_starts[r]..seed_line_starts[r + 1]].concat();
            let new = origin_lines[mapped[r]..mapped[r + 1]].concat();
            (old != new).then_some(new)
        })
        .collect()
}
```

- [ ] **Step 4: Widen the pane geometry from 3 to 4**

The merge view hardcodes **three** input panes in its geometry, not just its
layout, and the result pane occupies index 3:

```
merge.rs:675   region_line_starts: Rc<Vec<[usize; 3]>>
merge.rs:771   pane_scrolls: [gpui::ScrollHandle; 3]
merge.rs:2119  fn region_at(starts: &[[usize; 3]], col, line)
merge.rs:2142  fn line_at(starts: &[[usize; 3]], col, k, f)
merge.rs:2070  const RESULT_LEADER: usize = 3
merge.rs:1233  for col in 0..3   (inside sync_from)
```

**Keep the indices static.** Widen the arrays to 4, move `RESULT_LEADER` to 4,
and when origin is absent simply do not render column 3 — while keeping its
geometry well-defined by mirroring column 2's line starts into slot 3, so
`region_at`/`line_at` never see a zero-length region. A hidden pane's scroll
handle syncing is harmless.

Do **not** make the pane count dynamic. `RESULT_LEADER`, `region_at`,
`line_at` and the divider indices would all become variable, and scroll sync
in this file was already rebuilt once after the 2026-08-22 editor rework —
making its indices dynamic invites that regression back.

- [ ] **Step 5: Render the pane and the pick**

- `pane_col` gains a fourth column when `loaded.inputs.origin.is_some()`,
  labelled `"source · origin"` tinted `theme.remote`, built exactly like the
  others (read-only `TextArea` + `pane_decors`).
- `decision_strip` gains an `origin` pick button (tint `theme.remote`),
  rendered only when that region has origin text.
- Picking it calls `merge.apply(region, Choice::Origin(text), cx)`.
- The existing three panes keep their labels; the local source pane is
  relabelled `"source · this machine"` when the origin pane is present.

- [ ] **Step 6: Gallery state + screenshot**

Add a `merge-origin` fixture: `conflict_inputs()` plus an `origin` differing
on one region. Run:

```bash
./scripts/shoot.sh merge-origin dark && ./scripts/shoot.sh merge-origin light
```

READ both. Confirm four columns are legible at the 840px minimum and the
purple reads as its own provenance.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(merge): fourth origin pane and per-region origin pick"
```

---

### Task 9: Review — split actions and the comparison selector

**Files:**
- Modify: `crates/app/src/views/review.rs`
- Modify: `crates/app/src/views/fixtures.rs` (`review-origin` state)

**Interfaces:**
- Consumes: `DriftSummary::origin_differs` (Task 5), `MergeInputs::origin` (Task 6).
- Produces: `ResolveAction::{KeepDisk, KeepSourceLocal, KeepSourceOrigin}`;
  `DiffPair::{DiskVsLocal, DiskVsOrigin, LocalVsOrigin}`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn source_action_splits_only_when_origin_differs() {
    assert_eq!(
        source_actions(false),
        vec![ResolveAction::KeepSourceLocal],
        "one action when origin agrees — the common case stays simple"
    );
    assert_eq!(
        source_actions(true),
        vec![ResolveAction::KeepSourceLocal, ResolveAction::KeepSourceOrigin]
    );
}

#[test]
fn diff_pair_labels_name_both_sides() {
    assert_eq!(DiffPair::DiskVsLocal.label(), "on disk ↔ source · this machine");
    assert_eq!(DiffPair::DiskVsOrigin.label(), "on disk ↔ source · origin");
    assert_eq!(DiffPair::LocalVsOrigin.label(), "source: this machine ↔ origin");
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p tomte-app source_action_splits`
Expected: FAIL.

- [ ] **Step 3: Implement**

- Rename `ResolveAction::KeepSource` → `KeepSourceLocal`, add
  `KeepSourceOrigin`. Labels: `"Keep source · this machine"`,
  `"Keep source · origin"`; when origin agrees the single button keeps
  reading `"Keep source"`.
- Tints: `KeepDisk` → `theme.drift`, `KeepSourceLocal` → `theme.accent`,
  `KeepSourceOrigin` → `theme.remote` (surface rule: `Tonal` on the header).
- Add `DiffPair` and a segmented control above the diff, shown only when
  `origin_differs`. Selecting recomputes `PreviewState` from the chosen pair.

- [ ] **Step 4: Gallery + screenshots**

Add `review-origin` (selected file with `origin_differs = true`). Run:

```bash
./scripts/shoot.sh review-origin dark && ./scripts/shoot.sh review-origin light
```

READ both: three header actions, the selector, and the purple field in the
diff for origin-only rows.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(review): split source actions and add the diff comparison selector"
```

---

### Task 10: Per-file decisions resolve the git merge

**Files:**
- Modify: `crates/app/src/resolve.rs`
- Modify: `crates/app/src/views/mod.rs` (drop `reconcile_queue` / `reconcile_in_flight`)
- Test: `crates/app/tests/` (new integration test over a real repo)

**Interfaces:**
- Consumes: `merge_tree_conflicts` (Task 2), `resolve_repo_conflict`,
  `finish_repo_reconcile` (existing).
- Produces: `ResolveEngine::ensure_merge_started() -> Result<bool, ResolveError>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn resolving_a_diverged_file_starts_and_finishes_the_merge() {
    // Diverged repo, one conflicting source file. Resolving it must:
    //   1. start the real merge lazily (not before),
    //   2. write + `git add` the resolution,
    //   3. commit the merge once nothing is left conflicted.
    let s = diverged_scratch_with_conflict();
    let engine = engine_for(&s);
    assert!(!s.git().merge_in_progress(), "no merge before a decision");

    engine.ensure_merge_started().unwrap();
    assert!(s.git().merge_in_progress());

    engine.resolve_repo_conflict(&s.source_path(), "resolved\n").unwrap();
    engine.finish_repo_reconcile().unwrap();

    assert!(!s.git().merge_in_progress(), "merge committed");
    assert_eq!(s.git().divergence("origin/main").unwrap().behind, 0);
}
```

**Fixture warning (learned in Task 3).** `diverged_scratch_with_conflict()`
must create a **same-file** conflict. As of Task 3 the daemon auto-joins any
divergence a virtual merge proves clean, so the old idiom — `other_clone` plus
commits to *different* files — no longer produces a stuck divergence; it
produces one that resolves itself. Only a same-file conflict still represents
a divergence a human must answer. `engine_for()` likewise does not exist yet;
build both following the idiom already in `crates/core/tests/git_sync.rs`
(`Scratch` exposes `root`/`home`/`source`/`bare`; `write()` takes a full path).

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p tomte-app resolving_a_diverged_file`
Expected: FAIL — `ensure_merge_started` not found.

- [ ] **Step 3: Implement**

```rust
/// Start the real merge if the repo has diverged and one is not already
/// running. Called from the resolve path the moment the user commits to a
/// decision — never speculatively, so the repo is only ever put into a
/// merge state in response to an explicit choice.
pub fn ensure_merge_started(&self) -> Result<bool, ResolveError> {
    if self.git.merge_in_progress() {
        return Ok(true);
    }
    let branch = self.git.head_branch()?;
    let upstream = format!("origin/{branch}");
    if self.git.divergence(&upstream)?.behind == 0 {
        return Ok(false);
    }
    // Conflicts are the EXPECTED outcome — that is exactly the state the
    // per-file decisions go on to resolve. `Clean` means git merged it
    // unaided and there is nothing left for the user to answer.
    match self.git.merge(&upstream)? {
        MergeStart::Clean => Ok(false),
        MergeStart::Conflicts(_) => Ok(true),
    }
}
```

Call it at the head of the resolve path for a file whose `origin_differs`
is set; route the resolution through `resolve_repo_conflict` when a merge is
in progress, and call `finish_repo_reconcile` when
`conflicted_files()` comes back empty.

Delete `reconcile_queue` / `reconcile_in_flight` from `Shell` and the
`start_reconcile` / `present_next_reconcile` / `cancel_reconcile` plumbing;
the engine calls now hang off ordinary queue rows.

- [ ] **Step 4: Full gate**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets && cargo test --workspace`
Expected: clean, 26 suites pass.

- [ ] **Step 5: Verify on the real repo**

Run: `OP_ACCOUNT=<from settings.toml> ./target/debug/tomted --once`
Expected: a sane drifted/in-sync count and no ERROR lines.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: per-file decisions resolve the git merge; drop the reconcile flow"
```

---

## Self-Review

**Spec coverage.** §2.1 → Task 9. §2.2 → Task 9. §2.3 → Task 8. §2.4 →
Task 4. §3 (Option B, `Choice::Origin`) → Task 7 + Task 8's projection. §4
(render origin, cheap detection / on-demand content) → Task 5 (detection) +
Task 6 (content). §5 (clean auto-join, conflicting → queue rows) → Tasks 2,
3, 10. §6.1 → Task 10. §6.2 → Task 1. §6.3 → Task 8 Step 5. §6.4 (show
whenever content differs, regardless of why) → Task 5 uses
`changed_files("HEAD", remote_ref)`, which does not distinguish behind-only
from diverged. Covered.

**Placeholders.** None: every step names exact files, real code, and a
runnable command with an expected result.

**Type consistency.** `origin_differs` is `BTreeSet<PathBuf>` in
`ScanReport` (Task 5) and `bool` per row in `DriftSummary` (Task 5) — the
narrowing happens in the daemon, stated in Task 5 Step 4.
`MergeInputs::origin` is `Option<String>` throughout (Tasks 6, 8).
`Choice::Origin(String)` and `ChoiceKind::Origin` pair up (Task 7).
`ResolveAction::KeepSourceLocal` is used in Tasks 9 and 10 with the same
name.

**Known risk carried into execution.** Task 5 bumps `PROTOCOL_VERSION` to 4.
The daemon and app ship together in one bundle and the hello-mismatch
takeover already handles a stale daemon, but a developer running an old
`tomted` by hand will see a version mismatch until it is rebuilt — expected,
and it fails loudly rather than silently.
