# UI Refinement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans. Executed inline (not subagent-per-task): every task's acceptance test is visual — `scripts/shoot.sh` before/after read by the executor.

**Goal:** Execute the approved refinement spec (docs/superpowers/specs/2026-07-27-ui-refinement-design.md) — same style, more craft.

**Architecture:** No new crates, no new views. Copy/label centralization in `czui_app::model` (map already exists there), layout surgery inside the four existing view files, new gallery fixtures for new sub-states.

**Tech stack:** GPUI 0.2.2, existing Theme tokens only.

## Global constraints

- GitHub palette + Zed density; `Theme` tokens only, no raw hex in views.
- No em dashes anywhere in UI copy; buttons sentence case.
- Raw class/kind slugs and the name "chezmoid" never render (Settings paths + diagnostics excepted).
- Every task: `cargo test --workspace` green; shoot affected states dark+light; read the PNGs before calling it done; commit per task.
- Spec item numbers (1–26) refer to the spec document.

---

### Task 1: One vocabulary (spec 1, 3, 4, 7-copy, 23-copy)

**Files:** `crates/app/src/views/review.rs`, `merge.rs`, `dashboard.rs` (empty-state strings), `settings.rs` (interval copy), `model.rs` (add `kind_glyph` case fix: ⛔ → glyph, see Task 3), tests inline.

- `review.rs` `provenance_row`: render `kind_label(&row.kind)` and `class_label(class)` chips instead of raw `row.kind`/class (mod-level import from `czui_app::model`).
- Banner copy: `format!("{} — committed & pushed", verb)` → `format!("{}. Committed and pushed.", verb)` with verbs "Kept disk version"/"Kept source version"; merge.rs `"merged — committed & pushed"` → `"Merged. Committed and pushed."`. Update the two assertions at review.rs:1385/1402 and merge.rs:1081, plus any other `—` in banner/error format strings (grep `—` in crates/app/src).
- merge.rs:504 progress: `{open} of {total} regions need you` → `match`: 0 total → "No conflicts to resolve"; else `"{total} conflicts, {resolved} resolved"`.
- Button labels sentence case: "keep disk" → "Keep disk", "keep source" → "Keep source" (dashboard + review), "next ↓" → "Next ↓" if lowercase.
- Dashboard disconnected text "waiting for chezmoid…" → "connecting to the sync daemon…"; settings interval help → "How often to check origin for changes (5–120 min, steps of 5)".
- Verify: `cargo test --workspace`; shoot `review-banner`, `merge`, `dashboard-disconnected`, `settings` dark; read. Commit `refine: one humanized vocabulary across all views`.

### Task 2: Count reconciliation + tile honesty (spec 2, 26)

**Files:** `crates/app/src/views/dashboard.rs`, `model.rs` (if the severe-count helper needs extraction), fixtures.

- Tile 1 label "need attention" → "need a decision"; number red only when > 0, `text_muted` when 0. Confirm tile counts conflict+eval classes (live shot implies it already does).
- Disconnected/unknown tiles: the in-sync "–" renders `text_muted`, never `ok` green.
- Origin tile: value color by staleness (≤2× fetch interval → text, > → drift, never/None → muted); "never fetched" stays normal weight.
- Verify: shoot `dashboard`, `dashboard-empty`, `dashboard-disconnected` both themes. Commit `refine: reconcile decision count vs review queue; honest tile colors`.

### Task 3: Row anatomy (spec 5, 6, 7, 8)

**Files:** `crates/app/src/views/dashboard.rs`, `model.rs` (`kind_glyph`: ⛔ → "⊘"), fixtures (hover can't be posed; add none), render-smoke untouched.

- Fixed time column: `w(px(56.))`, right-aligned muted, row layout `[time][glyph w_4 centered][name+path flex_1][chip][actions]`.
- Glyphs uniform: all from `kind_glyph`, tinted `class_color` (⊘ for eval_failed replaces emoji ⛔; check dashboard renders glyph not literal).
- Chip moves next to name/path block; quick actions right-aligned, `opacity_0` at rest, full on `group_hover` (gpui: `.group("row")` + `.group_hover("row", |s| s.opacity(1.))`) — verify gpui 0.2.2 API name in local source before using; fall back to `on_hover` state toggle if absent.
- Verify: shoot `dashboard` both themes; hover state manually via live app later (untestable in static shot — note in commit). Commit `refine: dashboard row anatomy (fixed columns, uniform glyphs, hover actions)`.

### Task 4: States (spec 9, 10, 11)

**Files:** `crates/app/src/views/dashboard.rs`, `mod.rs` (footer), fixtures (`dashboard-degraded` gets remedy button state).

- `empty_state(glyph, color, primary, secondary, action: Option<…>)` helper in dashboard.rs; used for in-sync / disconnected; top-aligned under tiles (not vertically centered), ACTIVITY header suppressed when timeline empty.
- Scanning: 3 skeleton rows (h_4 washed bars, widths 40%/65%/52%) under ACTIVITY; keep footer "scanning…".
- Degraded strip gains "Open Settings" action when hint mentions 1Password/OP_ACCOUNT.
- Footer (mod.rs): single line, `text_ellipsis` + TextTooltip full text; drop duplicate origin line when identical info shows in tiles… keep origin line only when disconnected. Fix the mid-word clip (live-shot bug).
- Verify: shoot `dashboard-empty`, `dashboard-scanning`, `dashboard-disconnected`, `dashboard-degraded`, `dashboard-rescanning` both themes; `scripts/shoot.sh live dashboard` for footer fix. Commit `refine: structured empty/scanning/disconnected states; footer truncation fix`.

### Task 5: Review pane (spec 12, 13, 14, 15)

**Files:** `crates/app/src/views/review.rs`, fixtures.

- History rows → fixed columns: time w-56 right, glyph, humanized event, machine muted; chip only when class differs from event label.
- Header: [Keep disk][Keep source] pair · [Open merge editor] accent-when-conflict · editor icon-button (muted glyph "↗" or pencil + TextTooltip "Open in $EDITOR").
- Path subtitle (muted, 11px) under filename.
- Group headers: "NEEDS A DECISION", "SAFE TO RESOLVE".
- Verify: shoot `review`, `review-banner`, `review-empty` both themes. Commit `refine: review columns, action hierarchy, path subtitle`.

### Task 6: Merge editor (spec 16, 17, 18, 19)

**Files:** `crates/app/src/views/merge.rs`, fixtures (`merge` unresolved keeps Save disabled state capturable).

- Pane labels move into panes (header row inside border: title + muted provenance); toolbar keeps only progress + Cancel/Save.
- 🔒 → "⬢"/"⚿"-class glyph (pick a clean SF-symbol-adjacent unicode: "🔒"→"" no — use "●"+tooltip? choose "⛨"? decide in-code from what renders crisply in Menlo; likely "🔒" replaced by styled text "LOCKED" chip or "⚷". Verify visually via shoot before committing).
- Save: disabled (muted border/text, no hover) until `unresolved == 0`, TextTooltip "N conflicts left".
- Keyboard hint line at result pane footer: "1 ours · 2 theirs · 3 base · n next" muted 11px (display only this phase; bindings later — but wire `on_key_down` if trivial with existing focus model, else display-only and note).
- Verify: shoot `merge`, `merge-templated`, `merge-resolved` both themes. Commit `refine: merge pane labels, save affordance, protected-span glyph`.

### Task 7: Settings + chrome (spec 20, 21, 22, 23, 24, 25)

**Files:** `crates/app/src/views/settings.rs`, `mod.rs` (nav), `main.rs` (min size), fixtures (`settings-dirty` new posed state if cheap).

- Account picker → radio rows (28px: ○/● glyph accent, label; hover wash).
- Sections: header outside box (muted caps 11px), controls grouped tight; stepper value fixed-width.
- Save disabled when clean; muted "unsaved changes" when dirty.
- Nav rows ~30px; badge chip smaller (px_1p5, radius 4, 11px).
- `WindowOptions`… min size: gpui 0.2.2 `WindowOptions.window_min_size` (verify field name in local source) ≈ 840×540.
- Tabular numbers: check gpui 0.2.2 `TextStyle`/`font_features` for `tnum` support; apply to tile numbers + timestamps if present, else skip (fixed columns already mitigate).
- Verify: shoot `settings` both themes + full sweep `scripts/shoot.sh` (regression pass over all 15). Commit `refine: settings controls, nav density, window min size`.

### Task 8: Final sweep

- Full `scripts/shoot.sh` + `live dashboard` + `live review`; read every changed PNG against spec items 1–26; fix strays; `cargo test --workspace`; update memory project-state; final commit `refine: post-sweep fixes`.
