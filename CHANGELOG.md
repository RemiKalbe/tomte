# Changelog

The `## X.Y.Z` section for a version becomes the GitHub release body (and
what the in-app updater shows) — `scripts/release.sh` refuses to tag a
version without one.

## 0.1.11 — 2026-08-18

### Changed

- Review diff markers: back to the tinted side dots (the 0.1.9 design)
  — the operator experiment in 0.1.10 wasn't it.

## 0.1.10 — 2026-08-18

### Changed

- Third time's the charm for the review diff: `+`/`−` are back meaning
  what they always mean — present/absent — and COLOR carries the side
  (blue = source's line, amber = disk's). Legend reads "− source only ·
  + disk only", color-matched.

## 0.1.9 — 2026-08-17

### Changed

- The review diff stops abusing git's vocabulary: `+`/`−` meant sides
  here (source vs disk), not add/delete — replaced with the app's tinted
  side dots, in the gutter and the legend.

## 0.1.8 — 2026-08-17

Reconcile survives itself.

### Fixed

- **Crash after saving a reconcile resolution.** Advancing to the next
  conflict re-entered the merge editor's own update and hit a GPUI
  reentrancy panic — found via the panic now landing in the log with the
  exact line. The hand-off is unnested.
- **A merge interrupted mid-reconcile now resumes.** Reconcile detects an
  in-progress merge and picks up the remaining conflicts (or concludes
  directly when everything was already resolved) instead of failing
  against the half-merged state.
- The Reconcile button no longer wears accent blue on the red banner.

## 0.1.7 — 2026-08-17

The missing piece: when your dotfiles repo itself diverges, Tomte can
now fix it — in the same merge editor as everything else.

### Added

- **Repo reconciliation.** When this machine and origin both have
  commits the other lacks, every push is doomed until the histories
  meet — previously Tomte could only say "resolve manually". Now the
  dashboard shows a "dotfiles repo diverged" banner with one button:
  Reconcile merges origin, and if source files conflict, each one opens
  in the three-pane merge editor (common ancestor · this machine's
  commits · origin's). Resolve them, and Tomte concludes the merge,
  pushes, and rescans. Cancel aborts the whole merge cleanly. Stray
  uncommitted source edits are committed first, same policy as resolves.

## 0.1.6 — 2026-08-17

Previews stop lying about 1Password, and errors learn some manners.

### Fixed

- **Previews and merge loading now use your selected 1Password account.**
  The 0.1.4 fix covered the resolve engine but three other code paths
  (diff preview, keep-both probe, merge-editor loading) still built
  their own chezmoi clients without `OP_ACCOUNT`. There is now exactly
  ONE factory for app-side chezmoi clients, and a build-failing test
  that catches any future bypass.
- **Failed fetches journaled before a restart no longer masquerade as
  successful "fetch" rows.**

### Changed

- The origin tile shows "fetch failed" instead of a raw git error
  crammed into the header; hover it for the full message. Fetch
  failures also name their likely cause — a locked 1Password holding
  the SSH key, or the network being unreachable — instead of
  "timed out; stderr tail: \"\"".
- The activity feed collapses runs of background noise (scans, fetches,
  fetch failures) into one expandable "N background events" line — no
  more wall of identical rows after a night of retries. The bogus
  "can't evaluate" chip on fetch failures is gone.

## 0.1.5 — 2026-08-10

The sync loop closes: fetch integrates, push recovers.

### Fixed

- **Fetch now integrates, not just observes.** Fetch downloaded origin's
  commits but nothing ever moved the local branch forward — so on a
  machine where origin was ahead, every push after a resolve was doomed
  to a non-fast-forward rejection. When origin is ahead, the local tree
  is clean, and there are no local commits, fetch now fast-forwards
  (lossless by construction). Diverged or dirty repos stay untouched.
- **A rejected push heals itself.** If origin moved between fetch and
  push, Tomte re-fetches, rebases its commit on top, and pushes again.
  If the rebase conflicts it aborts cleanly and says exactly that —
  repo-level divergence is your call, never Tomte's.

## 0.1.4 — 2026-08-10

You can always hand Tomte's homework to someone. Also: the 1Password
account you pick now applies to everything.

### Fixed

- **Resolves/merges now use your selected 1Password account.** Only the
  daemon ever read it; the app — which runs the apply/merge pipeline —
  called chezmoi without `OP_ACCOUNT`, so multi-account machines failed
  applies even after configuring an account.

### Added

- **`tomte --diagnose`**: one command, one pasteable report — versions,
  tool availability, paths, socket and process state, daemon status,
  settings summary, and the tails of every log. This is THE thing to run
  when something misbehaves.
- **Real logs.** Both the app and the daemon write timestamped, rotating
  logs to `~/Library/Application Support/Tomte/logs/` — every chezmoi /
  git / op / curl invocation with duration, exit code, and stderr on
  failure; every resolve-pipeline step; commit/push outcomes; panics.
  The daemon spawn log also stopped truncating itself on every respawn.
- Settings → Paths shows the logs directory.

## 0.1.3 — 2026-08-10

First-run on a fresh machine actually works, and failures explain themselves.

### Fixed

- **Fresh installs never started.** On a machine that had never run Tomte,
  the support directory didn't exist and the very first boot step died on
  a bare "No such file or directory" — forever. The directory is now
  created up front (found on a real second-machine install).
- **Startup failures name their remedy.** If the daemon can't start
  because of configuration — several 1Password accounts with none
  selected, locked 1Password, a missing age identity — the status now
  says exactly that, the dashboard banner gains its "Open Settings"
  button, and the daemon keeps serving that answer instead of exiting
  and respawning in a loop the app could only report as "not connected".
- **"Not connected" explains itself**: what the background watcher is,
  that it restarts automatically, and the last error it hit.

### Changed

- Settings: when several 1Password accounts exist and none is selected,
  the Account row says so — right where the fix is.

## 0.1.2 — 2026-08-09

Tomte gets its real face, everywhere.

### Changed

- **App icon, rendered by Apple's toolchain.** The Icon Composer document
  is now compiled with `actool`: correct proportions and glass treatment
  in the flattened icon, and on macOS 26 the app carries the genuine
  dynamic Liquid Glass icon (`Assets.car`), not an approximation.
- Update checks run every 20 minutes (was daily) — the app is young and
  releases land often.
- Release notes now live in this file and ship with the release the moment
  it publishes; no more window where a fresh release shows only an
  auto-generated compare link.
- The in-app release-notes view renders `**bold**` markers and `-` bullets
  instead of showing them raw.

## 0.1.1 — 2026-08-09

Fixes the first-run experience of a fresh install, plus Tomte gets its face.

### Fixed

- **1Password/chezmoi/git not found in the released app.** Apps launched
  from Finder inherit macOS's minimal PATH — no Homebrew. Tomte now adopts
  your login shell's PATH at startup (the same technique editors use), so
  every tool your terminal can see, Tomte can see.
- The 1Password account picker's error state now has **Retry detection**
  instead of being a dead end.
- Last remnants of the old app name removed from the menubar menu.

### Changed

- **App icon**: the gnome, on a proper macOS squircle.
- **Menubar**: the gnome glyph (template-tinted, light/dark aware) replaces
  the text label; the only text is now `●N` when something needs a decision
  — no more shifting neighbors while scanning.

## 0.1.0 — 2026-08-09

First release. Tomte is a quiet macOS menubar app that keeps your
[chezmoi](https://chezmoi.io)-managed dotfiles from drifting — named after
the Scandinavian household gnome who tends the home while you sleep.

- **See drift the moment it happens**: a background daemon watches the
  chezmoi source repo and every managed file; drift shows up in the
  menubar, classified by what it needs from you.
- **Resolve without leaving the app**: one-click Keep disk / Keep source,
  and a three-pane merge editor (source rendered · last written · on disk)
  with per-region decision strips, region-aware synced scrolling, and
  document-level undo/redo. Template-generated lines are protected from
  write-back and marked with the template source on hover.
- **Trust what it did**: every apply is journaled to SQLite with snapshots —
  per-file history, a global activity timeline, one-click Undo.
- **Stays honest in the background**: scheduled origin fetches with real
  freshness reporting, actionable degraded states (locked 1Password), and a
  daemon built to survive sleep/wake, crashes, stale sockets, version skew.
- **Updates itself**: signed, notarized builds; updates are re-verified
  locally (signature + Team ID) before staging, with release notes shown in
  Settings and a one-click restart.

Requires macOS 13+, Apple silicon, and [chezmoi](https://chezmoi.io) with
an existing source repo.
