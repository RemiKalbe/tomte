//! Multi-line plain-text editor (merge-editor-v2 spec step 4): a `String`
//! buffer with byte-offset cursor/selection, a custom Element that shapes
//! one line per `\n`, and a full UTF-16 [`EntityInputHandler`] so IME
//! composition (marked text), dictation, and press-and-hold accents work.
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

use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine, Style,
    TextRun, UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*, px,
    relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::Theme;

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

/// Key bindings for the editor, active only inside its "TextArea" context.
/// Hosts register these once at startup next to their own bindings. The
/// context is DEEPER than any host context (e.g. "MergeEditor"), so shared
/// keys like cmd-z resolve to the text history while the editor is focused.
pub fn bindings() -> Vec<KeyBinding> {
    const CTX: Option<&str> = Some("TextArea");
    vec![
        KeyBinding::new("backspace", Backspace, CTX),
        KeyBinding::new("delete", Delete, CTX),
        KeyBinding::new("left", Left, CTX),
        KeyBinding::new("right", Right, CTX),
        KeyBinding::new("up", Up, CTX),
        KeyBinding::new("down", Down, CTX),
        KeyBinding::new("shift-left", SelectLeft, CTX),
        KeyBinding::new("shift-right", SelectRight, CTX),
        KeyBinding::new("shift-up", SelectUp, CTX),
        KeyBinding::new("shift-down", SelectDown, CTX),
        KeyBinding::new("cmd-a", SelectAll, CTX),
        KeyBinding::new("home", LineStart, CTX),
        KeyBinding::new("end", LineEnd, CTX),
        KeyBinding::new("cmd-left", LineStart, CTX),
        KeyBinding::new("cmd-right", LineEnd, CTX),
        KeyBinding::new("ctrl-a", LineStart, CTX),
        KeyBinding::new("ctrl-e", LineEnd, CTX),
        KeyBinding::new("cmd-shift-left", SelectToLineStart, CTX),
        KeyBinding::new("cmd-shift-right", SelectToLineEnd, CTX),
        KeyBinding::new("cmd-up", DocStart, CTX),
        KeyBinding::new("cmd-down", DocEnd, CTX),
        KeyBinding::new("enter", Newline, CTX),
        KeyBinding::new("cmd-v", Paste, CTX),
        KeyBinding::new("cmd-c", Copy, CTX),
        KeyBinding::new("cmd-x", Cut, CTX),
        KeyBinding::new("cmd-z", Undo, CTX),
        KeyBinding::new("cmd-shift-z", Redo, CTX),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, CTX),
    ]
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
/// cursor geometry, and the IME's `bounds_for_range`.
struct LastLayout {
    lines: Vec<ShapedLine>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
}

pub struct TextArea {
    focus_handle: FocusHandle,
    content: String,
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
}

impl TextArea {
    /// A fresh editor seeded with `content`, caret at the start.
    pub fn new(content: String, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            preferred_x: None,
            last_layout: None,
            is_selecting: false,
            history: History::default(),
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Collapse the selection to a caret at byte `offset` (clamped). Callers
    /// pass char-boundary offsets — region starts are line starts.
    pub fn place_cursor(&mut self, offset: usize) {
        let offset = offset.min(self.content.len());
        self.selected_range = offset..offset;
        self.selection_reversed = false;
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
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
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
        let x = self.preferred_x.unwrap_or_else(|| {
            layout
                .lines
                .get(row)
                .map(|l| l.x_for_index(cursor - ranges[row].start))
                .unwrap_or(px(0.))
        });
        self.preferred_x = Some(x);
        let target = &ranges[target_row];
        let col = layout
            .lines
            .get(target_row)
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
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
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
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        // Any pending grouping window ends here: undo after undo walks
        // transactions, never re-groups.
        self.history.last_edit = None;
        if let Some(tx) = self.history.undo.pop() {
            self.content = tx.before.0.clone();
            self.selected_range = tx.before.1.clone();
            self.selection_reversed = false;
            self.marked_range = None;
            self.history.redo.push(tx);
            cx.notify();
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        self.history.last_edit = None;
        if let Some(tx) = self.history.redo.pop() {
            self.content = tx.after.0.clone();
            self.selected_range = tx.after.1.clone();
            self.selection_reversed = false;
            self.marked_range = None;
            self.history.undo.push(tx);
            cx.notify();
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    // ---- mouse -----------------------------------------------------------

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let Some(layout) = &self.last_layout else {
            return 0;
        };
        let bounds = &layout.bounds;
        if position.y < bounds.top() {
            return 0;
        }
        if position.y >= bounds.top() + layout.line_height * layout.lines.len() as f32 {
            return self.content.len();
        }
        let row = (f32::from(position.y - bounds.top()) / f32::from(layout.line_height)) as usize;
        let ranges = line_ranges(&self.content);
        let (Some(line), Some(range)) = (layout.lines.get(row), ranges.get(row)) else {
            return self.content.len();
        };
        let col = line.closest_index_for_x(position.x - bounds.left());
        (range.start + col).min(range.start + line_text(&self.content, range).len())
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
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
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let before = (self.content.clone(), self.selected_range.clone());
        self.content =
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..];
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        self.marked_range.take();
        self.preferred_x = None;
        self.history
            .record(before, (self.content.clone(), self.selected_range.clone()));
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
        // Composition steps land within the grouping window, so the whole
        // IME sequence collapses into one undo step (spec requirement).
        self.history
            .record(before, (self.content.clone(), self.selected_range.clone()));
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
        let line = layout.lines.get(row)?;
        let line_range = &ranges[row];
        let start_x = line.x_for_index(range.start - line_range.start);
        let end_x = if range.end <= line_range.end {
            line.x_for_index(range.end - line_range.start)
        } else {
            line.width
        };
        let top = bounds.top() + layout.line_height * row as f32;
        Some(Bounds::from_corners(
            point(bounds.left() + start_x, top),
            point(bounds.left() + end_x, top + layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let index = self.index_for_mouse_position(point);
        Some(self.offset_to_utf16(index))
    }
}

/// The custom Element: shapes one line per `\n`, paints selection quads →
/// glyph runs → caret, and registers the entity as the window's input
/// handler while focused (that registration is what makes IME work).
struct TextAreaElement {
    area: Entity<TextArea>,
}

struct AreaPrepaint {
    lines: Vec<ShapedLine>,
    line_height: Pixels,
    selections: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
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
        let lines = line_ranges(&self.area.read(cx).content).len();
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * lines as f32).into();
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
        let area = self.area.read(cx);
        let content = area.content.clone();
        let selected_range = area.selected_range.clone();
        let marked_range = area.marked_range.clone();
        let cursor_offset = area.cursor_offset();
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let theme = Theme::for_appearance(window.appearance());

        let ranges = line_ranges(&content);
        let mut lines = Vec::with_capacity(ranges.len());
        let mut selections = Vec::new();
        let mut cursor = None;
        for (row, range) in ranges.iter().enumerate() {
            let text = line_text(&content, range);
            let visible_range = range.start..range.start + text.len();
            // Runs: plain text, with the marked (IME composition) span
            // underlined where it intersects this line.
            let base_run = TextRun {
                len: text.len(),
                font: style.font(),
                color: style.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let runs = match &marked_range {
                Some(marked)
                    if marked.start < visible_range.end && visible_range.start < marked.end =>
                {
                    let m0 = marked.start.max(visible_range.start) - range.start;
                    let m1 = marked.end.min(visible_range.end) - range.start;
                    [
                        TextRun {
                            len: m0,
                            ..base_run.clone()
                        },
                        TextRun {
                            len: m1 - m0,
                            underline: Some(UnderlineStyle {
                                color: Some(style.color),
                                thickness: px(1.0),
                                wavy: false,
                            }),
                            ..base_run.clone()
                        },
                        TextRun {
                            len: text.len() - m1,
                            ..base_run.clone()
                        },
                    ]
                    .into_iter()
                    .filter(|run| run.len > 0)
                    .collect()
                }
                _ => vec![base_run],
            };
            let line =
                window
                    .text_system()
                    .shape_line(text.to_owned().into(), font_size, &runs, None);

            let top = bounds.top() + line_height * row as f32;
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
                        point(bounds.left() + line.x_for_index(s0), top),
                        point(bounds.left() + x1, top + line_height),
                    ),
                    Theme::wash(theme.accent, 0.25),
                ));
            }
            if selected_range.is_empty()
                && cursor_offset >= range.start
                && cursor_offset <= visible_range.end
                && (cursor_offset < range.end || row == ranges.len() - 1)
            {
                let x = line.x_for_index(cursor_offset - range.start);
                cursor = Some(fill(
                    Bounds::new(point(bounds.left() + x, top), size(px(2.), line_height)),
                    theme.accent,
                ));
            }
            lines.push(line);
        }
        AreaPrepaint {
            lines,
            line_height,
            selections,
            cursor,
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
        let focus_handle = self.area.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.area.clone()),
            cx,
        );
        for quad in prepaint.selections.drain(..) {
            window.paint_quad(quad);
        }
        let line_height = prepaint.line_height;
        for (row, line) in prepaint.lines.iter().enumerate() {
            let origin = point(bounds.left(), bounds.top() + line_height * row as f32);
            line.paint(origin, line_height, window, cx).ok();
        }
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        let lines = std::mem::take(&mut prepaint.lines);
        self.area.update(cx, |area, _cx| {
            area.last_layout = Some(LastLayout {
                lines,
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("TextArea")
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
            .w_full()
            .child(TextAreaElement { area: cx.entity() })
    }
}

#[cfg(test)]
mod tests {
    use super::{line_of, line_ranges};

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
}
