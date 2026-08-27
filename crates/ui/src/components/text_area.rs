//! Multi-line plain-text editor (merge-editor-v2 spec step 4, extended
//! 2026-08-22 into the merge editor's ONE result surface): a `String`
//! buffer with byte-offset cursor/selection, a custom Element that shapes
//! one line per `\n`, and a full UTF-16 [`EntityInputHandler`] so IME
//! composition (marked text), dictation, and press-and-hold accents work.
//!
//! Host-driven decoration, Zed's conflict-view idiom: per-line washes,
//! rails, gutter glyphs, span highlights and state pills live IN the buffer
//! and follow it as it changes, and **blocks** — host-rendered rows inserted
//! above a line (Zed's `BlockPlacement::Above`) — carry the decision strips.
//! A read-only mode keeps selection + copy (the user's "I want to select
//! text in every diff") while refusing mutation and hiding the caret.
//!
//! Unlike every other component in this crate, this is an Entity — focus,
//! input-handler registration, and edit history are stateful by nature.
//! Adapted from gpui 0.2.2's `examples/input.rs` (the canonical text input),
//! generalized from one `ShapedLine` to a column of them.
//!
//! Undo: time-grouped transactions (300ms — Zed's grouping interval). Each
//! transaction snapshots (content, selection) before/after; empty edits are
//! dropped; any edit clears redo. The HOST view decides what an edit session
//! means (commit/cancel) — this component only edits text.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, AvailableSpace, BorderStyle, Bounds, ClipboardItem, Context, CursorStyle,
    Element, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable,
    GlobalElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, Point, Rgba, ShapedLine, SharedString, Style, TextRun,
    UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*, px, quad,
    relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::Theme;

/// Per-line decoration supplied by the host. All fields optional; an
/// undecorated line renders plain.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LineDecor {
    /// Full-width background wash behind the line.
    pub bg: Option<Rgba>,
    /// 2px provenance rail at the left edge.
    pub rail: Option<Rgba>,
    /// Text color override (e.g. muted context lines).
    pub text_color: Option<Rgba>,
    /// Glyph in the gutter cell (the protected-template `{}`, diff dots).
    pub gutter: Option<(SharedString, Rgba)>,
    /// Right-aligned pill naming the block ("on disk", "source",
    /// "edited") — rendered on the block's first line.
    pub label: Option<(SharedString, Rgba)>,
    /// Byte ranges (into the line's text) to emphasize with a background —
    /// word-diff highlights.
    pub spans: Vec<(Range<usize>, Rgba)>,
    /// Hover text for the whole row (the protected-template explanation).
    /// A custom element can't use `div().tooltip()`, so the row gets its own
    /// hitbox and drives [`Window::set_tooltip`] directly.
    pub tooltip: Option<SharedString>,
}

/// Renders a block's element each frame (host builds it fresh).
pub type BlockRender = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// A host-rendered row inserted ABOVE text line `above_line` (`== line
/// count` appends after the last line). The element is built every frame
/// from `render`, laid out at full width × `height` rows.
#[derive(Clone)]
pub struct Block {
    pub above_line: usize,
    pub height: u32,
    pub render: BlockRender,
}

/// Everything the host contributes for one content revision.
#[derive(Default)]
pub struct Decoration {
    pub lines: Vec<LineDecor>,
    pub blocks: Vec<Block>,
}

/// Host-supplied decorator: current content → decoration. Called only when
/// the content changes or the host invalidates (revision-cached), so it may
/// diff against a seed without per-frame cost.
pub type Decorator = Box<dyn Fn(&str) -> Decoration>;

actions!(
    text_area,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        LineStart,
        LineEnd,
        SelectToLineStart,
        SelectToLineEnd,
        DocStart,
        DocEnd,
        Newline,
        Paste,
        Cut,
        Copy,
        Undo,
        Redo,
        ShowCharacterPalette,
    ]
);

/// Key bindings for the editor. Editable surfaces use the "TextArea"
/// context (deeper than any host context, so cmd-z hits the text history);
/// read-only surfaces use "TextAreaReadOnly", which binds ONLY navigation,
/// selection and copy — everything else (cmd-z, 1/2/3…) falls through to
/// the host's own bindings.
pub fn bindings() -> Vec<KeyBinding> {
    const EDIT: Option<&str> = Some("TextArea");
    const RO: Option<&str> = Some("TextAreaReadOnly");
    let mut out = Vec::new();
    for ctx in [EDIT, RO] {
        out.extend([
            KeyBinding::new("left", Left, ctx),
            KeyBinding::new("right", Right, ctx),
            KeyBinding::new("up", Up, ctx),
            KeyBinding::new("down", Down, ctx),
            KeyBinding::new("shift-left", SelectLeft, ctx),
            KeyBinding::new("shift-right", SelectRight, ctx),
            KeyBinding::new("shift-up", SelectUp, ctx),
            KeyBinding::new("shift-down", SelectDown, ctx),
            KeyBinding::new("cmd-a", SelectAll, ctx),
            KeyBinding::new("home", LineStart, ctx),
            KeyBinding::new("end", LineEnd, ctx),
            KeyBinding::new("cmd-left", LineStart, ctx),
            KeyBinding::new("cmd-right", LineEnd, ctx),
            KeyBinding::new("ctrl-a", LineStart, ctx),
            KeyBinding::new("ctrl-e", LineEnd, ctx),
            KeyBinding::new("cmd-shift-left", SelectToLineStart, ctx),
            KeyBinding::new("cmd-shift-right", SelectToLineEnd, ctx),
            KeyBinding::new("cmd-up", DocStart, ctx),
            KeyBinding::new("cmd-down", DocEnd, ctx),
            KeyBinding::new("cmd-c", Copy, ctx),
        ]);
    }
    out.extend([
        KeyBinding::new("backspace", Backspace, EDIT),
        KeyBinding::new("delete", Delete, EDIT),
        KeyBinding::new("enter", Newline, EDIT),
        KeyBinding::new("cmd-v", Paste, EDIT),
        KeyBinding::new("cmd-x", Cut, EDIT),
        KeyBinding::new("cmd-z", Undo, EDIT),
        KeyBinding::new("cmd-shift-z", Redo, EDIT),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, EDIT),
    ]);
    out
}

/// Byte range of each display line, split on `\n` (the terminator is part of
/// its line, so ranges tile the whole buffer). Always at least one line; a
/// trailing `\n` yields a final empty line the cursor can sit on.
pub(crate) fn line_ranges(content: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (ix, b) in content.bytes().enumerate() {
        if b == b'\n' {
            out.push(start..ix + 1);
            start = ix + 1;
        }
    }
    out.push(start..content.len());
    out
}

/// The line (index into `line_ranges`) containing byte `offset`. A cursor at
/// a line's end (just before `\n`) belongs to that line; at the terminator
/// boundary it belongs to the next.
pub(crate) fn line_of(ranges: &[Range<usize>], offset: usize) -> usize {
    ranges
        .iter()
        .position(|r| offset < r.end)
        .unwrap_or(ranges.len() - 1)
}

/// A line's content slice without its `\n` terminator.
fn line_text<'a>(content: &'a str, range: &Range<usize>) -> &'a str {
    content[range.clone()]
        .strip_suffix('\n')
        .unwrap_or(&content[range.clone()])
}

/// Where a block row is laid out and how wide, given the element's bounds
/// and the currently visible rect. Blocks are HORIZONTALLY STICKY: the rows
/// they decorate can be far wider than the pane (see the scroll extent), and
/// a strip stretched across that width puts its trailing buttons thousands
/// of pixels off-screen. Zed solves the same problem with
/// `BlockStyle::Sticky`.
pub(crate) fn sticky_block_x(element: Bounds<Pixels>, visible: Bounds<Pixels>) -> (Pixels, Pixels) {
    let left = element.left().max(visible.left());
    let right = element.right().min(visible.right());
    let width = (right - left).max(px(0.));
    (left, width)
}

/// Vertical geometry with blocks: text line `i` sits at row `line_rows[i]`;
/// the rows in between belong to blocks. Rows are `line_height` tall.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct RowMap {
    pub line_rows: Vec<usize>,
    /// `(first row, height, block index)` in row order.
    pub blocks: Vec<(usize, u32, usize)>,
    pub total_rows: usize,
}

/// Where a row lands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RowHit {
    Line(usize),
    /// Inside a block; payload = the line it sits above (clamped).
    Block(usize),
}

/// Build the row map for `n_lines` lines and blocks given as
/// `(above_line, height)`. Blocks are placed in `above_line` order; a block
/// above line `n_lines` trails the last line.
pub(crate) fn row_map(n_lines: usize, blocks: &[(usize, u32)]) -> RowMap {
    let mut order: Vec<usize> = (0..blocks.len()).collect();
    order.sort_by_key(|&ix| blocks[ix].0.min(n_lines));
    let mut line_rows = Vec::with_capacity(n_lines);
    let mut placed = Vec::with_capacity(blocks.len());
    let mut row = 0usize;
    let mut next = order.into_iter().peekable();
    for line in 0..n_lines {
        while let Some(&ix) = next.peek() {
            if blocks[ix].0.min(n_lines) > line {
                break;
            }
            placed.push((row, blocks[ix].1, ix));
            row += blocks[ix].1 as usize;
            next.next();
        }
        line_rows.push(row);
        row += 1;
    }
    for ix in next {
        placed.push((row, blocks[ix].1, ix));
        row += blocks[ix].1 as usize;
    }
    RowMap {
        line_rows,
        blocks: placed,
        total_rows: row,
    }
}

impl RowMap {
    pub(crate) fn hit(&self, row: usize) -> RowHit {
        let n = self.line_rows.len();
        if n == 0 {
            return RowHit::Line(0);
        }
        for &(start, height, _) in &self.blocks {
            if row >= start && row < start + height as usize {
                // The line this block sits above = first line at a later row.
                let line = self.line_rows.partition_point(|&r| r < start);
                return RowHit::Block(line.min(n - 1));
            }
        }
        // Last line whose row <= `row` (rows past the end clamp to the last).
        let ix = self.line_rows.partition_point(|&r| r <= row);
        RowHit::Line(ix.saturating_sub(1).min(n - 1))
    }
}

/// One undoable step: whole-buffer snapshots (dotfile-scale documents make
/// diff-based history pointless complexity).
struct Transaction {
    before: (String, Range<usize>),
    after: (String, Range<usize>),
}

/// Zed's grouping policy: edits within 300ms merge into one transaction;
/// empty edits are dropped; any edit clears redo.
const GROUP_WITHIN: Duration = Duration::from_millis(300);

#[derive(Default)]
struct History {
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    last_edit: Option<Instant>,
}

impl History {
    fn record(&mut self, before: (String, Range<usize>), after: (String, Range<usize>)) {
        if before.0 == after.0 {
            return;
        }
        self.redo.clear();
        let now = Instant::now();
        let group = self
            .last_edit
            .is_some_and(|t| now.duration_since(t) < GROUP_WITHIN);
        self.last_edit = Some(now);
        if group && let Some(top) = self.undo.last_mut() {
            top.after = after;
            return;
        }
        self.undo.push(Transaction { before, after });
    }
}

/// Layout captured at paint time — the coordinate system for mouse hits,
/// cursor geometry, and the IME's `bounds_for_range`. Off-screen lines are
/// `None` (viewport culling); callers fall back to line starts for them.
struct LastLayout {
    lines: Vec<Option<ShapedLine>>,
    rows: RowMap,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
}

/// Shaped-line cache key: text, resolved text color, and span highlights
/// (all three live in the runs).
type ShapeKey = (String, [u32; 4], Vec<(usize, usize, [u32; 4])>);

fn color_key(color: gpui::Hsla) -> [u32; 4] {
    [
        color.h.to_bits(),
        color.s.to_bits(),
        color.l.to_bits(),
        color.a.to_bits(),
    ]
}

fn rgba_key(color: Rgba) -> [u32; 4] {
    [
        color.r.to_bits(),
        color.g.to_bits(),
        color.b.to_bits(),
        color.a.to_bits(),
    ]
}

/// Split one line into runs: span backgrounds and the IME underline each
/// cut the line at their boundaries; every segment gets the union of the
/// styles covering it.
fn build_runs(
    len: usize,
    base: &TextRun,
    spans: &[(Range<usize>, Rgba)],
    marked: Option<&Range<usize>>,
) -> Vec<TextRun> {
    let mut cuts: BTreeSet<usize> = BTreeSet::from([0, len]);
    for (range, _) in spans {
        cuts.insert(range.start.min(len));
        cuts.insert(range.end.min(len));
    }
    if let Some(m) = marked {
        cuts.insert(m.start.min(len));
        cuts.insert(m.end.min(len));
    }
    let cuts: Vec<usize> = cuts.into_iter().collect();
    let mut runs = Vec::with_capacity(cuts.len());
    for pair in cuts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if b <= a {
            continue;
        }
        let background_color = spans
            .iter()
            .find(|(r, _)| r.start <= a && b <= r.end)
            .map(|(_, c)| (*c).into());
        let underline = marked
            .filter(|m| m.start <= a && b <= m.end)
            .map(|_| UnderlineStyle {
                color: Some(base.color),
                thickness: px(1.0),
                wavy: false,
            });
        runs.push(TextRun {
            len: b - a,
            background_color,
            underline,
            ..base.clone()
        });
    }
    if runs.is_empty() {
        runs.push(base.clone());
    }
    runs
}

pub struct TextArea {
    focus_handle: FocusHandle,
    content: String,
    /// Bumped on every text mutation or host invalidation — keys the
    /// decoration cache.
    content_rev: usize,
    /// Byte-offset selection; empty = caret.
    selected_range: Range<usize>,
    selection_reversed: bool,
    /// IME composition (underlined; replaced by the commit).
    marked_range: Option<Range<usize>>,
    /// Goal column for a run of vertical moves, cleared by anything else.
    preferred_x: Option<Pixels>,
    last_layout: Option<LastLayout>,
    is_selecting: bool,
    history: History,
    /// Selection + copy only: no caret, no mutation, no IME registration.
    read_only: bool,
    /// Host-supplied decorations (washes/rails/labels/blocks).
    decorator: Option<Decorator>,
    decor_cache: RefCell<Option<(usize, Rc<Decoration>)>>,
    /// Shaped lines survive across frames; only changed lines reshape.
    shape_cache: RefCell<HashMap<ShapeKey, ShapedLine>>,
    /// Left inset of the text column (rail + gutter live inside it), so the
    /// editor can line up with the host's read-only rows.
    left_inset: Pixels,
    /// Cursor blink: solid on every interaction, then 600ms toggles.
    blink_show: bool,
    blink_epoch: usize,
    /// Set during paint — lets the blink loop skip notifying while blurred.
    focused_hint: bool,
    /// Cached tooltip view, keyed by its text (see the paint loop).
    tooltip_view: RefCell<Option<(SharedString, gpui::AnyView)>>,
    /// Width of the longest line in the WHOLE document — the element
    /// requests it (floored at 100% of the parent) so an `overflow_x_scroll`
    /// container has something to scroll. Derived from the longest line's
    /// character count times one character's advance: every surface using
    /// this is monospace, so that is exact, and unlike measuring the shaped
    /// lines it does not depend on which rows happen to be on screen.
    content_width: Rc<std::cell::Cell<Pixels>>,
    /// `(content_rev, longest line in chars)`.
    longest_line: std::cell::Cell<(usize, usize)>,
}

impl TextArea {
    /// A fresh editable buffer seeded with `content`, caret at the start.
    pub fn new(content: String, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            content,
            content_rev: 0,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            preferred_x: None,
            last_layout: None,
            is_selecting: false,
            history: History::default(),
            read_only: false,
            decorator: None,
            decor_cache: RefCell::new(None),
            shape_cache: RefCell::new(HashMap::new()),
            left_inset: px(0.),
            blink_show: true,
            blink_epoch: 0,
            focused_hint: false,
            content_width: Rc::new(std::cell::Cell::new(px(0.))),
            longest_line: std::cell::Cell::new((usize::MAX, 0)),
            tooltip_view: RefCell::new(None),
        };
        this.wake_cursor(cx);
        this
    }

    /// A read-only buffer: selectable, copyable, never mutated.
    pub fn read_only(content: String, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new(content, cx);
        this.read_only = true;
        this
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The selected text, if any.
    pub fn selected_text(&self) -> Option<&str> {
        (!self.selected_range.is_empty()).then(|| &self.content[self.selected_range.clone()])
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Switch between read-only and editable in place (same buffer, same
    /// decorations): the merge editor's "edit" just flips this.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
        self.marked_range = None;
        self.is_selecting = false;
    }

    /// Replace the whole buffer (host re-seed after a pick). Resets the
    /// selection and the text history — the old text is the host's to undo.
    pub fn set_content(&mut self, content: String) {
        if content != self.content {
            self.content = content;
            self.content_rev += 1;
            self.history = History::default();
        }
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.preferred_x = None;
        self.invalidate_decorations();
    }

    /// Install per-line decorations (recomputed whenever content changes or
    /// the host invalidates).
    pub fn set_decorator(&mut self, decorator: Decorator) {
        self.decorator = Some(decorator);
        self.invalidate_decorations();
    }

    /// Force the decorator to run again on the next frame (host state that
    /// isn't text changed — e.g. which region is focused).
    pub fn invalidate_decorations(&mut self) {
        *self.decor_cache.borrow_mut() = None;
    }

    /// Left inset of the text column (rail + gutter render inside it).
    pub fn set_left_inset(&mut self, inset: Pixels) {
        self.left_inset = inset;
    }

    /// Collapse the selection to a caret at byte `offset` (clamped). Callers
    /// pass char-boundary offsets — region starts are line starts.
    pub fn place_cursor(&mut self, offset: usize) {
        let offset = offset.min(self.content.len());
        self.selected_range = offset..offset;
        self.selection_reversed = false;
    }

    /// Hold the cursor solid and restart the blink cadence — called on
    /// every interaction so the caret never blinks away mid-typing.
    fn wake_cursor(&mut self, cx: &mut Context<Self>) {
        self.blink_show = true;
        self.blink_epoch += 1;
        if self.read_only {
            return;
        }
        let epoch = self.blink_epoch;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(600))
                    .await;
                let alive = this.update(cx, |area, cx| {
                    if area.blink_epoch != epoch || area.read_only {
                        return false;
                    }
                    if area.focused_hint {
                        area.blink_show = !area.blink_show;
                        cx.notify();
                    }
                    true
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Width the content wants: the longest line's character count times one
    /// character's advance, plus the gutter inset. Every surface using this
    /// is monospace, so that is exact — and unlike measuring the shaped
    /// lines it doesn't depend on which rows happen to be on screen. Cached
    /// per content revision; a scroll must not recount 700 lines per frame.
    fn scroll_extent(&self, window: &mut Window) -> Pixels {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let (cached_rev, mut longest) = self.longest_line.get();
        if cached_rev != self.content_rev {
            longest = self
                .content
                .lines()
                .map(|l| l.chars().count())
                .max()
                .unwrap_or(0);
            self.longest_line.set((self.content_rev, longest));
        }
        let advance = window
            .text_system()
            .shape_line(
                "0".into(),
                font_size,
                &[TextRun {
                    len: 1,
                    font: style.font(),
                    color: style.color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            )
            .width;
        let extent = self.left_inset + advance * longest as f32 + px(16.);
        self.content_width.set(extent);
        extent
    }

    /// Current decoration, recomputed only when the revision moved.
    fn current_decoration(&self) -> Rc<Decoration> {
        let Some(decorator) = self.decorator.as_ref() else {
            return Rc::new(Decoration::default());
        };
        let mut cache = self.decor_cache.borrow_mut();
        if let Some((rev, decoration)) = &*cache
            && *rev == self.content_rev
        {
            return decoration.clone();
        }
        let decoration = Rc::new(decorator(&self.content));
        *cache = Some((self.content_rev, decoration.clone()));
        decoration
    }

    // ---- movement ------------------------------------------------------

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.wake_cursor(cx);
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.wake_cursor(cx);
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    /// The byte offset one line up/down from the cursor, aiming at the
    /// remembered goal column (pixel-based via the painted layout; falls
    /// back to line starts before the first paint).
    fn vertical_target(&mut self, delta: isize) -> usize {
        let ranges = line_ranges(&self.content);
        let cursor = self.cursor_offset();
        let row = line_of(&ranges, cursor);
        let target_row = row.saturating_add_signed(delta).min(ranges.len() - 1);
        if target_row == row {
            // Off the top/bottom edge: pin to the document boundary.
            return if delta < 0 { 0 } else { self.content.len() };
        }
        let Some(layout) = &self.last_layout else {
            return ranges[target_row].start;
        };
        let shaped = |r: usize| layout.lines.get(r).and_then(|l| l.as_ref());
        let x = self.preferred_x.unwrap_or_else(|| {
            shaped(row)
                .map(|l| l.x_for_index(cursor - ranges[row].start))
                .unwrap_or(px(0.))
        });
        self.preferred_x = Some(x);
        let target = &ranges[target_row];
        let col = shaped(target_row)
            .map(|l| l.closest_index_for_x(x))
            .unwrap_or(0);
        (target.start + col).min(target.start + line_text(&self.content, target).len())
    }

    // ---- action handlers -------------------------------------------------

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(-1);
        self.move_to(target, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(1);
        self.move_to(target, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(-1);
        self.select_to(target, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(1);
        self.select_to(target, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    /// Start of the cursor's line (byte offset).
    fn line_start_offset(&self) -> usize {
        let ranges = line_ranges(&self.content);
        ranges[line_of(&ranges, self.cursor_offset())].start
    }

    /// End of the cursor's line, before its `\n`.
    fn line_end_offset(&self) -> usize {
        let ranges = line_ranges(&self.content);
        let range = &ranges[line_of(&ranges, self.cursor_offset())];
        range.start + line_text(&self.content, range).len()
    }

    fn line_start(&mut self, _: &LineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        let target = self.line_start_offset();
        self.move_to(target, cx);
    }

    fn line_end(&mut self, _: &LineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        let target = self.line_end_offset();
        self.move_to(target, cx);
    }

    fn select_to_line_start(
        &mut self,
        _: &SelectToLineStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preferred_x = None;
        let target = self.line_start_offset();
        self.select_to(target, cx);
    }

    fn select_to_line_end(&mut self, _: &SelectToLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        let target = self.line_end_offset();
        self.select_to(target, cx);
    }

    fn doc_start(&mut self, _: &DocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.move_to(0, cx);
    }

    fn doc_end(&mut self, _: &DocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.move_to(self.content.len(), cx);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "\n", window, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        // Unlike the single-line input, newlines are the point here.
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        // Any pending grouping window ends here: undo after undo walks
        // transactions, never re-groups.
        self.history.last_edit = None;
        if let Some(tx) = self.history.undo.pop() {
            self.content = tx.before.0.clone();
            self.content_rev += 1;
            self.selected_range = tx.before.1.clone();
            self.selection_reversed = false;
            self.marked_range = None;
            self.history.redo.push(tx);
            self.wake_cursor(cx);
            cx.notify();
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        self.history.last_edit = None;
        if let Some(tx) = self.history.redo.pop() {
            self.content = tx.after.0.clone();
            self.content_rev += 1;
            self.selected_range = tx.after.1.clone();
            self.selection_reversed = false;
            self.marked_range = None;
            self.history.undo.push(tx);
            self.wake_cursor(cx);
            cx.notify();
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        if !self.read_only {
            window.show_character_palette();
        }
    }

    // ---- mouse -----------------------------------------------------------

    /// Which text line (and where in it) a window point lands on. `None`
    /// inside a block — those rows belong to the host's elements.
    fn hit(&self, position: Point<Pixels>) -> Option<usize> {
        let Some(layout) = &self.last_layout else {
            return Some(0);
        };
        let bounds = &layout.bounds;
        if position.y < bounds.top() {
            return Some(0);
        }
        let row = (f32::from(position.y - bounds.top()) / f32::from(layout.line_height)) as usize;
        if row >= layout.rows.total_rows {
            return Some(self.content.len());
        }
        let line_ix = match layout.rows.hit(row) {
            RowHit::Line(ix) => ix,
            RowHit::Block(_) => return None,
        };
        let ranges = line_ranges(&self.content);
        let Some(range) = ranges.get(line_ix) else {
            return Some(self.content.len());
        };
        let col = layout
            .lines
            .get(line_ix)
            .and_then(|l| l.as_ref())
            .map(|line| line.closest_index_for_x(position.x - bounds.left() - self.left_inset))
            .unwrap_or(0);
        Some((range.start + col).min(range.start + line_text(&self.content, range).len()))
    }

    /// Drag target: like `hit`, but a block row resolves to the start of the
    /// line it sits above so a drag across a strip keeps extending.
    fn drag_target(&self, position: Point<Pixels>) -> usize {
        if let Some(offset) = self.hit(position) {
            return offset;
        }
        let Some(layout) = &self.last_layout else {
            return 0;
        };
        let row =
            (f32::from(position.y - layout.bounds.top()) / f32::from(layout.line_height)) as usize;
        match layout.rows.hit(row) {
            RowHit::Block(line) | RowHit::Line(line) => line_ranges(&self.content)
                .get(line)
                .map(|r| r.start)
                .unwrap_or(0),
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Clicks on a block belong to the block's own controls.
        let Some(offset) = self.hit(event.position) else {
            return;
        };
        self.preferred_x = None;
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let target = self.drag_target(event.position);
            self.select_to(target, cx);
        }
    }

    // ---- UTF-16 offset mapping (IME speaks UTF-16) -----------------------

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }
}

impl EntityInputHandler for TextArea {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let before = (self.content.clone(), self.selected_range.clone());
        self.content =
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..];
        self.content_rev += 1;
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        self.marked_range.take();
        self.preferred_x = None;
        self.history
            .record(before, (self.content.clone(), self.selected_range.clone()));
        self.wake_cursor(cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let before = (self.content.clone(), self.selected_range.clone());
        self.content =
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..];
        if new_text.is_empty() {
            self.marked_range = None;
        } else {
            self.marked_range = Some(range.start..range.start + new_text.len());
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.selection_reversed = false;
        self.preferred_x = None;
        self.content_rev += 1;
        // Composition steps land within the grouping window, so the whole
        // IME sequence collapses into one undo step (spec requirement).
        self.history
            .record(before, (self.content.clone(), self.selected_range.clone()));
        self.wake_cursor(cx);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let ranges = line_ranges(&self.content);
        let row = line_of(&ranges, range.start);
        let line = layout.lines.get(row)?.as_ref()?;
        let line_range = &ranges[row];
        let start_x = line.x_for_index(range.start - line_range.start);
        let end_x = if range.end <= line_range.end {
            line.x_for_index(range.end - line_range.start)
        } else {
            line.width
        };
        let top = bounds.top()
            + layout.line_height * layout.rows.line_rows.get(row).copied().unwrap_or(row) as f32;
        let left = bounds.left() + self.left_inset;
        Some(Bounds::from_corners(
            point(left + start_x, top),
            point(left + end_x, top + layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let index = self.drag_target(point);
        Some(self.offset_to_utf16(index))
    }
}

/// The custom Element: shapes the visible lines, paints washes → selection
/// → rails → gutters → text → labels → blocks → caret, and registers the
/// entity as the window's input handler while editable (that registration
/// is what makes IME work).
struct TextAreaElement {
    area: Entity<TextArea>,
}

/// A right-aligned block label ("on disk", "edited"): pill quad + text.
struct LabelPaint {
    /// Opaque under-layer first: `Theme::wash` only sets alpha, so a tinted
    /// pill painted straight onto the row let the code beneath bleed through
    /// and the label became unreadable (2026-08-27).
    backing: PaintQuad,
    pill: PaintQuad,
    line: ShapedLine,
    origin: Point<Pixels>,
}

struct AreaPrepaint {
    lines: Vec<Option<ShapedLine>>,
    rows: RowMap,
    line_height: Pixels,
    washes: Vec<PaintQuad>,
    selections: Vec<PaintQuad>,
    rails: Vec<PaintQuad>,
    gutters: Vec<(ShapedLine, Point<Pixels>)>,
    labels: Vec<LabelPaint>,
    blocks: Vec<AnyElement>,
    cursor: Option<PaintQuad>,
    shapes: HashMap<ShapeKey, ShapedLine>,
    /// One hitbox per visible row that carries hover text.
    tooltips: Vec<(gpui::Hitbox, SharedString)>,
}

impl IntoElement for TextAreaElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextAreaElement {
    type RequestLayoutState = ();
    type PrepaintState = AreaPrepaint;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let area = self.area.read(cx);
        let n_lines = line_ranges(&area.content).len();
        let decoration = area.current_decoration();
        let blocks: Vec<(usize, u32)> = decoration
            .blocks
            .iter()
            .map(|b| (b.above_line, b.height))
            .collect();
        let rows = row_map(n_lines, &blocks).total_rows;
        let mut style = Style::default();
        // The element just fills its wrapper; the WRAPPER owns the scroll
        // extent (see `Render`). Sizing the leaf itself does NOT work: a
        // measured leaf gets clamped back to the container's width by the
        // flex pass however it is styled — the test at the bottom of this
        // file caught exactly that, twice.
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * rows as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // Snapshot everything we need, then release the entity borrow —
        // block elements need `&mut App` to render.
        let (content, selected_range, marked_range, cursor_offset, inset, read_only) = {
            let area = self.area.read(cx);
            (
                area.content.clone(),
                area.selected_range.clone(),
                area.marked_range.clone(),
                area.cursor_offset(),
                area.left_inset,
                area.read_only,
            )
        };
        let decoration = self.area.read(cx).current_decoration();
        let mut old_shapes = self.area.read(cx).shape_cache.take();
        let mut new_shapes: HashMap<ShapeKey, ShapedLine> = HashMap::new();

        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let label_size = font_size * 0.85;
        let line_height = window.line_height();
        let theme = Theme::for_appearance(window.appearance());

        let ranges = line_ranges(&content);
        let block_specs: Vec<(usize, u32)> = decoration
            .blocks
            .iter()
            .map(|b| (b.above_line, b.height))
            .collect();
        let rows = row_map(ranges.len(), &block_specs);

        // Viewport culling: only rows inside the current clip (± a margin)
        // are shaped and painted; the caret's row always is (IME geometry).
        let mask = window.content_mask().bounds;
        let row_at = |y: Pixels| (f32::from(y - bounds.top()) / f32::from(line_height)).floor();
        let first_visible = (row_at(mask.top()) as isize - 2).max(0) as usize;
        let last_visible = (row_at(mask.bottom()) as isize + 2).max(0) as usize;
        let visible = |row: usize| row >= first_visible && row <= last_visible;
        let caret_line = line_of(&ranges, cursor_offset);

        let mut lines = Vec::with_capacity(ranges.len());
        let mut washes = Vec::new();
        let mut selections = Vec::new();
        let mut rails = Vec::new();
        let mut gutters = Vec::new();
        let mut labels = Vec::new();
        let mut tooltips = Vec::new();
        let mut cursor = None;
        let text_left = bounds.left() + inset;
        for (line_ix, range) in ranges.iter().enumerate() {
            let row = rows.line_rows[line_ix];
            let is_caret_line = selected_range.is_empty() && line_ix == caret_line;
            if !visible(row) && !is_caret_line {
                lines.push(None);
                continue;
            }
            let text = line_text(&content, range);
            let visible_range = range.start..range.start + text.len();
            let decor = decoration.lines.get(line_ix).cloned().unwrap_or_default();
            let text_color: gpui::Hsla = decor.text_color.map(Into::into).unwrap_or(style.color);
            let top = bounds.top() + line_height * row as f32;
            let row_bounds = Bounds::new(
                point(bounds.left(), top),
                size(bounds.size.width, line_height),
            );

            // Decor: full-width wash, provenance rail, gutter glyph, label.
            if let Some(bg) = decor.bg {
                washes.push(fill(row_bounds, bg));
            }
            if let Some(rail) = decor.rail {
                rails.push(fill(
                    Bounds::new(point(bounds.left(), top), size(px(2.), line_height)),
                    rail,
                ));
            }
            if let Some((glyph, tint)) = &decor.gutter {
                let run = TextRun {
                    len: glyph.len(),
                    font: style.font(),
                    color: (*tint).into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped =
                    window
                        .text_system()
                        .shape_line(glyph.clone(), font_size, &[run], None);
                gutters.push((shaped, point(bounds.left() + px(8.), top)));
            }
            if let Some((label, tint)) = &decor.label {
                let run = TextRun {
                    len: label.len(),
                    font: style.font(),
                    color: (*tint).into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped =
                    window
                        .text_system()
                        .shape_line(label.clone(), label_size, &[run], None);
                let pad = px(6.);
                let pill_w = shaped.width + pad * 2.;
                let pill_h = line_height - px(4.);
                // Right edge of what the user can SEE, so the pill doesn't
                // sail off with the horizontal scroll.
                let right = bounds.right().min(mask.right());
                let pill_origin = point(right - pill_w - px(8.), top + px(2.));
                let pill_bounds = Bounds::new(pill_origin, size(pill_w, pill_h));
                labels.push(LabelPaint {
                    backing: quad(
                        pill_bounds,
                        px(4.),
                        theme.bg,
                        px(0.),
                        theme.bg,
                        BorderStyle::Solid,
                    ),
                    pill: quad(
                        pill_bounds,
                        px(4.),
                        Theme::wash(*tint, 0.12),
                        px(1.),
                        Theme::wash(*tint, 0.35),
                        BorderStyle::Solid,
                    ),
                    origin: point(pill_origin.x + pad, top),
                    line: shaped,
                });
            }

            if let Some(tip) = &decor.tooltip {
                tooltips.push((
                    window.insert_hitbox(row_bounds, gpui::HitboxBehavior::Normal),
                    tip.clone(),
                ));
            }

            // Active line: a neutral wash on the caret's row (over the
            // decor wash, under the selection), the standard editor cue.
            // Read-only surfaces have no caret, so no active line either.
            if is_caret_line && !read_only {
                washes.push(fill(row_bounds, Theme::wash(theme.text_muted, 0.08)));
            }

            let base_run = TextRun {
                len: text.len(),
                font: style.font(),
                color: text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            // Marked (IME) span local to this line, if any.
            let marked_local = marked_range.as_ref().and_then(|m| {
                (m.start < visible_range.end && visible_range.start < m.end).then(|| {
                    m.start.max(visible_range.start) - range.start
                        ..m.end.min(visible_range.end) - range.start
                })
            });
            let line = if marked_local.is_some() {
                // Composition lines bypass the cache (transient styling).
                let runs = build_runs(text.len(), &base_run, &decor.spans, marked_local.as_ref());
                window
                    .text_system()
                    .shape_line(text.to_owned().into(), font_size, &runs, None)
            } else {
                let key: ShapeKey = (
                    text.to_owned(),
                    color_key(text_color),
                    decor
                        .spans
                        .iter()
                        .map(|(r, c)| (r.start, r.end, rgba_key(*c)))
                        .collect(),
                );
                let shaped = old_shapes.remove(&key).unwrap_or_else(|| {
                    let runs = build_runs(text.len(), &base_run, &decor.spans, None);
                    window
                        .text_system()
                        .shape_line(text.to_owned().into(), font_size, &runs, None)
                });
                new_shapes.insert(key, shaped.clone());
                shaped
            };

            // Selection quad: this line's intersection with the selection.
            // The `\n` terminator paints as a stub past the last glyph so
            // full-line selections read as a block, not per-word islands.
            if !selected_range.is_empty()
                && selected_range.start < range.end
                && range.start < selected_range.end
            {
                let s0 = selected_range.start.max(range.start) - range.start;
                let s1 = selected_range.end.min(visible_range.end) - range.start;
                let mut x1 = line.x_for_index(s1);
                if selected_range.end > visible_range.end {
                    x1 += px(4.);
                }
                selections.push(fill(
                    Bounds::from_corners(
                        point(text_left + line.x_for_index(s0), top),
                        point(text_left + x1, top + line_height),
                    ),
                    Theme::wash(theme.accent, 0.25),
                ));
            }
            if is_caret_line && !read_only {
                let x = line.x_for_index(cursor_offset - range.start);
                cursor = Some(fill(
                    Bounds::new(point(text_left + x, top), size(px(2.), line_height)),
                    theme.accent,
                ));
            }
            lines.push(Some(line));
        }

        // Blocks: host elements laid out at full width × height rows.
        let mut blocks = Vec::new();
        for &(start_row, height, block_ix) in &rows.blocks {
            if start_row + height as usize <= first_visible || start_row > last_visible {
                continue;
            }
            let block = &decoration.blocks[block_ix];
            let mut el = (block.render)(window, cx);
            let block_h = line_height * height as f32;
            let (block_left, block_w) = sticky_block_x(bounds, mask);
            el.layout_as_root(
                size(
                    AvailableSpace::Definite(block_w),
                    AvailableSpace::Definite(block_h),
                ),
                window,
                cx,
            );
            el.prepaint_at(
                point(block_left, bounds.top() + line_height * start_row as f32),
                window,
                cx,
            );
            blocks.push(el);
        }

        AreaPrepaint {
            lines,
            rows,
            line_height,
            washes,
            selections,
            rails,
            gutters,
            labels,
            blocks,
            cursor,
            shapes: new_shapes,
            tooltips,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (focus_handle, inset, blink_show, read_only) = {
            let area = self.area.read(cx);
            (
                area.focus_handle.clone(),
                area.left_inset,
                area.blink_show,
                area.read_only,
            )
        };
        if !read_only {
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.area.clone()),
                cx,
            );
        }
        let focused = focus_handle.is_focused(window);
        for wash in prepaint.washes.drain(..) {
            window.paint_quad(wash);
        }
        for sel in prepaint.selections.drain(..) {
            window.paint_quad(sel);
        }
        for rail in prepaint.rails.drain(..) {
            window.paint_quad(rail);
        }
        let line_height = prepaint.line_height;
        for (glyph, origin) in prepaint.gutters.drain(..) {
            glyph.paint(origin, line_height, window, cx).ok();
        }
        for (line_ix, line) in prepaint.lines.iter().enumerate() {
            let Some(line) = line else {
                continue;
            };
            let row = prepaint.rows.line_rows[line_ix];
            let origin = point(
                bounds.left() + inset,
                bounds.top() + line_height * row as f32,
            );
            line.paint(origin, line_height, window, cx).ok();
        }
        for label in prepaint.labels.drain(..) {
            window.paint_quad(label.backing);
            window.paint_quad(label.pill);
            label.line.paint(label.origin, line_height, window, cx).ok();
        }
        for block in prepaint.blocks.iter_mut() {
            block.paint(window, cx);
        }
        if focused
            && blink_show
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        // Row tooltips: `div().tooltip()` needs an interactive element, which
        // a custom Element is not — so drive the window directly from the
        // row's own hitbox. The view is cached per text, otherwise every
        // hovered frame would allocate another entity.
        for (hitbox, text) in prepaint.tooltips.drain(..) {
            if !hitbox.is_hovered(window) {
                continue;
            }
            let view = self.area.update(cx, |area, cx| {
                let mut cache = area.tooltip_view.borrow_mut();
                match &*cache {
                    Some((cached, view)) if *cached == text => view.clone(),
                    _ => {
                        let view: gpui::AnyView = cx
                            .new(|_| crate::components::tooltip::TextTooltip { text: text.clone() })
                            .into();
                        *cache = Some((text.clone(), view.clone()));
                        view
                    }
                }
            });
            window.set_tooltip(gpui::AnyTooltip {
                view,
                mouse_position: window.mouse_position(),
                // Visible for as long as the row stays hovered; the next
                // frame simply doesn't re-set it once the pointer leaves.
                check_visible_and_update: Rc::new(|_bounds, _window, _cx| true),
            });
            break;
        }
        let lines = std::mem::take(&mut prepaint.lines);
        let rows = std::mem::take(&mut prepaint.rows);
        let shapes = std::mem::take(&mut prepaint.shapes);
        self.area.update(cx, |area, _cx| {
            area.focused_hint = focused;
            *area.shape_cache.borrow_mut() = shapes;
            area.last_layout = Some(LastLayout {
                lines,
                rows,
                bounds,
                line_height,
            });
        });
    }
}

impl Focusable for TextArea {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TextArea {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // `min_w_full` + an explicit width is what gives an
        // `overflow_x_scroll` host something to scroll: never narrower than
        // the pane (so full-width row washes still span it), as wide as the
        // longest line when that is wider.
        let extent = self.scroll_extent(window);
        div()
            .w(extent)
            .min_w_full()
            .key_context(if self.read_only {
                "TextAreaReadOnly"
            } else {
                "TextArea"
            })
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::line_start))
            .on_action(cx.listener(Self::line_end))
            .on_action(cx.listener(Self::select_to_line_start))
            .on_action(cx.listener(Self::select_to_line_end))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::show_character_palette))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .flex_none()
            .child(TextAreaElement { area: cx.entity() })
    }
}

#[cfg(test)]
mod tests {
    use super::{RowHit, TextArea, line_of, line_ranges, row_map};
    use gpui::{Context, Window, div, prelude::*, px};

    #[test]
    fn line_ranges_tile_the_buffer() {
        assert_eq!(line_ranges(""), vec![0..0]);
        assert_eq!(line_ranges("a"), vec![0..1]);
        assert_eq!(line_ranges("a\n"), vec![0..2, 2..2]);
        assert_eq!(line_ranges("a\nbc\n"), vec![0..2, 2..5, 5..5]);
        assert_eq!(line_ranges("a\n\nb"), vec![0..2, 2..3, 3..4]);
    }

    #[test]
    fn line_of_maps_boundaries_to_the_right_row() {
        let ranges = line_ranges("ab\ncd\n");
        // "ab\n" = 0..3, "cd\n" = 3..6, "" = 6..6
        assert_eq!(line_of(&ranges, 0), 0);
        assert_eq!(line_of(&ranges, 2), 0, "before the terminator = same line");
        assert_eq!(line_of(&ranges, 3), 1, "after the terminator = next line");
        assert_eq!(line_of(&ranges, 5), 1);
        assert_eq!(line_of(&ranges, 6), 2, "trailing newline = empty last row");
    }

    #[test]
    fn row_map_interleaves_blocks_above_their_lines() {
        // 4 lines; a 1-row block above line 1 and a 2-row block above line 3.
        let map = row_map(4, &[(3, 2), (1, 1)]);
        assert_eq!(map.line_rows, vec![0, 2, 3, 6]);
        assert_eq!(map.blocks, vec![(1, 1, 1), (4, 2, 0)]);
        assert_eq!(map.total_rows, 7);
        assert_eq!(map.hit(0), RowHit::Line(0));
        assert_eq!(
            map.hit(1),
            RowHit::Block(1),
            "block row resolves to the line below it"
        );
        assert_eq!(map.hit(2), RowHit::Line(1));
        assert_eq!(map.hit(5), RowHit::Block(3));
        assert_eq!(map.hit(6), RowHit::Line(3));
        assert_eq!(map.hit(99), RowHit::Line(3), "past the end clamps");
    }

    #[test]
    fn row_map_trailing_block_and_no_blocks() {
        let map = row_map(2, &[(2, 1)]);
        assert_eq!(map.line_rows, vec![0, 1]);
        assert_eq!(map.blocks, vec![(2, 1, 0)]);
        assert_eq!(map.total_rows, 3);
        assert_eq!(map.hit(2), RowHit::Block(1));
        let plain = row_map(3, &[]);
        assert_eq!(plain.line_rows, vec![0, 1, 2]);
        assert_eq!(plain.total_rows, 3);
    }

    /// Blocks must stay inside the visible rect however far the text runs
    /// past it, or a decision strip's trailing buttons end up thousands of
    /// pixels off-screen (2026-08-27 report).
    #[test]
    fn blocks_stay_pinned_to_the_visible_rect() {
        use gpui::{Bounds, point, size};
        // The element is 5000 wide; only 800 of it is on screen, and the
        // pane has been scrolled 1200px to the right.
        let element = Bounds::new(point(px(-1200.), px(0.)), size(px(5000.), px(400.)));
        let visible = Bounds::new(point(px(0.), px(0.)), size(px(800.), px(400.)));
        let (left, width) = super::sticky_block_x(element, visible);
        assert_eq!(left, px(0.), "pinned to the left edge of what is visible");
        assert_eq!(width, px(800.), "and no wider than the viewport");

        // Unscrolled and narrower than the pane: the block simply fills it.
        let element = Bounds::new(point(px(0.), px(0.)), size(px(600.), px(400.)));
        let (left, width) = super::sticky_block_x(element, visible);
        assert_eq!((left, width), (px(0.), px(600.)));
    }

    /// A host that nests the editor exactly as the app does: inside a
    /// horizontally-scrolling pane.
    struct ScrollHost {
        area: gpui::Entity<TextArea>,
    }

    impl gpui::Render for ScrollHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> impl gpui::IntoElement {
            div()
                .id("host")
                .size_full()
                .overflow_x_scroll()
                .child(self.area.clone())
        }
    }

    /// The scroll extent is what makes `overflow_x_scroll` do anything: if
    /// the element keeps requesting 100% of its container there is nothing
    /// to scroll, which is exactly how the first attempt failed (the
    /// measure pass ran before the width was known, so it saw zero).
    #[gpui::test]
    fn a_long_line_makes_the_element_wider_than_its_container(cx: &mut gpui::TestAppContext) {
        let long = "x".repeat(600);
        let (host, vis) = cx.add_window_view(|_window, cx| {
            let area = cx.new(|cx| {
                let mut a = TextArea::read_only(format!("short\n{long}\n"), cx);
                a.set_left_inset(px(28.));
                a
            });
            ScrollHost { area }
        });
        vis.run_until_parked();

        let (width, extent) = vis.update(|_window, cx| {
            let area = host.read(cx).area.read(cx);
            let layout = area.last_layout.as_ref().expect("painted");
            (layout.bounds.size.width, area.content_width.get())
        });
        assert!(
            f32::from(extent) > 1000.,
            "a 600-char monospace line must produce a wide scroll extent, got {extent:?}"
        );
        assert!(
            f32::from(width) >= f32::from(extent),
            "the element must be laid out at least as wide as its content \
             ({width:?} vs {extent:?}) or there is nothing to scroll"
        );
    }
}
