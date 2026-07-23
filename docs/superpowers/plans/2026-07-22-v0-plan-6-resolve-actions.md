# chezmoi-ui v0 — Plan 6: Resolution Actions & Sync Pipeline

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Drift becomes fixable from the UI (spec §5, §6.3): one-click **keep disk** (re-add to source) and **keep source** (apply) with session snapshots and **undo**, plus **Sync all** (pull + apply when no decisions are needed). Every mutation is journaled, pre-announced to the daemon, and reversible.

**Scope split:** the old "Plan 6" bundle is two plans. This plan = actions + pipeline for NON-CONFLICT drift. Plan 7 = the three-pane merge editor (conflicts, templated files, write-backs) + .app/LaunchAgent packaging.

**Architecture:** A `ResolveEngine` in the czui-app lib orchestrates: journal session via IPC (SessionStart → SnapshotBlobs → decisions → SessionEnd), pre-announce via ExpectChanges, mutate via chezmoi (`re-add`, `apply`, `update`) and git (commit/push fallback when chezmoi autoCommit is off), request Rescan after. All blocking work runs on the background executor. Failures are outcomes, not errors: a locked 1Password failing the commit still leaves the resolution DONE locally, reported honestly (spec §10).

**Key facts verified on the real machine:**
- `chezmoi re-add <target>` exists and re-adds modified files BUT "will not overwrite templates" (silently ignores them) → the engine must pre-detect `.tmpl` sources and refuse with a "needs the merge editor" outcome.
- `chezmoi update` = pull changes from the source repo and apply.
- Remi's chezmoi config has `[git] autoCommit = true, autoPush = true` — chezmoi commits/pushes source changes made through chezmoi commands itself; commits are SSH-signed via 1Password and CAN FAIL when it's locked. The engine treats commit/push as best-effort with honest reporting, and falls back to direct git commit only when the repo is left dirty.

## Global Constraints

Identical to Plan 1's (fmt/clippy `-D warnings`/full tests before every commit; no unwrap/expect in lib code; subprocess discipline), plus:

- No new dependencies.
- Mutations ONLY in the app process, user-initiated (spec §3.2); every mutation path MUST: snapshot blobs first, pre-announce via ExpectChanges, journal the decision, request Rescan after (spec §6.3).
- 1Password may be locked: on `git commit` signing failure, stage-and-report (never `--no-gpg-sign`); on subagent `git commit` failure for the REPO's own commits, report "staged, commit blocked".
- The running UI must never block: all ResolveEngine calls go through `cx.background_executor()`.

## File Structure

```
crates/core/src/git.rs            # + add_all, commit, push, head_sha
crates/core/src/chezmoi.rs        # + re_add, update
crates/journal/src/lib.rs         # + last_finished_session query
crates/app/src/lib.rs             # + pub mod resolve;
crates/app/src/resolve.rs         # ResolveEngine (actions + undo + outcomes)
crates/app/src/views/review.rs    # live action buttons + outcome banner + undo
crates/app/src/views/dashboard.rs # quick actions on drifted rows
crates/app/src/main.rs            # SyncAll menu command wired
crates/app/tests/resolve_e2e.rs   # drift stories: keep_disk/keep_source/sync_all/undo
```

---

### Task 1: Core mutations — git write ops + chezmoi re-add/update + journal session query

**Files:**
- Modify: `crates/core/src/git.rs`, `crates/core/src/chezmoi.rs`, `crates/journal/src/lib.rs`

**Interfaces:**
- `GitClient::add_all() -> Result<(), GitError>` (`git add -A`)
- `GitClient::commit(message: &str) -> Result<String, GitError>` (returns new HEAD sha via rev_parse; error surfaces stderr — 1Password signing failures arrive here)
- `GitClient::push(remote: &str) -> Result<(), GitError>` (120s timeout like fetch)
- `GitClient::head_sha() -> Result<String, GitError>` (`rev_parse("HEAD")` convenience)
- `ChezmoiClient::re_add(target: &Path) -> Result<(), ChezmoiError>`
- `ChezmoiClient::update() -> Result<(), ChezmoiError>` (`chezmoi update --no-tty`; 120s timeout override — it pulls over the network)
- `Journal::last_finished_session(&self) -> Result<Option<(i64, serde_json::Value)>, JournalError>` — newest session with `finished_ts NOT NULL`, returning (id, decisions array)

TDD: hermetic tests in each file's existing test module — git ops against the existing `scratch()` temp-repo helper (commit asserts sha changes + `-c commit.gpgsign=false` in the helper already handles signing); `re_add` via FakeRunner asserting args `["re-add", <target>]`; `update` asserting args + timeout; journal query via in-memory sessions. Commit: `feat(core,journal): mutation primitives for resolution actions`.

- [x] Write failing tests (6 new: add_all+commit+push roundtrip in a scratch repo asserting a second commit exists and the bare remote received it; commit-failure error surface; re_add args; update args+timeout; last_finished_session returns newest finished only / None when unfinished)
- [x] Implement (each method ~5 lines following existing patterns; `update` passes `Duration::from_secs(120)` like `fetch`)
- [x] Full gate; commit.

---

### Task 2: ResolveEngine

**Files:**
- Modify: `crates/app/src/lib.rs` (`pub mod resolve;`)
- Create: `crates/app/src/resolve.rs`

**Interfaces (UI and tests build against these exactly):**
```rust
pub struct ResolveEngine {
    // Arc'd so background closures can own clones cheaply
    pub chezmoi: czui_core::chezmoi::ChezmoiClient,
    pub git: czui_core::git::GitClient,
    pub ipc: std::sync::Arc<czui_app::ipc::IpcClient>, // path within crate: crate::ipc
    pub journal_path: std::path::PathBuf,               // read-only opens for undo
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    Done { session: i64, committed: bool, pushed: bool, note: Option<String> },
    /// Templated source: one-click is unsafe; merge editor (Plan 7) required.
    NeedsMergeEditor,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError { /* Chezmoi/Git/Ipc/Journal/Io via #[from] + Failed(String) */ }

impl ResolveEngine {
    /// Keep the on-disk version: re-add into the source state.
    pub fn keep_disk(&self, target: &Path) -> Result<ResolveOutcome, ResolveError>;
    /// Restore chezmoi's version: apply the target.
    pub fn keep_source(&self, target: &Path) -> Result<ResolveOutcome, ResolveError>;
    /// Pull + apply. Caller guarantees zero pending decisions (menu gating).
    pub fn sync_all(&self) -> Result<ResolveOutcome, ResolveError>;
    /// Restore destination files of the LAST finished session from blobs.
    pub fn undo_last(&self) -> Result<Option<i64>, ResolveError>; // session undone
}
```

**Behavior contract (each step, in order — the tests encode this):**
1. `keep_disk`: `source_path(target)`; if extension == "tmpl" → `Ok(NeedsMergeEditor)` (chezmoi re-add silently ignores templates — never pretend success). Else: IPC `SessionStart` → `SnapshotBlobs([target, source_abs])` → decision json `{action:"keep_disk", target, blobs}` via `SessionDecision` → `ExpectChanges([target, source_abs], ttl 60)` → `chezmoi re-add` → commit phase → `SessionEnd` → IPC `Rescan` → `Done`.
2. Commit phase (shared): if `git.dirty_files()` is non-empty (chezmoi autoCommit off or failed): `add_all` + `commit("chezmoi-ui: <action> <file-name>")`; a commit error (e.g. 1Password locked) sets `committed:false, note:Some(stderr-ish)` and CONTINUES — the resolution itself succeeded. `push("origin")` best-effort likewise (only attempted after a successful/preexisting commit; failure → `pushed:false`, note appended).
3. `keep_source`: snapshot `[target]` only, `ExpectChanges([target])`, `chezmoi apply <target>`, no commit phase (apply doesn't touch the source repo), SessionEnd + Rescan.
4. `sync_all`: SessionStart (decision `{action:"sync_all"}`) → `chezmoi update` (its applies make targets == rendered; the daemon probes them as InSync, so no ExpectChanges needed — but pass the journal a `Fetch`-style meaning via the decision) → SessionEnd + Rescan. `update` failure → `ResolveError::Failed` with the stderr (network/merge conflicts belong to Plan 7).
5. `undo_last`: `Journal::open_read_only(journal_path)` → `last_finished_session` → for each decision with a `target` + `dest_blob`: `ExpectChanges([target])`, write blob bytes back to the target path (create parents). Source-side revert is Plan 7 (note in doc-comment). New session journaling the undo (`{action:"undo", of: <id>}`), Rescan. Returns `Ok(Some(id))` or `Ok(None)` when no session exists.

TDD: unit-test the pure helpers (template detection from a source path, commit-message formatting); the full flows are integration-tested in Task 4. Commit: `feat(app): ResolveEngine — keep disk/source, sync all, undo with sessions`.

- [x] Failing unit tests → implement → gate → commit.

---

### Task 3: UI wiring

**Files:**
- Modify: `crates/app/src/views/review.rs`, `crates/app/src/views/dashboard.rs`, `crates/app/src/views/mod.rs`, `crates/app/src/main.rs`

**Behavior:**
- `Shell` owns `Option<Arc<ResolveEngine>>` (built in main.rs once the IPC client connects; None while disconnected → buttons disabled with "daemon not connected" tooltip). Passed down like `paths`.
- **Review detail**: "keep disk" / "keep source" buttons become live for the selected target (accent-styled, `.cursor_pointer`): click → spawn on background executor → outcome lands via `WeakEntity::update` into a `last_outcome: Option<OutcomeBanner>` on ReviewView, rendered as a slim banner above the diff: success (`ok` tint) "kept disk version — committed & pushed" / degraded variants ("committed, push failed: …" in drift tint) / `NeedsMergeEditor` ("templated file — needs the merge editor, arriving next milestone"). While an action runs: buttons disabled + "working…" text. After outcome: preview reloads (existing `select()` re-trigger).
- **Undo**: when `last_outcome` is a `Done`, the banner includes an "Undo" button → `undo_last()` → banner updates ("restored N files from snapshots").
- **Dashboard**: drifted rows get compact `keep disk` / `keep source` inline buttons (right-aligned, before the chip) calling the same engine; outcome surfaces via osascript notification (reuse `notify_osa`) instead of a banner (the dashboard is transient).
- **Menubar / tile Sync all**: `MenuCommand::SyncAll` (already plumbed as a no-op) now calls `sync_all()` when the model says `drifted.is_empty() && !scanning && degraded.is_none()`; the tile's Review link stays as-is. Success → notification "synced with origin"; failure → notification with the error note.
- All spawns follow the existing pattern (`cx.background_executor().spawn` + `WeakEntity::update`); NO engine call on the main thread.

Gate + render smoke tests still pass (buttons render in the existing smoke windows; no new test API needed, but extend the smoke model with a non-templated drifted target so the live buttons render). Commit: `feat(app): live resolve actions in review/dashboard, sync-all in menubar`.

- [x] Implement per contract → extend render smoke → gate → commit.

---

### Task 4: E2E drift stories

**Files:**
- Create: `crates/app/tests/resolve_e2e.rs`

Using the established pattern (scratch home + bare origin + in-process `serve()` daemon + real chezmoi/git via `czui_core::testsupport::Scratch`), one test per story, asserting BOTH filesystem truth and journal truth:

- [x] **Story A — keep disk**: drift `.testrc` on disk → `keep_disk` → source file content == disk content; a new git commit exists (scratch has no autoCommit → engine's fallback commit fired, `committed: true`); journal has session with `keep_disk` decision + blobs exist; daemon probe reports InSync after.
- [x] **Story B — keep source**: drift on disk → `keep_source` → disk content restored to rendered; journal session recorded; InSync after.
- [x] **Story C — sync all**: second clone pushes a change; local `fetch` shows behind → `sync_all` → destination file contains the remote change; InSync.
- [x] **Story D — undo**: after Story-B-style `keep_source`, `undo_last` → disk content back to the drifted version; journal has the undo session; daemon reports the drift again after rescan.
- [x] **Story E — templated file**: make the source a `.tmpl` → `keep_disk` returns `NeedsMergeEditor` and NOTHING changed (no session mutation beyond none, source untouched).

> Task 4 finding: stories B/D exposed that `chezmoi apply` on an externally modified destination prompts `(diff/overwrite/…)?` and dies with `EOF` under `--no-tty` — `keep_source` could never restore a drifted file. Fixed in `ChezmoiClient::apply` by passing `--force` (callers apply only on an explicit user decision), with a unit test pinning the args.

Commit: `test(app): e2e drift stories for resolve actions`.

---

### Task 5: Real-machine smoke (user-attended) + docs

- [ ] Full gate; update the plan checkboxes; brief README section in `docs/` if absent — skip if time-boxed.
- [ ] Hand off to Remi: relaunch app → click "keep disk" on `~/.claude/settings.json` (a real, non-templated drifted file) → expect: banner success, a signed commit in `~/.local/share/chezmoi` (1Password prompt may appear — that's chezmoi autoCommit doing its job), drift count drops to 3, and Undo restores it. This step is the acceptance test for the whole plan.

---

## Self-Review Notes
- Spec coverage: §5 one-click defaults ✓ (merge editor path explicitly deferred with honest UI); §6.3 snapshot-first, undo, journaled decisions ✓; §7.4 Sync-all gating ✓ (zero decisions AND not degraded/scanning); §10 honest failure outcomes (1P-locked commits) ✓.
- Deliberate deferrals to Plan 7: conflicts & `pull_and_apply` for diverged repos, templated files, source-side undo (git revert), merge editor, packaging.
- Type consistency: ResolveOutcome/ResolveError names used identically in Tasks 2–4; engine methods take `&Path` matching existing view code's `PathBuf` targets.
