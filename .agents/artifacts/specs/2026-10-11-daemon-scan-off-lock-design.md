# Daemon: scans off the core lock, nested `.git` noise, queue cancel

**Status:** decided (conversation, 2026-10-11). The diagnosis in §1 was
measured on the user's machine; the decisions in §2 were presented in chat
and approved ("sure"). Recorded here so they can be overruled before or
during implementation.
**Implemented by:** `.agents/artifacts/plans/2026-10-11-daemon-scan-off-lock.md`

> "undo suffer from the same issue that we had before + we should be able
> to cancel something in the queue" · "why did this happen AGAIN" — two
> banners reading `the daemon stayed busy for 102.2s (daemon busy (scan in
> progress) — retry shortly) — try again`, one on Undo, one on a merge
> editor Save.

## 1. What happens today

Three facts, each measured on 2026-10-11 against tomted 0.1.20 (pid 97606)
with 1,725 managed files of which 499 are drifted (a mass change under
`~/.agents/skills`).

**A full scan holds the core lock for minutes.** `DaemonCore::full_rescan`
runs `DriftScanner::scan` with the `Mutex<DaemonCore>` held. `chezmoi
status --path-style=absolute` alone takes 10–24 s on this tree; then every
drifted candidate costs four subprocesses in `probe_file` — `chezmoi cat`,
`chezmoi source-path`, and two `git rev-list --count … -- rel` — at roughly
0.2 s each, 8 s for a 1Password-backed template. 584 candidates × 4 ≈ 2–2.5
minutes with the lock held.

**Every app request waits behind that lock and loses.** `server.rs`
answers `Status` from a placeholder when the core is busy, and polls
`try_lock` for `busy_wait` (20 s) for everything else before replying
`BUSY_REPLY`. The app's `request_patiently` retries that bounce for
`DAEMON_PATIENCE` (90 s), which is five bounces ≈ 102 s — the number on both
banners. 0.1.20 moved the threshold from 20 s to 90 s; a scan is 150 s and
grows with the number of drifted files. No wait can win this: the daemon
must not hold the lock while it scans. Every resolve batch ends with
`Request::Rescan`, so the next batch's `SessionStart`, and an Undo, line up
behind a scan that the previous batch itself requested.

**The daemon is also in a self-sustaining rescan loop.** The source repo
carries two gitlinks (mode 160000: `dot_claude/skills/design-doc-mermaid`,
`dot_claude/skills/humanizer`), each with its own `.git` inside the chezmoi
source tree. `git status --porcelain`, which `rescan_source_side` runs,
refreshes each nested repo's index and creates/removes
`<source>/dot_claude/skills/*/.git/index.lock`. The watcher is recursive on
the source dir, so those paths arrive; `handle_paths_changed` exempts only
paths whose FIRST component is `.git`, so a nested `.git/index.lock` counts
as a source change → `reconcile_managed` + `rescan_source_side` → `git
status` again → another `index.lock` → … The log shows one `managed` /
`diff` / `status` triplet every ~0.6 s, indefinitely (watched live at 20:34
UTC). Each iteration takes the lock briefly, so between full scans the core
is busy a third of the time, and every app request is a timing lottery.

## 2. Decisions

### 2.1 Nested `.git` is noise

A watcher path under the source dir is classified by one pure function:

- any component equal to `.git` that is NOT the first component → ignored
  (nested repositories: their internals are not chezmoi source content; the
  files a nested checkout changes arrive as ordinary source paths anyway);
- first component `.git` → counts only if the path ends with `HEAD` or
  contains `refs` (today's rule, unchanged: `HEAD`, `ORIG_HEAD`,
  `FETCH_HEAD`, `refs/**`); `index`, `index.lock`, `objects/**` are ignored;
- any other path under the source dir → source content changed;
- a path outside the source dir → a target if it is managed, else ignored.

This kills the loop at its only entry point. `rescan_source_side` keeps
running `git status`; it just no longer hears itself.

### 2.2 Scans run with the lock released

The scan is split in three, and only the first and last take the lock:

1. **`begin_scan`** (locked, microseconds): hands back a clone of the
   scanner (its chezmoi and git clients are cheap `Arc` clones) and a
   `ScanTicket`; marks `scan_in_progress`; clears `touched_during_scan`.
   If a scan is already in progress it returns `None` and sets
   `rescan_wanted` instead — a request is never lost, and two scans never
   run at once (each one is minutes of CPU).
2. **`DriftScanner::scan`** (unlocked, minutes): exactly today's scan,
   run by the caller's thread with no lock held. `Status`, `SessionStart`,
   `ExpectChanges`, `SessionEnd`, the per-event path and the fetch all take
   the core freely meanwhile.
3. **`apply_scan`** (locked, ~1 s): what `full_rescan` does after
   `scanner.scan()` today — `reconcile_managed`, journal each drifted
   target, drop stale entries, counts, `degraded`, the journal "scan" row,
   `ScanDone` — with one new rule: **a target the per-event path touched
   while the scan ran keeps its current state.** The report's view of it is
   older than what the daemon already knows (the user resolved it and the
   watcher saw the apply; or it drifted after the scan probed it). Touched
   means: `probe_and_journal`, `record_expected_apply` or
   `reconcile_managed` wrote `drift_state` for it since `begin_scan`.
   `in_sync_count` is the report's number as is; it can be off by the
   number of touched targets until the next scan, which is documented on
   the field rather than guessed at.

`DaemonCore::full_rescan(now)` stays as the one-call shape (begin + scan +
apply, lock held throughout) for `tomted --once` and for tests that hold the
core anyway. The daemon binary never calls it: its three scan sites (initial
scan, hourly safety net, `Request::Rescan`) go through one helper,
`tomte_daemon::scan::run_scan(&core, now_fn)`, which loops `begin → scan →
apply` while `rescan_wanted` keeps coming back true. `run_scan` runs the
scan under `catch_unwind` and always reaches `apply_scan` (with an `Err` on
panic), so a dropped ticket cannot leave `scan_in_progress` set forever; as
a second guard `begin_scan` takes over a scan that has been "in progress"
for more than 30 minutes, with a warning in the log.

**`scanning` stays honest.** `Response::Status` now reports `scanning:
c.scan_in_progress()` alongside REAL data (`drift_state`, counts) whenever
the lock is free. The app already handles this shape: `hydrate_status` keeps
last-known numbers only for the all-zero placeholder, lands real data
otherwise, and shows "scanning…" while the flag is up; `is_all_in_sync`
refuses while scanning; the stuck-daemon watchdog fires at 360 s of
continuous scanning, which a 150 s scan never reaches and which an initial
scan on a degraded machine reached before too. The `try_lock` fallback that
answers the placeholder during the brief locked windows stays as is.

**What this does NOT change.** `busy_wait` (20 s) and `DAEMON_PATIENCE`
(90 s) are untouched: they are the right numbers once the only lock
holders are the per-event path, the fetch and `apply_scan`. The per-event
path (`handle_paths_changed` → `rescan_source_side` → per-target probes)
and `handle_fetch`'s per-changed-file probes still run under the lock; both
are bounded by the number of changed source files, not by the size of the
tree, and neither has shown up in a banner. If one does, it gets the same
begin/apply split (§5).

### 2.3 Cheaper probes

Two of the four subprocesses per candidate become two per scan:

- **`chezmoi source-path` is batched.** `chezmoi source-path t1 t2 …`
  prints one line per target in argument order (verified on this machine
  with two targets). `ChezmoiClient::source_paths(&[PathBuf]) ->
  Vec<Option<PathBuf>>` calls it in chunks of 128; a chunk whose exit is
  non-zero or whose line count differs from its argument count is retried
  one target at a time (`None` for the ones that fail) — two lists of
  different lengths are never zipped.
- **Ahead/behind come from two `git log --name-only` sets.**
  `GitClient::files_touched_in(range) -> BTreeSet<PathBuf>` runs `git log
  --name-only --format= <range>` once; `rel ∈ files_touched_in("origin/main..HEAD")`
  is `commits_touching("origin/main..HEAD", rel) > 0` for every
  non-merge commit (both list the files each commit in the range changed;
  rev-list's path filter and `git log --name-only` both leave TREESAME
  merge commits out). `scan` computes both sets once and hands them to
  `probe_file`; `probe_one` (the per-event probe of ONE file) keeps its two
  `rev-list` calls — two subprocesses for one file is already cheap, and
  the daemon's event path must not pay for two whole-history walks per
  file event.

`probe_file` therefore takes `source_rel` and a `GitSignalSource` as
parameters instead of shelling out for them. `chezmoi cat` stays per file:
it is the template render, and chezmoi has no batch form whose failures are
per file (`dump` fails as a whole). On this tree the scan goes from ~584 × 4
to ~584 × 1 subprocesses plus three: roughly 60 s instead of 150 s.
`apply_scan` logs one summary line per scan (`managed`, `drifted`, probed
count, duration) so the next number comes from the log, not a stopwatch.

### 2.4 A queued decision can be removed

The queue holds decisions that have not started (`QueuedStatus::Waiting`)
and the batch that is running. Only the former can be taken back: a
running batch is one in-flight `re-add`/`apply`/commit/push on the user's
repo, and the engine blocks in a background thread for it.

- `ActionQueue::cancel(&target) -> bool` removes the PENDING entry for
  `target`; `false` (and nothing else) when none is pending. It records no
  failure: the user withdrew the decision, nothing went wrong.
- `Shell::cancel_queued(&target, cx)` logs `removed the queued <label> for
  <target>` and notifies. It takes no latch and touches no repo.
- The Review sidebar: a PROCESSING row whose status is `Waiting` carries a
  ✕ micro ghost button in its trailing slot (the slot is blank on these
  rows today), tooltip "Remove from queue", the same `ui::button` the
  selection bar's ✕ is. Clicking it stops propagation (the row is not
  selected by it). The row then reappears under its severity group with
  its checkbox, exactly as before the decision. A `Running` row shows no
  ✕: its sub-line already says "running…", and offering a control that
  cannot act would be the kind of button that teaches nothing.
- `SidebarRow::Target` gains `cancellable: bool`, derived in
  `sidebar_rows` from `queued.status == Waiting` — a field like `locked`,
  for the same reason: the render site uses it verbatim and a test reads
  it.

The deferred repo ops (`PendingRepoOp`: finish, roll back, undo) waiting
for the latch are not queue rows and get no cancel here (§5).

### 2.5 Behaviour the user sees

- Undo, Save, Keep disk pressed while a scan runs: answered within the
  per-event path's own cost (sub-second; a few seconds for a 1Password
  template), never "stayed busy for 102 s" because of a scan.
- The log stops cycling `managed`/`diff`/`status` at rest; the loop is
  gone, not slowed.
- The Review list and the dashboard keep showing real data during a scan,
  with "scanning…" in the status line — no more placeholder zeros for two
  minutes at a time.
- A queued row has a ✕; pressing it puts the file back where it was.

## 3. Components and interfaces

```rust
// crates/daemon/src/core.rs
pub enum PathChange { SourceContent, SourceGitRefs, Target, Ignored }
/// Pure. `managed` is the current managed-target set.
pub fn classify_path(source_dir: &Path, managed: &BTreeSet<PathBuf>, path: &Path) -> PathChange;

pub struct ScanTicket { /* generation, started_ts; not Clone */ }
pub struct ScanOutcome { pub drifted: u32, pub rescan_wanted: bool }
impl DaemonCore {
    pub fn begin_scan(&mut self, now_ts: u64) -> Option<(DriftScanner, ScanTicket)>;
    pub fn apply_scan(&mut self, ticket: ScanTicket,
                      report: Result<ScanReport, ScanError>, now_ts: u64)
        -> Result<ScanOutcome, DaemonError>;   // always clears scan_in_progress
    pub fn scan_in_progress(&self) -> bool;
    pub fn full_rescan(&mut self, now_ts: u64) -> Result<u32, DaemonError>; // begin+scan+apply, locked
}

// crates/daemon/src/scan.rs (new)
/// `Ok(None)`: a scan was already running; it has been asked to go again.
pub fn run_scan(core: &Arc<Mutex<DaemonCore>>, now_fn: fn() -> u64) -> Result<Option<u32>, DaemonError>;

// crates/core/src/chezmoi.rs
pub fn source_paths(&self, targets: &[PathBuf]) -> Result<Vec<Option<PathBuf>>, ChezmoiError>;
// crates/core/src/git.rs
pub fn files_touched_in(&self, range: &str) -> Result<BTreeSet<PathBuf>, GitError>;
// crates/core/src/scanner.rs
enum GitSignalSource<'a> { PerFile, Sets { local: &'a BTreeSet<PathBuf>, remote: &'a BTreeSet<PathBuf> } }

// crates/app/src/action_queue.rs
pub fn cancel(&mut self, target: &Path) -> bool;
// crates/app/src/views/mod.rs
pub fn cancel_queued(&mut self, target: &Path, cx: &mut Context<Self>);
// crates/app/src/views/review.rs
SidebarRow::Target { …, cancellable: bool }
```

`DriftScanner` must be `Send` (it already is: both clients hold an
`Arc<dyn CommandRunner + Send + Sync>`); `apply_scan` is the only writer of
`in_sync_count`, `origin_differs`, `degraded`, `last_scan_drifted`.

## 4. Testing

- `classify_path`: nested `…/.git/index.lock` and nested `…/.git/HEAD` →
  `Ignored`; top-level `.git/index.lock` → `Ignored`; `.git/HEAD`,
  `.git/refs/remotes/origin/main` → `SourceGitRefs`; `dot_zshrc` →
  `SourceContent`; a managed target → `Target`; `/tmp/x` → `Ignored`.
  Mutations: drop the "first component" check (nested HEAD becomes refs);
  drop the any-component check (nested `index.lock` becomes source).
- `daemon_core.rs`, wiring: dirty the scratch source behind the core's
  back, call `handle_paths_changed` with a nested `.git/index.lock` path →
  no `Drift` push; call it with the real source path → `Drift`. Mutation:
  classify inline with the old first-component rule.
- `begin_scan` / `apply_scan`: (a) a report that lists a target
  `record_expected_apply` cleared during the scan leaves it cleared;
  (b) a report that omits a target `probe_and_journal` set during the scan
  leaves it set; (c) `begin_scan` during a scan returns `None`, and
  `apply_scan` then reports `rescan_wanted`; (d) `apply_scan(Err)` clears
  `scan_in_progress`. Mutations: skip the touched set in
  `record_expected_apply` (a fails); apply stale removals to touched
  targets (b fails); never set `rescan_wanted` (c fails).
- `server_ipc.rs`: with a scan "in progress" (ticket held, scanner not
  run), `SessionStart` and `ExpectChanges` are answered at once and
  `Status` carries `scanning: true` WITH real data. Mutation: hold the
  lock for the scan's duration again → the request bounces at the test's
  short `busy_wait`.
- `scanner_integration.rs`: the existing classification tests pass
  unchanged with `GitSignalSource::Sets` (a local-ahead and a remote-ahead
  fixture each prove one set), and a batch `source_paths` over a managed
  target, an unmanaged path and a directory returns `Some, None, …` in
  argument order. Mutation: zip a short reply against the full chunk.
- `ActionQueue::cancel`: removes a pending entry and returns `true`;
  leaves a running one and returns `false`; records no failure. Mutation:
  cancel from `running` too.
- `Shell::cancel_queued` under a posed latch: two queued, cancel the
  second → `depth` 1, `processing` lists one, the plan lists the cancelled
  file under SAFE TO RESOLVE with `cancellable: false`. `sidebar_rows`:
  Waiting → `cancellable`, Running → not. Mutation: derive from
  `queued.is_some()`.
- Gallery: `review-processing` shows the ✕ on the waiting row and none on
  the running one; dark and light, READ.
- Field check after the bundle is installed: the tomted log at rest shows
  no `managed`/`diff`/`status` cycle; `--print-status` during a scan shows
  `scanning: true` with non-zero counts; a Save during a scan lands.

## 5. Out of scope, deliberately

- The per-event path and `handle_fetch` still probe under the lock (§2.2):
  bounded by changed-file counts; same split if it ever shows up.
- Batching `chezmoi cat` via `chezmoi dump` (all-or-nothing failures).
- Cancelling a RUNNING batch, or an undo/finish/roll-back waiting for the
  latch.
- Making `rescan_source_side` batch its `target-path` calls (one call per
  changed source file today; a non-entry file fails the whole call).
- Any change to `busy_wait` or `DAEMON_PATIENCE`.
