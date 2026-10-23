# AGENTS.md

Context for coding agents working on Tomte. Read this before touching code.
`README.md` is the human intro, `PRODUCT.md` the product register, `DESIGN.md`
the design system; this file is the engineering map and the house rules.

## What this is

Tomte is a macOS menubar app that keeps [chezmoi](https://chezmoi.io)-managed
dotfiles from drifting. For every managed file it watches three states — the
chezmoi source, the file on disk, and git origin — classifies the divergence,
and lets the user resolve each file with one click (keep disk / keep source)
or a template-aware three-pane merge editor. Every mutation snapshots first
and is undoable. Built by one developer for his own setup first: chezmoi + one
git remote + 1Password on Apple-silicon macOS. Rust, edition 2024, GPUI 0.2.2
(pinned exactly), `stable` toolchain.

Two binaries, one workspace:

- **`tomte`** — the menubar app (GPUI). Dashboard, Review, merge editor,
  Settings, changelog window, self-updater.
- **`tomted`** — the daemon. Watches the source repo and every managed
  target, scans for drift, fetches origin, journals to SQLite, serves the app
  over a Unix socket.

The user's mental model, from `PRODUCT.md`: "glance, trust, clear the queue,
leave". Trust is the currency — the app touches shell configs and secrets
templates, so every state must be honest and every action must say what it
did.

## Workspace map

```
crates/core     tomte-core     chezmoi + git clients, drift classification, scanner,
                               structured 3-way merge, template span pipeline,
                               structured/ = JSONC parser, per-key decisions, splicer,
                               SystemRunner (every subprocess goes through it),
                               rotating file logging. test-support feature = Scratch.
crates/daemon   tomte-daemon   tomted: DaemonCore state machine (no clocks, callers
                               drive it), ndjson socket server, fs debounce, Settings.
crates/journal  tomte-journal  SQLite WAL journal + zstd blob store. Single writer =
                               the daemon; the app opens it read-only.
crates/proto    tomte-proto    IPC wire types: Request / Response / Event (ndjson).
crates/ui       tomte-ui       Theme tokens, embedded SVG assets, every reusable
                               component (one file each under src/components/),
                               component previews for the gallery.
crates/app      tomte-app      lib: ipc, model (pure), resolve (ResolveEngine),
                               action_queue, merge_inputs/merge_state (pure),
                               structured_state (pure, Structure mode's picks), update.
                               bin: main.rs (boot, reconnect loop, CLI flags),
                               views/ (mod.rs = Shell, dashboard, review, merge,
                               merge_structured = Structure mode's strips and header,
                               settings, changelog, fixtures = gallery poses,
                               review_selection/review_tree = pure Review
                               multi-select + folder-tree projections),
                               platform_mac (status item, activation policy).
```

`.agents/artifacts/specs/2026-07-19-chezmoi-ui-v0-design.md` is "the spec"
that doc comments cite as `spec §N`. Later specs and plans live beside it;
the newest shape of a subsystem is in the newest spec that touches it:
`2026-08-29-sync-axes-design.md` (history vs content axes, origin as a fourth
content state) and `2026-09-02-async-action-queue-design.md` (the serial
action queue, batching, per-file undo).

### Data flow

1. `tomted` scans (chezmoi `status` + git) — with the core lock RELEASED for
   the subprocess phase (`scan::run_scan`: `begin_scan` /
   `DriftScanner::scan` / `apply_scan`; a target the per-event path touched
   meanwhile keeps its state) → `DaemonCore` classifies each
   target → journals → pushes `Event::{Drift, ScanDone, FetchDone, …}` to
   subscribers.
2. `tomte` connects on boot (`IpcClient::connect_or_spawn` launches `tomted`
   if the socket is dead, and first shuts down a daemon from any OTHER
   build — `HelloOk` carries the daemon's `BuildId`, exe path + mtime at
   its start), hydrates `SyncModel` from `Request::Status`, and reconnects
   forever.
3. A user decision goes `view → Shell::enqueue → ActionQueue → Shell::pump →
   ResolveEngine` on the background executor. The engine snapshots (journal
   session), announces expected changes to the daemon, writes (chezmoi
   `re-add` / `apply`, or a source write mid-merge), commits and pushes, ends
   the session, asks for a rescan. Consecutive one-click decisions share one
   re-add / apply / commit / push.
4. Results land back on the Shell in a fresh update: row failure, outcome
   banner naming the file (this is what Undo reverts), model refresh.

### Invariant: one latch for all git work

Every app-initiated git mutation runs while `Shell::queue_pumping` is true,
and only `Shell::pump` starts one: queued decisions, merge-editor saves,
dashboard quick actions, and the repo-level ops (finish merge, roll back,
undo) which are `PendingRepoOp`s with fixed precedence (a roll-back replaces a
pending finish; a finish never displaces a pending or running roll-back; undo
displaces neither). The one pre-existing exception is the tray's "Sync all"
(`run_sync_all` in `main.rs`); do not add another. Nothing outside `pump` may
call an engine method that mutates the repo.

### Files on disk (per user)

```
~/Library/Application Support/Tomte/
  settings.toml   fetch interval, 1Password account (shared by app + daemon)
  journal.db      SQLite journal (daemon writes, app reads)
  daemon.sock     IPC socket
  logs/           tomte.log, tomted.log — rotating; every subprocess, IPC
                  failure, pipeline step and panic lands here
  updates/        staged self-update bundles
```

Overrides: `TOMTE_SETTINGS`, `TOMTE_JOURNAL`, `TOMTE_SOCKET`, `TOMTE_DAEMON`
(path to the tomted binary; default = sibling of `tomte`, else PATH),
`TOMTE_NO_SHELL_PATH=1` (skip adopting the login shell's PATH — hermetic
tests), `TOMTE_PERF=1` (per-tag timing aggregates), `TOMTE_NOSYNC=1`
(perf bisection: disables follower updates), `TOMTE_TEAM_ID` (baked at build
for the updater's signature check).

## Setup and everyday commands

Prerequisites: Rust stable, `chezmoi` on PATH (the e2e suites drive the real
binary; CI does `brew install chezmoi`), git. Use `fd` instead of `find`,
`rg` instead of `grep`.

```sh
cargo build --workspace                  # target/debug/{tomte,tomted}
cargo test --workspace                   # ~370 tests, ~26 suites, a few minutes
./scripts/bundle.sh                      # target/bundle/Tomte.app (ad-hoc signed unless SIGN_IDENTITY)
./scripts/install-launchagent.sh         # user-invoked only: LaunchAgent com.remikalbe.tomted
```

Run against the real daemon and real dotfiles:

```sh
target/debug/tomte                       # menubar app; spawns tomted if needed
target/debug/tomte --live review         # one route, real data (dashboard|review|settings)
target/debug/tomted --once               # one scan, then exit
target/debug/tomte --print-status        # headless: boot path → Status → print
target/debug/tomte --verify-connectivity # spawn the real daemon over a real socket
target/debug/tomte --diagnose            # THE pasteable bug report; run it first for any machine issue
```

Gallery (synthetic, posed UI states — never ask the user for screenshots):

```sh
target/debug/tomte --gallery-list                 # every named state with a description
target/debug/tomte --gallery review-processing --dark
./scripts/shoot.sh                                # every state, dark + light → shots/<state>-<theme>.png
./scripts/shoot.sh review-failed dark             # one state, one theme
./scripts/shoot.sh live dashboard                 # the REAL app with live data
```

Then READ the PNG. `shoot.sh` deletes a stale PNG when a capture fails, so
an "OK" line means this run wrote the file. `screencapture` fails inside a
sandboxed shell; run it unsandboxed. Every new view sub-state gets a pose in
`crates/app/src/views/fixtures.rs` (`STATES`); the render-smoke test builds
all of them. `merge-structured`, `merge-structured-locked`, and
`merge-structured-text` pose Structure mode on a JSON settings file (a
conflict, keys added and removed on disk, array rows), a templated source
with a locked row, and the Text stage with its not-valid-JSON chip;
`merge-not-structured` poses a JSON-looking file that does not parse (the
line editor with its "line merge" note).
`review-multiselect`, `review-multiselect-closed`,
`review-tree`, and `review-tree-selected` pose the sidebar's multi-select
and folder-tree states (checkboxes ticked, ticked with no file open, a
folder collapsed, a whole folder selected). `review-many` is the 500-file
perf pose.

## The gate

Run before every commit, in the foreground, and read the output. Check
clippy's exit status AND the grep: `-D warnings` reports lints as `error:`
lines, which a `^warning`-only grep never sees.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings > /tmp/clippy.log 2>&1; echo "clippy exit=$?"  # must be 0
rg -n '^(error|warning)' /tmp/clippy.log | rg -v 'block v0\.1\.6|proc-macro-error2'          # must print nothing
cargo test --workspace
GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null cargo test --workspace
```

The last line mimics CI, which has no global git config; two releases failed
on this after a green local run. Never run the gate in the background and
assume it passed.

## Testing

- Unit tests sit beside the code. Integration tests: `crates/core/tests`
  (real git repos, real `chezmoi execute-template`), `crates/daemon/tests`
  (DaemonCore against a scratch home; socket round-trips),
  `crates/app/tests` (`resolve_e2e.rs` = the engine against a scratch home +
  bare origin + in-process daemon; `e2e_real_binaries.rs` = the compiled
  binaries over a real socket; `single_chezmoi_factory.rs` = a structural
  gate, see Rules).
- Views are bin-only, so view and Shell tests live in `crates/app/src/views/
  mod.rs`'s `#[cfg(test)]` module. `EngineLab::publish(cx)` there builds the
  whole stack (scratch home, bare origin, daemon thread, connected engine)
  and publishes it as the app's `EngineSlot`, so a `#[gpui::test]` can drive
  `Shell::enqueue` for real under `cx.run_until_parked()`. Its engine git
  client runs over a recording runner, so tests can count push invocations.
- `tomte_core::testsupport::Scratch` is the scratch chezmoi home; it writes a
  repo-local unsigned git identity because production `GitClient` runs plain
  `git`. `chezmoi_commits_and_pushes_for_itself` (in `resolve_e2e.rs`) turns
  on autoCommit/autoPush the way the user's machine has them — without it a
  fixture is blind to the push chezmoi itself performs.
- `FakeRunner` (`crates/core/src/cmd/fake.rs`) scripts subprocess replies for
  client-level tests (the 1Password retry tests use it).
- Subsets: `cargo test -p tomte-core --lib <name>`,
  `cargo test -p tomte-app --test resolve_e2e <name>`,
  `cargo test -p tomte-app --bin tomte <name>` (Shell/view tests).
- A test earns its place by naming, in a comment, the mutation that fails
  it — and the mutation gets run. Reviewers do this; several tests were
  rewritten because a mutation passed. A behaviour visible only in a
  screenshot has no test: plan the rows as data and test the plan.
- No sleeps or wall-clock dependence; everything through `run_until_parked`.

## Rules

Hard rules, each one learned the expensive way.

**Git and commits**
- Commits are SSH-signed via 1Password. If signing fails (`1Password: failed
  to fill whole buffer` — the user is away), leave the work staged, keep
  going, retry later. Never `--no-gpg-sign`, never touch git config.
- Conventional commit subjects (`feat(review): …`, `fix(shell): …`,
  `docs(plan): …`, `test: …`, `chore: 0.1.18`); the body says what and why
  in the user's terms.
- Never leave the user's dotfiles repo mid-merge on Tomte's own initiative.
- Releases go ONLY through `./scripts/release.sh X.Y.Z` (see Releasing).

**On this machine**
- `~/.config` and the user's dotfiles are chezmoi-managed. Never edit them
  directly; Tomte is the thing that edits them.
- After any pkill/relaunch of `tomte` or `tomted`, confirm with
  `ps ax -o pid,lstart,command` that the start time is new. Old processes can
  inherit `SIGTERM=SIG_IGN` and survive `pkill`; escalate to `kill -KILL` or
  the socket `Shutdown` request. A "fix that changed nothing" is usually a
  stale process or binary, not code — read the COMMAND column too: the
  bundle's `/Applications/Tomte.app/…/tomted` served a `target/debug/tomte`
  for two days (2026-10-12) because it spoke the same protocol. Since then
  the app takes over a daemon from another build at launch (`BuildId` in
  `HelloOk`, compared with the `tomted` it would spawn); `tomte --diagnose`
  prints both identities.
- Unix socket paths are capped at 104 bytes; throwaway daemon probes need a
  short `/tmp` path, not a deep scratchpad one.

**Code**
- `app_chezmoi_client()` in `main.rs` is the only place that builds an
  app-side chezmoi client (it carries the 1Password account from settings);
  `single_chezmoi_factory.rs` fails the build on a bypass.
- `chezmoi re-add` / `apply` with an empty target list means EVERY file;
  `re_add(&[])` and `apply_many(&[])` are hard errors. Keep them so.
- A templated source (`.tmpl`) is never `re-add`ed: chezmoi silently ignores
  it. Keep disk on one goes through the template write-back
  (`resolve_merged_as`), which refuses with `ProtectedSpan` only when the
  disk text cannot be expressed in the template's literal segments. Undo
  skips such refused attempts and reverts the last session that landed.
- Never swallow an error or return a confident zero. Failures are logged
  (`tomte_core::log_error!`) and reach the user: row chip, banner, or
  notification. Degraded says why and what to do; scanning never claims
  in-sync.
- The daemon bounces a request it cannot serve within its core-lock wait
  (`ServeCtx::busy_wait`, 20 s) with `tomte_proto::BUSY_REPLY`, and answers
  `STARTING_PREFIX` before it has a core: nothing executed either way.
  `tomte_proto::is_daemon_bounce` is the one matcher; every user-initiated
  request goes through `IpcClient::request_patiently`, which retries the
  bounce for `DAEMON_PATIENCE` (90 s, per request) and then fails typed
  (`IpcError::Busy`). Never surface a single bounce. A batch makes the budget
  per batch: after the first exhausted one it stops asking, the closing
  rescan included (later files are "not attempted — … — try again"; files
  whose work already landed are "resolved, but the session was not closed",
  never "try again"). A client-side timeout is NOT retried — the daemon may
  have executed it. Since 2026-10-11 a scan never holds the core; the lock
  holders are the per-event path, the fetch and a scan's apply phase.
- The IPC reader thread's EOF is the ONLY "daemon gone" signal the app's
  boot loop has: it sets `closed`, fails every in-flight request
  (`IpcError::Closed`) and drops the event Sender so the loop sees
  `Disconnected` and reconnects. Until 2026-10-12 the Sender outlived the
  socket and a shut-down daemon was never replaced. Keep that cleanup.
- Watcher paths go through `classify_path`: anything inside a NESTED `.git`
  is ignored (vendored repos in the source tree made the daemon rescan
  itself every 0.6 s).
- Subprocess waits are WNOHANG polling with timeouts, never signal-based;
  fetch runs with `GIT_TERMINAL_PROMPT=0` and ssh `BatchMode` so a locked
  1Password agent fails fast instead of hanging the daemon.
- Transient 1Password template failures (`OnePasswordAuth`) retry 3× with
  250 ms backoff inside `run_ok_with_timeout`; multiple-accounts is a config
  error and does not retry.
- Views use Theme tokens only (no raw hex), humanized labels only (see
  `model.rs` `kind_label` / `class_label`), and new reusable styling goes in
  `crates/ui` first, never inline in a view. The review diff uses tinted dots
  (● source-only / ● disk-only), never +/− markers — the user rejected those.
- Any scrollable list rides `uniform_list` (fixed-height rows) or gpui's
  `list` (mixed heights — the Review sidebar, whose plan is spliced into its
  `ListState` by `splice_window` over `row_keys`, never `reset` on an
  ordinary rebuild); plain `overflow_y_scroll` columns drop frames.

**GPUI (0.2.2) — see `.claude/skills/gpui-tomte` for the verified detail**
- Never update an entity from inside its own update closure. A view handing
  off to the Shell from inside its own update is safe only if the Shell path
  never synchronously updates that view back; otherwise `cx.spawn` first,
  then `shell.update`. This crashed the app in the field twice.
- `.truncate()` never ellipsizes; it hard-clips mid-glyph. Use `line_clamp(1)`
  in a fixed-width container, or shorten the content, and verify with a
  screenshot.
- A panic inside a background task re-panics at the foreground `.await`.
  Work that holds a latch runs under `catch_unwind` inside the background
  closure and converts the panic into an ordinary failure.
- Verify framework claims against the pinned source
  (`~/.cargo/registry/src/*/gpui-0.2.2/`), never against web search.

## Debugging playbook

1. `tomte --diagnose` first. It prints paths, versions, connectivity, and the
   tails of both logs — one pasteable report.
2. `~/Library/Application Support/Tomte/logs/{tomte,tomted}.log`: every
   subprocess with duration, exit code and stderr on failure.
3. Reproduce in the failing context, not a simulation: `ps eww` the sick
   process, `sample <pid>` its stack, run `--print-status` / `tomted --once`
   from the user's own terminal. Tests that pass in the agent's shell prove
   nothing about the user's machine.
4. Known field truths: scheduled fetches fail while 1Password is locked (the
   SSH key lives in its agent) — expected and self-explaining in the UI;
   `index.lock` contention between a daemon scan and an app decision is
   retried (6 × 100 ms) inside `GitClient::run`.

## How work is done here

Specs and plans are written before code and committed under
`.agents/artifacts/` (see the layout below). Non-trivial features run as
subagent-driven development: one implementer and one independent reviewer
per task, fix rounds with scoped re-reviews, a final whole-branch review,
then `superpowers:finishing-a-development-branch`. A plan may be amended
mid-flight by rulings from reviews; the plan in the tree reflects them.
Feature branches merge to `main` fast-forward.

### Agent configuration layout

`.agents/` is the canonical tree for everything agent-facing; the
tool-specific directories hold only what each tool requires.

```
.agents/skills/<skill>/     vendored skills, pinned in skills-lock.json (Codex reads here)
.agents/artifacts/          agent-produced documents (below)
.claude/skills/<skill>      relative symlinks into .agents/skills/, plus the local
                            gpui-tomte overlay (GPUI 0.2.2 facts the vendored skill lacks)
CLAUDE.md                   `@AGENTS.md` import and a Claude-only note; nothing else
```

Add a new skill under `.agents/skills/` and symlink it from `.claude/skills/`;
never copy. `.superpowers/` and `shots/` are gitignored scratch.

### Agent artifacts

Anything an agent produces for another agent, or as the record of a
decision, goes under `.agents/artifacts/<kind>/` — never under `docs/`,
which is for humans (`RELEASING.md`, assets). Files are dated, kebab-case:
`YYYY-MM-DD-<topic>.md`.

```
.agents/artifacts/specs/     design specs: brainstormed with the user, then approved
.agents/artifacts/plans/     implementation plans, executed task by task
.agents/artifacts/reports/   analysis reports, audits, investigation write-ups
```

A spec names the plan that implements it and a plan names its spec, by
path. Doc comments cite the v0 spec as `spec §N`. Add a kind directory when
none of the three fits; do not put loose files at the artifacts root.

## Releasing

`docs/RELEASING.md` has the one-time Apple/GitHub setup. Per release:

1. `CHANGELOG.md`: write `## X.Y.Z — YYYY-MM-DD` at the top (a `## Unreleased`
   section during development is renamed here). It becomes the GitHub
   release body verbatim and what the in-app updater shows; it cannot be
   edited after publish.
2. Bump `version` in the workspace `Cargo.toml` (single source of truth),
   `cargo check` to refresh `Cargo.lock`, commit `chore: X.Y.Z`, push `main`.
3. `./scripts/release.sh X.Y.Z` — gates: clean tree, HEAD == origin/main,
   version match, no existing tag, changelog section present, fmt, clippy,
   tests. Then it tags and pushes; the Release workflow builds, signs,
   notarizes and publishes `Tomte-X.Y.Z.{zip,dmg}`.
4. Watch the workflow to completion and verify `gh release view vX.Y.Z`
   before declaring it released. A release was once declared done while its
   workflow had failed.

CI (`ci.yml`, macos-14): fmt, build, clippy `-D warnings`, full tests, with
chezmoi installed and no global git config.

## Current state and open threads

Shipped: 0.1.18 (2026-09-04) — the async action queue. Decisions enqueue
and run in the background, a Processing section replaces any "working…"
button, consecutive one-click decisions share one re-add/commit/push, the
selection auto-advances, failures stick to their row, the landed banner names
the file Undo would revert, finish/roll-back/undo wait behind the latch.
0.1.19 (2026-09-21) added templated keep-disk via the template write-back,
with Undo skipping refused attempts. 0.1.20 (2026-09-21) waits out a busy
daemon for 90 s per request; a batch stops after the first exhausted budget.

Unreleased since 0.1.20 and carried on this branch: multi-select of SAFE
TO RESOLVE rows with bulk one-batch Keep disk / Keep source (the selection
bar is pinned under the sidebar's rows), Esc/⌘A, a virtualized List/Tree
folder sidebar, scans off the daemon lock, nested repositories ignored by
the watcher, a queued decision that can be ✕'d, and the app replacing a
daemon from another build at launch.

On top of that, this branch adds Structure mode to the merge editor for
JSON and JSONC settings files (`2026-10-21-structured-merge-design.md`).
Each changed key or array element is one decision with its own pick; keys
a template renders are locked and follow source; Save splices the picks
into the source text so comments and layout survive, and Edit opens a
Text stage on the assembled file. A JSON-looking file that does not parse opens in
the line editor with a note saying why. Deliberate limits: TOML comes
later, origin is shown only in Lines ("origin differs · see Lines"), and
objects inside array elements are not merged key by key.

Open, deliberately (design calls, not bugs):

- Tray "Sync all" runs `chezmoi update` outside the queue latch; routing it
  as a `PendingRepoOp` needs a Shell handle from a tray handler that only
  holds `&mut App`.
- The dashboard has no processing surface: a quick action pressed with no
  daemon parks with nothing on screen until the notification fires.
- Undo's button cannot name its file: `ui::button` has no tooltip slot.
- `review.rs` is ~4,700 lines; the sidebar planner (`SidebarRow`,
  `SidebarPlan`, `SidebarInputs`, `sidebar_rows`, plus the list's row keys,
  `splice_window` and `re_anchor_index`, and their tests) is cohesive, pure,
  and depends only on `review_selection` and `review_tree` — the next chunk
  to move into its own `review_plan.rs`.
- The per-event path (`rescan_source_side`) and `handle_fetch`'s per-file
  probes still run under the lock — bounded by changed-file counts; give
  them the begin/apply split if one ever shows in a banner.
- A merge-header path wider than about a third of the header still clips
  mid-glyph. The fix is a definite width or an ellipsis inside `crates/ui`,
  checked with its own Review screenshot.
- `merge.rs` grew by about 600 lines with Structure mode. Its
  Structure-mode branches, beside `merge_structured.rs`, are the next
  split after the Review planner.
- Free-text region editing in the merge editor (`tomte_ui::text_area`
  exists; integration remains), keyboard hints, tabular figures (gpui has no
  font-features hook), journal pruning, SMAppService instead of launchctl.
