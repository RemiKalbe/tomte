# chezmoi-ui design system

The style is settled and approved: GitHub palette, Zed density and idioms, GPUI flexbox layout. Refinement happens inside this language.

## Color tokens (crates/app/src/theme.rs — the only source of color)

| Token | Dark | Light | Role |
| --- | --- | --- | --- |
| bg | #0d1117 | #ffffff | window background |
| surface | #161b22 | #f6f8fa | sidebar, cards, panes |
| border | #30363d | #d0d7de | 1px hairlines |
| text | #c9d1d9 | #1f2328 | primary text |
| text_muted | #8b949e | #656d76 | secondary text, timestamps |
| accent | #58a6ff | #0969da | primary actions, selection, remote-ahead |
| ok | #3fb950 | #1a7f37 | in-sync, success |
| drift | #d29922 | #9a6700 | modified-on-disk severity, degraded |
| conflict | #f85149 | #cf222e | conflicts, eval failures, errors |

Rules:
- No raw hex in views; semantic tokens only. `Theme::wash(token, alpha)` for hover layers and chip fills (Zed-style ghost layering).
- Severity classes map through `class_color()`: conflict/eval red, disk-drift amber, remote-ahead blue, in-sync green.
- Accent is for actions/selection/state, never decoration. Restrained strategy.

## Typography

- System UI font for all chrome; `Menlo` for file content (diff/merge panes).
- Zed-like scale: ~11px meta/labels, ~13px body/rows, ~15-16px pane titles. Weight (not size) distinguishes filename from path within a row.
- Timestamps and counts should be tabular where they update live.

## Layout

- Left sidebar nav 200px (Zed settings shape), traffic lights float over it (`pt_10` reserve), content pane fills.
- Master-detail on Review (list left, provenance + diff right). Three inputs + result pane on Merge.
- Density: rows ~28-32px, 1px borders, radius 4-6px small elements, 8px panes.

## Where the system lives

`crates/ui` (czui-ui) owns the theme, embedded SVG assets, and every reusable component — one file per component under `src/components/`, pure builders (Theme + data + plain click handlers, no Entity coupling). `src/preview.rs` registers component-isolation previews; the gallery derives `comp:<name>` states from it (`scripts/shoot.sh comp:dropdown dark`). Views import via `use czui_ui::components as ui;`. New reusable styling goes in czui-ui first, never inline in a view.

## Components (established vocabulary)

- Tiles: dashboard summary cards (mockup B), number + label + optional action button.
- Rows: hover fills full width with `wash` layer; clickable rows navigate, never surprise-mutate.
- Chips: class labels ("modified on disk") in washed severity color, radius 4.
- Quick actions: bordered ghost buttons ("keep disk" / "keep source"), disabled when engine absent.
- OutcomeBanner: post-action strip above detail, tinted ok/conflict, with Undo when undoable.
- TextTooltip on truncation and icons.

## States that must always be honest

scanning (never claims in-sync), degraded (says why + what to do), disconnected, empty (in sync), loading (merge inputs), saving, action-in-flight.

## Motion

Almost none today, and GPUI animation is used sparingly by Zed too: state transitions only, 150ms-ish, no choreography. No decorative motion.
