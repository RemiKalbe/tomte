# Asynchronous, Batched Decisions — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Confirming a file never blocks confirming the next one; consecutive
one-click decisions share one `chezmoi re-add`, one `apply`, one commit and
one push; failures stay on their row; the selection advances by itself.

**Architecture:** `ActionQueue` (already on the branch, `9b0d1a8`) holds
decisions as data and drains them serially from `Shell`, which fetches the
`ResolveEngine` from the `EngineSlot` global. The engine gains one batch
entry point that runs per-file journal sessions (undo stays per file) around
a single shared mutation tail. Views stop guarding with booleans and enqueue
instead; a "Processing" section in the review sidebar shows what is queued.

**Tech Stack:** Rust 2024, gpui 0.2.2, chezmoi + git subprocesses, the
existing `FakeRunner` test double and `DriftLab` e2e harness.

**Spec:** `docs/superpowers/specs/2026-09-02-async-action-queue-design.md`

## Global Constraints

- **Work drains serially.** Every decision mutates one git repository. Two
  in parallel would contend for `index.lock` and interleave commits. Only the
  interface is asynchronous.
- **Undo stays per file.** Every file in a batch has its own journal session
  (`session_start` … `session_end`). Never one session for a batch.
- **Never call `re_add` with an empty slice.** To chezmoi that means "every
  modified file". The client refuses; callers must guard before calling.
- **Never swallow an error.** A failure is surfaced (typed error, log line,
  or a visible state) — never a silent `unwrap_or_default`, never a confident
  zero.
- **GPUI reentrancy:** never update an entity from inside its own update
  closure. A Shell hand-off from a view runs outside that view's `update`
  (see the comment at `merge.rs:1728`, a fixed 2026-08-18 crash).
- **Gate before every commit:** `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings` — **grep its output
  for `^warning`; the exit code is 0 on warnings** — and
  `cargo test --workspace`. Also run the suite with
  `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null` once per task;
  CI has no global git config and has failed two releases on that.
- **Screenshots for UI work:** `./scripts/shoot.sh <state> dark|light`, then
  look at the PNG. Delete the old PNG first — a failed capture with a
  pre-existing file used to print `OK` (fixed, but belt and braces).
- **Commits SSH-signed via 1Password.** On signing failure leave the work
  staged and report; never `--no-gpg-sign`.
- **Copy:** the processing state is labelled `PROCESSING` as a sidebar group
  header, matching `NEEDS A DECISION` / `SAFE TO RESOLVE`. The word
  "working…" is removed.

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/app/src/resolve.rs` | resolve engine | add `resolve_batch`, `commit_batch` |
| `crates/app/src/action_queue.rs` | queue | `Work` closure → `Decision` data; `take_batch` |
| `crates/app/src/views/mod.rs` | `Shell` | own the queue; `enqueue`; `pump`; completion |
| `crates/app/src/views/review.rs` | review view | enqueue not block; Processing section; failure chip; auto-advance |
| `crates/app/src/views/merge.rs` | merge editor | enqueue Save |
| `crates/app/src/views/dashboard.rs` | dashboard | quick actions enqueue |
| `crates/core/src/chezmoi.rs` | chezmoi client | bounded retry on `OnePasswordAuth` |
| `crates/app/src/views/fixtures.rs` | gallery | `review-processing`, `review-failed` |
| `crates/app/tests/resolve_e2e.rs` | e2e | batch pipeline stories |

---

### Task 1: `ResolveEngine::resolve_batch` — one tail for many files

**Files:**
- Modify: `crates/app/src/resolve.rs`
- Test: `crates/app/tests/resolve_e2e.rs`

**Interfaces:**
- Consumes: `ChezmoiClient::re_add(&[PathBuf])`, `apply_many(&[PathBuf])`
  (on the branch), `commit_phase`, `session_start/decision/end`,
  `snapshot_blobs`, `expect_changes`, `ensure_merge_started`, `rescan`.
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum BatchDecision { KeepDisk, KeepSourceLocal }

  pub struct BatchItem { pub target: PathBuf, pub decision: BatchDecision }

  pub struct BatchOutcome {
      /// One entry per item, in input order. `Done` carries that file's OWN
      /// session id, so per-file undo is unchanged.
      pub per_target: Vec<(PathBuf, Result<ResolveOutcome, ResolveError>)>,
  }

  pub fn resolve_batch(&self, items: Vec<BatchItem>) -> BatchOutcome
  ```

- [ ] **Step 1: Write the failing e2e story**

Add to `crates/app/tests/resolve_e2e.rs`, next to `story_a_…`. It uses
`DriftLab` and the `chezmoi_commits_and_pushes_for_itself` helper already in
that file, and counts subprocesses through the daemon log the lab already
captures — check `DriftLab` for its log accessor; if there is none, count
commits on the bare origin instead, which is the observable that matters:

```rust
/// Three one-click decisions must cost ONE re-add, ONE commit and ONE push —
/// not three of each. From the field log: re-add ≈ 2.5s and push ≈ 1.5s per
/// invocation, and both accept a list.
#[test]
fn story_m_a_batch_of_keep_disk_shares_one_commit_and_one_push() {
    let lab = DriftLab::new();
    // Three managed files, all drifted on disk.
    for (src, dst, body) in [
        ("dot_a", ".a", b"a=disk\n" as &[u8]),
        ("dot_b", ".b", b"b=disk\n"),
        ("dot_c", ".c", b"c=disk\n"),
    ] {
        std::fs::write(lab.s.source.join(src), b"x=1\n").unwrap();
        git(&lab.s.source, &["add", "."]);
        git(&lab.s.source, &["commit", "-m", src]);
        lab.s.chezmoi().apply(None).unwrap();
        std::fs::write(lab.s.home.join(dst), body).unwrap();
    }
    git(&lab.s.source, &["push"]);
    let commits_before = commit_count(&lab.bare_git());

    let outcome = lab.engine.resolve_batch(vec![
        BatchItem { target: lab.s.home.join(".a"), decision: BatchDecision::KeepDisk },
        BatchItem { target: lab.s.home.join(".b"), decision: BatchDecision::KeepDisk },
        BatchItem { target: lab.s.home.join(".c"), decision: BatchDecision::KeepDisk },
    ]);

    // Every file resolved, each with its OWN session (undo stays per file).
    let mut sessions = std::collections::BTreeSet::new();
    for (target, result) in &outcome.per_target {
        match result {
            Ok(ResolveOutcome::Done { session, committed, pushed, .. }) => {
                assert!(committed && pushed, "{}: the batch tail must land on origin", target.display());
                sessions.insert(*session);
            }
            other => panic!("{}: expected Done, got {other:?}", target.display()),
        }
    }
    assert_eq!(sessions.len(), 3, "three files, three sessions — undo is per file");

    // ONE commit reached origin for the whole batch.
    assert_eq!(
        commit_count(&lab.bare_git()) - commits_before,
        1,
        "a batch is one commit and one push, not one per file"
    );
    for (src, body) in [("dot_a", "a=disk\n"), ("dot_b", "b=disk\n"), ("dot_c", "c=disk\n")] {
        assert_eq!(std::fs::read_to_string(lab.s.source.join(src)).unwrap(), body);
    }
}

fn commit_count(git: &GitClient) -> usize {
    // rev-list on the bare origin's main
    String::from_utf8(git.run_raw_for_test(&["rev-list", "--count", "main"]).unwrap())
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}
```

`GitClient` has no `run_raw_for_test`; use whatever the file already uses to
inspect the bare repo (grep `bare_git` and `rev-list` in `resolve_e2e.rs` —
`story_i` and `an_unsignable_merge_commit…` count commits already). Copy that
idiom rather than adding a method to `GitClient`.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p tomte-app --test resolve_e2e story_m`
Expected: FAIL — `BatchItem`/`resolve_batch` not found.

- [ ] **Step 3: Implement**

In `crates/app/src/resolve.rs`, beside `keep_disk`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchDecision {
    KeepDisk,
    KeepSourceLocal,
}

pub struct BatchItem {
    pub target: PathBuf,
    pub decision: BatchDecision,
}

pub struct BatchOutcome {
    pub per_target: Vec<(PathBuf, Result<ResolveOutcome, ResolveError>)>,
}

impl ResolveEngine {
    /// Resolve several one-click decisions with ONE shared tail.
    ///
    /// Per file: its own journal session, snapshot and decision, exactly as
    /// `keep_disk` / `keep_source` do — so Undo is unchanged. Shared: one
    /// `expect_changes`, one `chezmoi re-add` for every keep-disk target, one
    /// `chezmoi apply` for every keep-source target, one commit, one push,
    /// one rescan. Those are the seconds (re-add ≈ 2.5s, push ≈ 1.5s, per
    /// INVOCATION); the sessions are milliseconds.
    ///
    /// Mid-merge the shared tail is not available (chezmoi's autoCommit
    /// cannot run with unmerged paths, and each file must be staged into the
    /// merge), so items fall back to their single-file paths one at a time.
    pub fn resolve_batch(&self, items: Vec<BatchItem>) -> BatchOutcome {
        let mut per_target = Vec::with_capacity(items.len());

        // Catch up with origin once for the whole batch. If that opens a
        // conflicted merge, the batch tail cannot be used — run singly.
        match self.ensure_merge_started() {
            Ok(true) => {
                for item in items {
                    let r = match item.decision {
                        BatchDecision::KeepDisk => self.keep_disk(&item.target),
                        BatchDecision::KeepSourceLocal => self.keep_source(&item.target),
                    };
                    per_target.push((item.target, r));
                }
                return BatchOutcome { per_target };
            }
            Ok(false) => {}
            Err(e) => {
                let msg = e.to_string();
                for item in items {
                    per_target.push((item.target, Err(ResolveError::Failed(msg.clone()))));
                }
                return BatchOutcome { per_target };
            }
        }

        // Phase 1 — per file, cheap: session, snapshot, decision. A templated
        // keep-disk is refused here exactly as `keep_disk` refuses it.
        struct Prepared {
            target: PathBuf,
            source: PathBuf,
            decision: BatchDecision,
            session: i64,
        }
        let mut prepared: Vec<Prepared> = Vec::new();
        let mut slots: Vec<Option<usize>> = Vec::with_capacity(items.len());
        for item in items {
            let prep = (|| -> Result<Option<Prepared>, ResolveError> {
                let source = self.chezmoi.source_path(&item.target)?;
                if item.decision == BatchDecision::KeepDisk && is_templated(&source) {
                    return Ok(None);
                }
                let session = self.session_start()?;
                let (action, snapshot_paths) = match item.decision {
                    BatchDecision::KeepDisk => ("keep_disk", vec![item.target.clone(), source.clone()]),
                    BatchDecision::KeepSourceLocal => ("keep_source", vec![item.target.clone()]),
                };
                let hashes = self.snapshot_blobs(snapshot_paths)?;
                let dest_blob = hashes.first().cloned().ok_or_else(|| {
                    ResolveError::Failed("snapshot returned no blob".into())
                })?;
                let source_blob = hashes.get(1).cloned();
                let decision = build_decision(action, &item.target, &dest_blob, source_blob.as_deref());
                self.session_decision(session, decision)?;
                Ok(Some(Prepared { target: item.target.clone(), source, decision: item.decision, session }))
            })();
            match prep {
                Ok(Some(p)) => {
                    slots.push(Some(prepared.len()));
                    per_target.push((p.target.clone(), Ok(ResolveOutcome::NeedsMergeEditor))); // placeholder, overwritten below
                    prepared.push(p);
                }
                Ok(None) => {
                    slots.push(None);
                    per_target.push((item.target, Ok(ResolveOutcome::NeedsMergeEditor)));
                }
                Err(e) => {
                    slots.push(None);
                    per_target.push((item.target, Err(e)));
                }
            }
        }
        if prepared.is_empty() {
            return BatchOutcome { per_target };
        }

        // Phase 2 — the shared tail.
        let tail: Result<(bool, bool, Option<String>), ResolveError> = (|| {
            let mut expect: Vec<PathBuf> = Vec::new();
            for p in &prepared {
                expect.push(p.target.clone());
                if p.decision == BatchDecision::KeepDisk {
                    expect.push(p.source.clone());
                }
            }
            self.expect_changes(expect)?;
            let readds: Vec<PathBuf> = prepared
                .iter()
                .filter(|p| p.decision == BatchDecision::KeepDisk)
                .map(|p| p.target.clone())
                .collect();
            if !readds.is_empty() {
                self.chezmoi.re_add(&readds)?;
            }
            let applies: Vec<PathBuf> = prepared
                .iter()
                .filter(|p| p.decision == BatchDecision::KeepSourceLocal)
                .map(|p| p.target.clone())
                .collect();
            if !applies.is_empty() {
                self.chezmoi.apply_many(&applies)?;
            }
            let targets: Vec<&Path> = prepared.iter().map(|p| p.target.as_path()).collect();
            Ok(self.commit_batch(&targets))
        })();

        // Phase 3 — per file again: end the session, report.
        for p in &prepared {
            let ix = slots
                .iter()
                .position(|s| *s == Some(prepared.iter().position(|q| q.target == p.target).unwrap()))
                .expect("every prepared item has a slot");
            let result = match &tail {
                Ok((committed, pushed, note)) => {
                    let label = match p.decision {
                        BatchDecision::KeepDisk => "keep_disk",
                        BatchDecision::KeepSourceLocal => "keep_source",
                    };
                    match self.session_end(p.session, &format!("{label} {}", display_name(&p.target))) {
                        Ok(()) => Ok(ResolveOutcome::Done {
                            session: p.session,
                            committed: *committed,
                            pushed: *pushed,
                            note: note.clone(),
                        }),
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(ResolveError::Failed(format!("batch failed before this file could land: {e}"))),
            };
            per_target[ix].1 = result;
        }
        if let Err(e) = self.rescan() {
            tomte_core::log_warn!("resolve", "rescan after batch failed: {e}");
        }
        BatchOutcome { per_target }
    }

    /// `commit_phase` for several targets: one commit whose message names
    /// the batch, one push, same rebase-and-retry recovery.
    fn commit_batch(&self, targets: &[&Path]) -> (bool, bool, Option<String>) {
        let message = match targets {
            [one] => commit_message("resolve", one),
            many => format!("Resolve {} files via tomte", many.len()),
        };
        self.commit_with_message(&message)
    }
}
```

`commit_phase(action, target)` builds a message and then commits/pushes.
Split it: extract the commit-and-push body into
`fn commit_with_message(&self, message: &str) -> (bool, bool, Option<String>)`
and have `commit_phase` call it with `commit_message(action, target)`. Do
not duplicate the rebase-and-retry block.

The slot bookkeeping above is deliberately explicit; if you find a cleaner
way to keep `per_target` in input order while some items are skipped, use
it, but the ORDER and the per-item session ids are the contract.

- [ ] **Step 4: Run the story; then the whole file**

Run: `cargo test -p tomte-app --test resolve_e2e story_m`
Expected: PASS.
Run: `cargo test -p tomte-app --test resolve_e2e`
Expected: all pass (the single-file paths are untouched).

- [ ] **Step 5: Add the mid-merge fallback story**

```rust
/// Mid-merge the shared tail is unavailable (autoCommit cannot run over
/// unmerged paths), so a batch must fall back to the single-file paths —
/// and still resolve every item.
#[test]
fn story_n_a_batch_while_mid_merge_falls_back_to_single_file_paths() {
    let lab = DriftLab::new();
    diverge_on_the_managed_file(&lab); // existing helper: same-file conflict
    std::fs::write(&lab.target, b"a=disk\n").unwrap();
    let outcome = lab.engine.resolve_batch(vec![BatchItem {
        target: lab.target.clone(),
        decision: BatchDecision::KeepDisk,
    }]);
    let (_, r) = &outcome.per_target[0];
    assert!(matches!(r, Ok(ResolveOutcome::Done { .. })), "got {r:?}");
    assert!(!lab.source_git().merge_in_progress(), "the only conflict was answered, so the merge concluded");
}
```

Run: `cargo test -p tomte-app --test resolve_e2e story_n` → PASS.

- [ ] **Step 6: Gate and commit**

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E '^warning' | grep -v 'block v0.1.6\|proc-macro-error2'   # must print nothing
cargo test --workspace
GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null cargo test --workspace
git add crates/app/src/resolve.rs crates/app/tests/resolve_e2e.rs
git commit -m "feat(resolve): resolve_batch — one re-add, one commit, one push for many files"
```

---

### Task 2: Decisions as data, and `take_batch`

**Files:**
- Modify: `crates/app/src/action_queue.rs`

**Interfaces:**
- Consumes: `BatchDecision`, `BatchItem` (Task 1), `MergeInputs`.
- Produces:
  ```rust
  pub enum Decision {
      KeepDisk,
      KeepSourceLocal,
      /// Origin's text as the view showed it; the engine refuses if origin
      /// moved since (existing `keep_source_origin` contract).
      KeepSourceOrigin { seen: String },
      /// Merge-editor Save on an ordinary (non-mid-merge) file.
      Merged { resolved: String, inputs: Arc<MergeInputs> },
      /// Merge-editor Save on a file left conflicted by an open merge.
      RepoConflict { source_path: PathBuf, resolved: String },
  }
  impl Decision {
      pub fn label(&self) -> &'static str;   // "Keep disk" / "Keep source · this machine" / "Keep source · origin" / "Save"
      pub fn batchable(&self) -> bool;        // KeepDisk | KeepSourceLocal
      pub fn as_batch(&self) -> Option<BatchDecision>;
  }
  pub struct QueuedAction { pub target: PathBuf, pub decision: Decision }
  impl ActionQueue {
      /// Take the next run of work: every consecutive batchable action, or
      /// exactly one non-batchable one. Marks them all running.
      pub fn take_batch(&mut self) -> Vec<QueuedAction>;
      pub fn finish_all(&mut self, results: &[(PathBuf, Option<SharedString>)]);
  }
  ```
  `running` becomes `Vec<PathBuf>`; `is_processing`, `processing`, `depth`
  keep their meaning.

- [ ] **Step 1: Rewrite the tests to the data model**

Replace the `Work` closure in the existing tests with `Decision::KeepDisk`
etc., then add:

```rust
#[test]
fn take_batch_groups_consecutive_one_click_decisions_and_isolates_the_rest() {
    let mut q = ActionQueue::default();
    q.push(action("/a", Decision::KeepDisk));
    q.push(action("/b", Decision::KeepSourceLocal));
    q.push(action("/c", Decision::Merged { resolved: "x".into(), inputs: fake_inputs("/c") }));
    q.push(action("/d", Decision::KeepDisk));

    let first = q.take_batch();
    assert_eq!(
        first.iter().map(|a| a.target.as_path()).collect::<Vec<_>>(),
        [Path::new("/a"), Path::new("/b")],
        "the leading run of batchable decisions goes together"
    );
    assert!(q.take_batch().is_empty(), "nothing starts while that batch runs");
    q.finish_all(&[(PathBuf::from("/a"), None), (PathBuf::from("/b"), None)]);

    let second = q.take_batch();
    assert_eq!(second.len(), 1, "a merge-editor save runs alone");
    assert!(matches!(second[0].decision, Decision::Merged { .. }));
    q.finish_all(&[(PathBuf::from("/c"), None)]);

    let third = q.take_batch();
    assert_eq!(third[0].target, Path::new("/d"), "the trailing keep-disk is its own batch");
}
```

`fake_inputs` builds a minimal `Arc<MergeInputs>`; the struct's fields are
public (`crates/app/src/merge_inputs.rs:19`) — fill them with empty strings
and `None`.

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p tomte-app --lib action_queue`
Expected: FAIL to compile — `Decision` not found.

- [ ] **Step 3: Implement**

Replace `Work`/`QueuedAction` with the enum above; `running: Vec<PathBuf>`;
`take_batch` pops while `pending.front().decision.batchable()` (collecting
at least one), or pops exactly one non-batchable; `finish_all` clears each
target from `running` and records failures; keep `push` semantics (replace a
pending choice for the same target, clear its failure).

- [ ] **Step 4: Run, gate, commit**

Run: `cargo test -p tomte-app --lib action_queue` → PASS.
Gate as in Task 1 Step 6, then:
```bash
git add crates/app/src/action_queue.rs
git commit -m "feat(app): decisions as data; take_batch groups one-click runs"
```

---

### Task 3: `Shell` owns the queue and pumps it

**Files:**
- Modify: `crates/app/src/views/mod.rs`
- Test: `crates/app/src/views/mod.rs` (existing `#[gpui::test]` idiom at the bottom of the file)

**Interfaces:**
- Consumes: `ActionQueue`, `Decision`, `take_batch`, `finish_all` (Task 2);
  `resolve_batch` (Task 1); `EngineSlot`; `SyncModel::confirm_resolved`;
  `Shell::refresh_pending_merge`; `notify_osa::notify`.
- Produces:
  ```rust
  pub struct Shell { …, pub queue: ActionQueue, pub queue_pumping: bool }
  impl Shell {
      /// Record a decision and start draining if idle. Returns immediately.
      pub fn enqueue(&mut self, action: QueuedAction, cx: &mut Context<Self>);
      fn pump(&mut self, cx: &mut Context<Self>);
  }
  ```

- [ ] **Step 1: Write the failing test**

```rust
/// Two decisions enqueued back to back: the second is accepted while the
/// first runs, and the interface never sees a blocking flag.
#[gpui::test]
fn enqueue_never_blocks_and_drains_in_order(cx: &mut TestAppContext) {
    // Follow `merge_scroll_sync_spans_four_panes_and_the_result` for the
    // window/shell setup in this file. Use a Shell with NO EngineSlot
    // published: pump must then park the queue (nothing runs) rather than
    // panic or drop the decisions — the row stays "processing" until an
    // engine appears, and reconnect re-pumps.
    let shell = /* posed shell */;
    shell.update(cx, |s, cx| {
        s.enqueue(QueuedAction { target: "/a".into(), decision: Decision::KeepDisk }, cx);
        s.enqueue(QueuedAction { target: "/b".into(), decision: Decision::KeepDisk }, cx);
        assert_eq!(s.queue.depth(), 2, "both remembered");
        assert!(s.queue.is_processing(Path::new("/b")), "the second was not discarded");
    });
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p tomte-app --bins enqueue_never_blocks` → FAIL.

- [ ] **Step 3: Implement**

```rust
pub fn enqueue(&mut self, action: QueuedAction, cx: &mut Context<Self>) {
    self.queue.push(action);
    cx.notify();
    self.pump(cx);
}

fn pump(&mut self, cx: &mut Context<Self>) {
    if self.queue_pumping {
        return;
    }
    let Some(engine) = cx.try_global::<crate::EngineSlot>().and_then(|s| s.0.clone()) else {
        // No engine (disconnected). Leave everything queued; `on_connected`
        // must call `pump` again. Never drop a decision silently.
        return;
    };
    let batch = self.queue.take_batch();
    if batch.is_empty() {
        return;
    }
    self.queue_pumping = true;
    cx.notify();
    let weak = cx.weak_entity();
    cx.spawn(async move |_this, cx| {
        let results = cx
            .background_executor()
            .spawn(async move { run_batch(&engine, batch) })
            .await;
        let _ = cx.update(|cx| {
            let _ = weak.update(cx, |shell, cx| {
                shell.queue_pumping = false;
                let mut finished = Vec::new();
                for (target, label, result) in results {
                    let err = match &result {
                        Ok(ResolveOutcome::Done { .. }) => {
                            shell.state.update(cx, |m, cx| { m.confirm_resolved(&target); cx.notify(); });
                            None
                        }
                        Ok(ResolveOutcome::NeedsMergeEditor) => Some("templated · use the merge editor".into()),
                        Ok(ResolveOutcome::ProtectedSpan { detail }) => Some(format!("touches a templated value: {detail}").into()),
                        Err(e) => Some(format!("{label} failed: {e}").into()),
                    };
                    finished.push((target, err));
                }
                shell.queue.finish_all(&finished);
                shell.refresh_pending_merge(cx);
                cx.notify();
                shell.pump(cx); // next batch, if any
            });
        });
    })
    .detach();
}

/// Runs on the background executor. Batchable runs go through
/// `resolve_batch`; everything else through its single-file engine method.
fn run_batch(engine: &ResolveEngine, batch: Vec<QueuedAction>)
    -> Vec<(PathBuf, &'static str, Result<ResolveOutcome, ResolveError>)>
{
    if batch.iter().all(|a| a.decision.batchable()) {
        let labels: Vec<&'static str> = batch.iter().map(|a| a.decision.label()).collect();
        let items = batch.into_iter().map(|a| BatchItem { target: a.target, decision: a.decision.as_batch().unwrap() }).collect();
        return engine.resolve_batch(items).per_target.into_iter().zip(labels).map(|((t, r), l)| (t, l, r)).collect();
    }
    let a = batch.into_iter().next().expect("take_batch never returns empty");
    let label = a.decision.label();
    let r = match a.decision {
        Decision::KeepDisk => engine.keep_disk(&a.target),
        Decision::KeepSourceLocal => engine.keep_source(&a.target),
        Decision::KeepSourceOrigin { seen } => engine.keep_source_origin(&a.target, &seen),
        Decision::Merged { resolved, inputs } => engine.resolve_merged(&inputs, &resolved),
        Decision::RepoConflict { source_path, resolved } => engine.resolve_repo_conflict(&source_path, &resolved).map(|()| ResolveOutcome::Done { session: 0, committed: false, pushed: false, note: None }),
    };
    vec![(a.target, label, r)]
}
```

Wire `pump` into the existing connected/reconnected path (grep
`pending_merge_probed` — the same place the probe runs on connect) so a
queue parked by a disconnect resumes.

`RepoConflict`'s success must ALSO call the existing `repo_conflict_saved`
so the merge concludes when it was the last conflict — look at how
`merge.rs:1749` hands off today and preserve it. Do this in the completion
closure (outside any view's update).

- [ ] **Step 4: Run, gate, commit**

Run: `cargo test -p tomte-app --bins enqueue_never_blocks` → PASS. Gate.
```bash
git add crates/app/src/views/mod.rs
git commit -m "feat(shell): own the action queue and drain it serially"
```

---

### Task 3b: Merge finish and abort take their turn behind the latch

**Why this task exists.** The Task 3 re-review (89319fd) found, pre-existing
and Important: nothing outside `pump`/`batch_finished` reads `queue_pumping`,
so `Shell::finish_pending_merge` (the dashboard's finish button,
`dashboard.rs:411`) and `Shell::abort_origin_merge` (the merge editor's roll
back, `merge.rs:1683`) still spawn a git commit+push or a merge abort while a
batch is draining — the same overlap class as the Critical race closed in
Task 3's fix round, now easier to hit because batches run longer. It also
found that no test reaches `pump`'s real path because no test publishes an
`EngineSlot`: removing the conclusion await, or `catching_panics`, leaves all
335 tests green. Views are bin-only (`crates/app/src/lib.rs` header), so that
coverage must live in `mod.rs`'s own test module — and `tomte-daemon` plus
`tomte-core`'s `test-support` are already dev-dependencies of the bin
(`crates/app/Cargo.toml`), so a real engine can be built there.

**Files:**
- Modify: `crates/app/src/views/mod.rs` — Shell fields, `finish_pending_merge` (:339), `abort_origin_merge` (:259), `pump` (:426), `batch_finished` (:510), `catching_panics` (:724), the park comment (:439-444), the render-latch comment (~:731), tests.
- Modify (struct literals only, for the new field): `crates/app/src/main.rs` (~:479), `crates/app/src/views/fixtures.rs` (~:367), and the eight literals in `mod.rs` tests — `grep -n 'merge_finish_in_flight:'` finds all ten.
- Do NOT touch `dashboard.rs`: it copies `merge_finish_in_flight` (:175-176, :396-411) for its button label. Keep that field and its meaning: "a finish is running right now".

**Interfaces:**
- Consumes (Task 3): `Shell::queue: ActionQueue`, `Shell::queue_pumping`, `Shell::pump`, `Shell::batch_finished(results, conclusion, cx)`, `Shell::conclude_staged_merge`, `catching_panics`, `run_batch`, `crate::EngineSlot(pub Option<Arc<ResolveEngine>>)` (`main.rs:48`).
- Produces:

```rust
/// A repo-level operation the user asked for; runs when the latch is free.
#[derive(Debug)]
pub(super) enum PendingRepoOp {
    FinishMerge,
    AbortMerge(merge::Rollback),
}
// on Shell:
pub(super) deferred_repo_op: Option<PendingRepoOp>,
```

and a test-only lab in `mod.rs`'s test module that Task 7 reuses:

```rust
#[cfg(test)]
pub(crate) struct EngineLab {
    pub s: tomte_core::testsupport::Scratch,
    pub engine: Arc<ResolveEngine>,
    pub journal_path: PathBuf,
}
#[cfg(test)]
impl EngineLab {
    /// Scratch home + bare origin + in-process daemon + connected engine,
    /// published as the app's `EngineSlot` so `pump` runs for real.
    pub(crate) fn publish(cx: &mut TestAppContext) -> Self;
    /// The managed destination file, `~/.testrc` in the scratch home.
    pub(crate) fn target(&self) -> PathBuf;
    /// Its source: `<source>/dot_testrc`.
    pub(crate) fn source_file(&self) -> PathBuf;
}
```

**The one rule this task installs:** `queue_pumping` is true while ANY
app-initiated git mutation runs — a queue batch, that batch's staged-merge
conclusion, a merge finish, or a merge abort. Everything that mutates the
repo enters through `pump`; nothing else spawns git work. Write that as the
doc comment on `queue_pumping`.

- [ ] **Step 1: A deferred-op slot, and finish/abort become requests**

Add `deferred_repo_op: Option<PendingRepoOp>` to `Shell` (all ten struct
literals get `deferred_repo_op: None`). Rewrite the two public methods as
requests — same signatures, so `dashboard.rs:411` and `merge.rs:1683` are
untouched:

```rust
pub fn finish_pending_merge(&mut self, cx: &mut Context<Self>) {
    self.request_repo_op(PendingRepoOp::FinishMerge, cx);
}

pub fn abort_origin_merge(&mut self, rolled_back: merge::Rollback, cx: &mut Context<Self>) {
    // The user leaves the editor NOW; the rollback runs when its turn
    // comes. Decisions they make in Review meanwhile queue behind it and
    // run against the rolled-back repo — which is what "abort" means.
    self.route = Route::Review;
    self.request_repo_op(PendingRepoOp::AbortMerge(rolled_back), cx);
}

fn request_repo_op(&mut self, op: PendingRepoOp, cx: &mut Context<Self>) {
    if let Some(prev) = self.deferred_repo_op.replace(op) {
        tomte_core::log_info!("reconcile", "{prev:?} superseded before it ran");
    }
    cx.notify();
    self.pump(cx);
}
```

- [ ] **Step 2: `pump` runs a deferred op before it takes a batch**

In `pump`, after the latch check and the engine lookup (a parked queue keeps
its deferred op — it stays `Some` until something runs it):

```rust
if let Some(op) = self.deferred_repo_op.take() {
    self.queue_pumping = true;
    cx.notify();
    match op {
        PendingRepoOp::FinishMerge => self.run_finish(engine, cx),
        PendingRepoOp::AbortMerge(rolled_back) => self.run_abort(engine, rolled_back, cx),
    }
    return;
}
let batch = self.queue.take_batch();
// … unchanged from here
```

`run_finish(&mut self, engine: Arc<ResolveEngine>, cx)` is today's body of
`finish_pending_merge` from `self.merge_finish_in_flight = true;` onward;
`run_abort(&mut self, engine, rolled_back, cx)` is today's body of
`abort_origin_merge` from `cx.spawn` onward. Delete the
`if self.merge_finish_in_flight { return; }` guard — the latch is the guard
now. Both completions (inside their `this.update`) end with:

```rust
shell.merge_finish_in_flight = false; // finish only
shell.queue_pumping = false;
cx.notify();
shell.pump(cx); // whatever queued meanwhile
```

The ordering this gives, and that Step 6 pins: a finish or abort pressed
while a batch drains runs *after that batch lands and before the next batch
starts*. For abort that matters: decisions queued after the press run
against the rolled-back repo instead of being staged into a merge the user
just discarded.

- [ ] **Step 3: No git closure can wedge the latch**

Generalise `catching_panics` to any result type, keeping the payload →
`String` extraction and the log line (now `"{what} panicked: {detail}"`):

```rust
fn catching_panics<T>(
    what: &str,
    on_panic: impl FnOnce(String) -> T,
    work: impl FnOnce() -> T + std::panic::UnwindSafe,
) -> T
```

`run_batch` passes `on_panic = |_| one Err per owed target` as today. The
three other background closures — the staged-merge conclusion inside `pump`
(`finish_merge_if_resolved`), `run_finish`'s, and `run_abort`'s — wrap their
work the same way with `on_panic = |d| Err(ResolveError::Failed(format!("internal error — see the log: {d}")))`
(for abort: whatever error shape its `rescanned` arm already reports), so
every completion still runs, still clears the latch, still calls `pump`.
Use `std::panic::AssertUnwindSafe` where a closure captures the engine `Arc`.

- [ ] **Step 4: Say what is true about the park**

`EngineSlot` is republished on EVERY (re)connect (`main.rs:660` loop,
publish at `:745-752`; `main.rs:45-47` says "rebuilt on every successful
(re)connect"), and a reconnect whose `build_engine` fails publishes
`EngineSlot(None)`. Rewrite the park comment (`mod.rs:439-444`) and the
render-latch comment (~`:731`): the park is reached before the first connect
AND after a reconnect that could not build an engine; the queue then waits
with its rows at "processing" and its deferred op intact until a connect
publishes an engine, at which point the render latch pumps. A daemon that
goes away without a reconnect attempt leaves a stale engine in place, and
the batch fails honestly at `SessionStart`.

- [ ] **Step 5: The lab — a real engine inside the bin's tests**

In `mod.rs`'s `#[cfg(test)] mod tests`, add `EngineLab::publish(cx)` by
copying the construction in `crates/app/tests/resolve_e2e.rs:55-89`:
`Scratch::new()`, `Journal::open(&journal_path, "e2e")`, `DaemonCore::new(s.chezmoi(), GitClient::new(Arc::new(SystemRunner), s.source.clone()), journal, "origin/main".into())`,
a `UnixListener::bind` + `std::thread::spawn(move || serve(listener, ServeCtx::ready(core, now_ts, Arc::new(|| {}))))`,
`IpcClient::connect(&sock)`, and the `ResolveEngine { chezmoi: s.chezmoi(), git: …, ipc, journal_path }`
literal — then `cx.update(|cx| cx.set_global(crate::EngineSlot(Some(engine.clone()))))`.
Imports: `tomte_daemon::core::DaemonCore`, `tomte_daemon::server::{ServeCtx, serve}`,
`tomte_core::testsupport::Scratch`, `tomte_journal::Journal`; copy `now_ts`
too. Give it one drifted managed file the way `resolve_e2e.rs` does after
construction (write `b"a=local\n"` to `s.home.join(".testrc")`; the scratch
source already holds `dot_testrc` = `"a=1\n"`), so a `KeepDisk` on
`lab.target()` has work to do. Build the Shell with `cx.new(|cx| Shell { … })`
the way `shell_renders_every_route_without_panicking` does (mod.rs
~:1038-1080). The Shell's `state` may be empty: check `model.rs`'s
`confirm_resolved` is a no-op on an unknown target (it is called from
`batch_finished`), and say so in the test if it is.

- [ ] **Step 6: Tests — each names the mutation that fails it**

```rust
#[gpui::test]
fn a_batch_really_drains_through_pump(cx: &mut TestAppContext) {
    // Finding 4 of the Task 3 re-review: without an engine no test reached
    // run_batch. MUTATION: make pump return before spawning → fails here.
    let lab = EngineLab::publish(cx);
    let shell = /* fresh Shell */;
    shell.update(cx, |s, cx| {
        s.enqueue(QueuedAction { target: lab.target(), decision: Decision::KeepDisk }, cx);
        assert!(s.queue_pumping);
    });
    cx.run_until_parked();
    shell.read_with(cx, |s, _| {
        assert_eq!(s.queue.depth(), 0);
        assert!(!s.queue_pumping);
        assert!(s.queue.failure(&lab.target()).is_none(), "{:?}", s.queue.failure(&lab.target()));
    });
    assert_eq!(std::fs::read(lab.source_file()).unwrap(), b"a=local\n");
}

#[gpui::test]
fn a_finish_pressed_mid_drain_runs_before_the_next_batch(cx: &mut TestAppContext) {
    // MUTATION: in pump, take the batch before checking deferred_repo_op →
    // the probe below sees running() non-empty.
    let lab = EngineLab::publish(cx);
    let shell = /* fresh Shell */;
    shell.update(cx, |s, cx| {
        s.queue_pumping = true; // stand in for a batch that is draining
        s.queue.push(QueuedAction { target: lab.target(), decision: Decision::KeepDisk });
        s.finish_pending_merge(cx); // pressed while draining
        assert!(matches!(s.deferred_repo_op, Some(PendingRepoOp::FinishMerge)));
        assert!(!s.merge_finish_in_flight, "nothing may run while the latch is held");
        assert!(s.queue.running().is_empty());
    });
    // The stand-in batch lands with nothing to report.
    shell.update(cx, |s, cx| s.batch_finished(Vec::new(), None, cx));
    // PROBE before anything is polled: the finish took the latch; the
    // pending batch did NOT.
    shell.read_with(cx, |s, _| {
        assert!(s.deferred_repo_op.is_none());
        assert!(s.merge_finish_in_flight);
        assert!(s.queue_pumping);
        assert!(s.queue.running().is_empty());
        assert_eq!(s.queue.depth(), 1);
    });
    cx.run_until_parked(); // the finish (NotMerging here), then the batch
    shell.read_with(cx, |s, _| {
        assert!(!s.merge_finish_in_flight);
        assert!(!s.queue_pumping);
        assert_eq!(s.queue.depth(), 0);
    });
    assert_eq!(std::fs::read(lab.source_file()).unwrap(), b"a=local\n");
}

#[gpui::test]
fn an_abort_pressed_mid_drain_routes_now_and_runs_later(cx: &mut TestAppContext) {
    // Same shape as the finish test with PendingRepoOp::AbortMerge(rollback),
    // and additionally: route == Route::Review immediately after the press,
    // while the latch is still held. Build the Rollback the way
    // merge.rs:1683's caller does (read merge::Rollback). On a non-merging
    // scratch repo the abort still rescans and merge_done shows the
    // rolled-back banner: assert the review view's outcome banner is Some
    // after run_until_parked.
}

#[gpui::test]
fn a_deferred_op_survives_a_parked_queue(cx: &mut TestAppContext) {
    // No EngineSlot published. MUTATION: pump drops deferred_repo_op when it
    // parks → the second assertion fails.
    let shell = /* fresh Shell */;
    shell.update(cx, |s, cx| s.finish_pending_merge(cx));
    shell.read_with(cx, |s, _| {
        assert!(!s.queue_pumping);
        assert!(matches!(s.deferred_repo_op, Some(PendingRepoOp::FinishMerge)));
    });
}

#[test]
fn catching_panics_hands_back_the_fallback_for_any_type() {
    let v: Result<u8, String> = catching_panics("t", Err, || panic!("boom"));
    assert!(matches!(v, Err(ref d) if d.contains("boom")));
    let v: Result<u8, String> = catching_panics("t", Err, || Ok(7));
    assert_eq!(v, Ok(7));
}
```

Keep `a_panicking_batch_reports_a_failure_for_every_target_it_owed` and
`enqueue_never_blocks_and_drains_in_order` green through the changes.

- [ ] **Step 7: Run, gate, commit**

Run the new tests by name, then the full gate IN THE FOREGROUND:
`cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings`
with its output grepped for `^warning` (only the `block v0.1.6` /
`proc-macro-error2` line may appear); `cargo test --workspace`; the same
once more with `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null`.

```bash
git add crates/app/src/views/mod.rs crates/app/src/main.rs crates/app/src/views/fixtures.rs
git commit -m "fix(shell): merge finish and abort wait their turn behind the queue latch"
```

---

### Task 4: Review view — enqueue, Processing section, failure chip, auto-advance

**Files:**
- Modify: `crates/app/src/views/review.rs`
- Modify: `crates/app/src/views/fixtures.rs` (add `review-processing`, `review-failed`)
- Modify: `crates/app/src/views/mod.rs` (small: `PendingRepoOp::UndoLast`, `Shell::undo_last`, `Shell::is_busy` — see Step 3.1)

**Interfaces:**
- Consumes: `Shell::enqueue`, `Shell::queue`, `Shell::queue_pumping` (Task 3); `Decision::{KeepDisk, KeepSourceLocal, KeepSourceOrigin{seen}, Merged{resolved, inputs}}` (Task 2); `PendingRepoOp`, `Shell::request_repo_op`, `Shell::deferred_repo_op`, `Shell::pump`, `Shell::merge_done(banner, cx)` (Task 3b).
- Produces: `ReviewView::action_in_flight` REMOVED; `fn next_after(...)`; `PendingRepoOp::UndoLast`; `pub fn Shell::undo_last(&mut self, cx)`; `pub fn Shell::is_busy(&self) -> bool`.

- [ ] **Step 1: Write the failing unit test for auto-advance**

The view's row order is `severity_groups`; make the "next" choice a pure
function so it is testable without a window:

```rust
/// After confirming `resolved`, select the next row that still needs a
/// decision: the one after it in display order, else the one before, else
/// none. Rows already processing are skipped — they are not answerable.
pub(super) fn next_after(
    resolved: &Path,
    ordered: &[PathBuf],
    processing: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf>

#[test]
fn next_after_prefers_the_following_row_then_the_preceding_one_and_skips_processing() {
    let order: Vec<PathBuf> = ["/a", "/b", "/c", "/d"].iter().map(PathBuf::from).collect();
    let none = |_: &Path| false;
    assert_eq!(next_after(Path::new("/b"), &order, &none), Some(PathBuf::from("/c")));
    assert_eq!(next_after(Path::new("/d"), &order, &none), Some(PathBuf::from("/c")), "last row: fall back to the one before");
    let c_busy = |p: &Path| p == Path::new("/c");
    assert_eq!(next_after(Path::new("/b"), &order, &c_busy), Some(PathBuf::from("/d")), "a processing row is not a place to land");
    let only: Vec<PathBuf> = vec![PathBuf::from("/a")];
    assert_eq!(next_after(Path::new("/a"), &only, &none), None);
}
```

- [ ] **Step 2: Run and watch it fail** — `cargo test -p tomte-app --bins next_after` → FAIL.

- [ ] **Step 3: Implement**

1. Delete `action_in_flight` and every read/write of it (`review.rs:796,
   828, 910, 927, 938, 968, 977, 985, 1009, 1018, 1195, 1390`). `run_action`,
   `run_action` and `run_keep_both` no longer spawn: they build a
   `QueuedAction` (keep-both is `Decision::Merged { resolved: assembled,
   inputs }`) and call `self.shell.update(cx, |s, cx| s.enqueue(a, cx))`.
   `run_undo` no longer spawns either — Undo is a git mutation, and after
   Task 3b EVERY app-initiated git mutation runs under the Shell latch.
   Undo is not per-target, so it is a repo op, not a decision: add
   `PendingRepoOp::UndoLast` in `mod.rs`, `pub fn Shell::undo_last(&mut self, cx)`
   that calls `request_repo_op(PendingRepoOp::UndoLast, cx)`, and a
   `run_undo(engine, cx)` arm in `pump` mirroring `run_finish`: background
   `engine.undo_last()` under `catching_panics`, completion clears the latch,
   hands `undo_banner(&result)` to the review view through `merge_done`
   (route is already Review — Undo is only pressed there; move `undo_banner`
   to `pub(super)` so `mod.rs` can call it), re-selects the current row from
   inside that review update, then `pump`s. Precedence: `UndoLast` never
   displaces a pending finish or abort and is dropped with a `log_info!` if
   one is pending. Add `pub fn Shell::is_busy(&self) -> bool { self.queue_pumping || self.queue.depth() > 0 || self.deferred_repo_op.is_some() }`
   and disable the Undo button while `shell.is_busy()` — "undo the last
   thing that landed" is only unambiguous when nothing is landing. Test in
   `mod.rs`: `undo_last` while idle takes the latch and, on the `EngineLab`
   with nothing to undo, lands a banner and clears the latch; while a
   `FinishMerge` is pending it is dropped and the finish stays.
2. After enqueueing, compute `ordered` from `severity_groups(&model.drifted)`
   (needs_you then one_click), call `next_after`, and `self.select(next, cx)`
   when `Some`.
3. Sidebar: before `NEEDS A DECISION`, when `shell.queue.depth() > 0`, add a
   `PROCESSING` group via `group_header("PROCESSING", theme)` listing
   `shell.queue.processing()` in order. Those rows render through
   `target_row` with a muted tint and a trailing `ui::status_text` of the
   decision's label; they are not clickable targets.
4. A row with `shell.queue.failure(target)` renders that message as a
   `ui::status_text(theme, msg, ui::StatusTone::Conflict)` under the file
   name, and its action buttons stay ENABLED — choosing again is the retry.
5. Header: replace the `if self.action_in_flight { "working…" }` branch with
   the action buttons always, disabled only for the SELECTED target while
   `shell.queue.is_processing(selected)`.
6. Fixtures: `review-processing` poses two rows processing (`shell.queue`
   pushed directly with `Decision::KeepDisk`, `queue_pumping = true`);
   `review-failed` poses one failure via `queue.finish_all(&[(t, Some("Keep disk failed: chezmoi exited with code 1".into()))])`.

- [ ] **Step 4: Screenshots**

```bash
rm -f shots/review-processing-*.png shots/review-failed-*.png
./scripts/shoot.sh review-processing dark; ./scripts/shoot.sh review-processing light
./scripts/shoot.sh review-failed dark;     ./scripts/shoot.sh review-failed light
./scripts/shoot.sh review dark
```
READ all five. The `PROCESSING` header must match the other two groups'
typography; failed rows must be legible in both themes; the plain `review`
state must be unchanged.

- [ ] **Step 5: Gate and commit**

Gate as before (the `every_gallery_state_builds_and_renders` test covers the
new states automatically). Then:
```bash
git add crates/app/src/views/review.rs crates/app/src/views/fixtures.rs crates/app/src/views/mod.rs
git commit -m "feat(review): decisions enqueue; undo behind the latch; Processing section; failure chips; auto-advance"
```

---

### Task 5: Merge editor and dashboard enqueue too

**Files:**
- Modify: `crates/app/src/views/merge.rs` (`save`, `saving`)
- Modify: `crates/app/src/views/dashboard.rs` (`run_quick_action`, `dashboard_action_in_flight`)

**Interfaces:**
- Consumes: `Shell::enqueue`, `Decision::{Merged, RepoConflict, KeepDisk, KeepSourceLocal}`.
- Produces: `MergeView::saving` REMOVED; `Shell::dashboard_action_in_flight` REMOVED; `run_quick_action` REMOVED.

- [ ] **Step 1: Merge editor**

`save()` (merge.rs:1690) builds `Decision::RepoConflict { source_path, resolved }`
when `self.repo_reconcile`, else `Decision::Merged { resolved, inputs }`,
enqueues via `self.shell.update(...)`, then hands back to Review exactly as
the `Done` branch does today (`shell.merge_done(banner, cx)` with a banner
reading `"Save queued · {file}"`, tint `BannerTint::Ok`, `undoable: false`).
The outcome banner for the actual result arrives through the queue's
completion (Task 3) — extend that completion to call
`shell.review.update(|r| r.last_outcome = Some(outcome_banner(...)))` for
the target, so the user still sees "committed & pushed" when it lands.
Delete `saving` (merge.rs:991, 1071, 1111, 1131, 1691, 1715).

- [ ] **Step 2: Dashboard**

`quick_action_button`'s handler calls
`shell.update(cx, |s, cx| s.enqueue(QueuedAction { target, decision }, cx))`
where `decision` is `Decision::KeepDisk` / `Decision::KeepSourceLocal` from
the `ResolveAction`. Delete `run_quick_action` and
`Shell::dashboard_action_in_flight` (mod.rs) and every read of it. Keep
`quick_action_body` + `notify`: call them from the queue completion
(Task 3) when the decision originated from the dashboard — add a
`pub notify_on_done: bool` to `QueuedAction`, `false` from Review, `true`
from the dashboard.

- [ ] **Step 3: Tests**

- `the_dashboards_own_action_can_strand_a_merge` (resolve_e2e.rs) still
  passes — it drives the engine directly.
- Add to `views/mod.rs` tests: a `#[gpui::test]` that enqueues a
  `Decision::Merged` then a `Decision::KeepDisk` and asserts `depth() == 2`
  and both are `is_processing` — the merge editor's Save no longer blocks a
  review decision.

- [ ] **Step 4: Gate, screenshots (`merge`, `dashboard` unchanged), commit**

```bash
git add crates/app/src/views/merge.rs crates/app/src/views/dashboard.rs crates/app/src/views/mod.rs
git commit -m "feat: merge-editor Save and dashboard quick actions go through the queue"
```

---

### Task 6: Retry transient 1Password failures

**Files:**
- Modify: `crates/core/src/chezmoi.rs` (`run_ok_with_timeout`)
- Test: `crates/core/src/chezmoi.rs` `mod tests`

**Interfaces:**
- Produces: bounded retry inside `run_ok_with_timeout` on
  `EvalFailureKind::OnePasswordAuth` only.

- [ ] **Step 1: Write the failing tests**

```rust
/// A templated file rendered through `onepasswordRead` fails when `op`
/// hiccups, and retrying by hand works — so retry by machine, bounded and
/// logged, the way `GitClient` treats index.lock contention.
#[test]
fn a_transient_1password_failure_is_retried() {
    let fake = Arc::new(FakeRunner::new());
    fake.push_ok(1, "", "chezmoi: template: x.tmpl:1:1: executing at <onepasswordRead>: error calling onepasswordRead: op signin --raw: exit status 1\n");
    fake.push_ok(0, "rendered\n", "");
    let out = client(fake.clone()).cat(Path::new("/Users/x/.zshrc")).unwrap();
    assert_eq!(out, b"rendered\n");
    assert_eq!(fake.calls().len(), 2, "retried once, then succeeded");
}

/// Multiple-accounts is a CONFIGURATION error, not a hiccup: retrying it
/// only delays the real hint ("select an account in Settings").
#[test]
fn a_multiple_accounts_failure_is_not_retried() {
    let fake = Arc::new(FakeRunner::new());
    fake.push_ok(1, "", "[ERROR] multiple accounts found. Use the --account flag or set the OP_ACCOUNT environment variable\n");
    let err = client(fake.clone()).cat(Path::new("/Users/x/.zshrc")).unwrap_err();
    assert!(matches!(err, ChezmoiError::Eval(EvalFailure { kind: EvalFailureKind::OnePasswordMultipleAccounts, .. })));
    assert_eq!(fake.calls().len(), 1, "no retry");
}

#[test]
fn a_persistent_1password_failure_still_fails_after_the_bounded_retries() {
    let fake = Arc::new(FakeRunner::new());
    for _ in 0..8 {
        fake.push_ok(1, "", "error calling onepasswordRead: op signin --raw: exit status 1\n");
    }
    let err = client(fake.clone()).cat(Path::new("/Users/x/.zshrc")).unwrap_err();
    assert!(matches!(err, ChezmoiError::Eval(EvalFailure { kind: EvalFailureKind::OnePasswordAuth, .. })));
    assert!(fake.calls().len() <= 4, "bounded: {} attempts", fake.calls().len());
}
```

- [ ] **Step 2: Run and watch them fail** — `cargo test -p tomte-core --lib 1password` → the first and third FAIL.

- [ ] **Step 3: Implement**

In `run_ok_with_timeout`, wrap the runner call in a loop of at most 3
attempts; on `classify_eval_stderr(..) == Some(kind)` with
`kind == OnePasswordAuth` and attempts remaining, `log_info!("chezmoi",
"1Password not ready (attempt {n}/3) for `chezmoi {}`: retrying", …)`,
sleep 250ms, retry. Any other failure returns immediately as today.

- [ ] **Step 4: Run, gate, commit**

```bash
git add crates/core/src/chezmoi.rs
git commit -m "fix(chezmoi): retry a transient 1Password failure, bounded and logged"
```

---

### Task 7: End-to-end: a sweep of three files is three clicks and one push

**Files:**
- Test: `crates/app/src/views/mod.rs` (its `#[cfg(test)] mod tests` — views are bin-only, so `crates/app/tests/resolve_e2e.rs` cannot reach `Shell`; Task 3b's `EngineLab::publish(cx)` builds the whole stack inside the bin's tests, and that is where this story lives)
- Modify: `CHANGELOG.md` (an `## Unreleased` section; the release task bumps it)

**Interfaces:**
- Consumes: `EngineLab { s: Scratch, engine: Arc<ResolveEngine>, journal_path }` with `publish(cx)`, `target()`, `source_file()` (Task 3b, mod.rs tests); `Shell::enqueue(QueuedAction, cx)` (Task 3); `Decision::KeepDisk`, `ActionQueue::{depth, failure, running}` (Task 2); `Shell::pump` and `queue_pumping` (Task 3).

- [ ] **Step 1: The story — through the user's own path**

Drive `Shell::enqueue`, not `run_batch`: the point is that three clicks
produce the right number of pushes, and only the real entry point knows
where batch boundaries fall. On the lab's scratch, do what `resolve_e2e.rs`
does for its batch stories — copy `chezmoi_commits_and_pushes_for_itself`
(writes autoCommit/autoPush into the scratch's `chezmoi.toml`; without it
the fixture is blind to the push the user's config performs) and the setup
that adds two more managed files beside `.testrc` and diverges all three on
disk (search `resolve_e2e.rs` for `MANAGED_BASE`). `origin_commit_count`
is `git rev-list --count main` in `lab.s.bare` via `std::process::Command`
(Task 1's idiom). "Not mid-merge" is whatever `resolve_e2e.rs` asserts after
its batch stories (search it for `MERGE_HEAD`).

Two stories, because `enqueue` pumps synchronously and so the FIRST click
always drains alone — batching only ever joins decisions made *while* one is
running. Pin both halves honestly; do not fudge a count to match the title:

```rust
#[gpui::test]
fn three_decisions_queued_before_the_drain_are_one_push(cx: &mut TestAppContext) {
    let lab = EngineLab::publish(cx);
    // chezmoi_commits_and_pushes_for_itself(&lab.s); two more managed files; all three drifted on disk
    let before = origin_commit_count(&lab);
    let shell = /* fresh Shell as Task 3b's tests build one */;
    shell.update(cx, |s, cx| {
        for t in &targets {
            s.queue.push(QueuedAction { target: t.clone(), decision: Decision::KeepDisk });
        }
        s.pump(cx); // one drain, one batch of three
        assert_eq!(s.queue.running().len(), 3);
    });
    cx.run_until_parked();
    shell.read_with(cx, |s, _| {
        assert_eq!(s.queue.depth(), 0);
        assert!(!s.queue_pumping);
        for t in &targets {
            assert!(s.queue.failure(t).is_none(), "{t:?}: {:?}", s.queue.failure(t));
        }
    });
    for (src, want) in sources.iter().zip(&disk_contents) {
        assert_eq!(std::fs::read(src).unwrap(), *want);
    }
    assert_eq!(origin_commit_count(&lab), before + 1, "three decisions, ONE push");
    assert!(!mid_merge(&lab));
    // Per-file undo (§2.3): three DISTINCT finished sessions in the journal —
    // open `lab.journal_path` with `Journal::open_read_only` and count the
    // way `resolve_e2e.rs`'s batch story does.
}

#[gpui::test]
fn clicks_during_a_drain_join_the_next_batch(cx: &mut TestAppContext) {
    // Same lab. enqueue(a) starts a drain of ONE; enqueue(b), enqueue(c)
    // land while it runs and must drain together afterwards: exactly TWO
    // pushes for three clicks, never three.
    let lab = EngineLab::publish(cx);
    let before = origin_commit_count(&lab);
    let shell = /* fresh Shell */;
    shell.update(cx, |s, cx| {
        s.enqueue(QueuedAction { target: a.clone(), decision: Decision::KeepDisk }, cx);
        assert_eq!(s.queue.running(), &[a.clone()]);
        s.enqueue(QueuedAction { target: b.clone(), decision: Decision::KeepDisk }, cx);
        s.enqueue(QueuedAction { target: c.clone(), decision: Decision::KeepDisk }, cx);
        assert_eq!(s.queue.depth(), 3);
        assert_eq!(s.queue.running().len(), 1, "b and c wait for a's batch");
    });
    cx.run_until_parked();
    shell.read_with(cx, |s, _| assert_eq!(s.queue.depth(), 0));
    assert_eq!(origin_commit_count(&lab), before + 2, "one push for a, one for b+c");
}
```

If `run_until_parked` returns before the second batch has been taken
(because `batch_finished` → `pump` → spawn happens inside the same
foreground update), call it again and note in the test why. If either count
comes out different from what is written here, the test is telling you how
the queue actually behaves: report it rather than adjusting the assertion.

- [ ] **Step 2: Run, gate, screenshot the whole gallery once**

```bash
cargo test --workspace
GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null cargo test --workspace
rm -f shots/*.png && ./scripts/shoot.sh all dark
```
Look at `review`, `review-processing`, `review-failed`, `merge`, `dashboard`.

- [ ] **Step 3: Changelog**

Under a new `## Unreleased` heading at the top of `CHANGELOG.md`, in the
existing voice, cover: decisions no longer block each other — confirm one,
move on, it runs in the background; decisions made while one is running
share a single re-add, commit and push (say it that way — the first click
of a sweep drains alone, and the entry must not promise one push per
sweep); failures stay on the row; the selection advances; a merge finish or
roll-back pressed during a drain waits its turn; transient 1Password
failures retry.

- [ ] **Step 4: Commit**

```bash
git add crates/app/src/views/mod.rs CHANGELOG.md
git commit -m "test: a sweep of decisions shares one push; changelog"
```

---

## Self-Review

**Spec coverage.** §2.1 (serial, non-blocking) → Tasks 2, 3, 4, 5. §2.2
(batch one-click) → Tasks 1, 2, 3. §2.3 (per-file undo) → Task 1's
per-item sessions and the `sessions.len() == 3` assertion. §2.4 (last choice
wins) → foundation commit, re-tested in Task 2. §2.5 (failures stick) →
Task 3 `finish_all` + Task 4 chip. §2.6 (advance) → Task 4 `next_after`.
§2.7 (op retry) → Task 6. §2.8 (empty re_add) → foundation.

**Placeholders.** Task 1's `commit_count` helper is told to copy an existing
idiom rather than invent an API. Task 3's test setup says which existing
test to copy. Nothing says "TBD".

**Type consistency.** `BatchDecision`/`BatchItem`/`BatchOutcome` (Task 1)
are consumed by `Decision::as_batch` (Task 2) and `run_batch` (Task 3) with
the same names. `Shell::enqueue(QueuedAction, cx)` is the one entry point
Tasks 4 and 5 call. `finish_all(&[(PathBuf, Option<SharedString>)])` matches
Task 3's `finished` vector.

**Known risk.** Task 3's `RepoConflict` completion must call
`repo_conflict_saved` outside any view update — the reentrancy constraint
that crashed gpui on 2026-08-18. The executor is told where the existing
hand-off lives.
