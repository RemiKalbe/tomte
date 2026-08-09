# tomte

## Register

product

## Product purpose

A macOS menubar companion for chezmoi that turns dotfile drift into a two-minute review instead of a context-free nvim 3-way merge. It watches three states per file (source dir, disk, git origin), classifies divergence, and lets the user resolve each drift with one click (keep disk / keep source), or a template-aware three-pane merge editor for real conflicts. Every mutation snapshots first and is undoable.

## Users

One primary user archetype: the multi-machine developer (the author is the first user). Fluent in Zed, Linear, Raycast, terminals. Checks this tool in short bursts, a few times a day, when the menubar count changes or after switching machines. Not a dashboard they stare at; a queue they clear. Trust is the currency: the tool touches their shell configs and secrets templates, so every action must say exactly what it will do and what it just did.

## End-user experience (north star)

"Glance, trust, clear the queue, leave." The user should be able to:
- read machine state in under a second (am I in sync? what needs me?),
- act on a drift without fear (provenance visible, undo always one click away),
- never wonder what the app is doing (scanning, degraded, and disconnected are honest, distinct states).

The tool should feel like a native sibling of Zed: quiet, dense, keyboard-friendly, zero decoration that doesn't carry state.

## Tone

Calm and factual. Humanized labels ("modified on disk", not "destination_drift"). No exclamation marks, no praise, no anthropomorphizing. Severity is conveyed by color tokens and position, not by louder copy.

## Anti-references

- Electron-style marketing chrome, hero metrics, onboarding tours.
- GitHub Desktop's roominess: this wants Zed density.
- Raw CLI vocabulary leaking into the UI (class slugs, exit codes, "tomted" internals) except in diagnostics.

## Strategic principles

1. Honest states: scanning is never presented as in-sync; degraded says why and what to do.
2. Everything reversible: no action without a snapshot; undo is a first-class affordance.
3. Provenance before action: what changed, when, on which machine, always one glance away.
4. The style is settled: GitHub light/dark palette, Zed idioms and density. Refinement means polish within that language, never a redesign.
