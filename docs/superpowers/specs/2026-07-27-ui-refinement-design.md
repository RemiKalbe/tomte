# UI Refinement Design (post-v0)

**North star:** "Glance, trust, clear the queue, leave." The user reads machine state in under a second, acts without fear, and always knows what the app is doing. Feels like a native sibling of Zed: quiet, dense, keyboard-friendly, zero decoration that doesn't carry state.

**Non-goals:** No restyle. GitHub palette, Zed density, current layout skeleton (sidebar nav, master-detail review, three-pane merge) all stay. No new features beyond affordances for existing ones.

Evidence: gallery + live screenshots in `shots/` (2026-07-27). Every item below was observed in a shot, not imagined.

---

## Phase 1 — Language: one honest vocabulary (copy + tokens only)

The same concept currently has two or three names, and daemon internals leak into the UI. Trust dies here first.

1. **One humanization map, used everywhere.** Review's History pane shows raw `dest_changed`, `remote_advanced`, `applied`, and a `destination_drift` chip while the dashboard says "modified on disk". Create `czui_app::humanize` as the single source: event kinds → "edited on disk", "origin moved ahead", "applied", "left management", "scan"; classes → existing chip labels. Raw slugs never render. (`chezmoid` may appear only in Settings paths and diagnostics.)
2. **Reconcile the three counts.** Tile says "0 need attention" while the same tile's button says "Review 3 →" and the sidebar badge says 3 (live shot). Rule: the tile number = files needing a decision (conflict + eval-failed), relabeled **"need a decision"**; the Review button and sidebar badge = full queue. When the decision count is 0 but queue > 0, tile number renders muted (not red, not alarming) so the two numbers stop looking contradictory.
3. **"waiting for chezmoid…" → "connecting to the sync daemon…"**; settings copy "how often chezmoid fetches origin" → "How often to check origin for changes (5–120 min)".
4. **Copy sweep:** no em dashes ("kept disk version — committed & pushed" → "Kept disk version. Committed and pushed."); sentence case on every button ("keep disk" → "Keep disk"; "Save" stays); "X of Y regions need you" → "Y conflicts, X resolved" (and "No conflicts to resolve" at 0).

## Phase 2 — Row anatomy: make the activity list scannable

The dashboard row is the most-read element in the app.

5. **Fixed time column.** Timestamps ("3m ago", "1d ago") currently rag left; give them a fixed-width right-aligned column so icons and filenames form a clean vertical edge.
6. **One icon language.** The eval-failed row uses a colored emoji (🛑) among thin monochrome glyphs (Δ ↓ ✓ −). Replace all row icons with same-weight glyphs tinted by `class_color`; no emoji anywhere in rows (merge's 🔒 becomes a glyph too, item 17).
7. **Chip next to the fact, actions at the edge.** Rows currently read: name … [Keep disk] [Keep source] [chip]. The chip is the information; buttons are the response. New order: time | icon | name | path | chip …… actions right-aligned.
8. **Quick actions appear on hover** (Zed ghost-until-hover). Four drifted rows currently show eight identical bordered buttons at rest, pure noise. At rest: chip only. On hover/selection: buttons fade in (150 ms opacity). Keyboard focus counts as hover.

## Phase 3 — States: no more voids

Empty, scanning, and disconnected currently render one small centered sentence in ~700 px of blackness, with an "ACTIVITY" header over nothing.

9. **Structured empty block** replacing the centered text, top-aligned under the tiles: glyph + primary line + muted secondary line (+ remedy action when one exists).
   - In sync: ✓(ok) "Everything in sync" / "955 files · last scan 2m ago".
   - Scanning: skeleton rows (3 washed bars animating subtle pulse) under ACTIVITY instead of a sentence; tiles show "–" muted.
   - Disconnected: dot(conflict) "Sync daemon not connected" / "Reconnecting automatically…" — and the in-sync tile's green "–" (dishonest) becomes muted.
10. **Degraded strip** (live shot: 1Password locked): keep the amber wash bar, add the remedy as an inline action when known ("Open Settings") and give it the humanized text from item 1.
11. **Fix the sidebar footer truncation bug** (live shot: "1Password CLI could not a"). Footer becomes one status line, ellipsis truncation with full text on hover (TextTooltip), never a mid-word hard clip; drop the second "origin:" line when it duplicates the tile.

## Phase 4 — Review pane

12. **History becomes columns** (time | glyph | humanized event | machine), class chip only when present; no raw slugs (item 1 covers the map, this covers layout).
13. **Action hierarchy in the header.** Four equal buttons today, and the accent outline sits on "Open in editor", the least consequential action. New: [Keep disk] [Keep source] (bordered pair) · [Open merge editor] (accent when the file is a conflict, since that's its real resolution path) · "Open in editor" demoted to a muted icon-button with tooltip.
14. **File path subtitle** under the filename in the detail header (list shows bare names; provenance principle says the full path is one glance away).
15. **Group labels:** "NEEDS YOU" → "NEEDS A DECISION", "ONE CLICK" → "SAFE TO RESOLVE" (matches item 2 vocabulary). "In sync (951)" stays as a muted terminal line.

## Phase 5 — Merge editor

16. **Label the panes, not the toolbar.** The source/snapshot/disk chips float in the window header while three unlabeled panes sit below; the user maps them by position. Move each label into its pane's top edge (title + muted provenance, e.g. "on disk · edited 2h ago on this mac" when known).
17. **Protected spans:** 🔒 emoji → lock glyph in drift color, tooltip "Protected: this text is written by the template". 
18. **Save states:** disabled until every conflict is decided, with the reason in a tooltip ("2 conflicts left"); after save, existing banner flow unchanged.
19. **Keyboard hints:** the ‹pick one› chips gain visible shortcut hints (1 ours · 2 theirs · 3 base, n next conflict), one muted hint line at the result pane's edge. First keyboard affordance in the app; groundwork for a later pass.

## Phase 6 — Settings

20. **The account picker is four 64 px bordered boxes pretending to be text fields** (worst offender in the app). Replace with compact radio rows: 28 px, radio glyph + label, accent check on selected.
21. **Zed-style sections:** headers outside the boxes, controls in tight groups, consistent vertical rhythm; fetch-interval stepper keeps – / + but the value gets a fixed-width tabular slot so it doesn't shift.
22. **Save affordance:** button disabled when nothing changed; dirty state shows "Save" enabled + muted "unsaved changes" note. (Explicit save stays: it restarts the daemon.)

## Phase 7 — Global chrome

23. Sidebar nav rows tightened toward Zed (~30 px), selected wash + hover distinct; Review badge smaller (matches chip radius).
24. Tabular numbers for tile numerals, counts, and timestamps if gpui 0.2.2 exposes font features; otherwise fixed-width columns (item 5 already covers the worst case).
25. Window minimum size (~840×540) so panes never collapse into overlap.
26. Tile freshness color: "origin: fetched 4m ago" value inherits ok/drift/muted by staleness vs the fetch interval; "never fetched" renders muted with the reconnect/degraded hint nearby, not as a bold alarm headline.

---

## Verification

Every phase: `scripts/shoot.sh` before/after for the touched states (both themes), self-reviewed against this spec; render-smoke tests keep passing; new sub-states (hover-revealed actions, disabled Save, dirty settings) get gallery fixtures so they're capturable. Live check via `scripts/shoot.sh live dashboard` after phases 2–3.

## Out of scope (candidates for the pass after this one)

Full keyboard navigation (j/k queue traversal, cmd-1/2/3 routes), command palette, styled menubar popup, motion beyond the 150 ms fades above, custom app icon.
