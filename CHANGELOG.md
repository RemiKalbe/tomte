# Changelog

The `## X.Y.Z` section for a version becomes the GitHub release body (and
what the in-app updater shows) — `scripts/release.sh` refuses to tag a
version without one.

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
