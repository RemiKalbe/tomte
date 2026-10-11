# Changelog

The `## X.Y.Z` section for a version becomes the GitHub release body (and
what the in-app updater shows) — `scripts/release.sh` refuses to tag a
version without one.

## Unreleased

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
- **Scans cost less.** One `chezmoi source-path` call for all candidates
  and two `git log` walks per scan replace three subprocesses per file.
- **Review stays smooth with hundreds of files.** The sidebar renders only
  the rows in view and rebuilds its plan only when something changed; with
  500 drifted files the tree used to lag on every mouse move. After a
  decision the next file scrolls into view instead of advancing
  off-screen.

### Added

- **Remove a queued decision.** A ✕ on every PROCESSING row that has not
  started yet takes it back; the file returns to its group. A running
  decision cannot be recalled mid-commit, and shows no ✕.
- **Resolve several safe files at once.** In Review, ⌘-click or tick the
  checkbox on any SAFE TO RESOLVE row (⇧-click for a range, the header's
  checkbox for all of them), then press Keep disk or Keep source · this
  machine once. The whole selection is one batch: one re-add or apply, one
  commit, one push, still one journal session per file so Undo stays per
  file. Esc clears, ⌘A selects all. Rows that need a decision cannot be
  ticked; they keep their own buttons.
- **A folder tree for the safe group.** The folder-tree toggle on the
  SAFE TO RESOLVE header shows that group as folders under `~`, single-child
  chains folded (`.config/nvim/`). Ticking a folder ticks every safe file
  under it, collapsed or not; its chip says `3 of 5` when two of its files
  need a decision and stay out. The other two groups stay flat. The choice
  and the collapse state last for the session.

## 0.1.20 — 2026-09-21

### Fixed

- **A busy daemon no longer fails a decision you already made.** While a scan
  (or a fetch) holds the daemon, a request that waits past the daemon's own
  bound is bounced with "daemon busy" — and that bounce used to land on the
  row as the decision's failure, so a scan that ran a few seconds long could
  fail a file you had just confirmed. The bounce executes nothing, so it is
  now retried, each retry logged, for up to a minute and a half; only a
  daemon that stays busy that long is reported, in those words. The same
  holds for the dashboard's Fetch now, and a daemon that stays busy through
  a whole batch is reported once, not once per file after a full wait each.

## 0.1.19 — 2026-09-21

### Fixed

- **Keep disk works on templated files.** A file whose source is a `.tmpl`
  answered every "Keep disk" with "templated · use the merge editor", so a
  program rewriting its own config sent you into the editor to click "disk"
  on each region by hand. Keep disk now does what those clicks do: the
  on-disk text is written back into the template's literal segments,
  verified by re-rendering, then committed and pushed — in the same batch
  and the same commit as the plain files around it. What is refused now is
  only what the template's literal text cannot express — a disk edit inside
  a templated value (an expression's rendered text), or one with no literal
  home to land in — and the row says which; the merge editor remains the
  place to decide those. Keep source never needed the editor for a template
  and still does not.
- **Undo reverts the last thing that landed.** A write-back the app refused
  left a finished journal entry behind, and Undo, which took the newest
  entry, would "revert" that no-op instead of the last real change —
  shadowing it. Refused attempts are now skipped, so Undo always names and
  reverts the file the banner shows.

## 0.1.18 — 2026-09-04

### Changed

- **Confirming a file no longer blocks every other file.** A decision — a
  quick action or a merge-editor save — is recorded the instant you make it
  and runs in the background. In Review its row moves to **Processing** and
  the selection advances to the next file that still needs an answer, and you
  carry on. Until now one confirmation disabled every other row's buttons,
  and the clicks you made while you waited were discarded rather than
  remembered.
- **Decisions made while one is running share a single re-add, commit and
  push.** The slow part of a decision is per invocation, not per file:
  `chezmoi re-add` around 2.5 s and `git push` around 1.5 s, however long
  the list. So decisions that pile up behind a running one drain together
  and pay that tail once. The first click of a sweep still runs on its own —
  there is nothing to join it to yet — so answering thirty files costs two
  of those tails rather than thirty. Undo stays per file: each decision
  keeps its own journal session and reverts on its own.
- **A merge finish, a roll-back or an Undo pressed while decisions are
  draining waits its turn.** All three rewrite the very repository those
  decisions are writing, so they run between batches, never beside one. You
  still leave the merge editor the moment you press it, and the decisions
  queued behind a roll-back run against the rolled-back repo. Undo stays
  visible but disabled while the queue owes work, and says it is finishing
  queued decisions first, rather than reverting a file a decision is still
  on its way to. Discarding a merge also throws away the merge-editor saves
  still queued for it, and says how many went with it.
- **A decision made while the daemon is away is remembered, not refused.** All
  three surfaces used to turn the click down when nothing was connected: a
  review row's buttons and the dashboard's quick actions went dead with
  "daemon not connected", and the merge editor's Save answered "Daemon not
  connected · cannot save" over work you had already done. The decision is
  now recorded, the row reads as processing, and it runs on reconnect —
  including the finish, roll-back or Undo you pressed in the meantime.

### Fixed

- **A failed decision stays on its row, with its message.** The queue moves
  on within milliseconds of an error, so a banner would have vanished before
  you looked up — which is how the 1Password flake became "random". The row
  keeps the message in place, its buttons re-enable, and choosing again
  retries.
- **A roll-back that failed no longer reports success.** Git can refuse to
  abort a merge — an `index.lock` held past its retries, most often — and
  the banner said "Merge with origin rolled back" over a merge that was
  still sitting there with your answers staged in it. A refused abort now
  says so.
- **A 1Password hiccup no longer fails the decision outright.** A templated
  file that reads a secret through `onepasswordRead` failed the whole
  decision when `op` was slow to answer, and retrying by hand worked — so
  the failure was never really the answer. Those are now retried a bounded
  number of times, each retry logged. Having more than one 1Password
  account configured is not transient, and is still reported at once.
- **The banner above Undo always names the file Undo would revert.** Undo
  reverts the last file that landed, which after a sweep is not necessarily
  the one you were last looking at.

## 0.1.17 — 2026-09-01

### Fixed

- **Resolved files disappear from the queue immediately.** They already did —
  and then came straight back. The list is rebuilt from the daemon's
  snapshot on every status poll, and the daemon only catches up after a full
  rescan, which walks every managed file; a single templated file that reads
  from 1Password can take several seconds on its own. So for ten or twenty
  seconds the queue kept showing work you had already finished. Files you
  resolve are now held aside until a scan has actually had a chance to see
  them, with the in-sync tally kept consistent with the list beside it. If a
  scan runs and still reports the file, it comes back — a resolution that
  did not take should not be hidden.

## 0.1.16 — 2026-09-01

A fix for 0.1.15: resolving a file while your dotfiles repo was behind origin
failed with a raw git push rejection, and left the repo diverged so the next
file failed too.

### Fixed

- **Resolving a file no longer fails when the repo is behind origin.** 0.1.15
  asked whether the file you were resolving happened to be one of origin's
  conflicts, and skipped catching up if it wasn't — then ran a chezmoi
  command anyway. With `autoCommit`/`autoPush` set, chezmoi commits and
  pushes from inside that command, and pushing while behind is rejected —
  after the local commit was already made, which turned "behind" into
  "diverged" and broke every later decision too. Whose file conflicts has no
  bearing on whether the repo needs origin's commits, so the question is
  gone: if Tomte is behind, it integrates first.
- **"Keep source" no longer leaves you silently behind.** It never failed —
  applying doesn't change the source, so there was nothing to push — but it
  never caught up either, so origin's changes never arrived and the next
  decision that did need to push hit the same wall.
- **Answering one file never waits on another.** Files are resolved
  independently, in any order, with no error and no banner. A decision made
  while others are outstanding reports "recorded — N more file(s) still
  differ from origin" instead of failing. Only the eventual push is
  collective, because git's unit of push is a commit.

### Internal

- The test fixtures wrote an empty chezmoi config, so `autoCommit`/`autoPush`
  were off in every test and the suite could not see what chezmoi does on a
  machine that has them on. They can be switched on now, and the failure
  above is pinned by a test that reproduces it exactly.

## 0.1.15 — 2026-08-31

Your dotfiles repo has a remote, and until now Tomte pretended it didn't.
"Source" was really two things — what's on this machine, and what's on
origin — and when they disagreed you got a red banner and a separate flow
to deal with it. Origin is now just another version of the file, visible
and pickable wherever the local one already was.

### Added

- **Origin, everywhere you resolve.** The review list marks the files
  origin has its own version of. "Keep source" splits into **this machine**
  and **origin** when the two differ, and stays a single "Keep source"
  when they agree. The diff gains a selector for which pair you're
  comparing — disk against either source, or the two sources against each
  other.
- **A fourth pane in the merge editor.** When origin has its own take on a
  file, it gets a column beside the others, in its own colour, with an
  **origin** button on every region it differs on. Picking it is recorded
  as origin's text — not as a hand edit — so the result rows say where the
  content came from.

### Changed

- **The "dotfiles repo diverged from origin" banner is gone.** Divergence
  isn't an event to announce any more, it's a property of the files it
  affects. When the histories can be joined safely Tomte does it in the
  background; when they genuinely conflict, those files show up in the
  review queue like any other change, and answering them drives the real
  git merge to completion. There's no separate reconcile flow to enter.

### Fixed

- **"0 in sync".** One unreadable path — a managed directory, say — used to
  abort the entire scan, freezing the count at zero indefinitely. A file
  that can't be checked is now recorded and skipped, and never counted as
  fine.
- **Panes falling off the window.** The merge editor's columns kept their
  pixel widths when the window changed size, so shrinking it collapsed one
  column and pushed the rest off screen. They now follow the window,
  keeping the proportions you dragged and never letting a pane disappear.
  Same for the split between the panes and the result.
- **A locked signing agent no longer stalls the app.** Joining histories in
  the background writes a commit, which needs your signing key. With
  1Password locked that used to block the daemon for a minute at a time —
  and leave the repo mid-merge. Tomte now asks the signer first, skips the
  join when it can't answer, and picks it up on the next check. Commits
  stay signed.

## 0.1.14 — 2026-08-27

The merge editor became a real editor, every code surface became selectable
and scrollable, and the buttons stopped looking like 2009.

### Added

- **Select and copy text anywhere.** The three merge panes, the merge
  result, and the review diff are all real editors now (read-only where
  they should be), so you can select a line and copy it instead of
  retyping it.
- **Horizontal scrolling** in every code surface. Long lines no longer
  vanish past the right edge with no way to reach them.
- **Draggable panels.** Every structural divider is a grab handle: the two
  dividers between the merge editor's three panes, the split between the
  panes and the result, the review file queue, and the review History
  section. Each keeps a sane minimum so nothing collapses out of reach.
- **A changelog window.** Settings → Updates has a Changelog button that
  opens the whole document in its own window, rendered as markdown —
  headings, nested lists, code, quotes, links. It reads the changelog
  compiled into the app, so it works offline. When an update is staged the
  button becomes "What's new" and shows that release's notes instead.
- **Template provenance on hover** in the merge editor: the `{}` marker
  says a line is generated, and hovering says by which template
  expression — now in the result pane as well as the source pane.

### Changed

- **The merge result IS the editor.** It used to be a read-only list that
  a separate editor replaced when you pressed "edit". Now there is one
  surface: read-only by default with the decision strips riding inside it,
  and "edit" flips that same surface editable in place. Nothing swaps,
  nothing jumps — toggling edit keeps the line you were looking at exactly
  where it was.
- **Decision strips** are inset cards (after Zed's multibuffer headers): a
  state dot and label on the left, actions on the trailing edge, a hairline
  border that turns red on the conflict you owe a decision on. A region
  that needed no decision now reads "auto-resolved" rather than
  "auto-merged" — nothing was merged, it was resolved for you.
- **Buttons, everywhere.** Hard full-strength 1px borders are gone. The
  variant is now chosen by the surface a button sits on: a tonal field on
  the plain background, a faint same-tint ring inside a bordered container,
  a solid fill on a tinted banner, and quiet for tertiary actions. Neutral
  actions are muted rather than foreground-strength — "Open merge editor"
  in particular was shouting.
- **The review diff** uses the merge editor's vocabulary: each changed row
  sits on its side's tinted field with a rail down its edge, context is
  dimmed, and the side dots are gone (the field and rail already say which
  side a row is). The footer legend now shows miniatures of the row
  treatment instead of dots.
- **Review header actions** carry their side's colour — Keep disk amber,
  Keep source blue — while "Open merge editor" is deliberately uncoloured,
  since it doesn't act on a side.
- **Tomte is a regular macOS app.** It was configured as a menu-bar
  accessory, which is why it never appeared in ⌘-Tab or the Dock — and why
  a full menu bar could hide the status icon and leave no way in at all.
  It now appears in both; the status item stays, it just isn't the only
  door. Clicking the Dock icon brings the window back, and the menu items
  raise the existing window instead of opening another one.
- Settings no longer prints a truncated slab of release notes with "… full
  notes on the GitHub release" underneath. Nothing shows by default; the
  button opens the real thing.

### Fixed

- Horizontal scrolling did not work at all — the editor always asked for
  exactly its container's width, so there was never anything to scroll.
- Decision strips and state pills drifted thousands of pixels off-screen
  once a document was wider than the pane; both now pin to the visible
  area.
- The "auto · disk" style pills were translucent, so code showed through
  and the label was unreadable.
- Strip buttons sat flush against the card's border with no breathing room.
- The merge editor's text fell back to the UI font instead of Menlo.
- Scroll sync between the input panes and the result stopped working when
  the result became an editor; it is rebuilt through region geometry, which
  the decision-strip blocks made necessary.
- The border between the sidebar and the content had doubled in width.
- The changelog window could not be scrolled, carried the app sidebar, and
  printed its title twice.

## 0.1.13 — 2026-08-20

### Changed

- Editing in the merge editor is now whole-file, not per-region
  (yesterday's version was too narrow): "edit" — on any strip, or the
  new header button — opens the ENTIRE result as one editable
  document, cursor on the region you came from. Edit any line
  anywhere. On Apply your changes are mapped back to the regions they
  touched, so the decision strips, revisiting, and per-step undo keep
  working — and a conflict you didn't touch still asks for its
  decision. One ⌘Z undoes the whole apply.
- Save is disabled while an edit is open ("apply or cancel the edit
  first") so it can never silently ignore text you're still typing.

## 0.1.12 — 2026-08-19

### Added

- The merge editor's "edit" button now does something: it opens a real
  text editor for that region, seeded with the region's current
  resolution (an undecided conflict starts as disk-then-source so all
  the material is in front of you). Type freely — cursor keys,
  selection, mouse, copy/paste, undo/redo (⌘Z, grouped like a real
  editor), and IME/accent input all work. Esc cancels, ⌘↵ or Apply
  commits the text as that region's decision; one ⌘Z at the document
  level undoes the whole edit.
- Regions containing template-generated text say so in the editor
  header — the save-time verification still has the final word.

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
