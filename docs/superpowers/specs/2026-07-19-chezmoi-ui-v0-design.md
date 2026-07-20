# chezmoi-ui — Design Spec

Date: 2026-07-19
Status: Approved pending final user review
Platform: macOS (v0), GPUI 0.2.2 (pinned), chezmoi ≥ 2.70

## 1. Purpose

A GPUI-based GUI for chezmoi focused on the sync/drift/merge workflow. The core pain it
solves: files managed by chezmoi drift across three places — the file on disk (rewritten
by tools), the local source directory, and the GitHub remote (pushed from other
machines) — and reconciling them today means a context-free nvim 3-way merge with no
answer to "which of these changed, when, and which panes are which?", plus the constant
risk of clobbering templated values.

v0 owns the **full sync loop**: fetch/pull from GitHub → visualize drift across all
states → resolve visually (3-way merge with template protection) → write back to source
→ apply to targets → commit & push. Later versions may grow toward broader chezmoi
command coverage; v0 does not.

## 2. Product decisions (settled during brainstorming)

| Decision | Choice |
|---|---|
| v0 scope | Full sync loop incl. git fetch/merge/commit/push |
| Drift history | Always-running watcher daemon + drift journal + notifications |
| Platform | macOS only for v0; platform-specific code isolated for later Linux |
| Templates | Protected spans + verified auto write-back; assisted manual fallback |
| Git conflicts | Unified resolution queue (one decision per file, staged under the hood) |
| Architecture | Split: headless watcher daemon + GPUI app (IPC over unix socket) |
| Merge editor | Three panes (theirs / base / ours) over an editable result pane |
| Main window | Dashboard home (chronological actionable timeline) → Review shell → full-window merge editor |
| Menubar popup | Status glance only; resolution happens in the app, one surface ever |
| One-click actions | Default for one-sided drift, but merge editor is always available per item |

## 3. Architecture

Cargo workspace, two binaries shipped in one `Chezmoi UI.app` bundle.

### 3.1 `chezmoid` — watcher daemon (launchd LaunchAgent)

Strictly an **observer**: it never mutates managed files, the source repo (beyond
`git fetch`), or chezmoi state.

- Watches (FSEvents via `notify`): all targets from `chezmoi managed
  --path-style=absolute`, the source worktree, and the source repo's git refs
  (HEAD + branch refs). Debounce ~500ms with burst coalescing.
- **Managed-set reconciliation**: whenever the source side changes (covers
  `chezmoi forget`, deletion from source, `.chezmoiignore` additions) and on every
  periodic rescan, re-run `chezmoi managed` and diff against the active watch set.
  Departed entries: drop watches, journal a `left_management` event, dismiss pending
  queue items. New entries: add watches immediately.
- Ignores: chezmoi cache dir, our blob store, git internals other than refs, and
  changes pre-announced by the app (see 3.3) with a timeout so a crashed app cannot
  blind the daemon.
- On event: hash file → compare with journal state + chezmoi last-written state →
  targeted `chezmoi status <target>` (never full-scan per event) → classify → journal.
- Remote polling: `git fetch` every N minutes (default 15, configurable); afterwards
  classify per-file what origin has that local doesn't.
- Full rescan: on start, wake-from-sleep, and hourly (FSEvents can drop events).
- Subprocess discipline: every chezmoi/git/op invocation is non-interactive, with
  timeout and captured stderr (see §8, §9).

### 3.2 `Chezmoi UI` — GPUI app (menubar-resident)

- NSStatusItem built directly with `objc2` (gpui 0.2.2 has no tray API — verified
  against crate source; `src/platform/mac/status_item.rs` in the crate is dead legacy
  code, useful only as a reference). The activation policy is flipped to *Accessory*
  after launch (gpui hard-sets Regular at `platform.rs:1390`), so no Dock icon.
  Fallback if NSStatusItem-in-GPUI proves unworkable: resident window-less app,
  global hotkey + notifications (loses only the icon).
- All mutations happen here, user-present: merge write-backs, per-target
  `chezmoi apply`, `git` merge/commit/push, re-add.
- Pre-announces expected changes to the daemon before mutating.
- **Never blocks the UI on subprocesses:** chezmoi/git can be slow, so no
  `CommandRunner` call may ever run on the GPUI main thread. All app-side
  subprocess work (per-file `cat`, `apply`, `execute-template`, git operations)
  is dispatched to GPUI's background executor / worker threads with the standard
  per-command timeout; results return to entities as events on the main thread,
  and in-flight operations render as progress states, never frozen frames. Bulk
  work (scans, watching, fetch) already lives in the daemon process and cannot
  block the app by construction.
- Owns notifications (UNUserNotificationCenter via objc2); the app autostarts at
  login, so it is the notification surface. Daemon emits events only.

### 3.3 IPC

Unix domain socket, newline-delimited JSON messages with request ids (`proto` crate,
serde types):

- Daemon → app: event stream (drift, remote_advanced, eval_failed, left_management, …).
- App → daemon: queries, `expect_changes(paths, ttl)`, `pause/resume`, `rescan`, and
  journaling commands — `snapshot_blobs(paths)`, `record_session_start/decision/end` —
  so the daemon remains the journal's sole writer while sessions are app-driven.
- Version handshake on connect; on mismatch the app restarts the daemon via
  SMAppService (which also installs/updates the LaunchAgent).
- The app reads the journal SQLite directly **read-only** for history/timeline;
  the daemon is the only writer (single-writer WAL).

### 3.4 Crates

```
crates/
  core/      # domain types, chezmoi+git CLI wrappers, merge engine, template mapper (pure, no I/O in engine code)
  journal/   # SQLite schema, content-addressed zstd blob store, GC
  proto/     # IPC message types
  daemon/    # chezmoid binary
  app/       # GPUI app binary
bundle/      # .app packaging, LaunchAgent plist template, icons
```

## 4. Domain model

Per managed file, four content states plus one anchor:

| State | Source of truth |
|---|---|
| Remote source | `origin/<branch>` blob of the source file |
| Local source | worktree file under `~/.local/share/chezmoi` |
| Rendered target | `chezmoi cat` output (what apply would write) |
| Destination | actual file at the target path |

Anchor: chezmoi's **last-written state** (`chezmoi state dump --format=json`).
Destination ≠ last-written ⇒ disk side drifted. Rendered ≠ last-written ⇒ source side
moved. Git adds the remote dimension.

Classification enum: `InSync`, `DestinationDrift`, `SourceAhead`, `RemoteAhead`,
`LocalSourceDiverged` (git-level conflict brewing), `Conflict` (combinations),
`EvalFailed` (template/secret/decrypt errors — a first-class state carrying the
captured error and a remediation hint, never a silent gap).

Entry-type scope v0: files, symlinks, dirs. Scripts (`run_*`) shown in status ("will
run on apply") but never merged. Encrypted files merge only when decryptable.
Externals read-only. `modify_` files: assisted manual mode only. Unmanaged files out
of scope.

## 5. Resolution queue

One queue item per file needing a decision — never per git operation. Ordered by
severity (conflicts first), each item shows classification, a provenance timeline from
the journal (FSEvents timestamps for disk edits; commit author/date for source
changes — pane labels like "GitHub · machine B · 2d ago" vs "this Mac · edited 3h
ago"), and actions:

- One-sided drift defaults to one-click: `DestinationDrift` → "keep disk (re-add)" /
  "restore chezmoi's version (apply)" with diff preview; clean `RemoteAhead` →
  "pull & apply".
- **Every item, regardless of classification, can open the merge editor** for manual
  hunk-level inspection and picking.
- A file with both a git conflict and destination drift is ONE item with two forced
  steps in the editor: step 1 reconcile sources (base = git merge-base), step 2
  reconcile disk edits vs the now-merged rendered target (base = last-written).

Engine staging under the hood (all executed by the app; the daemon only observes):
fetch → git merge (step-1 decisions) → commit →
chezmoi-level 3-way merges (template protection) → write-backs + per-target
`chezmoi apply` → commit write-backs → push. A completed session ends with all four
states equal. Sessions journal every decision (audit + undo).

## 6. Merge engine & template span mapping

### 6.1 Merge engine (`core`, pure)

No conflict-marker text; the GUI needs structure. Compute base→ours and base→theirs
diffs (`imara-diff`), align hunks by base ranges, produce a `MergeDocument`: ordered
regions of `Unchanged | OursOnly | TheirsOnly | BothSame | Conflict`. Everything but
`Conflict` pre-resolves; UI reports "N of M regions need you". Same structure for both
merge kinds (git-level and chezmoi-level). Word-level intra-region highlighting.
Normalization toggles: whitespace-only, trailing newline.

### 6.2 Template span mapping — a verified heuristic, never trusted blindly

1. **Lex** the `.tmpl` (Go-template lexer: delimiters, `{{-`/`-}}` trim markers,
   comments, strings inside actions). Output: alternating literal/action segments;
   actions classified value-producing vs control-flow.
2. **Anchor** literal segments in order in the rendered output; gaps between anchors
   are action output → **protected spans** (locked in the UI, originating expression
   on hover).
3. **Control flow** (`if`/`range`): anchor what's actually present; ambiguous
   alignment ⇒ whole region unmapped ⇒ conservatively protected.
4. **Write-back**: resolutions touching only unprotected bytes map 1:1 to literal
   segment edits in the `.tmpl`. Then **verify**: re-render via
   `chezmoi execute-template` with real machine data; output must byte-equal the
   resolved text. Mismatch ⇒ revert, drop to assisted manual.
5. **Assisted manual mode** (protected-span edits, `modify_` files, unmapped regions):
   split view — `.tmpl` left, live re-render diff vs intended output right; edit until
   clean.

### 6.3 Safety rails

Every resolution session snapshots all files it will touch into the blob store first.
One-click **"undo last resolution session"**: source restored via git, destinations
restored from blobs. Mutation order (snapshot → write source → apply → commit → push)
guarantees interruption never strands an unrecoverable state.

## 7. UI

### 7.1 Home — dashboard

Health tiles (needs-attention count, origin commits behind/ahead + freshness, in-sync
count) above **one chronological activity timeline** (newest first) mixing:

- Actionable entries (drift, conflicts, remote pushes) carrying inline quick actions
  (Merge… / keep disk / keep source / pull & apply).
- Informational entries (applies, resolutions, own pushes) as history.

"Review →" opens the triage shell.

### 7.2 Review shell

Mail-app shape: left sidebar = severity-grouped queue (Needs you / One click / In sync
collapsed); right pane = selected file's provenance timeline, diff preview, actions.

### 7.3 Merge editor

Full-window takeover. Three panes — theirs / base / ours, each with provenance chips —
over an editable **Result** pane. Region controls: take theirs / take ours / edit;
auto-resolved regions pre-filled; protected template spans rendered locked (amber, 🔒,
expression on hover). Two-step flow header when a file needs source-merge then
disk-merge. Footer: "N of M regions need you", next-conflict navigation.

### 7.4 Menubar

Status item icon = state (calm / amber dot drift / red dot conflict) + count. Popup =
**status glance only**: counts, last-fetch/last-apply freshness, and:

- **Sync all** — enabled *only* when zero decisions are needed; performs
  fetch + apply + push of clean state.
- Otherwise the button reads "Review N items ↗" and opens the app.

One resolution surface, ever. No merging from the popup.

### 7.5 Visual design

Zed-inspired design language — not a copy, an idiom: compact density, flat surfaces,
hairline borders, muted chrome, restrained type scale, monospace where content is
code. Rationale is speed as much as taste: Zed is the canonical large GPUI codebase,
so when a component pattern is needed (lists, split panes, tab bars, overlays), the
answer is in `zed-industries/zed` source rather than invented — the fastest path to
v0.

Color: light **and** dark themes from day one, following system appearance, with a
palette inspired by Zed's GitHub theme (light + dark variants). Theme values live in
a single token module in `app` (semantic roles — surface, border, accent, drift-amber,
conflict-red, ok-green — not raw hex scattered through views).

During planning, evaluate `gpui-component` (longbridge) as a component base vs
hand-rolled Zed-style components; adopt only if it pays for itself.

### 7.6 Notifications

On new drift discovery (coalesced per burst: "3 files drifted after starship update")
and on remote pushes from other machines. Never for self-caused changes; no repeat
reminders. Click → app opens on Review.

## 8. Journal

SQLite (WAL, single writer = daemon; the app journals sessions and pre-session blob
snapshots through the IPC commands in §3.3) + content-addressed zstd blob store.

- `entries(id, target_path, source_path, kind, managed, unmanaged_at)`
- `events(id, entry_id, ts, machine, kind, from_hash, to_hash, meta)` — kinds:
  `dest_changed, source_changed, remote_advanced, applied, readded, resolved,
  eval_failed, fetch, left_management, session_start, session_end`
- `blobs(hash, content_zstd)`
- `sessions(id, started, finished, summary, decisions_json)`

Retention: keep-everything by default with a size cap (~500 MB) and GC of
unreferenced blobs; dotfiles are tiny.

## 9. Settings & secrets context

App settings live in a shared config file (`~/Library/Application
Support/ChezmoiUI/settings.toml`) read by both app and daemon. No secrets stored —
only non-sensitive context like account identifiers.

**1Password (required for v0):** a "secrets context" settings section where the user
selects the 1Password account (and default vault where applicable), enumerated via
`op account list` / `op vault list` — a real picker, not free text. Every chezmoi/op
subprocess in both processes runs non-interactively with `OP_ACCOUNT` (and vault
argument where applicable) injected. If an `EvalFailed` matches the
"multiple accounts found" error and no account is configured, the **app** prompts
with the picker + "remember this choice" (saved to settings). The **daemon** never
prompts — it journals `EvalFailed` until the choice exists. No subprocess may ever
block on a hidden interactive prompt.

Other settings: fetch interval, notification toggles, retention cap, source repo
path override.

## 10. Error handling

- Every subprocess: timeout, captured stderr, structured classification. Secret /
  template / decrypt failures ⇒ `EvalFailed` + remediation hint (e.g. "select a
  1Password account in Settings").
- Network failure on fetch/push ⇒ remote column explicitly stale ("origin as of 2h
  ago"), never silently wrong.
- Missing/too-old chezmoi ⇒ onboarding screen.
- Daemon crash: launchd KeepAlive restart, WAL-safe journal, full rescan reconciles.
- App crash mid-session: sessions journal step-by-step; next launch offers
  resume-or-rollback.
- File changes underneath an open merge session (watcher event + pre-write hash
  check) ⇒ session pauses: "this file changed while you were merging".
- IPC protocol mismatch ⇒ app restarts daemon via SMAppService.

## 11. Testing

- `core` (pure): golden-file tests for merge regions; real-world template corpus
  (nushell, gitconfig, …) for lexer + anchoring; property tests for the round-trip
  invariant *render → anchor → identity-writeback → re-render == original* — the same
  invariant enforced at runtime on every write-back.
- `journal`: in-memory SQLite tests, GC tests.
- `daemon`: integration tests against a scratch chezmoi home with the real chezmoi
  binary; socket protocol tests.
- **E2E drift stories**: temp `$HOME` + bare "GitHub" repo + two simulated machines
  replaying the real-world scenario (machine B pushes; machine A has disk drift +
  local source edits); assert all four states converge and the journal audit is
  correct.
- `app`: GPUI test framework for queue/editor interaction; merge document model lives
  in `core` so the GUI layer stays thin.

## 12. Dependencies & pins

- gpui = 0.2.2 exact (pre-1.0, breaking changes between versions; local rustdoc
  generated from the pinned version is the API reference — the bundled gpui skill is
  ~6 months stale and must not be trusted over the crate source).
- Build requirement (discovered 2026-07-20): gpui's build script compiles Metal
  shaders, which requires full Xcode **plus the Metal Toolchain component**
  (`xcodebuild -downloadComponent MetalToolchain`) — Xcode 26 ships without it.
- imara-diff (merge engine), notify (FSEvents), rusqlite (journal), zstd, objc2 +
  objc2-app-kit (status item, activation policy, notifications), serde.
- chezmoi ≥ 2.70 and user's own `git` binary invoked as CLIs (SSH keys, signing,
  hooks all just work). chezmoi is the single source of truth for chezmoi semantics —
  no reimplementation of templating/encryption/ignore logic beyond the lexer in §6.2.

## 13. Non-goals (v0)

Linux/Windows GUI; script (`run_*`) merging; chezmoi command palette; secrets
management UI; auto-resolution beyond the zero-decision "Sync all"; multi-repo;
editing arbitrary chezmoi config from the app.

## 14. Risks & mitigations

| Risk | Mitigation |
|---|---|
| NSStatusItem inside GPUI's run loop misbehaves | objc2 direct integration; fallback: window-less resident app + hotkey + notifications |
| Template span anchoring ambiguity | Conservative protection + mandatory re-render verification; assisted manual fallback |
| chezmoi CLI output drift across versions | Pin minimum version; `--format=json` where available; integration tests against the real binary |
| FSEvents dropped events | Hourly full rescan + rescan on start/wake |
| gpui pre-1.0 churn | Exact pin + local rustdoc; upgrade deliberately |
