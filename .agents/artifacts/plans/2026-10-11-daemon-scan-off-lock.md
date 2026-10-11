# Daemon Scan Off the Lock Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A full daemon scan never holds the core lock, the daemon stops rescanning itself over nested `.git` directories, a scan costs one subprocess per candidate instead of four, and a queued decision can be taken back with a ✕.

**Architecture:** `DaemonCore` gains a three-phase scan (`begin_scan` under the lock, `DriftScanner::scan` on the caller's thread with the lock released, `apply_scan` under the lock with a "touched meanwhile" guard); one driver, `tomte_daemon::scan::run_scan`, is the only way the binary scans. A pure `classify_path` decides what a watcher path means and ignores nested repositories. `DriftScanner::scan` computes the git ahead/behind sets and the source paths once per scan. `ActionQueue::cancel` removes a pending decision; the Review sidebar shows a ✕ on waiting rows.

**Tech Stack:** Rust 2024, the existing `tomte_core::testsupport::Scratch` (real chezmoi + git), the daemon's socket tests, GPUI 0.2.2 (pinned) for the one view change, `scripts/shoot.sh` for the screenshot.

**Spec:** `.agents/artifacts/specs/2026-10-11-daemon-scan-off-lock-design.md`

## Global Constraints

- The daemon never runs `DriftScanner::scan` while holding the `Mutex<DaemonCore>` (spec §2.2). `DaemonCore::full_rescan` stays for `tomted --once` and tests only.
- A target the per-event path wrote `drift_state` for during a scan keeps its state when the scan's report lands (spec §2.2).
- `busy_wait` (20 s) and `DAEMON_PATIENCE` (90 s) are not changed (spec §2.2).
- Only a `Waiting` decision can be cancelled; cancelling records no failure (spec §2.4).
- Never swallow an error or return a confident zero: every skipped scan, failed chunk and cancelled decision is logged with `tomte_core::log_*!`.
- Every new test names, in a comment, the mutation that fails it, and the implementer RUNS that mutation against production code (edit, run, revert from a `cp` backup — never `git checkout --` on uncommitted work).
- The gate before every commit, IN THE FOREGROUND, and read: `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E '^warning' | grep -v -E 'block v0\.1\.6|proc-macro-error2'` (must print nothing); `cargo test --workspace`; `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null cargo test --workspace`. Never in the background.
- Commits are SSH-signed via 1Password; on failure leave the work staged and report, never `--no-gpg-sign`, never touch git config.
- `~/.config` and the user's dotfiles are chezmoi-managed: never edit them. Tests use `Scratch` only.
- Screenshots are read, not assumed: `./scripts/shoot.sh review-processing dark` then open the PNG. `screencapture` fails in a sandboxed shell; run it unsandboxed.

## Review Focus

Inputs the spec implies but no task's tests exercised on first draft; each now has its test in the task that owns the code.

1. A file taken out of management (`chezmoi forget`, or its source deleted) while a scan runs: the report still lists it as drifted; `apply_scan` must drop it, not journal it — Task 2, `a_target_forgotten_during_a_scan_is_not_resurrected_by_its_report`.
2. A target that is running AND re-queued (merge-editor Save on a file whose batch already started): ✕ removes only the pending copy; the running one keeps its "running…" row — Task 5, `cancelling_a_re_queued_running_target_removes_only_the_pending_copy`.
3. A scan that fails (chezmoi missing, git error) must release `scan_in_progress`, or no scan ever runs again — Task 2, `a_failed_scan_releases_the_in_progress_flag`.
4. A scan thread that dies (a panic inside `scan`) must still reach `apply_scan` — Task 3, `run_scan` wraps the scan in `catch_unwind`; the stale-takeover test in Task 2 covers the ticket that never comes back.
5. A `source-path` batch where one target is unmanaged must not shift every later source path by one — Task 4, `source_paths_answers_in_argument_order_and_never_misaligns`.

---

### Task 1: Nested `.git` paths are noise (`classify_path`)

**Files:**
- Modify: `crates/daemon/src/core.rs` (`handle_paths_changed`, lines 357–405; new `PathChange` + `classify_path` above `impl DaemonCore`; a `#[cfg(test)] mod tests` at the end of the file — there is none today)
- Test: `crates/daemon/tests/daemon_core.rs`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum PathChange { SourceContent, SourceGitRefs, Target, Ignored }
  pub fn classify_path(source_dir: &Path, managed: &BTreeSet<PathBuf>, path: &Path) -> PathChange;
  ```

- [ ] **Step 1: Write the unit tests**

At the end of `crates/daemon/src/core.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn managed(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    /// MUTATION: drop the "first component" distinction (treat ANY `.git`
    /// component with the HEAD/refs rule) → the nested `HEAD` case reads
    /// `SourceGitRefs`. MUTATION: drop the any-component check (only the
    /// first component is ever looked at, the 0.1.20 shape) → the nested
    /// `index.lock` case reads `SourceContent`, which is the self-sustaining
    /// rescan loop of 2026-10-11.
    #[test]
    fn classify_path_ignores_nested_repositories_and_keeps_the_top_level_refs_rule() {
        let src = Path::new("/src");
        let m = managed(&["/home/.zshrc"]);
        let at = |rel: &str| classify_path(src, &m, &src.join(rel));
        assert_eq!(at("dot_claude/skills/humanizer/.git/index.lock"), PathChange::Ignored);
        assert_eq!(at("dot_claude/skills/humanizer/.git/HEAD"), PathChange::Ignored);
        assert_eq!(at("dot_claude/skills/humanizer/.git/refs/heads/main"), PathChange::Ignored);
        assert_eq!(at(".git/index.lock"), PathChange::Ignored);
        assert_eq!(at(".git/objects/ab/cdef"), PathChange::Ignored);
        assert_eq!(at(".git/HEAD"), PathChange::SourceGitRefs);
        assert_eq!(at(".git/ORIG_HEAD"), PathChange::SourceGitRefs);
        assert_eq!(at(".git/refs/remotes/origin/main"), PathChange::SourceGitRefs);
        assert_eq!(at("dot_zshrc"), PathChange::SourceContent);
        assert_eq!(at("dot_claude/skills/humanizer/SKILL.md"), PathChange::SourceContent);
        assert_eq!(classify_path(src, &m, Path::new("/home/.zshrc")), PathChange::Target);
        assert_eq!(classify_path(src, &m, Path::new("/tmp/x")), PathChange::Ignored);
    }
}
```

- [ ] **Step 2: Run them to see them fail to compile**

Run: `cargo test -p tomte-daemon --lib classify_path`
Expected: error: cannot find `classify_path` / `PathChange`.

- [ ] **Step 3: Implement `classify_path` and use it**

Above `pub struct DaemonCore` in `core.rs`:

```rust
/// What one watcher path means to the daemon (spec 2026-10-11 §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathChange {
    /// A file of the chezmoi source tree changed.
    SourceContent,
    /// The source repo's own HEAD or refs moved (commit, fetch, checkout).
    SourceGitRefs,
    /// A managed destination file changed.
    Target,
    /// Nothing the daemon acts on: the top-level `.git`'s index, objects and
    /// locks; anything inside a NESTED repository's `.git` (a vendored skill
    /// with its own history — `git status` in the source repo refreshes its
    /// index and writes `index.lock` there, and hearing that as a source
    /// change was a self-sustaining rescan loop, 2026-10-11); an unmanaged
    /// path outside the source tree.
    Ignored,
}

/// Pure, so the rule is a value a test can read. `managed` is the current
/// managed-target set.
pub fn classify_path(source_dir: &Path, managed: &BTreeSet<PathBuf>, path: &Path) -> PathChange {
    let Ok(rel) = path.strip_prefix(source_dir) else {
        return if managed.contains(path) {
            PathChange::Target
        } else {
            PathChange::Ignored
        };
    };
    let mut comps = rel.components();
    let first_is_git = comps.next().is_some_and(|c| c.as_os_str() == ".git");
    // `comps` now starts at the second component: a `.git` anywhere below
    // the first one is a nested repository.
    if comps.any(|c| c.as_os_str() == ".git") {
        return PathChange::Ignored;
    }
    if first_is_git {
        let s = rel.to_string_lossy();
        if s.ends_with("HEAD") || s.contains("refs") {
            PathChange::SourceGitRefs
        } else {
            PathChange::Ignored
        }
    } else {
        PathChange::SourceContent
    }
}
```

Replace the `for p in paths { … }` loop in `handle_paths_changed` with:

```rust
        for p in paths {
            match classify_path(&self.source_dir, &self.managed, p) {
                PathChange::SourceContent => source_changed = true,
                PathChange::SourceGitRefs => git_refs_changed = true,
                PathChange::Target => targets.push(p.clone()),
                PathChange::Ignored => {}
            }
        }
```

- [ ] **Step 4: Run the unit test**

Run: `cargo test -p tomte-daemon --lib classify_path`
Expected: PASS.

- [ ] **Step 5: Write the wiring test**

In `crates/daemon/tests/daemon_core.rs`, after `paused_core_ignores_changes`:

```rust
/// The 2026-10-11 loop: the source repo's `git status` wrote `index.lock`
/// inside a vendored skill's own `.git`, the watcher reported it, and the
/// daemon heard a source change — forever. The nested path must not start a
/// source-side rescan; the real source path still does.
///
/// MUTATION: classify inline with the 0.1.20 rule (only
/// `rel.components().next() == ".git"` is looked at) → the nested
/// `index.lock` reads as a source change, `rescan_source_side` finds the
/// dirty `dot_testrc`, and the first assertion sees a Drift push.
#[test]
fn a_nested_repos_index_lock_is_not_a_source_change() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let rx = core.subscribe();
    // Dirty the source behind the core's back: a source-side rescan WOULD
    // find this (source_edit_is_source_ahead in the scanner tests).
    std::fs::write(s.source.join("dot_testrc"), "a=edited\n").unwrap();

    let nested = s.source.join("dot_claude/skills/humanizer/.git/index.lock");
    core.handle_paths_changed(std::slice::from_ref(&nested), 100)
        .unwrap();
    assert!(
        rx.try_recv().is_err(),
        "a nested .git path must not trigger a source-side rescan"
    );

    core.handle_paths_changed(&[s.source.join("dot_testrc")], 101)
        .unwrap();
    assert!(
        matches!(rx.try_recv().unwrap(), Event::Drift { .. }),
        "the real source path does"
    );
}
```

- [ ] **Step 6: Run it, then run the named mutations**

Run: `cargo test -p tomte-daemon --test daemon_core a_nested_repos`
Expected: PASS. Then `cp crates/daemon/src/core.rs /tmp/core.rs.bak`, apply each mutation from Step 1's and Step 5's comments to `core.rs`, run both tests (each mutation must FAIL at least one), restore with `cp /tmp/core.rs.bak crates/daemon/src/core.rs`.

- [ ] **Step 7: Gate and commit**

Run the full gate (Global Constraints). Then:

```bash
git add crates/daemon/src/core.rs crates/daemon/tests/daemon_core.rs
git commit -m "fix(daemon): ignore nested .git directories in the source tree

The source repo's own git status refreshes vendored skills' indexes and
writes index.lock inside their .git; the watcher reported those as source
changes and the daemon rescanned itself every 0.6 s, forever."
```

---

### Task 2: `begin_scan` / `apply_scan` with the touched-meanwhile guard

**Files:**
- Modify: `crates/daemon/src/core.rs` (struct fields; `journal_probe` 310–317; `probe_and_journal` 326–334; `record_expected_apply` 353; `reconcile_managed` 442; `full_rescan` 463–517 replaced)
- Modify: `crates/core/src/scanner.rs:17` (`#[derive(Clone)]` on `DriftScanner`)
- Test: `crates/daemon/tests/daemon_core.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct ScanTicket { /* private */ }                      // not Clone
  pub struct ScanOutcome { pub drifted: u32, pub rescan_wanted: bool }
  impl DaemonCore {
      pub fn begin_scan(&mut self, now_ts: u64) -> Option<(DriftScanner, ScanTicket)>;
      pub fn apply_scan(&mut self, ticket: ScanTicket, report: Result<ScanReport, ScanError>, now_ts: u64)
          -> Result<ScanOutcome, DaemonError>;
      pub fn scan_in_progress(&self) -> bool;
      pub fn full_rescan(&mut self, now_ts: u64) -> Result<u32, DaemonError>;   // unchanged signature
  }
  ```
- Consumes: `tomte_core::scanner::{DriftScanner, ScanReport, ScanError}`.

- [ ] **Step 1: Write the tests**

In `crates/daemon/tests/daemon_core.rs` (add `use tomte_core::scanner::ScanError;` at the top):

```rust
/// A scan now runs OFF the lock (spec 2026-10-11 §2.2), so the app can
/// resolve a file while the scan is still looking at it. The report then
/// says "drifted" about a file the daemon already saw resolved; the
/// daemon's own, fresher knowledge wins.
///
/// MUTATION: drop the touched-set note from `record_expected_apply` → the
/// stale report re-adds `.testrc` to `drift_state`, and the status lists
/// a file the user just resolved.
#[test]
fn a_target_the_app_resolved_during_a_scan_stays_resolved() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let target = s.home.join(".testrc");
    let source_text = std::fs::read(s.source.join("dot_testrc")).unwrap();
    std::fs::write(&target, "a=drift\n").unwrap();

    let (scanner, ticket) = core.begin_scan(100).expect("no scan in progress");
    let report = scanner.scan();
    assert_eq!(report.as_ref().unwrap().drifted.len(), 1, "the scan saw the drift");

    // While it ran: the app announced and applied the resolution.
    core.expect_changes(std::slice::from_ref(&target), 60, 101);
    std::fs::write(&target, &source_text).unwrap();
    core.handle_paths_changed(std::slice::from_ref(&target), 102)
        .unwrap();

    let outcome = core.apply_scan(ticket, report, 103).unwrap();
    assert_eq!(outcome.drifted, 0, "the stale verdict lost to the fresher apply");
    assert!(core.status_snapshot().0.is_empty());
    assert!(!core.scan_in_progress());
}

/// The mirror image: the file drifted AFTER the scan looked at it. The
/// report says clean; the daemon already journaled the drift.
///
/// MUTATION: apply the stale removals to touched targets too (drop the
/// `touched.contains` guard on the stale loop) → `.testrc` vanishes from
/// the status although it is drifted right now.
#[test]
fn a_target_that_drifted_during_a_scan_stays_drifted() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let target = s.home.join(".testrc");

    let (scanner, ticket) = core.begin_scan(100).unwrap();
    let report = scanner.scan();
    assert!(report.as_ref().unwrap().drifted.is_empty(), "clean when the scan looked");

    std::fs::write(&target, "a=later\n").unwrap();
    core.handle_paths_changed(std::slice::from_ref(&target), 101)
        .unwrap();

    let outcome = core.apply_scan(ticket, report, 102).unwrap();
    assert_eq!(outcome.drifted, 1);
    assert_eq!(core.status_snapshot().0[0].class, "destination_drift");
}

/// One scan at a time, and a request made during one is not lost: the
/// caller is told to go again.
///
/// MUTATION: never set `rescan_wanted` in `begin_scan` → `None` still
/// comes back, but `apply_scan` reports `rescan_wanted: false` and a
/// Rescan pressed during a scan silently vanishes.
#[test]
fn a_scan_requested_during_a_scan_runs_after_it() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let (scanner, ticket) = core.begin_scan(100).unwrap();
    assert!(core.scan_in_progress());
    assert!(core.begin_scan(101).is_none(), "one scan at a time");
    let outcome = core.apply_scan(ticket, scanner.scan(), 102).unwrap();
    assert!(outcome.rescan_wanted);
    assert!(!core.scan_in_progress());
    assert!(core.begin_scan(103).is_some(), "and the next one may start");
}

/// MUTATION: clear `scan_in_progress` only on the `Ok` path → a failed scan
/// leaves `begin_scan` returning `None` forever: no scan ever runs again.
#[test]
fn a_failed_scan_releases_the_in_progress_flag() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let (_scanner, ticket) = core.begin_scan(100).unwrap();
    let err = core.apply_scan(
        ticket,
        Err(ScanError::Io(std::io::Error::other("boom"))),
        101,
    );
    assert!(err.is_err());
    assert!(!core.scan_in_progress());
    assert!(core.begin_scan(102).is_some());
}

/// A scan whose thread never came back (30 min and counting) is taken over
/// rather than blocking every later scan; if its report does turn up, it
/// is the OLDER one and must not clobber the newer scan's flag or state.
///
/// MUTATION: drop the generation check in `apply_scan` → the old report
/// applies and clears the NEW scan's `scan_in_progress`.
#[test]
fn a_stale_scan_is_taken_over_and_its_late_report_is_ignored() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let (old_scanner, old_ticket) = core.begin_scan(100).unwrap();
    let (_new_scanner, _new_ticket) = core
        .begin_scan(100 + 31 * 60)
        .expect("a 31-minute-old scan is taken over");
    let outcome = core.apply_scan(old_ticket, old_scanner.scan(), 100 + 31 * 60 + 1).unwrap();
    assert!(!outcome.rescan_wanted);
    assert!(core.scan_in_progress(), "the newer scan is still the one in progress");
}

/// A file taken out of management while the scan ran (its source deleted,
/// like `chezmoi forget`): the report still lists it, because it was
/// managed when the scan looked. `apply_scan` reconciles first and must
/// not journal a drift for a file that is no longer managed.
///
/// MUTATION: drop the `self.managed.contains(&fd.target)` filter in
/// `apply_scan` → `.testrc` is listed as drifted after it left management.
#[test]
fn a_target_forgotten_during_a_scan_is_not_resurrected_by_its_report() {
    let s = Scratch::new();
    let mut core = core_for(&s);
    let rx = core.subscribe();
    let target = s.home.join(".testrc");
    std::fs::write(&target, "a=drift\n").unwrap();
    let (scanner, ticket) = core.begin_scan(100).unwrap();
    let report = scanner.scan();
    assert_eq!(report.as_ref().unwrap().drifted.len(), 1);

    std::fs::remove_file(s.source.join("dot_testrc")).unwrap();

    let outcome = core.apply_scan(ticket, report, 101).unwrap();
    assert_eq!(outcome.drifted, 0);
    assert!(core.status_snapshot().0.is_empty());
    assert!(
        kinds(&core, &target).contains(&"left_management".to_string()),
        "{:?}",
        kinds(&core, &target)
    );
    let mut saw_left = false;
    while let Ok(ev) = rx.try_recv() {
        saw_left |= matches!(ev, Event::LeftManagement { .. });
    }
    assert!(saw_left);
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p tomte-daemon --test daemon_core scan`
Expected: compile errors: no `begin_scan` / `apply_scan` / `scan_in_progress`.

- [ ] **Step 3: Implement**

`crates/core/src/scanner.rs:17`: `#[derive(Clone)] pub struct DriftScanner { … }` (both clients are already `Clone`; `DaemonCore::new_with_subscribers` clones them).

`core.rs` — new constants and types after `AUTO_JOIN_MERGE`:

```rust
/// A scan "in progress" for longer than this is presumed dead (its thread
/// never came back) and taken over by the next `begin_scan`.
const SCAN_STALE_SECS: u64 = 30 * 60;

/// Proof that `begin_scan` ran, consumed by `apply_scan`. Carries the
/// generation so a report from a scan that was taken over is recognised.
pub struct ScanTicket {
    generation: u64,
    started_ts: u64,
}

/// What `apply_scan` hands back: the live drifted count, and whether a
/// rescan was asked for while this one ran (the caller goes again).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanOutcome {
    pub drifted: u32,
    pub rescan_wanted: bool,
}
```

New fields on `DaemonCore` (after `subscribers`), initialised in `new_with_subscribers` to `false`, `0`, `0`, `false`, `BTreeSet::new()`:

```rust
    /// A scan is running OFF the lock (spec 2026-10-11 §2.2): `begin_scan`
    /// set it, `apply_scan` clears it. `Status` reports it as `scanning`.
    scan_in_progress: bool,
    scan_started_ts: u64,
    /// Bumped by every `begin_scan`; a `ScanTicket` carries the value it
    /// was issued with, so a taken-over scan's late report is recognised.
    scan_generation: u64,
    /// A rescan asked for while one ran. `apply_scan` hands it back.
    rescan_wanted: bool,
    /// Targets the per-event path wrote `drift_state` for since
    /// `begin_scan`. `apply_scan` leaves these alone: the report's view of
    /// them is older than the daemon's own.
    touched_during_scan: BTreeSet<PathBuf>,
```

The two write helpers, in `impl DaemonCore` next to `is_expected`:

```rust
    /// The ONLY two writers of `drift_state` outside `apply_scan`. Both note
    /// the target while a scan is in progress, which is what lets the scan
    /// run with the lock released and still never overwrite something the
    /// daemon learned after the scan looked.
    fn set_drift(&mut self, target: PathBuf, class: String, since: u64) {
        if self.scan_in_progress {
            self.touched_during_scan.insert(target.clone());
        }
        self.drift_state.insert(target, (class, since));
    }

    fn clear_drift(&mut self, target: &Path) {
        if self.scan_in_progress {
            self.touched_during_scan.insert(target.to_path_buf());
        }
        self.drift_state.remove(target);
    }
```

Route every existing write through them: `journal_probe`'s `self.drift_state.insert(target.clone(), (cs.to_string(), since));` → `self.set_drift(target.clone(), cs.to_string(), since);`; `probe_and_journal`'s `self.drift_state.remove(target);` → `self.clear_drift(target);`; `record_expected_apply`'s `self.drift_state.remove(target);` → `self.clear_drift(target);`; `reconcile_managed`'s `self.drift_state.remove(departed);` → collect `departed` paths first (the loop borrows `self.managed`), then `self.clear_drift(&p)` for each — the simplest shape is to compute `let departed: Vec<PathBuf> = self.managed.difference(&fresh).cloned().collect();` before the loop and iterate that.

Replace `full_rescan` with the three-phase API:

```rust
    /// Phase 1 of a scan (spec 2026-10-11 §2.2), under the lock for
    /// microseconds: a scanner the caller runs WITHOUT the lock, and the
    /// ticket `apply_scan` wants back. `None` while another scan is in
    /// progress — the request is remembered as `rescan_wanted`, never lost.
    /// A scan in progress for more than `SCAN_STALE_SECS` is presumed dead
    /// and taken over.
    pub fn begin_scan(&mut self, now_ts: u64) -> Option<(DriftScanner, ScanTicket)> {
        if self.scan_in_progress {
            let age = now_ts.saturating_sub(self.scan_started_ts);
            if age < SCAN_STALE_SECS {
                self.rescan_wanted = true;
                return None;
            }
            tomte_core::log_warn!("scan", "a scan has been in progress for {age}s — taking over");
        }
        Some(self.start_scan(now_ts))
    }

    fn start_scan(&mut self, now_ts: u64) -> (DriftScanner, ScanTicket) {
        self.scan_in_progress = true;
        self.scan_started_ts = now_ts;
        self.scan_generation += 1;
        self.touched_during_scan.clear();
        (
            self.scanner.clone(),
            ScanTicket {
                generation: self.scan_generation,
                started_ts: now_ts,
            },
        )
    }

    pub fn scan_in_progress(&self) -> bool {
        self.scan_in_progress
    }

    /// Phase 3: land a report. Always releases `scan_in_progress` for the
    /// scan the ticket belongs to, `Err` included. A target the per-event
    /// path touched meanwhile keeps its state (see `set_drift`); a target
    /// that left management meanwhile is dropped, not journaled.
    pub fn apply_scan(
        &mut self,
        ticket: ScanTicket,
        report: Result<ScanReport, ScanError>,
        now_ts: u64,
    ) -> Result<ScanOutcome, DaemonError> {
        if ticket.generation != self.scan_generation {
            tomte_core::log_warn!(
                "scan",
                "ignoring the report of a scan that was taken over (started t={})",
                ticket.started_ts
            );
            return Ok(ScanOutcome {
                drifted: self.drift_state.len() as u32,
                rescan_wanted: false,
            });
        }
        let touched = std::mem::take(&mut self.touched_during_scan);
        self.scan_in_progress = false;
        let rescan_wanted = std::mem::take(&mut self.rescan_wanted);
        let report = report?;
        self.reconcile_managed(now_ts)?;
        self.degraded = report.degraded.as_ref().map(|f| f.hint.clone());
        // The report's number as is: off by the touched count until the next
        // scan, which is the honest option — recounting here would mean
        // guessing which touched files the scan had tallied as in sync.
        self.in_sync_count = report.in_sync_count as u64;
        self.origin_differs = report.origin_differs;
        if !report.unprobed.is_empty() {
            tomte_core::log_warn!("scan", "{} target(s) could not be checked", report.unprobed.len());
            for u in &report.unprobed {
                tomte_core::log_warn!("scan", "  {}: {}", u.target.display(), u.reason);
            }
        }
        let fresh_targets: BTreeSet<PathBuf> =
            report.drifted.iter().map(|d| d.target.clone()).collect();
        let stale: Vec<PathBuf> = self
            .drift_state
            .keys()
            .filter(|t| !fresh_targets.contains(*t) && !touched.contains(*t))
            .cloned()
            .collect();
        for t in &stale {
            self.clear_drift(t);
        }
        let mut skipped = 0usize;
        for fd in &report.drifted {
            if touched.contains(&fd.target) || !self.managed.contains(&fd.target) {
                skipped += 1;
                continue;
            }
            self.journal_probe(fd, now_ts)?;
        }
        let drifted = self.drift_state.len() as u32;
        tomte_core::log_info!(
            "scan",
            "{} managed, {} drifted ({} report entries skipped: changed or unmanaged meanwhile), {} unprobed, {}s",
            self.managed.len(),
            drifted,
            skipped,
            report.unprobed.len(),
            now_ts.saturating_sub(ticket.started_ts)
        );
        if self.last_scan_drifted != Some(drifted) {
            self.journal.record_event(NewEvent {
                target: None,
                ts: now_ts,
                kind: EventKind::Fetch,
                from_hash: None,
                to_hash: None,
                meta: Some(serde_json::json!({"scan": true, "drifted": drifted})),
            })?;
        }
        self.last_scan_drifted = Some(drifted);
        self.emit(Event::ScanDone { ts: now_ts, drifted });
        Ok(ScanOutcome { drifted, rescan_wanted })
    }

    /// The one-call shape, lock held throughout: `tomted --once` and tests
    /// that hold the core anyway. The daemon binary never calls this — it
    /// goes through `crate::scan::run_scan`, which releases the lock for
    /// phase 2. Takes over any scan in progress: the caller holds the core,
    /// so nothing else can be applying.
    pub fn full_rescan(&mut self, now_ts: u64) -> Result<u32, DaemonError> {
        let (scanner, ticket) = self.start_scan(now_ts);
        let report = scanner.scan();
        Ok(self.apply_scan(ticket, report, now_ts)?.drifted)
    }
```

The `journal_probe` doc still says "skip an event if…"; add one line to its comment: "Writes `drift_state` through `set_drift`, which is what makes an off-lock scan safe."

- [ ] **Step 4: Run the daemon tests**

Run: `cargo test -p tomte-daemon`
Expected: all PASS, including the pre-existing `full_rescan_populates_status` (its `in_sync` 0 is unchanged: the only managed file is drifted).

- [ ] **Step 5: Run the six named mutations**

`cp crates/daemon/src/core.rs /tmp/core.rs.bak`; apply each mutation from the test comments in Step 1, one at a time; run `cargo test -p tomte-daemon --test daemon_core scan`; each must fail the test that names it; restore from the backup after each.

- [ ] **Step 6: Gate and commit**

```bash
git add crates/daemon/src/core.rs crates/core/src/scanner.rs crates/daemon/tests/daemon_core.rs
git commit -m "feat(daemon): three-phase scan — begin, scan off the lock, apply with a touched-meanwhile guard"
```

---

### Task 3: `run_scan` drives every scan in the binary; `Status` stays honest

**Files:**
- Create: `crates/daemon/src/scan.rs`
- Modify: `crates/daemon/src/lib.rs` (add `pub mod scan;`)
- Modify: `crates/daemon/src/main.rs:294-299` (initial scan), `:375-402` (hourly thread)
- Modify: `crates/daemon/src/server.rs` (`Request::Status` locked arm, `Request::Rescan` arm; the `scanning` doc comment on the big `try_lock` block)
- Test: `crates/daemon/tests/server_ipc.rs`

**Interfaces:**
- Produces: `pub fn run_scan(core: &Arc<Mutex<DaemonCore>>, now_fn: fn() -> u64) -> Result<Option<u32>, DaemonError>`.
- Consumes: `DaemonCore::{begin_scan, apply_scan, scan_in_progress}` from Task 2.

- [ ] **Step 1: Write the tests**

In `crates/daemon/tests/server_ipc.rs` (the file's `setup` builds a `ServeCtx::ready` with `now_fn || 1000`; these tests build their own context like `a_shortened_busy_wait_bounces_a_core_request_promptly` does, so they can shrink `busy_wait`):

```rust
/// A scan in progress is a FLAG, not a held lock (spec 2026-10-11 §2.2):
/// requests are answered meanwhile, and `Status` says `scanning: true`
/// next to the real drift list rather than placeholder zeros.
///
/// MUTATION: report `scanning: false` from the locked Status arm (drop
/// `c.scan_in_progress()`) → the scanning assertion fails.
#[test]
fn status_reports_scanning_with_real_data_while_a_scan_is_in_progress() {
    let s = Scratch::new();
    let chezmoi: ChezmoiClient = s.chezmoi();
    let git = GitClient::new(Arc::new(SystemRunner), s.source.clone());
    let journal = Journal::open_in_memory("scanning-test").unwrap();
    let core = Arc::new(Mutex::new(
        DaemonCore::new(chezmoi, git, journal, "origin/main".into()).unwrap(),
    ));
    std::fs::write(s.home.join(".testrc"), "a=drift\n").unwrap();
    core.lock().unwrap().full_rescan(1).unwrap();
    // A scan "in progress" without running it: deterministic.
    let (_scanner, ticket) = core.lock().unwrap().begin_scan(2).unwrap();

    let sock = s.root.path().join("sc.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let mut ctx = tomte_daemon::server::ServeCtx::ready(core.clone(), || 7, Arc::new(|| {}));
    ctx.busy_wait = std::time::Duration::from_millis(200);
    std::thread::spawn(move || serve(listener, ctx));

    let stream = UnixStream::connect(&sock).unwrap();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    send(&mut writer, 1, Request::Hello { version: PROTOCOL_VERSION });
    let _ = recv(&mut reader);

    send(&mut writer, 2, Request::SessionStart { ts: 3 });
    assert!(
        matches!(
            recv(&mut reader),
            ServerFrame::Reply { id: 2, response: Response::SessionStarted { .. } }
        ),
        "answered, not bounced, while a scan is in progress"
    );
    send(&mut writer, 3, Request::Status);
    match recv(&mut reader) {
        ServerFrame::Reply {
            id: 3,
            response: Response::Status { scanning, drifted, .. },
        } => {
            assert!(scanning, "a scan is in progress");
            assert_eq!(drifted.len(), 1, "and the real list is still served");
        }
        other => panic!("expected Status, got {other:?}"),
    }
    drop(ticket);
}

/// The Rescan arm runs the scan with the lock RELEASED: a request sent
/// right after it is answered at once, long before the scan is done.
///
/// MUTATION: in `dispatch`'s Rescan arm, lock the core on the thread and
/// call `full_rescan` (the 0.1.20 shape) → the scratch scan holds the lock
/// for its ~1 s of subprocesses, `SessionStart` waits past the 200 ms
/// `busy_wait` and bounces, and the match below fails.
#[test]
fn a_rescan_request_does_not_hold_the_core_for_the_scan() {
    let s = Scratch::new();
    let chezmoi: ChezmoiClient = s.chezmoi();
    let git = GitClient::new(Arc::new(SystemRunner), s.source.clone());
    let journal = Journal::open_in_memory("rescan-off-lock-test").unwrap();
    let core = Arc::new(Mutex::new(
        DaemonCore::new(chezmoi, git, journal, "origin/main".into()).unwrap(),
    ));
    let sock = s.root.path().join("rs.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let mut ctx = tomte_daemon::server::ServeCtx::ready(core.clone(), || 7, Arc::new(|| {}));
    ctx.busy_wait = std::time::Duration::from_millis(200);
    std::thread::spawn(move || serve(listener, ctx));

    let stream = UnixStream::connect(&sock).unwrap();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(20))).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    send(&mut writer, 1, Request::Hello { version: PROTOCOL_VERSION });
    let _ = recv(&mut reader);
    send(&mut writer, 2, Request::Subscribe);
    let _ = recv(&mut reader);

    send(&mut writer, 3, Request::Rescan);
    assert!(matches!(recv(&mut reader), ServerFrame::Reply { id: 3, response: Response::Ok }));
    send(&mut writer, 4, Request::SessionStart { ts: 5 });
    // Pushes and replies share the stream: skip pushes until reply 4.
    loop {
        match recv(&mut reader) {
            ServerFrame::Reply { id: 4, response: Response::SessionStarted { .. } } => break,
            ServerFrame::Reply { id: 4, response } => panic!("expected SessionStarted, got {response:?}"),
            ServerFrame::Push { .. } => continue,
            other => panic!("unexpected {other:?}"),
        }
    }
    // Let the scan finish before the scratch is torn down.
    loop {
        if let ServerFrame::Push { event: tomte_proto::Event::ScanDone { .. } } = recv(&mut reader) {
            break;
        }
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p tomte-daemon --test server_ipc scan`
Expected: the first fails on `scanning` (the locked arm hard-codes `false`); the second may pass or fail on timing — it is the mutation run in Step 5 that proves it.

- [ ] **Step 3: Implement `run_scan`**

`crates/daemon/src/scan.rs`:

```rust
//! The daemon's scan driver (spec 2026-10-11 §2.2): the one place a full
//! scan is started in the binary, so every caller runs it with the core
//! UNLOCKED. Phase 1 and 3 take the lock for microseconds and about a
//! second; phase 2 — minutes on a large tree — runs on the caller's thread
//! with nothing held, and every IPC request is served meanwhile.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use tomte_core::scanner::ScanError;

use crate::core::{DaemonCore, DaemonError};

/// Begin → scan (lock released) → apply, and again while a rescan was asked
/// for meanwhile. `Ok(None)`: a scan was already running on another thread;
/// it has been told to go again and its `ScanDone` is the one to wait for.
/// `Ok(Some(n))`: the live drifted count after the last apply.
pub fn run_scan(
    core: &Arc<Mutex<DaemonCore>>,
    now_fn: fn() -> u64,
) -> Result<Option<u32>, DaemonError> {
    let mut last = None;
    loop {
        let begun = core.lock().map_err(|_| poisoned())?.begin_scan(now_fn());
        let Some((scanner, ticket)) = begun else {
            return Ok(last);
        };
        // No lock held here — this line is the whole point of the module.
        // A panic inside the scan must still reach `apply_scan`, or the
        // in-progress flag would outlive its thread.
        let report = match catch_unwind(AssertUnwindSafe(|| scanner.scan())) {
            Ok(report) => report,
            Err(_) => Err(ScanError::Io(std::io::Error::other("the scan panicked"))),
        };
        let outcome = core
            .lock()
            .map_err(|_| poisoned())?
            .apply_scan(ticket, report, now_fn())?;
        last = Some(outcome.drifted);
        if !outcome.rescan_wanted {
            return Ok(last);
        }
        tomte_core::log_info!("scan", "a rescan was requested meanwhile — going again");
    }
}

fn poisoned() -> DaemonError {
    DaemonError::Io(std::io::Error::other("daemon state poisoned"))
}
```

`lib.rs`: add `pub mod scan;` (alphabetical: after `pub mod debounce;`).

Check `[profile.*]` in the workspace `Cargo.toml` has no `panic = "abort"` (then `catch_unwind` is real); say so in the report.

- [ ] **Step 4: Wire the three sites and the Status flag**

`main.rs` initial scan (replace lines 294–299):

```rust
    // Initial scan, OFF the lock (`scan::run_scan`): clients connect and get
    // an honest `scanning: true` meanwhile instead of waiting on the core.
    // A failure must not kill the daemon — the hourly rescan and manual
    // Rescan requests can recover it (spec §10).
    match tomte_daemon::scan::run_scan(&core, now_ts) {
        Ok(Some(drifted)) => println!("tomted: initial scan done, {drifted} drifted"),
        Ok(None) => println!("tomted: initial scan already running"),
        Err(e) => eprintln!("tomted: initial scan failed: {e}"),
    }
```

`main.rs` hourly thread: replace the `if let Ok(mut c) = core.lock() && let Err(e) = c.full_rescan(now_ts()) { … }` with `if let Err(e) = tomte_daemon::scan::run_scan(&core, now_ts) { eprintln!("tomted: rescan failed: {e}"); }`.

`server.rs` Rescan arm:

```rust
        Request::Rescan => {
            // Scans take minutes on real dotfile trees — far beyond the
            // client's request timeout, and (since 2026-10-11) they run with
            // the core lock RELEASED. Acknowledge now, scan on a thread;
            // completion arrives as the ScanDone push. A scan already in
            // progress is told to go again rather than doubled.
            drop(c);
            let core = ctx.core.get().cloned();
            let now_fn = ctx.now_fn;
            std::thread::spawn(move || {
                if let Some(core) = core
                    && let Err(e) = crate::scan::run_scan(&core, now_fn)
                {
                    eprintln!("tomted: requested rescan failed: {e}");
                }
            });
            Response::Ok
        }
```

`server.rs` locked Status arm: `scanning: c.scan_in_progress(),` instead of `scanning: false,`. In the long comment above the `try_lock` ("Everything else needs the core…"), replace the sentence "scans hold the core for seconds and run constantly" with "a scan's subprocess phase no longer holds the core at all (2026-10-11, `scan::run_scan`); what still does is the per-event path, the fetch and a scan's apply phase, each a bounded few seconds".

- [ ] **Step 5: Run, then the mutations**

Run: `cargo test -p tomte-daemon` — all PASS. Then `cp crates/daemon/src/server.rs /tmp/server.rs.bak`; mutation A: `scanning: false` back in the Status arm → `status_reports_scanning_with_real_data_while_a_scan_is_in_progress` fails; restore. Mutation B: in the Rescan arm, replace the `run_scan` call with `if let Some(core) = core && let Ok(mut c) = core.lock() && let Err(e) = c.full_rescan(now_fn()) { … }` → `a_rescan_request_does_not_hold_the_core_for_the_scan` fails with a busy bounce (if it does NOT fail, the scratch scan was under 200 ms: lower `busy_wait` in that test to 50 ms and rerun); restore.

- [ ] **Step 6: Run the real daemon once against the user's tree**

Build and run `target/debug/tomted --once` from this repo (read-only against the real chezmoi home; `--once` uses `full_rescan`, which is fine). Expected: the drifted list prints and the new `scan:` summary line appears in `~/Library/Application Support/Tomte/logs/tomted.log` with a duration. Paste that line in the report.

- [ ] **Step 7: Gate and commit**

```bash
git add crates/daemon/src/scan.rs crates/daemon/src/lib.rs crates/daemon/src/main.rs crates/daemon/src/server.rs crates/daemon/tests/server_ipc.rs
git commit -m "feat(daemon): run every scan with the core lock released; Status reports scanning honestly

The daemon bounced every request for the length of a scan (2.5 min with
499 drifted files) and the app's 90 s patience could never outlast it."
```

---

### Task 4: One subprocess per candidate — batched `source-path`, git sets per scan

**Files:**
- Modify: `crates/core/src/chezmoi.rs` (new `source_paths`, next to `source_path` at line 415)
- Modify: `crates/core/src/git.rs` (new `files_touched_in`, next to `commits_touching` at line 232)
- Modify: `crates/core/src/scanner.rs` (`scan` 84–228, `probe_one` 231–240, `probe_file` 242–287)
- Test: `crates/core/tests/scanner_integration.rs`

**Interfaces:**
- Produces:
  ```rust
  impl ChezmoiClient { pub fn source_paths(&self, targets: &[PathBuf]) -> Result<Vec<Option<PathBuf>>, ChezmoiError>; }
  impl GitClient { pub fn files_touched_in(&self, range: &str) -> Result<BTreeSet<PathBuf>, GitError>; }
  // scanner.rs, private:
  enum GitSignalSource<'a> { PerFile, Sets { local: &'a BTreeSet<PathBuf>, remote: &'a BTreeSet<PathBuf> } }
  fn probe_file(&self, source_dir: &Path, target: &Path, last_written: Option<ContentHash>,
                source_rel: Option<PathBuf>, signals: &GitSignalSource<'_>) -> Result<Option<FileDrift>, ScanError>;
  ```

- [ ] **Step 1: Write the tests**

In `crates/core/tests/scanner_integration.rs`:

```rust
/// `source-path` is batched (spec 2026-10-11 §2.3): one call, one line per
/// target in argument order. A target chezmoi does not know makes the whole
/// call fail, and the chunk is then answered one target at a time — never
/// by zipping a short reply against the full chunk.
///
/// MUTATION: on a failed chunk, extend the result with `None` for EVERY
/// target of the chunk instead of retrying per target → the third entry
/// is `None`. MUTATION: skip the line-count check and zip whatever came
/// back → a misaligned reply lands on the wrong targets.
#[test]
fn source_paths_answers_in_argument_order_and_never_misaligns() {
    let s = Scratch::new();
    let chezmoi = s.chezmoi();
    let testrc = s.home.join(".testrc");
    let unmanaged = s.home.join(".nope");
    let got = chezmoi
        .source_paths(&[testrc.clone(), unmanaged, testrc.clone()])
        .unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].as_deref(), Some(s.source.join("dot_testrc").as_path()));
    assert_eq!(got[1], None);
    assert_eq!(got[2], got[0]);
}

/// The scan reads ahead/behind from two `git log --name-only` sets computed
/// once (spec 2026-10-11 §2.3). A committed, unpushed source edit is
/// `local_ahead`; the per-event probe of one file still asks git per file.
///
/// MUTATION: hand `probe_file` an empty `local` set in `scan` (drop the
/// `files_touched_in("origin/main..HEAD")` call) → `local_ahead` reads
/// false here. (`remote_push_is_remote_ahead` above pins the `remote` set
/// the same way: an empty one turns its RemoteAhead into something else.)
#[test]
fn a_committed_unpushed_source_edit_is_local_ahead_in_both_probes() {
    let s = Scratch::new();
    std::fs::write(s.source.join("dot_testrc"), "a=committed\n").unwrap();
    git(&s.source, &["add", "."]);
    git(&s.source, &["commit", "-m", "local edit"]);
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    assert!(report.drifted[0].probe.git.local_ahead, "{:?}", report.drifted[0].probe.git);
    assert!(!report.drifted[0].probe.git.remote_ahead);
    let one = s
        .scanner()
        .probe_one(&s.home.join(".testrc"))
        .unwrap()
        .expect("drifted");
    assert_eq!(one.probe.git, report.drifted[0].probe.git, "both probes agree");
}
```

`GitSignals` must derive `PartialEq` for the last assertion; check `crates/core/src/drift.rs:30` and add `PartialEq, Eq` to its derive if missing.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p tomte-core --test scanner_integration source_paths local_ahead`
Expected: compile error, no `source_paths`.

- [ ] **Step 3: Implement the two client calls**

`chezmoi.rs`, after `source_path`:

```rust
    /// `chezmoi source-path t1 t2 …` for MANY targets in one invocation: one
    /// line per target, in argument order (verified against chezmoi on
    /// 2026-10-11). Chunked so an argv stays a few thousand paths at most.
    /// A chunk that exits non-zero (one unmanaged target fails the whole
    /// call) or answers the wrong number of lines is retried one target at
    /// a time — two lists of different lengths are never zipped.
    pub fn source_paths(&self, targets: &[PathBuf]) -> Result<Vec<Option<PathBuf>>, ChezmoiError> {
        const CHUNK: usize = 128;
        let mut out = Vec::with_capacity(targets.len());
        for chunk in targets.chunks(CHUNK) {
            let mut args: Vec<String> = vec!["source-path".into()];
            args.extend(chunk.iter().map(|p| p.to_string_lossy().into_owned()));
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            // Expected to fail when the chunk holds an unmanaged target:
            // logged as such, not as an error, like `cat_probe`.
            let req = self.request(&arg_refs).failure_is_expected();
            let reply = self.runner.run(req)?;
            let lines: Vec<PathBuf> = if reply.success() {
                String::from_utf8_lossy(&reply.stdout)
                    .lines()
                    .map(|l| PathBuf::from(l.trim()))
                    .collect()
            } else {
                Vec::new()
            };
            if reply.success() && lines.len() == chunk.len() {
                out.extend(lines.into_iter().map(Some));
            } else {
                for target in chunk {
                    out.push(self.source_path(target).ok());
                }
            }
        }
        Ok(out)
    }
```

(`self.request(..)` and `self.runner.run(req)?` are the shape `cat_probe` uses at line 222; `reply.success()` / `reply.stdout` are `CommandOutput`'s.)

`git.rs`, after `commits_touching`:

```rust
    /// Every path some commit in `range` touched, from ONE history walk:
    /// `rel ∈ files_touched_in(range)` is `commits_touching(range, rel) > 0`
    /// without a subprocess per path (both list the files of each commit in
    /// the range; both leave TREESAME merge commits out). `--format=` drops
    /// the commit headers, so the output is file names and blank lines.
    pub fn files_touched_in(&self, range: &str) -> Result<BTreeSet<PathBuf>, GitError> {
        Ok(self
            .run_utf8(&["log", "--name-only", "--format=", range])?
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect())
    }
```

Add `use std::collections::BTreeSet;` to `git.rs` if absent.

- [ ] **Step 4: Rework the scanner**

In `scanner.rs`, add after `UnprobedTarget`:

```rust
/// Where `probe_file` reads a target's ahead/behind from.
enum GitSignalSource<'a> {
    /// Two `rev-list --count` for THIS file: the per-event probe of one
    /// target, where two subprocesses are cheap and a whole-history walk
    /// per file event would not be.
    PerFile,
    /// Computed once per scan (spec 2026-10-11 §2.3): the paths touched by
    /// `remote..HEAD` and by `HEAD..remote`.
    Sets {
        local: &'a BTreeSet<PathBuf>,
        remote: &'a BTreeSet<PathBuf>,
    },
}
```

In `scan()`, replace the `let mut drifted = Vec::new(); for target in candidates { … }` loop with:

```rust
        // The probe loop's two git questions and its source-path lookup are
        // answered once for the whole scan (spec 2026-10-11 §2.3): three
        // subprocesses instead of three per candidate. A set that cannot be
        // computed degrades to the per-file questions, logged — never to a
        // guessed "not ahead".
        let local_range = format!("{}..HEAD", self.remote_ref);
        let remote_range = format!("HEAD..{}", self.remote_ref);
        let sets = match (
            self.git.files_touched_in(&local_range),
            self.git.files_touched_in(&remote_range),
        ) {
            (Ok(local), Ok(remote)) => Some((local, remote)),
            (l, r) => {
                let why = l.err().or(r.err()).map(|e| e.to_string()).unwrap_or_default();
                crate::log_warn!("scan", "git ahead/behind sets unavailable ({why}); asking per file");
                None
            }
        };
        let probe_targets: Vec<PathBuf> = candidates
            .iter()
            .filter(|t| !unprobed.iter().any(|u| u.target == **t))
            .filter(|t| !matches!(dump.entry_state.get(*t), Some(entry) if entry.kind != "file"))
            .cloned()
            .collect();
        let source_abs = self.chezmoi.source_paths(&probe_targets)?;
        let mut drifted = Vec::new();
        for (target, abs) in probe_targets.into_iter().zip(source_abs) {
            let last_written = dump
                .entry_state
                .get(&target)
                .and_then(|entry| entry.contents_sha256.as_deref())
                .and_then(ContentHash::from_hex);
            let source_rel = abs.and_then(|a| a.strip_prefix(&source_dir).ok().map(Path::to_path_buf));
            let signals = match &sets {
                Some((local, remote)) => GitSignalSource::Sets { local, remote },
                None => GitSignalSource::PerFile,
            };
            match self.probe_file(&source_dir, &target, last_written, source_rel, &signals) {
                Ok(Some(d)) => drifted.push(d),
                Ok(None) => {}
                Err(ScanError::Chezmoi(ChezmoiError::NotAFile { .. })) => {}
                Err(e) => unprobed.push(UnprobedTarget {
                    target: target.clone(),
                    reason: e.to_string(),
                }),
            }
        }
```

Keep the two existing comments about the unprobed skip and the non-file entry where they still apply (fold them into the two `filter`s). `probe_one` becomes:

```rust
    pub fn probe_one(&self, target: &Path) -> Result<Option<FileDrift>, ScanError> {
        let source_dir = self.chezmoi.source_dir()?;
        let dump = self.chezmoi.state_dump()?;
        let last = dump
            .entry_state
            .get(target)
            .and_then(|e| e.contents_sha256.as_deref())
            .and_then(ContentHash::from_hex);
        let source_rel = self
            .chezmoi
            .source_path(target)
            .ok()
            .and_then(|abs| abs.strip_prefix(&source_dir).ok().map(Path::to_path_buf));
        self.probe_file(&source_dir, target, last, source_rel, &GitSignalSource::PerFile)
    }
```

and `probe_file`'s signature gains `source_rel: Option<PathBuf>, signals: &GitSignalSource<'_>`; its `source_rel` computation is deleted and its `git` block becomes:

```rust
        let git = match (&source_rel, signals) {
            (Some(rel), GitSignalSource::PerFile) => GitSignals {
                local_ahead: self.git.commits_touching(&format!("{}..HEAD", self.remote_ref), rel)? > 0,
                remote_ahead: self.git.commits_touching(&format!("HEAD..{}", self.remote_ref), rel)? > 0,
            },
            (Some(rel), GitSignalSource::Sets { local, remote }) => GitSignals {
                local_ahead: local.contains(rel),
                remote_ahead: remote.contains(rel),
            },
            (None, _) => GitSignals::default(),
        };
```

`source_dir` is a `PathBuf` in `scan` (from `self.chezmoi.source_dir()?`); `strip_prefix(&source_dir)` works on it.

- [ ] **Step 5: Run the core tests, then the mutations**

Run: `cargo test -p tomte-core` — all PASS (every scanner classification test unchanged). Then `cp crates/core/src/scanner.rs /tmp/scanner.rs.bak; cp crates/core/src/chezmoi.rs /tmp/chezmoi.rs.bak`, run the three mutations named in Step 1's comments (two on `source_paths`, one on `scan`'s local set) plus "empty remote set" against `remote_push_is_remote_ahead`; each must fail its test; restore.

- [ ] **Step 6: Measure on the user's tree**

`cargo build -p tomte-daemon && target/debug/tomted --once > /dev/null; tail -5 ~/Library/Application\ Support/Tomte/logs/tomted.log | rg 'scan\]'` — paste the summary line (duration) next to Task 3's number in the report. Expected: well under half.

- [ ] **Step 7: Gate and commit**

```bash
git add crates/core/src/chezmoi.rs crates/core/src/git.rs crates/core/src/scanner.rs crates/core/src/drift.rs crates/core/tests/scanner_integration.rs
git commit -m "perf(scanner): one subprocess per candidate — batched source-path, git ahead/behind sets per scan"
```

---

### Task 5: A queued decision can be removed (✕ on waiting rows)

**Files:**
- Modify: `crates/app/src/action_queue.rs` (new `cancel` after `push`, line ~208; tests)
- Modify: `crates/app/src/views/mod.rs` (new `Shell::cancel_queued` after `enqueue_many`, line ~870; tests)
- Modify: `crates/app/src/views/review.rs` (`QueuedDecision` impl ~line 875; `SidebarRow::Target` ~1016; `sidebar_rows` PROCESSING arm ~1085, the two `selectable: false` arms and the `target_row` closure; `sidebar()` call ~2070; `target_row` ~2256; new `ReviewView::cancel_queued`; planner tests ~3302)
- Modify: `CHANGELOG.md` (`## Unreleased`), `AGENTS.md` (Data flow, Rules, Current state)

**Interfaces:**
- Produces: `ActionQueue::cancel(&mut self, target: &Path) -> bool`; `Shell::cancel_queued(&mut self, target: &Path, cx: &mut Context<Self>)`; `SidebarRow::Target { …, cancellable: bool }`.

- [ ] **Step 1: Queue test and implementation**

In `action_queue.rs` tests:

```rust
    /// MUTATION: also drain `running` in `cancel` → `/a` stops processing.
    /// MUTATION: insert a failure for the cancelled target → the `failure`
    /// assertion fails: a withdrawn decision is not a failed one.
    #[test]
    fn a_pending_decision_can_be_cancelled_but_a_running_one_cannot() {
        let mut q = ActionQueue::default();
        q.push(action("/a", Decision::KeepDisk));
        let _running = q.take_batch();
        q.push(action("/b", Decision::KeepSourceLocal));
        assert!(q.cancel(Path::new("/b")));
        assert!(!is_processing(&q, "/b"));
        assert!(q.failure(Path::new("/b")).is_none(), "withdrawn, not failed");
        assert!(!q.cancel(Path::new("/a")), "running: cannot be recalled");
        assert!(is_processing(&q, "/a"));
        assert_eq!(q.depth(), 1);
    }

    /// A target that is running AND re-queued (a Save on a file whose batch
    /// already started) holds two entries; ✕ takes back only the pending
    /// one and the running row keeps saying "running…".
    ///
    /// MUTATION: make `cancel` return `true` without removing anything when
    /// the target is also running → the depth assertion fails.
    #[test]
    fn cancelling_a_re_queued_running_target_removes_only_the_pending_copy() {
        let mut q = ActionQueue::default();
        q.push(action("/a", Decision::KeepDisk));
        let _running = q.take_batch();
        q.push(action("/a", Decision::KeepSourceLocal));
        assert_eq!(q.depth(), 2);
        assert!(q.cancel(Path::new("/a")));
        assert_eq!(q.depth(), 1);
        assert_eq!(q.running().len(), 1);
        assert_eq!(q.decision_label(Path::new("/a")), Some("Keep disk"));
    }
```

(Check `Decision::label()` for `KeepDisk` returns exactly `"Keep disk"` at `action_queue.rs:80`; use the real string.) Then, after `push`:

```rust
    /// Take back a decision that has not started (spec 2026-10-11 §2.4).
    /// `true` when a pending entry for `target` was removed; `false` — and
    /// nothing else — when none was pending: a RUNNING decision is one
    /// in-flight re-add/apply/commit/push on the user's repo, and the
    /// engine is blocking in a background thread for it. Records no
    /// failure: the user withdrew the decision, nothing failed.
    pub fn cancel(&mut self, target: &Path) -> bool {
        let before = self.pending.len();
        self.pending.retain(|p| p.target != target);
        self.pending.len() != before
    }
```

Run: `cargo test -p tomte-app --lib cancel` → PASS; run both mutations; restore.

- [ ] **Step 2: `Shell::cancel_queued`**

In `views/mod.rs`, after `enqueue_many`:

```rust
    /// Take back a queued decision (spec 2026-10-11 §2.4). No latch and no
    /// repo: only the queue changes, so this may be reached from inside the
    /// review view's own update — it never updates that view back.
    pub fn cancel_queued(&mut self, target: &Path, cx: &mut Context<Self>) {
        let label = self.queue.decision_label(target).unwrap_or("a decision");
        if self.queue.cancel(target) {
            tomte_core::log_info!(
                "resolve",
                "removed the queued {label} for {}",
                target.display()
            );
            cx.notify();
        }
    }
```

- [ ] **Step 3: The plan carries `cancellable`; the row shows ✕**

`review.rs`:

- `impl QueuedDecision`: add
  ```rust
      /// Still takes a ✕: the decision has not started (spec 2026-10-11 §2.4).
      fn cancellable(self) -> bool {
          matches!(self.status, QueuedStatus::Waiting)
      }
  ```
- `SidebarRow::Target`: add the field with its doc:
  ```rust
          /// Under PROCESSING and not yet running: the row carries a ✕ that
          /// removes the decision from the queue. A field like `locked`, for
          /// the same reason — derived once here, read verbatim by the row.
          cancellable: bool,
  ```
- `sidebar_rows`: PROCESSING arm `cancellable: queued.cancellable(),`; NEEDS A DECISION arm and the `target_row` closure `cancellable: false,`.
- `sidebar()`'s `SidebarRow::Target { … }` destructuring and the `self.target_row(..)` call pass `*cancellable` after `*locked`.
- `target_row` gains `cancellable: bool` after `locked: bool`, and its trailing column becomes:
  ```rust
          let trailing: AnyElement = if cancellable {
              let t = target.clone();
              ui::button(
                  theme,
                  ElementId::named_usize("review-cancel", ix),
                  "✕".into(),
                  ui::ButtonVariant::Ghost,
                  ui::ButtonSize::Micro,
                  cx.listener(move |view, _ev, _w, cx| {
                      // The row must not be selected by its own ✕.
                      cx.stop_propagation();
                      view.cancel_queued(&t, cx);
                  }),
              )
              .tooltip(ui::text_tooltip("Remove from queue"))
              .flex_shrink_0()
              .into_any_element()
          } else {
              let text: SharedString = match queued {
                  Some(_) => "".into(),
                  None => entry.since_ts.map(|ts| time_ago(now, ts)).unwrap_or_default().into(),
              };
              div()
                  .flex_shrink_0()
                  .text_xs()
                  .text_color(theme.text_muted)
                  .child(text)
                  .into_any_element()
          };
  ```
  and the `.child(div().flex_shrink_0().text_xs()….child(trailing))` at the end of the name row becomes `.child(trailing)`. Update the comment above (“A processing row's trailing column goes BLANK”) to say it goes blank for a RUNNING row and carries the ✕ for a WAITING one.
- New method next to `enqueue`:
  ```rust
      /// The ✕ on a waiting PROCESSING row. Safe from inside this view's
      /// update for the same reason `enqueue` is: `Shell::cancel_queued`
      /// only touches the queue and notifies.
      fn cancel_queued(&mut self, target: &Path, cx: &mut Context<Self>) {
          let t = target.to_path_buf();
          if let Err(e) = self.shell.update(cx, |shell, cx| shell.cancel_queued(&t, cx)) {
              tomte_core::log_error!("resolve", "could not cancel {}: {e}", target.display());
          }
      }
  ```

Fixtures (`fixtures.rs`) construct no `SidebarRow` directly; the planner tests in `review.rs` destructure `SidebarRow::Target { entry, queued, locked, depth, selectable, checked }` — add `cancellable` (or `..`) where the compiler says so.

- [ ] **Step 4: Planner and Shell tests**

In `review.rs`'s tests, next to the test that asserts `locked` (the one whose comment names `queued.is_some()` in `target_row`, ~line 3342):

```rust
    /// MUTATION: `cancellable: queued.is_some()` → the running row reads
    /// cancellable and this fails.
    #[test]
    fn only_a_waiting_processing_row_is_cancellable() {
        let drifted = vec![entry("/a", "destination_drift"), entry("/b", "destination_drift")];
        let queue = QueueSnapshot {
            processing: vec![
                (PathBuf::from("/a"), QueuedDecision { status: QueuedStatus::Running, label: "Keep disk" }),
                (PathBuf::from("/b"), QueuedDecision { status: QueuedStatus::Waiting, label: "Keep disk" }),
            ],
            failures: BTreeMap::new(),
            busy: true,
        };
        let plan = plan_of(&drifted, &queue);
        let flags: Vec<(String, bool)> = plan
            .rows
            .iter()
            .filter_map(|r| match r {
                SidebarRow::Target { entry, cancellable, .. } => {
                    Some((entry.target.display().to_string(), *cancellable))
                }
                _ => None,
            })
            .collect();
        assert_eq!(flags, vec![("/a".to_string(), false), ("/b".to_string(), true)]);
    }
```

(`entry(..)` and `plan_of(..)` are the module's existing helpers at ~line 3050–3070; use their real names.)

In `views/mod.rs` tests, next to `a_batch_really_drains_through_pump`:

```rust
    /// ✕ on a waiting row goes `ReviewView::cancel_queued` → `Shell::
    /// cancel_queued` → `ActionQueue::cancel`, and the file comes back
    /// under its group with no ✕ while the running row is untouched.
    ///
    /// MUTATION: return from `Shell::cancel_queued` before
    /// `self.queue.cancel` → the depth stays 2 and the waiting row stays
    /// under PROCESSING. MUTATION: in `ReviewView::cancel_queued`, pass the
    /// RUNNING target's path to the Shell instead of `target` → the
    /// running assertion fails.
    #[gpui::test]
    fn the_x_on_a_waiting_row_removes_its_decision_from_the_queue(cx: &mut TestAppContext) {
        let (shell, vis) = cx.add_window_view(|_window, cx| {
            fixtures::build("review-processing", smoke_paths(), cx).expect("review-processing fixture")
        });
        let review = review_of(&shell, vis);
        vis.run_until_parked();
        let waiting = fixtures::home(".gitconfig");
        let running = fixtures::home(".config/zed/settings.json");
        let before = vis.update(|_w, cx| review.update(cx, |view, cx| view.plan(cx)));
        assert!(before.rows.iter().any(|r| matches!(r,
            SidebarRow::Target { entry, cancellable: true, .. } if entry.target == waiting)));
        vis.update(|_w, cx| review.update(cx, |view, cx| view.cancel_queued(&waiting, cx)));
        vis.run_until_parked();
        vis.update(|_w, cx| {
            let s = shell.read(cx);
            assert_eq!(s.queue.depth(), 1);
            assert_eq!(s.queue.running().len(), 1);
            assert_eq!(s.queue.running()[0].target, running);
            assert!(s.queue.failure(&waiting).is_none());
        });
        let after = vis.update(|_w, cx| review.update(cx, |view, cx| view.plan(cx)));
        let group_of = |rows: &[SidebarRow], target: &Path| -> &'static str {
            let mut current = "";
            for r in rows {
                match r {
                    SidebarRow::Group { label, .. } => current = label,
                    SidebarRow::Target { entry, .. } if entry.target == target => return current,
                    _ => {}
                }
            }
            "absent"
        };
        assert_eq!(group_of(&after.rows, &waiting), "SAFE TO RESOLVE");
        assert_eq!(group_of(&after.rows, &running), "PROCESSING");
    }
```

`fixtures.rs:144` has a private `fn home(sub: &str) -> PathBuf`; make it `pub(super) fn home` so the test can call `fixtures::home(..)`. `ReviewView::cancel_queued` and `plan` must be `pub(super)` for this test (`plan` already is).

- [ ] **Step 5: Run, mutations, screenshot**

Run: `cargo test -p tomte-app` — all PASS. Mutations: the planner one (`queued.is_some()`), the Shell one (early return), the queue ones from Step 1; each fails its test; restore from `cp` backups.

Then: `cargo build -p tomte-app && ./scripts/shoot.sh review-processing dark && ./scripts/shoot.sh review-processing light`, and READ both PNGs: the `.gitconfig` row (queued) shows a ✕ at its right edge; the `settings.json` row (running) shows none; nothing else moved. Fix and reshoot until true.

- [ ] **Step 6: Changelog and engineering map**

`CHANGELOG.md`, under `## Unreleased`, add a `### Fixed` section above `### Added` with:

```markdown
### Fixed

- **A scan no longer blocks what you do.** The daemon used to hold its one
  lock for the whole scan — minutes with hundreds of drifted files — and
  every Save, Undo or Keep disk pressed meanwhile waited, then failed with
  "the daemon stayed busy". Scans now run with the lock released; what you
  press is answered while they run, and the status line says "scanning…"
  next to real numbers instead of blanking them.
- **The daemon no longer rescans itself in a loop.** A vendored skill with
  its own `.git` inside the chezmoi source tree made every `git status`
  look like a source change; the log cycled every half second, forever.
  Nested repositories are ignored.
- **Scans cost a third of what they did.** One `chezmoi source-path` call
  for all candidates and two `git log` walks per scan replace three
  subprocesses per file.
```

and to `### Added`:

```markdown
- **Remove a queued decision.** A ✕ on every PROCESSING row that has not
  started yet takes it back; the file returns to its group. A running
  decision cannot be recalled mid-commit, and shows no ✕.
```

`AGENTS.md`:
- Data flow, item 1: after "`tomted` scans (chezmoi `status` + git)" add "— with the core lock RELEASED for the subprocess phase (`scan::run_scan`: `begin_scan` / `DriftScanner::scan` / `apply_scan`; a target the per-event path touched meanwhile keeps its state)".
- Rules → Code, the bullet starting "The daemon bounces a request it cannot serve within its core-lock wait": append "Since 2026-10-11 a scan never holds the core; the lock holders are the per-event path, the fetch and a scan's apply phase."
- Add a Code bullet: "Watcher paths go through `classify_path`: anything inside a NESTED `.git` is ignored (vendored repos in the source tree made the daemon rescan itself every 0.6 s)."
- Current state: add "Scans run off the daemon lock, nested repositories are ignored by the watcher, a queued decision can be ✕'d." to the branch paragraph; add to the open list: "The per-event path (`rescan_source_side`) and `handle_fetch`'s per-file probes still run under the lock — bounded by changed-file counts; give them the begin/apply split if one ever shows in a banner."

- [ ] **Step 7: Gate and commit**

```bash
git add crates/app/src/action_queue.rs crates/app/src/views/mod.rs crates/app/src/views/review.rs crates/app/src/views/fixtures.rs CHANGELOG.md AGENTS.md
git commit -m "feat(review): remove a queued decision with ✕; changelog and engineering map for the daemon lock work"
```
