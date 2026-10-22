//! Assembly of the structured merge result (spec §5.4).
//!
//! The result starts from the source text. A container (object) whose
//! decisions all resolve to source is copied byte for byte. Any other
//! container is rebuilt from an ordered list of items — kept source entries,
//! entries with a replaced value, entries inserted from another side — glued
//! by the original text between two items that were neighbours in source, or
//! by a synthesized separator otherwise. Containers are rebuilt innermost
//! first, and a rebuilt container's text replaces its range wherever its
//! parent copies source text.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use thiserror::Error;

use super::jsonc::{self, Document, Element, Entry, NodeId, NodeKind, ParseError};
use super::merge::{Change, Decision, Pick, Resolution, Side, SideText, StructuredDocument};

/// Why [`StructuredDocument::assemble`] produced no text.
#[derive(Debug, Error)]
pub enum AssembleError {
    #[error("decision {id} ({path}) is a conflict with no pick")]
    Unresolved { id: usize, path: String },
    #[error("the merged JSON does not parse: {error}")]
    Invalid { error: ParseError },
}

impl StructuredDocument {
    /// The result text; errors on an unresolved unlocked conflict or when the result does not re-parse.
    pub fn assemble(&self, res: &Resolution) -> Result<String, AssembleError> {
        let mut picks = Vec::with_capacity(self.decisions.len());
        for d in &self.decisions {
            match self.effective_pick(res, d.id) {
                Some(pick) => picks.push(pick),
                None => {
                    return Err(AssembleError::Unresolved {
                        id: d.id,
                        path: d.path.clone(),
                    });
                }
            }
        }
        let text = self.render(&picks).text;
        jsonc::parse(&text).map_err(|error| AssembleError::Invalid { error })?;
        Ok(text)
    }

    /// Like assemble, but an unresolved conflict takes Source so the text is always valid (the editor seed).
    pub fn seed(&self, res: &Resolution) -> String {
        self.render(&self.seed_picks(res)).text
    }

    /// For each decision in order, its byte range in `seed(res)`; empty at the insertion point when absent from the result.
    ///
    /// A `Value` decision covers the value; any other covers the entry from
    /// its key through its value (no comma, no comments).
    pub fn result_ranges(&self, res: &Resolution) -> Vec<Range<usize>> {
        let out = self.render(&self.seed_picks(res));
        self.decisions
            .iter()
            .map(|d| match out.marks.get(&d.id) {
                Some(range) => range.clone(),
                None => self.mapped_span(&out, d),
            })
            .collect()
    }

    /// Lines of SOURCE that the pick's splice touches (byte ranges of whole lines) — Task 7's lock input.
    ///
    /// Each range runs from a line start to the next line start, or to the
    /// end of the text; the ranges are sorted and distinct.
    pub fn touched_lines(&self, id: usize, pick: Pick) -> Vec<Range<usize>> {
        let d = &self.decisions[id];
        let doc = &self.theirs;
        // An empty source has no line to protect.
        let Some(container) = d.site.container else {
            return Vec::new();
        };
        let mut spans = Vec::new();
        if d.path.is_empty() {
            if pick != Pick::Source {
                spans.push(doc.node(container).range.clone());
            }
            return lines_of(&doc.text, &spans);
        }
        let items = items_of(doc, container);
        let collapsed = self.collapsed_copies(d, container);
        for &j in &collapsed {
            spans.push(items[j].start..items[j].end);
        }
        if pick != Pick::Source {
            match (d.site.entry, self.chosen(d, pick)) {
                (Some(k), Some(_)) => spans.push(replaced_span(&items[k], &d.what)),
                (Some(k), None) => {
                    spans.push(items[k].start..items[k].end);
                    // The last entry of a container without trailing
                    // commas takes the previous entry's comma with it.
                    let last = k + 1 == items.len();
                    if last && items[k].comma.is_none() {
                        let previous = (0..k).rev().find(|j| !collapsed.contains(j));
                        if let Some(comma) = previous.and_then(|j| items[j].comma.clone()) {
                            spans.push(comma);
                        }
                    }
                }
                (None, Some(_)) => {
                    let at = match items.len() {
                        // An empty container is reopened before its bracket.
                        0 => {
                            let close = doc.node(container).range.end - 1;
                            close..close + 1
                        }
                        n => {
                            // The predecessor's last line gets the comma and
                            // the new lines follow it.
                            let p = &items[d.site.after.unwrap_or(n - 1).min(n - 1)];
                            p.value.end..p.end
                        }
                    };
                    spans.push(at);
                }
                (None, None) => {}
            }
        }
        lines_of(&doc.text, &spans)
    }

    /// Blanks out the source's final closing bracket so that any assembled
    /// text fails the re-parse guard. The byte is replaced by a space, so
    /// every recorded range stays a valid slice.
    #[cfg(any(test, feature = "test-support"))]
    pub fn corrupt_source_for_tests(&mut self) {
        if let Some(i) = self.theirs.text.rfind(['}', ']']) {
            self.theirs.text.replace_range(i..i + 1, " ");
        }
    }

    /// The pick that applies: the user's, else the automatic one; a locked
    /// decision follows source. `None` is a conflict still owed — and so is
    /// `Both`, which only array runs accept (none exist yet).
    fn effective_pick(&self, res: &Resolution, id: usize) -> Option<Pick> {
        if self.decisions[id].locked.is_some() {
            return Some(Pick::Source);
        }
        match res.get(id).or_else(|| self.auto_pick(id))? {
            Pick::Both => None,
            pick => Some(pick),
        }
    }

    fn seed_picks(&self, res: &Resolution) -> Vec<Pick> {
        self.decisions
            .iter()
            .map(|d| self.effective_pick(res, d.id).unwrap_or(Pick::Source))
            .collect()
    }

    /// The side a pick selects, with its text for this decision; `None` when
    /// that side does not have the entry, so the result must not either.
    fn chosen<'a>(&'a self, d: &'a Decision, pick: Pick) -> Option<(&'a Document, &'a SideText)> {
        let (doc, text) = match pick {
            Pick::Disk => (&self.ours, &d.ours),
            Pick::LastWritten => (&self.base, &d.base),
            // `effective_pick` never yields Both.
            Pick::Source | Pick::Both => (&self.theirs, &d.theirs),
        };
        text.as_ref().map(|text| (doc, text))
    }

    /// The earlier copies of a key repeated in source, which the result
    /// drops whatever the pick (unless a template locks the row).
    fn collapsed_copies(&self, d: &Decision, container: NodeId) -> Vec<usize> {
        let (Some(k), Some(Side::Theirs), None) = (d.site.entry, d.duplicated, d.locked) else {
            return Vec::new();
        };
        let entries = entries_of(&self.theirs, container);
        (0..entries.len())
            .filter(|&j| j != k && entries[j].key == entries[k].key)
            .collect()
    }

    /// The result for `picks` (one per decision, never Both), with where
    /// each decision's text landed.
    fn render(&self, picks: &[Pick]) -> Out {
        let src = self.theirs.text.as_str();
        let mut by_container: BTreeMap<NodeId, Vec<usize>> = BTreeMap::new();
        let mut rootless = Vec::new();
        let mut rebuilt = Rebuilt::new();
        for d in &self.decisions {
            match d.site.container {
                None => rootless.push(d.id),
                Some(root) if d.path.is_empty() => {
                    self.replace_root(d, root, picks[d.id], &mut rebuilt)
                }
                Some(container) => by_container.entry(container).or_default().push(d.id),
            }
        }
        // Innermost first: a child's range is strictly shorter than its parent's.
        let mut containers: Vec<NodeId> = by_container.keys().copied().collect();
        containers.sort_by_key(|&c| self.theirs.node(c).range.len());
        for container in containers {
            let plan = self.plan(container, &by_container[&container], picks);
            if plan.changes() {
                let range = self.theirs.node(container).range.clone();
                let out = self.rebuild(container, &plan, &mut rebuilt);
                rebuilt.insert(range.start, (range.end, out));
            }
        }
        let mut out = Out::default();
        out.copy_with(src, 0..src.len(), &mut rebuilt);
        self.append_root(&rootless, picks, &mut out);
        out
    }

    /// A root array decided whole: the chosen side's array replaces it.
    fn replace_root(&self, d: &Decision, root: NodeId, pick: Pick, rebuilt: &mut Rebuilt) {
        if pick == Pick::Source {
            return;
        }
        let range = self.theirs.node(root).range.clone();
        let mut out = Out::default();
        if let Some((_, text)) = self.chosen(d, pick) {
            out.push(&with_newlines(&text.text, self.theirs.crlf));
        }
        out.marks.insert(d.id, 0..out.text.len());
        rebuilt.insert(range.start, (range.end, out));
    }

    /// The source has no root: the chosen entries become a new object after
    /// whatever comments the source holds (or the chosen array, for a root
    /// array decided whole). LF endings, two-space indent, no BOM.
    fn append_root(&self, ids: &[usize], picks: &[Pick], out: &mut Out) {
        let mut inserts = Vec::new();
        let mut whole = None;
        for &id in ids {
            let d = &self.decisions[id];
            match self.chosen(d, picks[id]) {
                Some((_, text)) if d.path.is_empty() => whole = Some((id, text)),
                Some((doc, text)) => match find_entry(doc, text.range.start, Span::Entry) {
                    Some(entry) => inserts.push(insertion(doc, entry, id, "  ", false)),
                    None => lost_entry(d, text),
                },
                None => {}
            }
        }
        if whole.is_none() && inserts.is_empty() {
            return;
        }
        if !out.text.is_empty() && !out.text.ends_with('\n') {
            out.push("\n");
        }
        if let Some((id, text)) = whole {
            let at = out.text.len();
            out.push(&with_newlines(&text.text, false));
            out.marks.insert(id, at..out.text.len());
            out.push("\n");
            return;
        }
        out.push("{");
        let mut walk = Walk::new(false, "  ".to_string(), "\n");
        for insert in &inserts {
            walk.insert(insert);
        }
        out.append(walk.finish(false, "\n"));
        out.push("}\n");
    }

    /// What each item of one source container becomes, and what is inserted
    /// into each gap.
    fn plan(&self, container: NodeId, ids: &[usize], picks: &[Pick]) -> Plan {
        let src = &self.theirs;
        let items = items_of(src, container);
        let gaps = items.len().max(1);
        let indent = indent_unit(src, container);
        let mut plan = Plan {
            fates: vec![Fate::Keep; items.len()],
            marks: vec![None; items.len()],
            inserts: (0..gaps).map(|_| Vec::new()).collect(),
            absent: vec![Vec::new(); gaps],
        };
        for &id in ids {
            let d = &self.decisions[id];
            let pick = picks[id];
            let span = if d.what == Change::Value {
                Span::Value
            } else {
                Span::Entry
            };
            let Some(k) = d.site.entry else {
                let gap = d.site.after.unwrap_or(gaps - 1).min(gaps - 1);
                let insert = self.chosen(d, pick).and_then(|(doc, text)| {
                    let entry = find_entry(doc, text.range.start, Span::Entry);
                    if entry.is_none() {
                        lost_entry(d, text);
                    }
                    entry.map(|entry| insertion(doc, entry, id, &indent, src.crlf))
                });
                match insert {
                    Some(insert) => plan.inserts[gap].push(insert),
                    None => plan.absent[gap].push(id),
                }
                continue;
            };
            for j in self.collapsed_copies(d, container) {
                plan.fates[j] = Fate::Drop;
            }
            plan.marks[k] = Some((id, span));
            if pick == Pick::Source {
                continue;
            }
            match self.chosen(d, pick) {
                None => plan.fates[k] = Fate::Drop,
                Some((doc, text)) => match find_entry(doc, text.range.start, span) {
                    Some(entry) => {
                        plan.fates[k] =
                            replacement(&src.text, &items[k], doc, entry, span, src.crlf);
                    }
                    None => lost_entry(d, text),
                },
            }
        }
        plan
    }

    /// One container's text from its plan: the items in order, each pair
    /// glued by the original text between them when they were neighbours in
    /// source and by [`glue`] otherwise.
    fn rebuild(&self, container: NodeId, plan: &Plan, rebuilt: &mut Rebuilt) -> Out {
        let doc = &self.theirs;
        let src = doc.text.as_str();
        let range = doc.node(container).range.clone();
        // `inner` is the first byte after the opening bracket, `close` the
        // closing bracket.
        let (inner, close) = (range.start + 1, range.end - 1);
        let items = items_of(doc, container);
        let n = items.len();
        let dropped = |k: usize| matches!(plan.fates[k], Fate::Drop);
        let mut walk = Walk::new(
            is_inline(doc, container),
            indent_unit(doc, container),
            newline(doc),
        );
        walk.out.copy(src, range.start..inner);
        let tail = if n == 0 {
            // An empty container is reopened on a new line; comments inside
            // it stay above the new entries.
            let kept = src[inner..close].trim_end().len();
            walk.out.copy(src, inner..inner + kept);
            let close_indent = if starts_line(src, close) {
                indent_of(src, close)
            } else {
                indent_of(src, range.start)
            };
            format!("{}{close_indent}", walk.nl)
        } else {
            walk.pending = trim(&src[inner..items[0].start], false, dropped(0)).to_string();
            walk.verbatim = !dropped(0);
            trim(&src[items[n - 1].end..close], dropped(n - 1), false).to_string()
        };
        for gap in 0..n.max(1) {
            if gap < n {
                walk.original(src, &items[gap], &plan.fates[gap], plan.marks[gap], rebuilt);
            }
            for insert in &plan.inserts[gap] {
                walk.insert(insert);
            }
            walk.deferred.extend(&plan.absent[gap]);
            if gap + 1 < n {
                let sep = trim(
                    &src[items[gap].end..items[gap + 1].start],
                    dropped(gap),
                    dropped(gap + 1),
                );
                if !dropped(gap) && !dropped(gap + 1) && plan.inserts[gap].is_empty() {
                    walk.pending = sep.to_string();
                    walk.verbatim = true;
                } else {
                    walk.pending.push_str(sep);
                }
            }
        }
        let trailing = items.last().is_some_and(|item| item.comma.is_some());
        let mut out = walk.finish(trailing, &tail);
        out.copy(src, close..range.end);
        out
    }

    /// Where a decision without a recorded mark sits in the result: its
    /// source span mapped through the verbatim copies.
    fn mapped_span(&self, out: &Out, d: &Decision) -> Range<usize> {
        let span = self.source_span(d);
        match (out.map(span.start), out.map(span.end)) {
            (Some(start), Some(end)) if start <= end => start..end,
            (Some(start), _) => start..start,
            _ => out.text.len()..out.text.len(),
        }
    }

    /// A decision's place in source: its value, or its entry from key to
    /// value, or the empty insertion point when source lacks the key.
    fn source_span(&self, d: &Decision) -> Range<usize> {
        let doc = &self.theirs;
        let Some(container) = d.site.container else {
            return doc.text.len()..doc.text.len();
        };
        if d.path.is_empty() {
            return doc.node(container).range.clone();
        }
        let items = items_of(doc, container);
        match d.site.entry {
            Some(k) => replaced_span(&items[k], &d.what),
            None => {
                let at = match items.len() {
                    0 => doc.node(container).range.end - 1,
                    n => items[d.site.after.unwrap_or(n - 1).min(n - 1)].end,
                };
                at..at
            }
        }
    }
}

/// The source span a non-source pick rewrites on an entry source keeps:
/// the value for a value change, else the key through the value.
fn replaced_span(item: &Item, what: &Change) -> Range<usize> {
    if *what == Change::Value {
        item.value.clone()
    } else {
        item.head..item.value.end
    }
}

fn lost_entry(d: &Decision, text: &SideText) {
    crate::log_error!(
        "structured",
        "decision {} ({}): no entry starts at byte {} of the chosen side; source kept",
        d.id,
        d.path,
        text.range.start
    );
}

/// One member of a container, seen the same way for an object entry and an
/// array element.
#[derive(Debug, Clone)]
pub(crate) struct Item {
    /// The start of the leading comment block, or of the item without one.
    pub(crate) start: usize,
    /// The key's first byte; the value's for an array element.
    pub(crate) head: usize,
    pub(crate) value: Range<usize>,
    pub(crate) comma: Option<Range<usize>>,
    /// The end of the comma or of a same-line comment, whichever is later.
    pub(crate) end: usize,
}

impl Item {
    pub(crate) fn of_entry(doc: &Document, entry: &Entry) -> Self {
        Self::new(
            doc,
            entry.leading.start,
            entry.key_range.start,
            entry.value,
            entry.range.end,
            &entry.comma,
            &entry.trailing_comment,
        )
    }

    pub(crate) fn of_element(doc: &Document, element: &Element) -> Self {
        Self::new(
            doc,
            element.leading.start,
            element.range.start,
            element.value,
            element.range.end,
            &element.comma,
            &element.trailing_comment,
        )
    }

    fn new(
        doc: &Document,
        start: usize,
        head: usize,
        value: NodeId,
        range_end: usize,
        comma: &Option<Range<usize>>,
        comment: &Option<Range<usize>>,
    ) -> Self {
        Self {
            start,
            head,
            value: doc.node(value).range.clone(),
            comma: comma.clone(),
            end: comment.as_ref().map_or(range_end, |c| c.end.max(range_end)),
        }
    }
}

/// The items of an object or array node; none for a scalar.
pub(crate) fn items_of(doc: &Document, node: NodeId) -> Vec<Item> {
    match &doc.node(node).kind {
        NodeKind::Object { entries } => entries.iter().map(|e| Item::of_entry(doc, e)).collect(),
        NodeKind::Array { elements } => elements.iter().map(|e| Item::of_element(doc, e)).collect(),
        _ => Vec::new(),
    }
}

fn entries_of(doc: &Document, node: NodeId) -> &[Entry] {
    match &doc.node(node).kind {
        NodeKind::Object { entries } => entries,
        _ => &[],
    }
}

/// The entry whose key (`Span::Entry`) or value (`Span::Value`) starts at
/// `start`.
fn find_entry(doc: &Document, start: usize, span: Span) -> Option<&Entry> {
    doc.nodes.iter().find_map(|node| match &node.kind {
        NodeKind::Object { entries } => entries.iter().find(|e| match span {
            Span::Value => doc.node(e.value).range.start == start,
            Span::Entry => e.range.start == start,
        }),
        _ => None,
    })
}

/// A source entry rewritten with the chosen side's value (a value change)
/// or its key and value (any other change), re-indented to the source entry.
fn replacement(
    src: &str,
    item: &Item,
    doc: &Document,
    entry: &Entry,
    span: Span,
    crlf: bool,
) -> Fate {
    let value = doc.node(entry.value).range.clone();
    let (from, text) = match span {
        Span::Value => (item.value.start, value),
        Span::Entry => (item.head, entry.key_range.start..value.end),
    };
    let text = reindent(
        &doc.text[text],
        indent_of(&doc.text, entry.key_range.start),
        indent_of(src, item.head),
    );
    Fate::Replace {
        from,
        text: with_newlines(&text, crlf),
    }
}

/// The chosen side's entry — leading comments, key, value, same-line
/// comment — re-indented to `indent` for insertion.
fn insertion(doc: &Document, entry: &Entry, id: usize, indent: &str, crlf: bool) -> Insert {
    let item = Item::of_entry(doc, entry);
    let from = indent_of(&doc.text, item.head);
    let lead = with_newlines(
        &reindent(&doc.text[item.start..item.head], from, indent),
        crlf,
    );
    let rest = with_newlines(
        &reindent(&doc.text[item.head..item.value.end], from, indent),
        crlf,
    );
    Insert {
        id,
        key_at: lead.len(),
        body: lead + &rest,
        suffix: Suffix::of(&doc.text, &item),
    }
}

/// What becomes of one source item.
#[derive(Debug, Clone)]
enum Fate {
    Keep,
    Drop,
    /// The source text from `from` to the value's end is replaced by `text`.
    Replace {
        from: usize,
        text: String,
    },
}

/// Which part of an item a decision's result range covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Span {
    Value,
    Entry,
}

/// An item that is not in source, ready to write.
#[derive(Debug)]
struct Insert {
    id: usize,
    body: String,
    /// Where the key starts in `body`, after any leading comments.
    key_at: usize,
    suffix: Suffix,
}

/// One container's rebuild instructions.
struct Plan {
    fates: Vec<Fate>,
    /// The decision that owns each source item, and what its range covers.
    marks: Vec<Option<(usize, Span)>>,
    /// Gap `k` is after source item `k`; an empty container has one gap.
    inserts: Vec<Vec<Insert>>,
    /// Decisions absent from the result, by the gap they would occupy.
    absent: Vec<Vec<usize>>,
}

impl Plan {
    fn changes(&self) -> bool {
        self.fates.iter().any(|f| !matches!(f, Fate::Keep))
            || self.inserts.iter().any(|gap| !gap.is_empty())
    }
}

/// What follows an item's value: its comma (if any) and same-line comment.
#[derive(Debug, Clone)]
struct Suffix {
    /// The text between the value and the comma, or all of it without one.
    before: String,
    comma: bool,
    after: String,
}

impl Suffix {
    fn of(text: &str, item: &Item) -> Self {
        match &item.comma {
            Some(c) => Self {
                before: text[item.value.end..c.start].to_string(),
                comma: true,
                after: text[c.end..item.end].to_string(),
            },
            None => Self {
                before: text[item.value.end..item.end].to_string(),
                comma: false,
                after: String::new(),
            },
        }
    }

    /// The suffix with or without a comma. A comma the item lacked goes
    /// right after the value, before any same-line comment.
    fn render(&self, comma: bool) -> String {
        match (self.comma, comma) {
            (true, true) => format!("{},{}", self.before, self.after),
            (true, false) => format!("{}{}", self.before, self.after),
            (false, true) => format!(",{}", self.before),
            (false, false) => self.before.clone(),
        }
    }
}

/// The separator rule. `pending` is the source text that survived between
/// two items that were not neighbours in source (or before the first item
/// when it is not source's first). An inline container glues with one
/// space; a multi-line one keeps the surviving text and makes sure it ends
/// on a fresh line at the container's indent. The comma comes from
/// [`Suffix::render`].
fn glue(pending: &str, first: bool, inline: bool, indent: &str, nl: &str) -> String {
    if inline {
        return if pending.is_empty() && !first {
            " ".to_string()
        } else {
            pending.to_string()
        };
    }
    match pending.rfind('\n') {
        None => format!("{}{nl}{indent}", pending.trim_end()),
        Some(i) if i + 1 == pending.len() => format!("{pending}{indent}"),
        Some(_) => pending.to_string(),
    }
}

/// The part of the source text between two items that survives when the
/// item on its left and/or right is dropped: a dropped item takes the rest
/// of its last line (through the first line break) and the indent of its
/// first line (after the last line break). Without a line break, all of it.
fn trim(sep: &str, left_dropped: bool, right_dropped: bool) -> &str {
    let mut s = sep;
    if left_dropped {
        s = s.find('\n').map_or("", |i| &s[i + 1..]);
    }
    if right_dropped {
        s = s.rfind('\n').map_or("", |i| &s[..=i]);
    }
    s
}

/// Writes one container's items in order and keeps the separator pending
/// until the next item shows whether it was a source neighbour.
struct Walk {
    out: Out,
    /// The suffix of the last item written; its comma depends on what follows.
    prev: Option<Suffix>,
    pending: String,
    /// `pending` is the untouched source separator between the last item
    /// and the next one.
    verbatim: bool,
    /// Decisions absent from the result, marked where the next text starts.
    deferred: Vec<usize>,
    inline: bool,
    indent: String,
    nl: &'static str,
}

impl Walk {
    fn new(inline: bool, indent: String, nl: &'static str) -> Self {
        Self {
            out: Out::default(),
            prev: None,
            pending: String::new(),
            verbatim: false,
            deferred: Vec::new(),
            inline,
            indent,
            nl,
        }
    }

    /// Writes what precedes the next item: the previous item's suffix with
    /// a comma, then the separator.
    fn begin(&mut self) {
        let first = match self.prev.take() {
            Some(suffix) => {
                self.out.push(&suffix.render(true));
                false
            }
            None => true,
        };
        let sep = if self.verbatim {
            std::mem::take(&mut self.pending)
        } else {
            glue(&self.pending, first, self.inline, &self.indent, self.nl)
        };
        self.out.push(&sep);
        self.pending.clear();
        self.verbatim = false;
        let at = self.out.text.len();
        for id in self.deferred.drain(..) {
            self.out.marks.insert(id, at..at);
        }
    }

    /// A source item, kept (with any rebuilt container inside it), with a
    /// replaced value, or dropped.
    fn original(
        &mut self,
        src: &str,
        item: &Item,
        fate: &Fate,
        mark: Option<(usize, Span)>,
        rebuilt: &mut Rebuilt,
    ) {
        if let Fate::Drop = fate {
            self.deferred.extend(mark.map(|(id, _)| id));
            return;
        }
        self.begin();
        let at = self.out.text.len();
        match fate {
            Fate::Replace { from, text } => {
                self.out.copy(src, item.start..*from);
                self.out.push(text);
            }
            _ => self.out.copy_with(src, item.start..item.value.end, rebuilt),
        }
        if let Some((id, span)) = mark {
            let start = match span {
                Span::Value => item.value.start,
                Span::Entry => item.head,
            };
            // Only text before the value can precede `start`, and it is
            // copied as it was.
            self.out
                .marks
                .insert(id, at + (start - item.start)..self.out.text.len());
        }
        self.prev = Some(Suffix::of(src, item));
    }

    fn insert(&mut self, insert: &Insert) {
        self.begin();
        let at = self.out.text.len();
        self.out.push(&insert.body);
        self.out
            .marks
            .insert(insert.id, at + insert.key_at..self.out.text.len());
        self.prev = Some(insert.suffix.clone());
    }

    /// Closes the container: the last item's suffix (a comma only in
    /// trailing-comma style), any surviving separator text, then `tail`.
    fn finish(mut self, trailing: bool, tail: &str) -> Out {
        if let Some(suffix) = self.prev.take() {
            self.out.push(&suffix.render(trailing));
        }
        let at = self.out.text.len();
        for id in self.deferred.drain(..) {
            self.out.marks.insert(id, at..at);
        }
        let pending = std::mem::take(&mut self.pending);
        self.out.push(&pending);
        self.out.push(tail);
        self.out
    }
}

/// Rebuilt containers by start offset: their end offset and new text.
type Rebuilt = BTreeMap<usize, (usize, Out)>;

/// Text being assembled, with the source runs it copied and the decision
/// ranges recorded so far.
#[derive(Debug, Default)]
struct Out {
    text: String,
    copies: Vec<Verbatim>,
    marks: HashMap<usize, Range<usize>>,
}

/// A run of source text copied unchanged.
#[derive(Debug, Clone, Copy)]
struct Verbatim {
    src: usize,
    out: usize,
    len: usize,
}

impl Out {
    fn push(&mut self, text: &str) {
        self.text.push_str(text);
    }

    fn copy(&mut self, src: &str, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.copies.push(Verbatim {
            src: range.start,
            out: self.text.len(),
            len: range.len(),
        });
        self.text.push_str(&src[range]);
    }

    fn append(&mut self, other: Out) {
        let shift = self.text.len();
        self.text.push_str(&other.text);
        self.copies
            .extend(other.copies.into_iter().map(|c| Verbatim {
                out: c.out + shift,
                ..c
            }));
        self.marks.extend(
            other
                .marks
                .into_iter()
                .map(|(id, r)| (id, r.start + shift..r.end + shift)),
        );
    }

    /// Copies `range` of source, writing each rebuilt container inside it in
    /// place of its source text (and consuming it).
    fn copy_with(&mut self, src: &str, range: Range<usize>, rebuilt: &mut Rebuilt) {
        let mut cursor = range.start;
        let starts: Vec<usize> = rebuilt.range(range.clone()).map(|(&s, _)| s).collect();
        for start in starts {
            let fits = rebuilt
                .get(&start)
                .is_some_and(|(end, _)| start >= cursor && *end <= range.end);
            if !fits {
                continue;
            }
            if let Some((end, sub)) = rebuilt.remove(&start) {
                self.copy(src, cursor..start);
                self.append(sub);
                cursor = end;
            }
        }
        self.copy(src, cursor..range.end);
    }

    /// Where source byte `offset` landed, when a verbatim run holds it.
    fn map(&self, offset: usize) -> Option<usize> {
        self.copies
            .iter()
            .find(|c| c.src <= offset && offset <= c.src + c.len)
            .map(|c| c.out + offset - c.src)
    }
}

fn newline(doc: &Document) -> &'static str {
    if doc.crlf { "\r\n" } else { "\n" }
}

/// `text` with every line break written the way the destination writes them.
fn with_newlines(text: &str, crlf: bool) -> String {
    let lf = text.replace("\r\n", "\n");
    if crlf { lf.replace('\n', "\r\n") } else { lf }
}

fn line_start(text: &str, offset: usize) -> usize {
    let start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    if start == 0 && text.starts_with('\u{feff}') {
        3.min(offset)
    } else {
        start
    }
}

/// Only spaces and tabs precede `offset` on its line.
fn starts_line(text: &str, offset: usize) -> bool {
    text[line_start(text, offset)..offset]
        .bytes()
        .all(|b| b == b' ' || b == b'\t')
}

/// The spaces and tabs that start the line holding `offset`.
pub(crate) fn indent_of(text: &str, offset: usize) -> &str {
    let line = &text[line_start(text, offset)..];
    let width = line.len() - line.trim_start_matches([' ', '\t']).len();
    &line[..width]
}

/// The indent of a container's entries: its first entry's, or for an empty
/// container the indent of its line plus the file's unit (the indent of the
/// first indented line, two spaces when there is none).
pub(crate) fn indent_unit(doc: &Document, container: NodeId) -> String {
    match items_of(doc, container).first() {
        Some(item) => indent_of(&doc.text, item.head).to_string(),
        None => {
            let unit = doc
                .text
                .lines()
                .find_map(|line| {
                    let body = line.trim_start_matches([' ', '\t']);
                    let width = line.len() - body.len();
                    (width > 0 && !body.trim().is_empty()).then(|| &line[..width])
                })
                .unwrap_or("  ");
            format!(
                "{}{unit}",
                indent_of(&doc.text, doc.node(container).range.start)
            )
        }
    }
}

/// A container whose first item sits on the opening bracket's line. An
/// empty container is not inline: it is reopened on a new line.
pub(crate) fn is_inline(doc: &Document, container: NodeId) -> bool {
    let inner = doc.node(container).range.start + 1;
    items_of(doc, container)
        .first()
        .is_some_and(|item| !doc.text[inner..item.start].contains('\n'))
}

/// Re-indents every line after the first: a line that starts with `from`
/// gets `to` in its place; any other line is left alone.
pub(crate) fn reindent(text: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i == 0 {
            out.push_str(line);
            continue;
        }
        out.push('\n');
        match line.strip_prefix(from) {
            Some(rest) => {
                out.push_str(to);
                out.push_str(rest);
            }
            None => out.push_str(line),
        }
    }
    out
}

/// The whole lines of `text` that the spans touch, sorted and distinct. An
/// empty span touches the line it sits on.
fn lines_of(text: &str, spans: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut lines = BTreeSet::new();
    for span in spans {
        let last = if span.end > span.start {
            span.end - 1
        } else {
            span.start
        };
        let mut at = text[..span.start].rfind('\n').map_or(0, |i| i + 1);
        loop {
            let end = text[at..].find('\n').map_or(text.len(), |i| at + i + 1);
            lines.insert((at, end));
            if end > last || end >= text.len() {
                break;
            }
            at = end;
        }
    }
    lines.into_iter().map(|(start, end)| start..end).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::RegionKind;
    use crate::structured::jsonc::Value;

    const ZED: &str = include_str!("../../tests/fixtures/jsonc/zed-settings.jsonc");
    const VSCODE: &str = include_str!("../../tests/fixtures/jsonc/vscode-settings.jsonc");
    /// Comments in every position the parser leaves outside an item: above
    /// the root, before a key on its own line, a commented-out key before the
    /// closing bracket, after the root. Two entries share a line.
    const HEADER: &str = "// Header comment above the root.\n{\n  /* block */ \"a\": 1,\n  \"x\": 1,  \"y\": 2,\n  \"obj\": {\n    \"k\": \"v\", // same line\n    \"m\": [1, 2]\n  },\n  \"last\": true\n  // \"commented\": \"out\",\n}\n// after the root\n";

    fn doc(base: Option<&str>, ours: &str, theirs: &str) -> StructuredDocument {
        StructuredDocument::compute(base, ours, theirs, None).expect("structured")
    }

    fn every(d: &StructuredDocument, pick: Pick) -> Resolution {
        let mut res = Resolution::default();
        for decision in &d.decisions {
            res.set(decision.id, pick);
        }
        res
    }

    /// Two-way, Disk on every decision.
    fn disk(ours: &str, theirs: &str) -> String {
        let d = doc(None, ours, theirs);
        d.assemble(&every(&d, Pick::Disk)).expect("assembles")
    }

    fn value(text: &str) -> Value {
        let d = jsonc::parse(text).expect("parses");
        d.value(d.root.expect("a root"))
    }

    /// `text` with each `(from, to)` replaced once; `from` must occur.
    fn edited(text: &str, edits: &[(&str, &str)]) -> String {
        let mut out = text.to_string();
        for (from, to) in edits {
            assert!(out.contains(from), "fixture lacks {from:?}");
            out = out.replacen(from, to, 1);
        }
        out
    }

    fn fixtures() -> Vec<(&'static str, String)> {
        vec![
            (
                ZED,
                edited(
                    ZED,
                    &[
                        ("\"buffer_font_size\": 14", "\"buffer_font_size\": 16"),
                        ("  \"ui_font_size\": 15.5,\n", ""),
                        ("\"dark\": \"One Dark\"", "\"dark\": \"Solarized\""),
                        (
                            "      \"tab_size\": 2,\n",
                            "      \"tab_size\": 2,\n      \"hard_tabs\": true,\n",
                        ),
                        (
                            "  \"vim_mode\": false,\n",
                            "  \"vim_mode\": false,\n  \"auto_update\": false,\n",
                        ),
                    ],
                ),
            ),
            (
                VSCODE,
                edited(
                    VSCODE,
                    &[
                        ("\"editor.fontSize\": 14", "\"editor.fontSize\": 15"),
                        (
                            "  /* Workbench */\n  \"workbench.colorTheme\": \"Default Dark Modern\",\n",
                            "",
                        ),
                        ("  \"window.zoomLevel\": 0.5,\n", ""),
                        ("\"editor.tabSize\": 4", "\"editor.tabSize\": 2"),
                        (
                            "  \"[json]\": {\n    \"editor.defaultFormatter\": \"vscode.json-language-features\",\n  },\n",
                            "",
                        ),
                        (
                            "  \"git.autofetch\": false,\n",
                            "  \"git.autofetch\": false,\n  \"git.confirmSync\": false,\n",
                        ),
                    ],
                ),
            ),
            (
                HEADER,
                edited(
                    HEADER,
                    &[
                        ("\"y\": 2", "\"y\": 3"),
                        ("\"k\": \"v\"", "\"k\": \"w\""),
                        ("  },\n  \"last\": true\n", "  },\n  \"n\": null\n"),
                    ],
                ),
            ),
        ]
    }

    // Mutations: (1) splice an unchanged entry — a Source pick on an entry
    // present in source is planned as Drop plus a re-insert of the source
    // text at its gap — so the Source-everywhere result differs from source;
    // (2) drop the neighbour rule so every separator goes through `glue` —
    // the HEADER line holding two entries is split and Disk no longer
    // reproduces disk.
    #[test]
    fn all_source_picks_reproduce_source_byte_for_byte() {
        for (theirs, ours) in fixtures() {
            let d = doc(None, &ours, theirs);
            assert!(d.decisions.len() >= 3, "the edits make decisions");
            let source = d.assemble(&every(&d, Pick::Source)).expect("assembles");
            assert_eq!(source, theirs);
            // Every container with a decision is rebuilt under Disk; each
            // byte the edits did not touch must come back as it was.
            let mine = d.assemble(&every(&d, Pick::Disk)).expect("assembles");
            assert_eq!(mine, ours);
        }
    }

    // Mutation: Disk selects the base side's text (`Pick::Disk => base`) —
    // two-way base is source, so nothing changes and the values differ.
    #[test]
    fn all_disk_picks_two_way_are_value_equal_to_disk() {
        let compact_zed = "{\"theme\": \"One Dark\", \"buffer_font_size\": 15, \"languages\": {\"Rust\": {\"tab_size\": 8, \"hard_tabs\": false}}, \"lsp\": {\"rust-analyzer\": {}}, \"new\": [1, {\"deep\": true}], \"vim_mode\": \"yes\"}";
        let compact_vscode = "{\n\t\"editor.fontSize\": 15,\n\t\"files.exclude\": {\n\t\t\"**/.git\": false\n\t},\n\t\"json.schemas\": {\"replaced\": [\n\t\t1\n\t]},\n\t\"[rust]\": 4\n}";
        let mut cases: Vec<(&str, String)> = fixtures();
        cases.push((ZED, compact_zed.to_string()));
        cases.push((VSCODE, compact_vscode.to_string()));
        for (theirs, ours) in cases {
            let d = doc(None, &ours, theirs);
            assert!(
                d.decisions.iter().all(|x| x.kind == RegionKind::OursOnly),
                "two-way decisions are disk-sided"
            );
            let result = d.assemble(&every(&d, Pick::Disk)).expect("assembles");
            assert_eq!(value(&result), value(&ours), "{result}");
        }
    }

    // Mutation: a Value replacement starts at the key instead of the value
    // (`from: item.head`) — the key disappears and the text is not JSON.
    #[test]
    fn a_changed_value_is_replaced_in_place() {
        assert_eq!(
            disk(
                "{\n  \"a\": 10,\n  \"b\": 2\n}\n",
                "{\n  \"a\": 1,\n  \"b\": 2\n}\n"
            ),
            "{\n  \"a\": 10,\n  \"b\": 2\n}\n"
        );
    }

    // Mutation: `reindent` returns its input unchanged — the inner lines keep
    // disk's deeper indent.
    #[test]
    fn a_multi_line_value_is_reindented_to_the_destination() {
        let theirs = "{\n  \"a\": 1,\n  \"b\": 2\n}\n";
        // A type change replaces the whole entry.
        let ours = "{\n    \"a\": {\n      \"x\": 1\n    },\n    \"b\": 2\n}\n";
        assert_eq!(
            disk(ours, theirs),
            "{\n  \"a\": {\n    \"x\": 1\n  },\n  \"b\": 2\n}\n"
        );
        // A value change replaces the value only.
        let theirs = "{\n  \"a\": [1],\n  \"b\": 2\n}\n";
        let ours = "{\n    \"a\": [\n      1,\n      2\n    ],\n    \"b\": 2\n}\n";
        assert_eq!(
            disk(ours, theirs),
            "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": 2\n}\n"
        );
    }

    // Mutation: always append a comma (`Suffix::render` ignores its flag) —
    // every case below fails.
    #[test]
    fn added_entries_get_commas_in_every_position() {
        // Middle.
        let ours = "{\n  \"a\": 1,\n  \"n\": 9,\n  \"b\": 2\n}\n";
        assert_eq!(disk(ours, "{\n  \"a\": 1,\n  \"b\": 2\n}\n"), ours);
        // Last, source without trailing commas.
        let ours = "{\n  \"a\": 1,\n  \"n\": 9\n}\n";
        assert_eq!(disk(ours, "{\n  \"a\": 1\n}\n"), ours);
        // Last, source with trailing commas: the new last entry keeps one
        // even though disk wrote none.
        assert_eq!(
            disk("{\n  \"a\": 1,\n  \"n\": 9\n}\n", "{\n  \"a\": 1,\n}\n"),
            "{\n  \"a\": 1,\n  \"n\": 9,\n}\n"
        );
        // Into an empty object: opened on a new line at the file's unit.
        assert_eq!(disk("{\"n\": 1}\n", "{}\n"), "{\n  \"n\": 1\n}\n");
        assert_eq!(
            disk(
                "{\n    \"e\": {\"n\": 1},\n    \"z\": 0\n}\n",
                "{\n    \"e\": {},\n    \"z\": 0\n}\n"
            ),
            "{\n    \"e\": {\n        \"n\": 1\n    },\n    \"z\": 0\n}\n"
        );
        // First on disk: nothing precedes it, so it goes to the end.
        assert_eq!(
            disk("{\n  \"n\": 0,\n  \"a\": 1\n}\n", "{\n  \"a\": 1\n}\n"),
            "{\n  \"a\": 1,\n  \"n\": 0\n}\n"
        );
    }

    // Mutation: `Suffix::render(false)` keeps an existing comma — the
    // last-entry removal leaves `"a": 1,` before the brace.
    #[test]
    fn removed_entries_fix_the_previous_comma_only_when_last() {
        assert_eq!(
            disk(
                "{\n  \"a\": 1,\n  \"c\": 3\n}",
                "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}"
            ),
            "{\n  \"a\": 1,\n  \"c\": 3\n}"
        );
        assert_eq!(
            disk("{\n  \"a\": 1\n}", "{\n  \"a\": 1,\n  \"b\": 2\n}"),
            "{\n  \"a\": 1\n}"
        );
        assert_eq!(
            disk("{\n  \"a\": 1\n}", "{\n  \"a\": 1,\n  \"b\": 2,\n}"),
            "{\n  \"a\": 1,\n}"
        );
    }

    // Mutation: an item starts at its key instead of its leading comment
    // block (`start: entry.key_range.start`) — "// why" survives the removal
    // and is missing from the insertion.
    #[test]
    fn leading_comments_and_same_line_comments_travel_with_their_entry() {
        let theirs = "{\n  \"a\": 1,\n\n  // why\n  \"b\": 2, // note\n  \"c\": 3\n}\n";
        assert_eq!(
            disk("{\n  \"a\": 1,\n  \"c\": 3\n}\n", theirs),
            "{\n  \"a\": 1,\n\n  \"c\": 3\n}\n"
        );
        let ours = "{\n  \"a\": 1,\n  // why\n  \"n\": 2 // note\n}\n";
        assert_eq!(disk(ours, "{\n  \"a\": 1\n}\n"), ours);
    }

    // Mutation: `glue` ignores `inline` and always breaks the line — the
    // added entry lands on a new line.
    #[test]
    fn inline_container_glue_is_comma_space_and_removal_keeps_the_line() {
        let theirs = "{\"a\": 1, \"b\": 2}\n";
        assert_eq!(
            disk("{\"a\": 1, \"b\": 2, \"c\": 3}\n", theirs),
            "{\"a\": 1, \"b\": 2, \"c\": 3}\n"
        );
        assert_eq!(disk("{\"b\": 2}\n", theirs), "{\"b\": 2}\n");
        assert_eq!(
            disk("{\"a\": 1, \"c\": 3}\n", "{\"a\": 1, \"b\": 2, \"c\": 3}\n"),
            "{\"a\": 1, \"c\": 3}\n"
        );
    }

    // Mutation: `Walk::finish` drops the pending separator text — the
    // emptied object collapses to `{}`.
    #[test]
    fn removing_every_entry_leaves_the_braces() {
        assert_eq!(disk("", "{\n  \"a\": 1\n}\n"), "{\n}\n");
        assert_eq!(
            disk(
                "{\n  \"e\": {}\n}\n",
                "{\n  \"e\": {\n    \"a\": 1\n  }\n}\n"
            ),
            "{\n  \"e\": {\n  }\n}\n"
        );
        assert_eq!(disk("{}", "{\"a\": 1}"), "{}");
    }

    // Mutation: synthesized newlines are always `\n` (`newline` ignores
    // `crlf`) — the CRLF result gains bare line feeds.
    #[test]
    fn crlf_and_bom_follow_source() {
        let theirs = "\u{feff}{\r\n  \"a\": 1\r\n}\r\n";
        let ours = "{\n    \"a\": 1,\n    \"n\": {\n      \"x\": 1\n    }\n}\n";
        assert_eq!(
            disk(ours, theirs),
            "\u{feff}{\r\n  \"a\": 1,\r\n  \"n\": {\r\n    \"x\": 1\r\n  }\r\n}\r\n"
        );
        assert_eq!(
            disk("{}", theirs),
            "\u{feff}{\r\n}\r\n",
            "a removal keeps the BOM and CRLF"
        );
    }

    // Mutation: skip the collapse of source copies — the Source result keeps
    // `"a":1`.
    #[test]
    fn source_duplicates_are_collapsed_to_the_last_copy_whatever_the_pick() {
        let theirs = "{\"a\":1,\n\"a\":2}";
        let d = doc(None, "{\"a\":2}", theirs);
        assert_eq!(d.decisions[0].duplicated, Some(Side::Theirs));
        assert_eq!(d.assemble(&every(&d, Pick::Source)).unwrap(), "{\"a\":2}");
        let d = doc(None, "{\"a\":3}", theirs);
        assert_eq!(d.assemble(&every(&d, Pick::Disk)).unwrap(), "{\"a\":3}");
        let d = doc(
            None,
            "{\n  \"a\": 2,\n  \"b\": 0\n}",
            "{\n  \"a\": 1,\n  \"b\": 0,\n  \"a\": 2\n}",
        );
        assert_eq!(
            d.assemble(&every(&d, Pick::Source)).unwrap(),
            "{\n  \"b\": 0,\n  \"a\": 2\n}"
        );
    }

    // Mutation: `assemble` resolves like `seed` (an unresolved conflict takes
    // Source) — it returns text instead of `Unresolved`.
    #[test]
    fn seed_takes_source_for_an_unresolved_conflict_and_assemble_refuses() {
        let theirs = "{\n  \"a\": 3,\n  \"b\": 1\n}";
        let d = doc(Some("{\"a\": 1, \"b\": 1}"), "{\"a\": 2, \"b\": 2}", theirs);
        assert_eq!(d.required_decisions(), vec![0]);
        let mut res = Resolution::default();
        assert!(matches!(
            d.assemble(&res),
            Err(AssembleError::Unresolved { id: 0, ref path }) if path == "a"
        ));
        // The disk-only change on b still lands in the seed.
        assert_eq!(d.seed(&res), "{\n  \"a\": 3,\n  \"b\": 2\n}");
        // Both is for array runs only.
        res.set(0, Pick::Both);
        assert!(matches!(
            d.assemble(&res),
            Err(AssembleError::Unresolved { id: 0, .. })
        ));
        assert_eq!(d.seed(&res), "{\n  \"a\": 3,\n  \"b\": 2\n}");
        res.set(0, Pick::Disk);
        assert_eq!(d.assemble(&res).unwrap(), "{\n  \"a\": 2,\n  \"b\": 2\n}");
        res.set(0, Pick::LastWritten);
        assert_eq!(d.assemble(&res).unwrap(), "{\n  \"a\": 1,\n  \"b\": 2\n}");
    }

    // Mutations: (1) a dropped entry's mark is never placed — its range is
    // missing; (2) an entry mark starts at the item's leading comment
    // instead of its key — the inserted entry's slice gains the comment.
    #[test]
    fn result_ranges_cover_each_decision_and_are_empty_for_removed_entries() {
        let theirs = "{\n  \"a\": 1,\n  \"b\": 2\n}";
        let ours = "{\n  \"a\": 10,\n  // c\n  \"n\": 5\n}";
        let d = doc(None, ours, theirs);
        let paths: Vec<_> = d.decisions.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(paths, ["a", "n", "b"]);

        let res = every(&d, Pick::Disk);
        let seed = d.seed(&res);
        assert_eq!(seed, ours);
        let ranges = d.result_ranges(&res);
        assert_eq!(ranges.len(), 3);
        assert_eq!(&seed[ranges[0].clone()], "10");
        assert_eq!(&seed[ranges[1].clone()], "\"n\": 5");
        // b is gone: its empty range sits where the result moves past n.
        assert_eq!(ranges[2], ranges[1].end..ranges[1].end);

        let res = every(&d, Pick::Source);
        let seed = d.seed(&res);
        let ranges = d.result_ranges(&res);
        assert_eq!(&seed[ranges[0].clone()], "1");
        assert!(ranges[1].is_empty());
        let after_a = seed.find(',').unwrap() + 1;
        assert_eq!(ranges[1], after_a..after_a);
        assert_eq!(&seed[ranges[2].clone()], "\"b\": 2");

        // A conflict left open shows the source value.
        let d = doc(Some("{\"a\": 1}"), "{\"a\": 2}", "{\"a\": 3}");
        let res = Resolution::default();
        assert_eq!(&d.seed(&res)[d.result_ranges(&res)[0].clone()], "3");
    }

    /// The byte range of 1-based line `n` of `text`, its line break included.
    fn line(text: &str, n: usize) -> Range<usize> {
        let mut start = 0;
        for _ in 1..n {
            start += text[start..].find('\n').unwrap() + 1;
        }
        let end = text[start..]
            .find('\n')
            .map_or(text.len(), |i| start + i + 1);
        start..end
    }

    // Mutation: a last-entry removal does not report the previous entry's
    // comma line — only b's line comes back.
    #[test]
    fn touched_lines_include_the_previous_line_for_a_last_entry_removal() {
        let theirs = "{\n  \"a\": 1,\n  \"b\": 2\n}\n";
        let d = doc(None, "{\n  \"a\": 1\n}\n", theirs);
        assert_eq!(
            d.touched_lines(0, Pick::Disk),
            vec![line(theirs, 2), line(theirs, 3)]
        );
        assert!(d.touched_lines(0, Pick::Source).is_empty());

        // Trailing-comma style: the previous comma stays, so only b's line.
        let theirs = "{\n  \"a\": 1,\n  \"b\": 2,\n}\n";
        let d = doc(None, "{\n  \"a\": 1\n}\n", theirs);
        assert_eq!(d.touched_lines(0, Pick::Disk), vec![line(theirs, 3)]);

        // A middle removal with its leading comment: those lines only.
        let theirs = "{\n  \"a\": 1,\n  // why\n  \"b\": 2,\n  \"c\": 3\n}\n";
        let d = doc(None, "{\n  \"a\": 1,\n  \"c\": 3\n}\n", theirs);
        assert_eq!(
            d.touched_lines(0, Pick::Disk),
            vec![line(theirs, 3), line(theirs, 4)]
        );

        // A value change: the value's lines.
        let theirs = "{\n  \"a\": [\n    1\n  ],\n  \"b\": 2\n}\n";
        let d = doc(None, "{\"a\": [2], \"b\": 2}", theirs);
        assert_eq!(
            d.touched_lines(0, Pick::Disk),
            vec![line(theirs, 2), line(theirs, 3), line(theirs, 4)]
        );

        // An insertion after a: a's line, where the comma goes.
        let theirs = "{\n  \"a\": 1\n}\n";
        let d = doc(None, "{\n  \"a\": 1,\n  \"n\": 2\n}\n", theirs);
        assert_eq!(d.touched_lines(0, Pick::Disk), vec![line(theirs, 2)]);

        // An insertion into an empty object: the closing bracket's line.
        let theirs = "{\n  \"e\": {\n  },\n  \"z\": 0\n}\n";
        let d = doc(None, "{\"e\": {\"n\": 1}, \"z\": 0}", theirs);
        assert_eq!(d.touched_lines(0, Pick::Disk), vec![line(theirs, 3)]);

        // A source duplicate: the dropped copy's line, whatever the pick.
        let theirs = "{\n  \"a\": 1,\n  \"a\": 2\n}\n";
        let d = doc(None, "{\"a\": 2}", theirs);
        assert_eq!(d.touched_lines(0, Pick::Source), vec![line(theirs, 2)]);
    }

    // Mutation: skip the re-parse — `assemble` returns the broken text.
    #[test]
    fn assemble_refuses_a_result_that_does_not_parse() {
        let mut d = doc(None, "{\"a\": 1, \"n\": 2}", "{\"a\": 1}");
        assert_eq!(d.decisions.len(), 1);
        assert_eq!(d.decisions[0].kind, RegionKind::OursOnly);
        d.corrupt_source_for_tests();
        let res = every(&d, Pick::Disk);
        assert!(matches!(
            d.assemble(&res),
            Err(AssembleError::Invalid { .. })
        ));
    }
}
