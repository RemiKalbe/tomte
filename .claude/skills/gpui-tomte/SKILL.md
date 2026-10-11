---
name: gpui-tomte
description: Facts verified against the pinned gpui 0.2.2 source that the generic gpui skill gets wrong or leaves out — reentrancy hand-off shapes, .truncate() never ellipsizing, TestAppContext driving real subprocesses, panics in background tasks, globals in tests, and what screenshots cannot prove. Use alongside the gpui skill whenever editing a view, an entity update, a cx.spawn, a #[gpui::test], or layout in this repo, or when a "cannot update while already being updated" panic, a clipped label, or a wedged latch shows up.
---

Every claim below was verified in the pinned crate source (`gpui = "=0.2.2"`,
`~/.cargo/registry/src/*/gpui-0.2.2/`) or by a test in this repo during the
async action queue work (0.1.18). When the generic `gpui` skill and this file
disagree, this file wins for 0.2.2; re-verify against the source before
trusting either after a gpui upgrade.

## Reentrancy — the practical shapes

The rule is "never update an entity from inside its own update closure".
The shapes that matter in practice:

- **View → Shell from inside the view's own update** (`self.shell.update(cx, |s, cx| …)`
  in a `Context<View>` method) is safe ONLY if the Shell path never
  synchronously updates that view back. Paths that set a banner or a route on
  the view must be handed off from a `cx.spawn` and then `shell.update` — a
  synchronous hand-off here crashed the app on 2026-08-18.
- **Completions** (`cx.spawn(async move |this, cx| { … this.update(cx, |shell, cx| …) })`)
  run in a fresh update and may update other entities freely; that is the
  place to touch a view from Shell code.
- **Render of a child inside a parent's render** is safe: `Entity::update`
  returns the element before children lay out (`view.rs:47`), so the parent's
  lease is released first.
- **Observers cannot fire mid-update**: effects flush only when
  `pending_updates == 1` (`app.rs:771`). A Shell may mutate itself from its own
  model observer (`cx.observe(&model, |shell, _, cx| …)`) without a guard.
- **Click handlers of a view rendered in another entity's context** (the
  dashboard renders inside Shell's `render`) receive `&mut App`, not the view's
  `Context`; a `shell.update` there is a plain call, not a nested one.
- A dropped request must be LOGGED, not bannered, when the caller may be the
  view the banner would update.

## Layout and text

- **`.truncate()` does not ellipsize in 0.2.2.** `whitespace_nowrap` leaves
  `wrap_width` `None`, the text-measure cache short-circuits, and the ellipsis
  pass never runs: text hard-clips mid-glyph. Use `line_clamp(1)` with a fixed
  width or `w_full`, and READ the screenshot.
- Hover-revealed controls (`opacity(0.)` until `hover`) are invisible in a
  screenshot, so a screenshot cannot prove their enabled/disabled state. Plan
  the rows as data (a `sidebar_rows`-style projection), render the plan
  verbatim, and test the plan.
- `.when(cond, |el| …)` for conditional chains; give `cursor_pointer` and
  `on_click` only to rows that are actually clickable, and derive that bool in
  one place a test can reach.

## Contexts and globals

- `Context<T>: Deref<Target = App>` — pass `cx` wherever `&App` is wanted;
  `Entity<T>::read(&self, cx: &App)`.
- `cx.try_global::<T>()` returns `Option<&T>`; publish with `cx.set_global`.
  In tests `TestAppContext` has `set_global` directly (`test_context.rs:375`);
  `cx.update(|cx| cx.set_global(..))` also works. The test app publishes no
  globals of its own — anything gated on one parks until a test publishes it.

## Async, background work and panics

- `cx.background_executor().spawn(async move { blocking_work() }).await` from a
  `cx.spawn` future is the shape for git/chezmoi/IPC calls.
- **A panic inside a background task re-panics at the foreground `.await`**
  (async-task 4.7.1 `resume_unwind`). Anything holding a latch or a "running"
  set across that await must run the work under
  `std::panic::catch_unwind(AssertUnwindSafe(..))` INSIDE the background
  closure and convert the panic into an ordinary failure. `catch_unwind` is
  real only while `panic = "unwind"` (check `[profile.*]` in Cargo.toml).
- Blocking on osascript/`notify` inside a Shell update stalls the UI; run it
  on the background executor, and gate it off under `cfg!(test)` — then say in
  the report that the production spawn is untested.

## Focus, hit-testing and keys

- `.track_focus(&handle)` already focuses the element on mouse-down and
  yields to a focusable child (a click in the diff pane's read-only editor
  keeps the editor focused). A hand-written `.on_mouse_down(.., focus(window))`
  on the same root runs last in the bubble and re-focuses the root
  unconditionally — it stole ⌘A/⌘C from the diff editor (2026-09-24). Never
  add one next to `track_focus`.
- An element at `opacity(0.)` is still hit-testable: a hover-revealed
  checkbox receives the click before it is visible. Say so in a comment when
  that is the intent; gate on hover/selection state when it is not.
- Key bindings: `gpui::actions!(ns, [A, B])`, `cx.bind_keys([KeyBinding::new("escape", A, Some("Ctx"))])`
  registered at every app entry point (main / gallery / live, next to the
  merge editor's), and `.key_context("Ctx")` + `.on_action(cx.listener(..))`
  on the root that carries the focus handle. Routes are exclusive here, so
  one view's `cmd-a` does not shadow another's.

## Virtualized lists (`list` / `ListState`)

- `gpui::list(ListState, FnMut(usize, &mut Window, &mut App) -> AnyElement)`
  is `uniform_list` for mixed heights: items are measured lazily, visible
  ones re-measured every frame, off-screen heights CACHED. Tell the state
  about changes with `splice(old_range, new_count)`. `splice` snaps the
  scroll anchor to the window's START whenever the anchor lies INSIDE the
  window (`list.rs` `splice_focusable`): `splice(0..old, new)` always does,
  and so does a minimal window whenever a change above the anchor and one
  below it fall in the same window (a Review decision on a row below the
  scroll top moves that row up into PROCESSING). So splice only the
  changed window (`splice_window`) AND re-anchor by row key around every
  splice or reset (`re_anchor_index` in `review.rs`). `reset(n)` only when
  the item set is replaced wholesale; it also drops every measured height.
- The item closure runs in the list's prepaint, after the owning view's
  `render` returned and its lease was released: `weak.upgrade().update(cx,
  |view, cx| …)` inside it is a fresh update, and the way a row gets a
  `Context` for `cx.listener` (`ReviewView::sidebar()`, 2026-10-11).
- `bounds_for_item(ix)` is `Some` for any MEASURED item at or after the
  scroll top, and it sums the heights in between, where an unmeasured
  (never rendered, or just spliced) item counts as 0. So it and
  `scroll_to_reveal_item(ix)` are reliable only when the previous layout
  rendered every item from the scroll top to `ix` — never on the frame
  that spliced, and not for a row measured on an earlier visit with
  unmeasured rows between. Review records the index range the item closure
  rendered last frame (`rendered_range`), trusts the bounds only inside
  it, defers a reveal off a splice frame (`cx.defer_in(window, |_, _, cx|
  cx.notify())` — a bare `cx.notify()` from `render` does nothing while
  drawing, `window.rs` `invalidate_view`), and otherwise uses
  `scroll_to(ListOffset { item_ix, .. })`.
- A `Rc<Cell<usize>>` counter in the item closure, reset in `render`, is
  the test seam for "only the rows in view render" (`rows_rendered`).
- The item closure is also called from `List::request_layout` and
  `prepaint` while the list holds its `ListState`'s `RefCell` borrowed
  (`list.rs` 0.2.2): reading or splicing that `ListState` from inside a
  row panics.
- Each item is laid out with `layout_as_root` at a definite width but no
  width of its own, so a bare row shrinks to its content (its background
  stops short). Wrap it in `div().w_full().flex().flex_col()` to stretch
  it the way a flex column did; `a_row_inside_the_list_spans_the_sidebar_width`
  pins this.

## Tests

- `debug_bounds` entries persist across frames (`Frame::clear` never
  clears them), so assert a selector's presence on the frame that renders
  it, never its absence after a re-render.
- Keys and clicks: `VisualTestContext::simulate_keystrokes("cmd-a")` goes
  through the bindings AND the key context of the focused node (remove the
  `key_context` and the test fails — that is the proof it exercises the
  binding); `dispatch_action(A)` skips the bindings and hits the focused
  node's `on_action`. `simulate_click(point, modifiers)` is a real
  mouse-down/up pair; take coordinates from `debug_bounds("name")` on an
  element that carries `.debug_selector(|| "name".into())` (test-support only,
  a no-op in the shipped binary). No repo test dispatched an action before
  2026-09-24; these are the shapes to copy.
- `#[gpui::test] fn x(cx: &mut TestAppContext)`; build with `cx.new(|cx| …)`,
  drive with `entity.update(cx, ..)`, assert with `entity.read_with(cx, |e, cx| ..)`.
- `cx.run_until_parked()` drives foreground AND background tasks
  deterministically, including real subprocesses and IPC to a daemon thread
  inside background tasks (this repo's `EngineLab` runs a real chezmoi, git and
  daemon under it). One call drains a chain in which a completion spawns the
  next task. No sleeps, no wall clock.
- Dev-dependency: `gpui = { workspace = true, features = ["test-support"] }`.
  Views are bin-only here, so lab-driven view tests live in the bin's own
  `#[cfg(test)] mod tests`, not in `tests/`.
- A behaviour that is only visible in a screenshot has no test. Name the
  mutation that would break it, run the mutation, and if nothing fails, add
  the pure seam the test needs.

## Verify before you trust

Pin a claim about gpui to the source: `rg -n "<symbol>" ~/.cargo/registry/src/*/gpui-0.2.2/src/`.
No web search for framework behaviour.
