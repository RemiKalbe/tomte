//! A lossless, iterative JSONC parser.
//!
//! The parser accepts JSON plus `//` and `/* */` comments and trailing commas
//! in objects and arrays. It keeps the original text and records byte ranges
//! for every key, value, comma and comment, so a later splicer can edit one
//! value and leave every other byte alone. Nesting never grows the Rust stack:
//! open containers live in an explicit stack.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;

/// Index into [`Document::nodes`].
pub type NodeId = usize;

/// A syntax error with the byte offset and 1-based line where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub offset: usize,
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at line {}", self.message, self.line)
    }
}

impl std::error::Error for ParseError {}

/// A parsed file: the original text plus a flat arena of value nodes.
#[derive(Debug)]
pub struct Document {
    pub text: String,
    pub nodes: Vec<Node>,
    /// `None` for an empty document (only whitespace and comments).
    pub root: Option<NodeId>,
    /// The text starts with U+FEFF. Ranges still count its three bytes.
    pub bom: bool,
    /// The first line break in the text is `\r\n`.
    pub crlf: bool,
    pub duplicates: Vec<Duplicate>,
}

/// One value. `range` covers the value only: no comma, no comments.
#[derive(Debug, Clone)]
pub struct Node {
    pub range: Range<usize>,
    pub kind: NodeKind,
}

#[derive(Debug, Clone)]
pub enum NodeKind {
    Object {
        entries: Vec<Entry>,
    },
    Array {
        elements: Vec<Element>,
    },
    /// The unescaped string.
    String(String),
    /// The raw literal, exactly as written.
    Number(String),
    Bool(bool),
    Null,
}

/// One `"key": value` member of an object.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The unescaped key.
    pub key: String,
    /// The key including its quotes.
    pub key_range: Range<usize>,
    pub value: NodeId,
    /// From the start of the key to the end of the value, or of the comma if
    /// there is one. A trailing comment written before the comma lies inside.
    pub range: Range<usize>,
    pub comma: Option<Range<usize>>,
    /// The contiguous comment lines directly above the entry, with no blank
    /// line between them or between the last one and the entry. It runs from
    /// the first comment's first byte through the line break after the last
    /// one. Empty, positioned at `range.start`, when there are none.
    pub leading: Range<usize>,
    /// A comment on the same line after the value or after the comma.
    pub trailing_comment: Option<Range<usize>>,
}

/// One value of an array, with the same trivia fields as [`Entry`].
#[derive(Debug, Clone)]
pub struct Element {
    pub value: NodeId,
    pub range: Range<usize>,
    pub comma: Option<Range<usize>>,
    pub leading: Range<usize>,
    pub trailing_comment: Option<Range<usize>>,
}

/// A key that appears more than once in one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Duplicate {
    pub object: NodeId,
    pub key: String,
    /// Indices into that object's entries, in order.
    pub entries: Vec<usize>,
}

/// Parse `text`. Duplicate keys are kept as separate entries and listed in
/// [`Document::duplicates`].
pub fn parse(text: &str) -> Result<Document, ParseError> {
    let mut parser = Parser {
        text,
        src: text.as_bytes(),
        bom: text.starts_with('\u{feff}'),
        nodes: Vec::new(),
        duplicates: Vec::new(),
        comments: Vec::new(),
    };
    let root = parser.run()?;
    let crlf = parser
        .src
        .iter()
        .position(|&b| b == b'\n')
        .is_some_and(|i| i > 0 && parser.src[i - 1] == b'\r');
    Ok(Document {
        text: text.to_string(),
        nodes: parser.nodes,
        root,
        bom: parser.bom,
        crlf,
        duplicates: parser.duplicates,
    })
}

/// The members collected so far for one open container.
enum Items {
    Object(Vec<Entry>),
    Array(Vec<Element>),
}

impl Items {
    fn closer(&self) -> u8 {
        match self {
            Items::Object(_) => b'}',
            Items::Array(_) => b']',
        }
    }
}

/// An entry or element whose value is still being parsed.
struct Pending {
    key: Option<(String, Range<usize>)>,
    start: usize,
    leading: Range<usize>,
}

/// One open container on the explicit stack.
struct Frame {
    start: usize,
    items: Items,
    pending: Option<Pending>,
}

enum State {
    /// `pos` is at the first byte of a value.
    Value,
    /// `pos` is after an opening bracket or a comma, or at the closing bracket.
    ItemOrClose,
}

enum Next {
    Root(NodeId),
    ItemOrClose(usize),
}

struct Parser<'a> {
    text: &'a str,
    src: &'a [u8],
    bom: bool,
    nodes: Vec<Node>,
    duplicates: Vec<Duplicate>,
    /// Comments met since the last item boundary, in order.
    comments: Vec<Range<usize>>,
}

impl Parser<'_> {
    fn error(&self, offset: usize, message: impl Into<String>) -> ParseError {
        ParseError {
            offset,
            line: line_at(self.src, offset),
            message: message.into(),
        }
    }

    fn unexpected(&self, pos: usize) -> ParseError {
        match self.text.get(pos..).and_then(|rest| rest.chars().next()) {
            Some(c) => self.error(pos, format!("unexpected character {c:?}")),
            None => self.error(pos, "unexpected end of input"),
        }
    }

    fn run(&mut self) -> Result<Option<NodeId>, ParseError> {
        let mut stack: Vec<Frame> = Vec::new();
        let first = if self.bom { 3 } else { 0 };
        let mut pos = self.scan_trivia(first)?;
        self.comments.clear();
        if pos >= self.src.len() {
            return Ok(None);
        }
        let mut state = State::Value;
        loop {
            match state {
                State::Value => {
                    let start = pos;
                    let (kind, end) = match self.src.get(pos).copied() {
                        Some(open @ (b'{' | b'[')) => {
                            let items = if open == b'{' {
                                Items::Object(Vec::new())
                            } else {
                                Items::Array(Vec::new())
                            };
                            stack.push(Frame {
                                start,
                                items,
                                pending: None,
                            });
                            self.comments.clear();
                            pos += 1;
                            state = State::ItemOrClose;
                            continue;
                        }
                        Some(b'"') => {
                            let (s, end) = self.parse_string(pos)?;
                            (NodeKind::String(s), end)
                        }
                        Some(b'-' | b'0'..=b'9') => {
                            let end = self.parse_number(pos)?;
                            (NodeKind::Number(self.text[pos..end].to_string()), end)
                        }
                        Some(b't') => (NodeKind::Bool(true), self.parse_word(pos, "true")?),
                        Some(b'f') => (NodeKind::Bool(false), self.parse_word(pos, "false")?),
                        Some(b'n') => (NodeKind::Null, self.parse_word(pos, "null")?),
                        _ => return Err(self.unexpected(pos)),
                    };
                    let id = self.push_node(start..end, kind);
                    match self.complete(&mut stack, id, end)? {
                        Next::Root(root) => return Ok(Some(root)),
                        Next::ItemOrClose(p) => {
                            pos = p;
                            state = State::ItemOrClose;
                        }
                    }
                }
                State::ItemOrClose => {
                    pos = self.scan_trivia(pos)?;
                    let Some(frame) = stack.last() else {
                        return Err(self.error(pos, "internal error: no open container"));
                    };
                    if pos >= self.src.len() {
                        return Err(self.unexpected(pos));
                    }
                    if self.src[pos] == frame.items.closer() {
                        let Some(frame) = stack.pop() else {
                            return Err(self.error(pos, "internal error: no open container"));
                        };
                        let id = self.nodes.len();
                        let kind = match frame.items {
                            Items::Object(entries) => {
                                self.record_duplicates(id, &entries);
                                NodeKind::Object { entries }
                            }
                            Items::Array(elements) => NodeKind::Array { elements },
                        };
                        self.push_node(frame.start..pos + 1, kind);
                        match self.complete(&mut stack, id, pos + 1)? {
                            Next::Root(root) => return Ok(Some(root)),
                            Next::ItemOrClose(p) => pos = p,
                        }
                        continue;
                    }
                    let item_start = pos;
                    let leading = self.leading_for(item_start);
                    self.comments.clear();
                    let is_object = matches!(frame.items, Items::Object(_));
                    let key = if is_object {
                        if self.src[pos] != b'"' {
                            return Err(self.error(pos, "expected a string key"));
                        }
                        let (key, end) = self.parse_string(pos)?;
                        let key_range = item_start..end;
                        pos = self.scan_trivia(end)?;
                        if self.src.get(pos) != Some(&b':') {
                            return Err(self.error(pos, "expected ':' after the key"));
                        }
                        pos = self.scan_trivia(pos + 1)?;
                        self.comments.clear();
                        Some((key, key_range))
                    } else {
                        None
                    };
                    if let Some(frame) = stack.last_mut() {
                        frame.pending = Some(Pending {
                            key,
                            start: item_start,
                            leading,
                        });
                    }
                    state = State::Value;
                }
            }
        }
    }

    fn push_node(&mut self, range: Range<usize>, kind: NodeKind) -> NodeId {
        self.nodes.push(Node { range, kind });
        self.nodes.len() - 1
    }

    fn record_duplicates(&mut self, object: NodeId, entries: &[Entry]) {
        let mut group_of: HashMap<&str, usize> = HashMap::new();
        let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            let g = *group_of.entry(entry.key.as_str()).or_insert_with(|| {
                groups.push((entry.key.as_str(), Vec::new()));
                groups.len() - 1
            });
            groups[g].1.push(i);
        }
        for (key, indices) in groups {
            if indices.len() > 1 {
                self.duplicates.push(Duplicate {
                    object,
                    key: key.to_string(),
                    entries: indices,
                });
            }
        }
    }

    /// A value ending at `end` is finished. Attach it to the open container
    /// (reading its comma and trailing comment), or accept it as the root.
    fn complete(
        &mut self,
        stack: &mut [Frame],
        id: NodeId,
        end: usize,
    ) -> Result<Next, ParseError> {
        let Some(frame) = stack.last_mut() else {
            let p = self.scan_trivia(end)?;
            if p < self.src.len() {
                return Err(self.error(p, "unexpected content after the top-level value"));
            }
            return Ok(Next::Root(id));
        };
        let Some(pending) = frame.pending.take() else {
            return Err(self.error(end, "internal error: value without an item"));
        };
        let closer = frame.items.closer();

        let mut trailing = self.same_line_comment(end)?;
        let mut p = trailing.as_ref().map_or(end, |r| r.end);
        self.comments.clear();
        p = self.scan_trivia(p)?;
        let mut comma = None;
        match self.src.get(p).copied() {
            Some(b',') => {
                comma = Some(p..p + 1);
                self.comments.clear();
                p += 1;
                if trailing.is_none() {
                    trailing = self.same_line_comment(p)?;
                    if let Some(r) = &trailing {
                        p = r.end;
                    }
                }
            }
            Some(c) if c == closer => {}
            Some(_) => {
                return Err(self.error(p, format!("expected ',' or '{}'", closer as char)));
            }
            None => return Err(self.unexpected(p)),
        }

        let range_end = comma.as_ref().map_or(end, |c| c.end);
        let range = pending.start..range_end;
        match (&mut frame.items, pending.key) {
            (Items::Object(entries), Some((key, key_range))) => entries.push(Entry {
                key,
                key_range,
                value: id,
                range,
                comma,
                leading: pending.leading,
                trailing_comment: trailing,
            }),
            (Items::Array(elements), None) => elements.push(Element {
                value: id,
                range,
                comma,
                leading: pending.leading,
                trailing_comment: trailing,
            }),
            _ => return Err(self.error(end, "internal error: item does not match container")),
        }
        Ok(Next::ItemOrClose(p))
    }

    /// Skip whitespace and comments from `p`, recording each comment in
    /// `self.comments`. Returns the offset of the next significant byte.
    fn scan_trivia(&mut self, mut p: usize) -> Result<usize, ParseError> {
        loop {
            match self.src.get(p).copied() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => p += 1,
                Some(b'/') => match self.comment_at(p)? {
                    Some(range) => {
                        p = range.end;
                        self.comments.push(range);
                    }
                    None => return Err(self.unexpected(p)),
                },
                _ => return Ok(p),
            }
        }
    }

    /// A comment that starts on the same line as `p`, after spaces and tabs.
    fn same_line_comment(&self, p: usize) -> Result<Option<Range<usize>>, ParseError> {
        let mut q = p;
        while matches!(self.src.get(q), Some(b' ' | b'\t')) {
            q += 1;
        }
        self.comment_at(q)
    }

    /// The comment starting exactly at `p`, if any. A line comment stops
    /// before its line break (and before the `\r` of a `\r\n`).
    fn comment_at(&self, p: usize) -> Result<Option<Range<usize>>, ParseError> {
        if self.src.get(p) != Some(&b'/') {
            return Ok(None);
        }
        match self.src.get(p + 1) {
            Some(b'/') => {
                let mut end = self.src[p..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(self.src.len(), |i| p + i);
                if end < self.src.len() && self.src[end - 1] == b'\r' {
                    end -= 1;
                }
                Ok(Some(p..end))
            }
            Some(b'*') => match self.src[p + 2..].windows(2).position(|w| w == b"*/") {
                Some(i) => Ok(Some(p..p + 2 + i + 2)),
                None => Err(self.error(p, "unterminated block comment")),
            },
            _ => Err(self.unexpected(p)),
        }
    }

    /// True when only spaces and tabs separate `pos` from the start of its line.
    fn starts_line(&self, pos: usize) -> bool {
        let mut i = pos;
        while i > 0 && matches!(self.src[i - 1], b' ' | b'\t') {
            i -= 1;
        }
        i == 0 || self.src[i - 1] == b'\n' || (self.bom && i == 3)
    }

    fn newlines_in(&self, range: Range<usize>) -> usize {
        self.src[range].iter().filter(|&&b| b == b'\n').count()
    }

    /// The contiguous comment lines directly above an item starting at
    /// `item_start`, taken from the comments recorded since the last boundary.
    fn leading_for(&self, item_start: usize) -> Range<usize> {
        let mut first = None;
        let mut next = item_start;
        for (i, c) in self.comments.iter().enumerate().rev() {
            if !self.starts_line(c.start) || self.newlines_in(c.end..next) != 1 {
                break;
            }
            first = Some(i);
            next = c.start;
        }
        let (Some(first), Some(last)) = (first, self.comments.last()) else {
            return item_start..item_start;
        };
        let newline = self.src[last.end..item_start]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(item_start, |i| last.end + i + 1);
        self.comments[first].start..newline
    }

    fn parse_word(&self, pos: usize, word: &str) -> Result<usize, ParseError> {
        let end = pos + word.len();
        if self.src.get(pos..end) == Some(word.as_bytes()) {
            Ok(end)
        } else {
            Err(self.error(pos, format!("invalid literal, expected {word:?}")))
        }
    }

    fn digit_run(&self, mut p: usize) -> usize {
        while self.src.get(p).is_some_and(u8::is_ascii_digit) {
            p += 1;
        }
        p
    }

    /// Validate a JSON number starting at `start`; returns its end offset.
    fn parse_number(&self, start: usize) -> Result<usize, ParseError> {
        let mut p = start;
        if self.src.get(p) == Some(&b'-') {
            p += 1;
        }
        match self.src.get(p) {
            Some(b'0') => p += 1,
            Some(b'1'..=b'9') => p = self.digit_run(p),
            _ => return Err(self.error(p, "invalid number")),
        }
        if self.src.get(p) == Some(&b'.') {
            let q = self.digit_run(p + 1);
            if q == p + 1 {
                return Err(self.error(p, "invalid number: no digits after the '.'"));
            }
            p = q;
        }
        if matches!(self.src.get(p), Some(b'e' | b'E')) {
            let mut q = p + 1;
            if matches!(self.src.get(q), Some(b'+' | b'-')) {
                q += 1;
            }
            let r = self.digit_run(q);
            if r == q {
                return Err(self.error(p, "invalid number: no digits in the exponent"));
            }
            p = r;
        }
        Ok(p)
    }

    fn hex4(&self, p: usize) -> Result<u32, ParseError> {
        let digits = self
            .text
            .get(p..p + 4)
            .filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| self.error(p, "invalid \\u escape"))?;
        u32::from_str_radix(digits, 16).map_err(|_| self.error(p, "invalid \\u escape"))
    }

    /// Parse the string starting at the quote at `start`; returns the
    /// unescaped value and the offset just past the closing quote.
    fn parse_string(&self, start: usize) -> Result<(String, usize), ParseError> {
        let mut out = String::new();
        let mut p = start + 1;
        let mut run = p;
        loop {
            let Some(&b) = self.src.get(p) else {
                return Err(self.error(start, "unterminated string"));
            };
            match b {
                b'"' => {
                    out.push_str(&self.text[run..p]);
                    return Ok((out, p + 1));
                }
                b'\\' => {
                    out.push_str(&self.text[run..p]);
                    let Some(&esc) = self.src.get(p + 1) else {
                        return Err(self.error(start, "unterminated string"));
                    };
                    p += 2;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4(p)?;
                            p += 4;
                            let code = if (0xD800..0xDC00).contains(&hi) {
                                if self.src.get(p..p + 2) != Some(b"\\u") {
                                    return Err(self.error(p, "unpaired surrogate in string"));
                                }
                                let lo = self.hex4(p + 2)?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(self.error(p, "unpaired surrogate in string"));
                                }
                                p += 6;
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else {
                                hi
                            };
                            let Some(c) = char::from_u32(code) else {
                                return Err(self.error(p, "unpaired surrogate in string"));
                            };
                            out.push(c);
                        }
                        _ => return Err(self.error(p - 2, "invalid escape in string")),
                    }
                    run = p;
                }
                0..=0x1f => return Err(self.error(p, "control character in string")),
                _ => p += 1,
            }
        }
    }
}

impl Document {
    /// True when the file has no value at all.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// The node for `id`. Panics when `id` did not come from this document.
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    /// The text covered by `range`. Panics when the range is not a valid slice of the text.
    pub fn slice(&self, range: &Range<usize>) -> &str {
        &self.text[range.clone()]
    }

    /// The 1-based line containing byte `offset`, counted by `\n`.
    pub fn line_of(&self, offset: usize) -> usize {
        line_at(self.text.as_bytes(), offset)
    }
}

fn line_at(src: &[u8], offset: usize) -> usize {
    1 + src[..offset.min(src.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every byte-range the document hands out, as leaf spans: brackets of
    /// containers, scalars, keys, commas, comments.
    fn spans(doc: &Document, id: NodeId, out: &mut Vec<Range<usize>>) {
        let node = doc.node(id);
        match &node.kind {
            NodeKind::Object { entries } => {
                out.push(node.range.start..node.range.start + 1);
                for e in entries {
                    if !e.leading.is_empty() {
                        out.push(e.leading.clone());
                    }
                    out.push(e.key_range.clone());
                    out.extend(e.comma.clone());
                    out.extend(e.trailing_comment.clone());
                    spans(doc, e.value, out);
                }
                out.push(node.range.end - 1..node.range.end);
            }
            NodeKind::Array { elements } => {
                out.push(node.range.start..node.range.start + 1);
                for e in elements {
                    if !e.leading.is_empty() {
                        out.push(e.leading.clone());
                    }
                    out.extend(e.comma.clone());
                    out.extend(e.trailing_comment.clone());
                    spans(doc, e.value, out);
                }
                out.push(node.range.end - 1..node.range.end);
            }
            _ => out.push(node.range.clone()),
        }
    }

    // Mutation: drop a comment's range from the tiling (never record the
    // trailing comment) -> its text lands in a gap and this fails.
    #[test]
    fn ranges_tile_the_input_byte_for_byte() {
        let text = "{\n  // lead\n  \"a\": 1, // trail\n  \"b\": [1, 2,],\n}\n";
        let doc = parse(text).unwrap();
        let mut all = Vec::new();
        spans(&doc, doc.root.unwrap(), &mut all);
        all.sort_by_key(|r| r.start);

        let mut cursor = 0;
        for r in &all {
            assert!(r.start >= cursor, "overlap at {r:?}, cursor {cursor}");
            let gap = &text[cursor..r.start];
            assert!(
                gap.chars().all(|c| c.is_whitespace() || c == ':'),
                "non-trivial gap {gap:?} before {r:?}"
            );
            cursor = r.end;
        }
        let tail = &text[cursor..];
        assert!(tail.chars().all(char::is_whitespace), "tail {tail:?}");

        let NodeKind::Object { entries } = &doc.node(doc.root.unwrap()).kind else {
            panic!("root is not an object");
        };
        assert_eq!(doc.slice(&entries[0].leading), "// lead\n");
        assert_eq!(
            doc.slice(entries[0].trailing_comment.as_ref().unwrap()),
            "// trail"
        );
        assert_eq!(doc.slice(entries[0].comma.as_ref().unwrap()), ",");
        assert_eq!(doc.slice(&entries[0].range), "\"a\": 1,");
        let NodeKind::Array { elements } = &doc.node(entries[1].value).kind else {
            panic!("b is not an array");
        };
        assert_eq!(elements.len(), 2);
        assert!(elements[0].comma.is_some());
        assert!(elements[1].comma.is_some());
    }

    // Mutation: never record the comma (`comma = None` in `complete`) -> the comma assertions fail.
    #[test]
    fn trailing_commas_and_both_comment_styles_parse() {
        let text = "{ \"a\": 1, /* c */ \"b\": 2, }";
        let doc = parse(text).unwrap();
        let root = doc.node(doc.root.unwrap());
        assert_eq!(root.range, 0..text.len());
        let NodeKind::Object { entries } = &root.kind else {
            panic!("root is not an object");
        };
        assert_eq!(entries.len(), 2);
        assert!(entries[1].comma.is_some());
        assert_eq!(
            doc.slice(entries[0].trailing_comment.as_ref().unwrap()),
            "/* c */"
        );
        assert!(entries[1].leading.is_empty());
    }

    // Mutation: decode the `n` escape to a literal `n` -> `"a\nb"` mismatches and this fails.
    #[test]
    fn strings_are_unescaped_numbers_kept_raw() {
        let doc = parse("{\"caf\\u00e9\": \"a\\nb\", \"n\": 1.50e2}").unwrap();
        let NodeKind::Object { entries } = &doc.node(doc.root.unwrap()).kind else {
            panic!("root is not an object");
        };
        assert_eq!(entries[0].key, "café");
        assert!(matches!(&doc.node(entries[0].value).kind, NodeKind::String(s) if s == "a\nb"));
        assert!(matches!(&doc.node(entries[1].value).kind, NodeKind::Number(n) if n == "1.50e2"));

        let doc = parse("\"\\ud83d\\ude00 \\\"q\\\" \\/\"").unwrap();
        assert!(
            matches!(&doc.node(doc.root.unwrap()).kind, NodeKind::String(s) if s == "😀 \"q\" /")
        );
        assert!(parse("\"\\ud83d\"").is_err(), "lone surrogate");
        assert!(parse("\"a\nb\"").is_err(), "raw newline in a string");
    }

    // Mutation: swallow a parse error into an empty document -> fails.
    #[test]
    fn errors_carry_offset_and_line() {
        let e = parse("{\n  \"a\": \n}").unwrap_err();
        assert_eq!((e.offset, e.line), (10, 3));
        assert_eq!(e.to_string(), format!("{} at line 3", e.message));

        let e = parse("{\"a\": 1").unwrap_err();
        assert_eq!((e.offset, e.line), (7, 1));

        assert!(parse("tru").is_err());
        assert!(parse("[1 2]").is_err());
        assert!(parse("{\"a\": 1} x").is_err());
        assert!(parse("/* open").is_err());
    }

    // Mutation: take the byte at the error offset as a char instead of
    // decoding from the text -> the message names the wrong character.
    #[test]
    fn errors_on_multibyte_text_do_not_panic_and_name_the_character() {
        let e = parse("{\n\"a\": é}").unwrap_err();
        assert_eq!((e.offset, e.line), (7, 2));
        assert!(e.message.contains('é'), "{}", e.message);
        let doc = parse("{\"日本\": \"é\"}").unwrap();
        assert_eq!(doc.line_of(3), 1);
        assert_eq!(doc.line_of(usize::MAX), 1);
    }

    // Mutation: drop the `record_duplicates` call -> `duplicates` stays empty and this fails.
    #[test]
    fn duplicate_keys_are_kept_and_listed() {
        let doc = parse("{\"a\": 1, \"a\": 2}").unwrap();
        let root = doc.root.unwrap();
        let NodeKind::Object { entries } = &doc.node(root).kind else {
            panic!("root is not an object");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(
            doc.duplicates,
            vec![Duplicate {
                object: root,
                key: "a".into(),
                entries: vec![0, 1]
            }]
        );
        assert!(parse("{\"a\": 1, \"b\": 2}").unwrap().duplicates.is_empty());
    }

    // Mutation: return an error instead of `Ok(None)` for a document with no value -> this fails.
    #[test]
    fn empty_and_comment_only_documents_have_no_root() {
        for text in ["", "  \n", "// nothing\n", "/* x */"] {
            let doc = parse(text).unwrap();
            assert!(doc.is_empty(), "{text:?}");
        }
    }

    // Mutation: force `bom` to false -> the BOM test fails; make `crlf` ignore which byte precedes the first `\n` -> the lone-LF half fails.
    #[test]
    fn bom_and_crlf_are_recorded() {
        let doc = parse("\u{feff}{\r\n\"a\": 1\r\n}").unwrap();
        assert!(doc.bom);
        assert!(doc.crlf);
        assert_eq!(doc.node(doc.root.unwrap()).range.start, 3);
        let doc = parse("{\n\"a\": 1\r\n}").unwrap();
        assert!(!doc.bom);
        assert!(!doc.crlf, "only the first line break decides");
    }

    // Mutation: recursive descent -> stack overflow at 10_000 levels. The
    // thread's stack is far too small for 10_000 recursive frames.
    #[test]
    fn ten_thousand_nested_arrays_parse() {
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let text = "[".repeat(10_000) + &"]".repeat(10_000);
                parse(&text).map(|d| d.nodes.len())
            })
            .unwrap();
        assert_eq!(handle.join().unwrap().unwrap(), 10_000);
    }

    // Mutation: accept leading zeros in `parse_number` (digit run after a 0) -> `[01]` parses and this fails.
    #[test]
    fn single_quotes_and_unquoted_keys_are_errors() {
        assert!(parse("{a: 1}").is_err());
        assert!(parse("{'a': 1}").is_err());
        assert!(parse("['a']").is_err());
        assert!(parse("[01]").is_err());
        assert!(parse("[1.]").is_err());
        assert!(parse("[,]").is_err());
        assert!(parse("[1,,2]").is_err());
    }

    // Mutation: stop requiring "no blank line" between comment lines (count
    // newlines >= 1 instead of == 1) -> the far comment joins the block.
    #[test]
    fn leading_is_the_contiguous_block_directly_above() {
        let text = "{\n  // far\n\n  // near\n  /* near2 */\n  \"a\": 1,\n  /* same */ \"b\": 2,\n  \"c\": 3 // t\n}";
        let doc = parse(text).unwrap();
        let NodeKind::Object { entries } = &doc.node(doc.root.unwrap()).kind else {
            panic!("root is not an object");
        };
        assert_eq!(doc.slice(&entries[0].leading), "// near\n  /* near2 */\n");
        assert!(entries[1].leading.is_empty());
        assert_eq!(entries[1].leading.start, entries[1].range.start);
        assert!(entries[2].leading.is_empty());
        assert_eq!(
            doc.slice(entries[2].trailing_comment.as_ref().unwrap()),
            "// t"
        );
    }

    // Mutation: start `line_at` at 0 instead of 1 -> line_of(0) is 0 and this fails.
    #[test]
    fn line_of_is_one_based_and_counts_newlines() {
        let doc = parse("{\n\"a\": 1,\n\"b\": 2\n}").unwrap();
        assert_eq!(doc.line_of(0), 1);
        assert_eq!(doc.line_of(1), 1);
        assert_eq!(doc.line_of(2), 2);
        assert_eq!(doc.line_of(doc.text.len()), 4);
    }
}
