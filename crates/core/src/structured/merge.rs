//! Per-key decisions for a three-way merge of JSON and JSONC files.
//!
//! [`StructuredDocument::compute`] walks the base, disk (`ours`) and source
//! (`theirs`) trees together and lists one [`Decision`] for every place where
//! the sides disagree. Equality is by value, never by text, so a reformatted
//! but equal value produces no decision. Arrays that are arrays on both
//! sides merge element-wise: each element is a token (its canonical value),
//! [`three_way_elements`] clusters the token sequences, and each region becomes rows.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;

use thiserror::Error;

use super::jsonc::{self, Document, Element, NodeId, NodeKind, Value};
use crate::merge::{MergeRegion, RegionKind, three_way_elements};
use crate::template::anchor::{SpanMap, SpanOrigin};

/// A structured format the merge editor can offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Jsonc,
}

/// Which text a decision, a parse error or a duplicate refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Base,
    Ours,
    Theirs,
}

/// Why the structured merge is not offered for a pair of texts.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NotStructured {
    #[error("{} is not valid JSON (line {line})", side_name(*side))]
    Parse { side: Side, line: usize },
    #[error("disk and source are different JSON shapes")]
    ShapeMismatch,
    #[error("the JSON root is not an object or an array")]
    ScalarRoot,
    #[error("both sides are empty")]
    BothEmpty,
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Ours => "disk",
        Side::Theirs => "source",
        Side::Base => "base",
    }
}

/// The type of a document's root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Root {
    Empty,
    Object,
    Array,
    Scalar,
}

fn root_of(doc: &Document) -> Root {
    match doc.root.map(|id| &doc.node(id).kind) {
        None => Root::Empty,
        Some(NodeKind::Object { .. }) => Root::Object,
        Some(NodeKind::Array { .. }) => Root::Array,
        Some(_) => Root::Scalar,
    }
}

/// Checks two parsed sides in the order the spec names: both empty, a scalar
/// root, then roots of different types. An empty side matches either type.
fn check_roots(ours: &Document, theirs: &Document) -> Result<Root, NotStructured> {
    let (o, t) = (root_of(ours), root_of(theirs));
    if o == Root::Empty && t == Root::Empty {
        return Err(NotStructured::BothEmpty);
    }
    if o == Root::Scalar || t == Root::Scalar {
        return Err(NotStructured::ScalarRoot);
    }
    match (o, t) {
        (Root::Empty, r) | (r, Root::Empty) => Ok(r),
        (a, b) if a == b => Ok(a),
        _ => Err(NotStructured::ShapeMismatch),
    }
}

fn parse_side(text: &str, side: Side) -> Result<Document, NotStructured> {
    jsonc::parse(text).map_err(|e| NotStructured::Parse { side, line: e.line })
}

/// Whether the structured merge applies to this pair: both texts parse as
/// JSONC and their roots are both objects or both arrays (an empty side
/// matches either), with at least one side non-empty.
pub fn detect(ours: &str, theirs: &str) -> Result<Format, NotStructured> {
    let ours = parse_side(ours, Side::Ours)?;
    let theirs = parse_side(theirs, Side::Theirs)?;
    check_roots(&ours, &theirs).map(|_| Format::Jsonc)
}

/// Whether a note about [`NotStructured`] is worth showing (spec §4): the
/// target is a `.json` or `.jsonc` file, or one side parsed as an object or
/// an array.
pub fn looked_like_json(target: &Path, ours: &str, theirs: &str) -> bool {
    let by_extension = target
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("json") || e.eq_ignore_ascii_case("jsonc"));
    let parses_as_container = |text: &str| {
        jsonc::parse(text).is_ok_and(|doc| matches!(root_of(&doc), Root::Object | Root::Array))
    };
    by_extension || parses_as_container(ours) || parses_as_container(theirs)
}

/// A piece of one side's text: its byte range and the text itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideText {
    /// The entry range for `Added`, `Removed`, `TypeChanged` and `Elements`;
    /// the value range for `Value`.
    pub range: Range<usize>,
    pub text: String,
}

/// What kind of difference a decision covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Value,
    Added,
    Removed,
    TypeChanged,
    Elements,
}

/// A decision pinned to a span of the template that renders the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lock {
    pub span: usize,
}

/// Where a decision lives in the source text.
///
/// A key row uses `entry` and `after` and leaves `elements` empty. An
/// element row (its `container` is a source array and its path is not
/// empty) uses `elements` only; `entry` and `after` are `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Site {
    /// The source object that holds the key, or the source array that holds
    /// the elements. `None` when the source is empty.
    pub(crate) container: Option<NodeId>,
    /// The index of the key's entry in the source container, the last copy
    /// when the key repeats. `None` when the key is absent from source.
    pub(crate) entry: Option<usize>,
    /// For a key absent from source: the source entry index to insert after.
    /// `None` means the end of the container.
    pub(crate) after: Option<usize>,
    /// Element rows: the source element indices the row covers. A single
    /// element present in source is `k..k + 1`; an element absent from
    /// source is the empty range at its insertion point (the source index
    /// it goes before); a conflict run is the source run, empty at an
    /// insertion point when source has none.
    pub(crate) elements: Range<usize>,
}

/// One place where the three sides disagree.
#[derive(Debug, Clone)]
pub struct Decision {
    /// Index in [`StructuredDocument::decisions`].
    pub id: usize,
    /// `editor.fontSize`; a key outside `[A-Za-z0-9_-]` is quoted.
    pub path: String,
    pub kind: RegionKind,
    pub what: Change,
    pub base: Option<SideText>,
    pub ours: Option<SideText>,
    pub theirs: Option<SideText>,
    /// The key appears more than once on that side.
    pub duplicated: Option<Side>,
    /// Array conflict rows only.
    pub both_allowed: bool,
    /// Follows source because a template pins the span.
    pub locked: Option<Lock>,
    pub(crate) site: Site,
}

/// The user's choice for one decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Disk,
    Source,
    LastWritten,
    Both,
}

/// The picks made so far, by decision id.
#[derive(Debug, Clone, Default)]
pub struct Resolution {
    picks: HashMap<usize, Pick>,
}

impl Resolution {
    pub fn set(&mut self, id: usize, pick: Pick) {
        self.picks.insert(id, pick);
    }

    pub fn get(&self, id: usize) -> Option<Pick> {
        self.picks.get(&id).copied()
    }

    pub fn unset(&mut self, id: usize) {
        self.picks.remove(&id);
    }
}

/// The three parsed sides and the decisions between them.
#[derive(Debug)]
pub struct StructuredDocument {
    pub decisions: Vec<Decision>,
    /// The base was missing or did not parse, so source stood in for it.
    pub degraded_base: bool,
    pub(super) base: Document,
    pub(super) ours: Document,
    pub(super) theirs: Document,
}

impl StructuredDocument {
    /// Compares `ours` (disk) and `theirs` (source) against `base`, the last
    /// written text. A missing or unparseable base degrades to two-way: base
    /// becomes source, so every difference is `OursOnly`.
    ///
    /// With a `span_map` (the template that renders `theirs`), a decision
    /// whose splice would touch a templated line is locked; see
    /// [`Self::lock_against`].
    pub fn compute(
        base: Option<&str>,
        ours: &str,
        theirs: &str,
        span_map: Option<&SpanMap>,
    ) -> Result<Self, NotStructured> {
        let ours = parse_side(ours, Side::Ours)?;
        let theirs = parse_side(theirs, Side::Theirs)?;
        let root = check_roots(&ours, &theirs)?;
        // A base of another root shape than the sides cannot be compared key
        // by key, so it degrades like an unparseable one. An empty base is a
        // real, empty base.
        let parsed_base = base
            .and_then(|text| jsonc::parse(text).ok())
            .filter(|doc| matches!(root_of(doc), Root::Empty) || root_of(doc) == root);
        let degraded_base = parsed_base.is_none();
        let base = match parsed_base {
            Some(doc) => doc,
            None => jsonc::parse(&theirs.text).map_err(|e| NotStructured::Parse {
                side: Side::Theirs,
                line: e.line,
            })?,
        };
        let mut walker = Walker {
            base: &base,
            ours: &ours,
            theirs: &theirs,
            decisions: Vec::new(),
        };
        match root {
            Root::Array => walker.walk_root_array(),
            _ => walker.walk_object("", root_if(&base, Root::Object), ours.root, theirs.root),
        }
        let decisions = walker.decisions;
        let mut document = Self {
            decisions,
            degraded_base,
            base,
            ours,
            theirs,
        };
        if let Some(map) = span_map {
            document.lock_against(map);
        }
        Ok(document)
    }

    /// Locks every decision whose splice would edit a protected line of
    /// source. A line is protected when it overlaps a rendered span whose
    /// origin is anything but an unrepeated template literal, because
    /// `write_back` refuses an edit there.
    ///
    /// The picks checked are Disk always, LastWritten when the base has the
    /// entry, Both when the row allows it, and Source only for a key that
    /// source repeats (collapsing the copies edits their lines). The lock
    /// records the lowest-indexed protected span the touched lines overlap.
    fn lock_against(&mut self, map: &SpanMap) {
        let protected: Vec<usize> = (0..map.spans.len())
            .filter(|&i| {
                !matches!(
                    map.spans[i].origin,
                    SpanOrigin::Literal {
                        repeated: false,
                        ..
                    }
                )
            })
            .collect();
        let locks: Vec<Option<Lock>> = (0..self.decisions.len())
            .map(|id| {
                let d = &self.decisions[id];
                let mut picks = vec![Pick::Disk];
                if self.has_base(id) {
                    picks.push(Pick::LastWritten);
                }
                if d.both_allowed {
                    picks.push(Pick::Both);
                }
                if d.duplicated == Some(Side::Theirs) {
                    picks.push(Pick::Source);
                }
                picks
                    .into_iter()
                    .flat_map(|pick| self.touched_lines(id, pick))
                    .flat_map(|line| {
                        protected.iter().copied().filter(move |&i| {
                            let span = &map.spans[i].range;
                            line.start < span.end && span.start < line.end
                        })
                    })
                    .min()
                    .map(|span| Lock { span })
            })
            .collect();
        for (d, lock) in self.decisions.iter_mut().zip(locks) {
            d.locked = lock;
        }
    }

    /// The pick that needs no user input: disk for a disk-only change,
    /// source for a source-only or identical change or a locked one, and
    /// `None` for an unlocked conflict.
    pub fn auto_pick(&self, id: usize) -> Option<Pick> {
        let decision = &self.decisions[id];
        if decision.locked.is_some() {
            return Some(Pick::Source);
        }
        match decision.kind {
            RegionKind::OursOnly => Some(Pick::Disk),
            RegionKind::TheirsOnly | RegionKind::BothSame => Some(Pick::Source),
            RegionKind::Conflict | RegionKind::Unchanged => None,
        }
    }

    /// Ids of the unlocked conflicts, in order.
    pub fn required_decisions(&self) -> Vec<usize> {
        self.decisions
            .iter()
            .filter(|d| d.kind == RegionKind::Conflict && d.locked.is_none())
            .map(|d| d.id)
            .collect()
    }

    /// Whether the last written text had this entry, so `LastWritten` can be
    /// offered.
    pub fn has_base(&self, id: usize) -> bool {
        self.decisions[id].base.is_some()
    }

    /// The full text of a side. `Base` is the effective base: source when the
    /// base was missing or unparseable.
    pub fn side_text(&self, side: Side) -> &str {
        match side {
            Side::Base => &self.base.text,
            Side::Ours => &self.ours.text,
            Side::Theirs => &self.theirs.text,
        }
    }
}

/// `Some(root node)` when the document's root has the given type.
fn root_if(doc: &Document, wanted: Root) -> Option<NodeId> {
    (root_of(doc) == wanted).then_some(doc.root).flatten()
}

/// One side's value for a key, with what a decision needs to show it.
#[derive(Clone)]
struct Found<'a> {
    doc: &'a Document,
    node: NodeId,
    /// Index of the entry in its object; `None` for a root value.
    entry: Option<usize>,
    entry_range: Range<usize>,
    /// The key appears more than once in its object.
    duplicated: bool,
}

impl Found<'_> {
    fn value(&self) -> Value {
        self.doc.value(self.node)
    }

    fn side_text(&self, what: &Change) -> SideText {
        let range = match what {
            Change::Value => self.doc.node(self.node).range.clone(),
            _ => self.entry_range.clone(),
        };
        SideText {
            text: self.doc.slice(&range).to_string(),
            range,
        }
    }
}

/// An object's keys in first-seen order, each pointing at its last copy.
struct ObjectView<'a> {
    doc: &'a Document,
    order: Vec<&'a str>,
    last: HashMap<&'a str, usize>,
    duplicated: HashSet<&'a str>,
}

impl<'a> ObjectView<'a> {
    /// A missing node, or one that is not an object, has no keys.
    fn new(doc: &'a Document, node: Option<NodeId>) -> Self {
        let mut view = Self {
            doc,
            order: Vec::new(),
            last: HashMap::new(),
            duplicated: HashSet::new(),
        };
        let Some(node) = node else { return view };
        let NodeKind::Object { entries } = &doc.node(node).kind else {
            return view;
        };
        for (index, entry) in entries.iter().enumerate() {
            if view.last.insert(entry.key.as_str(), index).is_none() {
                view.order.push(entry.key.as_str());
            }
        }
        for dup in doc.duplicates.iter().filter(|d| d.object == node) {
            if let Some((key, _)) = view.last.get_key_value(dup.key.as_str()) {
                view.duplicated.insert(key);
            }
        }
        view
    }

    fn find(&self, object: Option<NodeId>, key: &str) -> Option<Found<'a>> {
        let index = *self.last.get(key)?;
        let NodeKind::Object { entries } = &self.doc.node(object?).kind else {
            return None;
        };
        let entry = &entries[index];
        Some(Found {
            doc: self.doc,
            node: entry.value,
            entry: Some(index),
            entry_range: entry.range.clone(),
            duplicated: self.duplicated.contains(key),
        })
    }
}

/// A key in the result, with where an absent-from-source key is inserted.
struct Slot<'a> {
    key: &'a str,
    /// The source entry index to insert after; `None` is the end.
    anchor: Option<usize>,
}

/// Source keys in source order. A disk-only key goes right after the key
/// that precedes it on disk (which is in the result by then), or at the end
/// when nothing precedes it.
fn key_order<'a>(theirs: &ObjectView<'a>, ours: &ObjectView<'a>) -> Vec<Slot<'a>> {
    let mut slots: Vec<Slot<'a>> = theirs
        .order
        .iter()
        .map(|&key| Slot {
            key,
            anchor: theirs.last.get(key).copied(),
        })
        .collect();
    let mut previous: Option<&str> = None;
    for &key in &ours.order {
        if !theirs.last.contains_key(key) {
            let predecessor = previous
                .and_then(|p| slots.iter().position(|s| s.key == p))
                .map(|pos| (pos, slots[pos].anchor));
            match predecessor {
                Some((pos, anchor)) => slots.insert(pos + 1, Slot { key, anchor }),
                None => slots.push(Slot { key, anchor: None }),
            }
        }
        previous = Some(key);
    }
    slots
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Object,
    Array,
    Scalar,
}

fn shape(value: &Value) -> Shape {
    match value {
        Value::Object(_) => Shape::Object,
        Value::Array(_) => Shape::Array,
        _ => Shape::Scalar,
    }
}

fn node_shape(doc: &Document, node: NodeId) -> Shape {
    match doc.node(node).kind {
        NodeKind::Object { .. } => Shape::Object,
        NodeKind::Array { .. } => Shape::Array,
        _ => Shape::Scalar,
    }
}

/// `p[2]` for one union index, `p[2..4]` for more; `[0]` at the root.
fn element_path(parent: &str, union: Range<usize>) -> String {
    if union.len() == 1 {
        format!("{parent}[{}]", union.start)
    } else {
        format!("{parent}[{}..{}]", union.start, union.end)
    }
}

/// One side's array: its elements and their tokens. A missing node, or one
/// that is not an array, has no elements.
struct ArrayView<'a> {
    doc: &'a Document,
    elements: &'a [Element],
    tokens: Vec<String>,
}

impl<'a> ArrayView<'a> {
    fn new(doc: &'a Document, node: Option<NodeId>) -> Self {
        let elements: &[Element] = match node.map(|id| &doc.node(id).kind) {
            Some(NodeKind::Array { elements }) => elements,
            _ => &[],
        };
        let tokens = elements
            .iter()
            .map(|e| jsonc::canonical(&doc.value(e.value)))
            .collect();
        Self {
            doc,
            elements,
            tokens,
        }
    }

    /// The text of a run of elements, from the first element's start to the
    /// last one's end (its comma included); `None` for an empty run.
    fn text(&self, run: Range<usize>) -> Option<SideText> {
        if run.is_empty() {
            return None;
        }
        let range = self.elements[run.start].range.start..self.elements[run.end - 1].range.end;
        Some(SideText {
            text: self.doc.slice(&range).to_string(),
            range,
        })
    }
}

/// A path segment: the key as written, or quoted when it has a character
/// that would read as nesting or whitespace.
fn path_segment(key: &str) -> String {
    let plain = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if plain {
        return key.to_string();
    }
    let mut out = String::from("\"");
    for c in key.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn join_path(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        path_segment(key)
    } else {
        format!("{parent}.{}", path_segment(key))
    }
}

/// The three trees and the decisions found so far.
struct Walker<'a> {
    base: &'a Document,
    ours: &'a Document,
    theirs: &'a Document,
    decisions: Vec<Decision>,
}

impl<'a> Walker<'a> {
    /// Visits every key of the three objects, depth first. `base` and
    /// `ours` may be missing (an empty file or a base of another shape).
    fn walk_object(
        &mut self,
        path: &str,
        base: Option<NodeId>,
        ours: Option<NodeId>,
        theirs: Option<NodeId>,
    ) {
        let (b, o, t) = (
            ObjectView::new(self.base, base),
            ObjectView::new(self.ours, ours),
            ObjectView::new(self.theirs, theirs),
        );
        for slot in key_order(&t, &o) {
            let key = slot.key;
            let (bf, of, tf) = (b.find(base, key), o.find(ours, key), t.find(theirs, key));
            let key_path = join_path(path, key);
            if let (Some(of), Some(tf)) = (&of, &tf)
                && !of.duplicated
                && !tf.duplicated
            {
                let shape = node_shape(self.ours, of.node);
                if shape != Shape::Scalar && shape == node_shape(self.theirs, tf.node) {
                    let base_node = bf
                        .filter(|f| node_shape(self.base, f.node) == shape)
                        .map(|f| f.node);
                    if shape == Shape::Object {
                        self.walk_object(&key_path, base_node, Some(of.node), Some(tf.node));
                    } else {
                        self.walk_array(&key_path, base_node, of.node, tf.node);
                    }
                    continue;
                }
            }
            let site = Site {
                container: theirs,
                entry: tf.as_ref().and_then(|f| f.entry),
                after: if tf.is_none() { slot.anchor } else { None },
                elements: 0..0,
            };
            self.decide(key_path, site, bf, of, tf);
        }
    }

    /// Roots that are both arrays merge element-wise; an empty side makes
    /// one decision for the whole root.
    fn walk_root_array(&mut self) {
        if let (Some(ours), Some(theirs)) = (
            root_if(self.ours, Root::Array),
            root_if(self.theirs, Root::Array),
        ) {
            let base = root_if(self.base, Root::Array);
            self.walk_array("", base, ours, theirs);
            return;
        }
        let root_found = |doc: &'a Document| {
            let node = doc
                .root
                .filter(|&id| matches!(doc.node(id).kind, NodeKind::Array { .. }))?;
            Some(Found {
                doc,
                node,
                entry: None,
                entry_range: doc.node(node).range.clone(),
                duplicated: false,
            })
        };
        let site = Site {
            container: self.theirs.root,
            entry: None,
            after: None,
            elements: 0..0,
        };
        self.decide(
            String::new(),
            site,
            root_found(self.base),
            root_found(self.ours),
            root_found(self.theirs),
        );
    }

    /// Merges the elements of two arrays (and the base's, when it has one at
    /// this path) by their canonical values, one region at a time.
    fn walk_array(&mut self, path: &str, base: Option<NodeId>, ours: NodeId, theirs: NodeId) {
        let sides = [
            ArrayView::new(self.base, base),
            ArrayView::new(self.ours, Some(ours)),
            ArrayView::new(self.theirs, Some(theirs)),
        ];
        let [b, o, t] = &sides;
        let regions = three_way_elements(&b.tokens, &o.tokens, &t.tokens, |x, y| x == y);
        let mut union = 0;
        for region in &regions {
            union += self.element_rows(path, theirs, &sides, region, union);
        }
    }

    /// The rows of one region, and how many union indices it spans. Union
    /// indices count every element in result order: base elements in place,
    /// added elements right after their predecessor.
    ///
    /// A one-sided or `BothSame` region gives a `Removed` row per base
    /// element the changed side dropped, then an `Added` row per element it
    /// introduced. A conflict is one `Elements` row carrying each side's run,
    /// spanning as many union indices as its longest run.
    fn element_rows(
        &mut self,
        path: &str,
        container: NodeId,
        [b, o, t]: &[ArrayView<'a>; 3],
        r: &MergeRegion,
        union: usize,
    ) -> usize {
        let one = |side: &ArrayView<'a>, at: Option<usize>| at.and_then(|i| side.text(i..i + 1));
        let site = |elements: Range<usize>| Site {
            container: Some(container),
            entry: None,
            after: None,
            elements,
        };
        match r.kind {
            RegionKind::Unchanged => r.base.len(),
            RegionKind::Conflict => {
                let span = r.base.len().max(r.ours.len()).max(r.theirs.len()).max(1);
                self.push_row(
                    element_path(path, union..union + span),
                    r.kind,
                    Change::Elements,
                    [
                        b.text(r.base.clone()),
                        o.text(r.ours.clone()),
                        t.text(r.theirs.clone()),
                    ],
                    site(r.theirs.clone()),
                );
                span
            }
            RegionKind::OursOnly | RegionKind::TheirsOnly | RegionKind::BothSame => {
                // The unchanged side still holds the base run, at the same
                // offsets; a BothSame region's sides hold the same run.
                let ours_kept = r.kind == RegionKind::TheirsOnly;
                let theirs_kept = r.kind == RegionKind::OursOnly;
                let mut u = union;
                for k in 0..r.base.len() {
                    let ours_at = ours_kept.then_some(r.ours.start + k);
                    let theirs_at = theirs_kept.then_some(r.theirs.start + k);
                    let elements = theirs_at.map_or(r.theirs.start..r.theirs.start, |j| j..j + 1);
                    self.push_row(
                        element_path(path, u..u + 1),
                        r.kind,
                        Change::Removed,
                        [
                            one(b, Some(r.base.start + k)),
                            one(o, ours_at),
                            one(t, theirs_at),
                        ],
                        site(elements),
                    );
                    u += 1;
                }
                let added = if ours_kept {
                    r.theirs.len()
                } else {
                    r.ours.len()
                };
                for k in 0..added {
                    let ours_at = (!ours_kept).then_some(r.ours.start + k);
                    let theirs_at = (!theirs_kept).then_some(r.theirs.start + k);
                    let elements = theirs_at.map_or(r.theirs.end..r.theirs.end, |j| j..j + 1);
                    self.push_row(
                        element_path(path, u..u + 1),
                        r.kind,
                        Change::Added,
                        [None, one(o, ours_at), one(t, theirs_at)],
                        site(elements),
                    );
                    u += 1;
                }
                u - union
            }
        }
    }

    fn push_row(
        &mut self,
        path: String,
        kind: RegionKind,
        what: Change,
        [base, ours, theirs]: [Option<SideText>; 3],
        site: Site,
    ) {
        self.decisions.push(Decision {
            id: self.decisions.len(),
            path,
            kind,
            both_allowed: what == Change::Elements,
            what,
            base,
            ours,
            theirs,
            duplicated: None,
            locked: None,
            site,
        });
    }

    /// Classifies one key against the base and records a decision unless the
    /// sides agree. A key repeated in source always gets one, because the
    /// result starts from source and the splice collapses the copies.
    fn decide(
        &mut self,
        path: String,
        site: Site,
        bf: Option<Found<'a>>,
        of: Option<Found<'a>>,
        tf: Option<Found<'a>>,
    ) {
        let value = |f: &Option<Found<'a>>| f.as_ref().map(Found::value);
        let (bv, ov, tv) = (value(&bf), value(&of), value(&tf));
        let source_repeats = tf.as_ref().is_some_and(|f| f.duplicated);
        let kind = match (ov != bv, tv != bv) {
            (false, false) if source_repeats => RegionKind::BothSame,
            (false, false) => return,
            (true, false) => RegionKind::OursOnly,
            (false, true) => RegionKind::TheirsOnly,
            (true, true) if ov.is_none() && tv.is_none() => return,
            (true, true) if ov == tv => RegionKind::BothSame,
            (true, true) => RegionKind::Conflict,
        };
        let what = match (&ov, &tv) {
            (Some(o), Some(t)) if shape(o) != shape(t) => Change::TypeChanged,
            (Some(_), Some(_)) => Change::Value,
            _ if bv.is_none() => Change::Added,
            _ => Change::Removed,
        };
        let duplicated = if source_repeats {
            Some(Side::Theirs)
        } else if of.as_ref().is_some_and(|f| f.duplicated) {
            Some(Side::Ours)
        } else {
            None
        };
        let text = |f: &Option<Found<'a>>| f.as_ref().map(|f| f.side_text(&what));
        self.decisions.push(Decision {
            id: self.decisions.len(),
            path,
            kind,
            base: text(&bf),
            ours: text(&of),
            theirs: text(&tf),
            what,
            duplicated,
            both_allowed: false,
            locked: None,
            site,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(base: Option<&str>, ours: &str, theirs: &str) -> StructuredDocument {
        StructuredDocument::compute(base, ours, theirs, None).expect("structured")
    }

    fn kinds(d: &StructuredDocument) -> Vec<(String, RegionKind, Change)> {
        d.decisions
            .iter()
            .map(|d| (d.path.clone(), d.kind, d.what.clone()))
            .collect()
    }

    fn row(path: &str, kind: RegionKind, what: Change) -> (String, RegionKind, Change) {
        (path.to_string(), kind, what)
    }

    // Mutations: (1) check_roots skips the BothEmpty check; (2) detect parses
    // theirs before ours, so `Parse` names the wrong side when both fail.
    #[test]
    fn detect_accepts_objects_and_arrays_and_names_the_failure() {
        assert_eq!(detect("{}", "{\"a\":1}"), Ok(Format::Jsonc));
        assert_eq!(detect("[]", "[1]"), Ok(Format::Jsonc));
        assert_eq!(detect("", "{}"), Ok(Format::Jsonc));
        assert_eq!(
            detect("{", "{}"),
            Err(NotStructured::Parse {
                side: Side::Ours,
                line: 1
            })
        );
        assert_eq!(
            detect("{", "["),
            Err(NotStructured::Parse {
                side: Side::Ours,
                line: 1
            })
        );
        assert_eq!(detect("{}", "[]"), Err(NotStructured::ShapeMismatch));
        assert_eq!(detect("1", "2"), Err(NotStructured::ScalarRoot));
        assert_eq!(detect("", " "), Err(NotStructured::BothEmpty));
        assert_eq!(
            NotStructured::Parse {
                side: Side::Theirs,
                line: 3
            }
            .to_string(),
            "source is not valid JSON (line 3)"
        );
    }

    // Mutations: (1) looked_like_json ignores the extension; (2) it ignores
    // the parsed sides.
    #[test]
    fn looked_like_json_is_extension_or_one_parsed_side() {
        assert!(!looked_like_json(Path::new(".zshrc"), "x", "y"));
        assert!(looked_like_json(Path::new("settings.json"), "x", "y"));
        assert!(looked_like_json(Path::new(".rc"), "{}", "y"));
    }

    // Mutation: compare values by text.
    #[test]
    fn equal_values_in_different_formatting_produce_no_decision() {
        let d = doc(
            None,
            "{ \"a\" : 1, \"l\" : [1,  2] }",
            "{\"a\":1,\"l\":[1,2]}",
        );
        assert!(d.decisions.is_empty());
    }

    // Mutation: swap OursOnly and TheirsOnly in the (changed, changed) match.
    #[test]
    fn one_sided_scalar_changes_classify_by_side_and_auto_resolve() {
        let d = doc(
            Some("{\"a\":1,\"b\":1}"),
            "{\"a\":2,\"b\":1}",
            "{\"a\":1,\"b\":2}",
        );
        assert_eq!(
            kinds(&d),
            vec![
                row("a", RegionKind::OursOnly, Change::Value),
                row("b", RegionKind::TheirsOnly, Change::Value)
            ]
        );
        assert_eq!(d.auto_pick(0), Some(Pick::Disk));
        assert_eq!(d.auto_pick(1), Some(Pick::Source));
    }

    // Mutations: (1) `what` calls a key absent from base Value instead of
    // Added; (2) Added and Removed swapped.
    #[test]
    fn adds_and_removes_are_entry_decisions_with_absent_sides() {
        let d = doc(Some("{\"a\":1}"), "{\"a\":1,\"n\":2}", "{}");
        assert_eq!(
            kinds(&d),
            vec![
                row("a", RegionKind::TheirsOnly, Change::Removed),
                row("n", RegionKind::OursOnly, Change::Added)
            ]
        );
        assert!(d.decisions[0].theirs.is_none());
        assert_eq!(d.decisions[0].ours.as_ref().unwrap().text, "\"a\":1,");
        assert!(d.decisions[1].theirs.is_none());
        assert!(d.decisions[1].base.is_none());
    }

    // Mutation: classify equal changes as Conflict.
    #[test]
    fn same_change_on_both_sides_is_both_same() {
        let d = doc(Some("{\"a\":1}"), "{\"a\":2}", "{\"a\":2}");
        assert_eq!(
            kinds(&d),
            vec![row("a", RegionKind::BothSame, Change::Value)]
        );
        assert_eq!(d.auto_pick(0), Some(Pick::Source));
    }

    // Mutation: auto_pick returns Source for a conflict.
    #[test]
    fn different_changes_conflict_and_wait() {
        let d = doc(Some("{\"a\":1}"), "{\"a\":2}", "{\"a\":3}");
        assert_eq!(
            kinds(&d),
            vec![row("a", RegionKind::Conflict, Change::Value)]
        );
        assert_eq!(d.auto_pick(0), None);
        assert_eq!(d.required_decisions(), vec![0]);
    }

    // Mutation: skip the recursion and decide the whole "e" entry.
    #[test]
    fn nested_objects_recurse_and_paths_join() {
        let d = doc(
            Some("{\"e\":{\"f\":1,\"g\":1}}"),
            "{\"e\":{\"f\":2,\"g\":1}}",
            "{\"e\":{\"f\":1,\"g\":2,\"h\":3}}",
        );
        let paths: Vec<_> = d.decisions.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["e.f", "e.g", "e.h"]);
    }

    // Mutation: label a type change Value.
    #[test]
    fn type_change_is_one_decision_for_the_entry() {
        let same = "{\"a\":{\"x\":1}}";
        let d = doc(Some(same), "{\"a\":5}", same);
        assert_eq!(
            kinds(&d),
            vec![row("a", RegionKind::OursOnly, Change::TypeChanged)]
        );
    }

    // Mutation: join the raw key without quoting.
    #[test]
    fn paths_quote_keys_that_would_read_as_nesting() {
        let d = doc(None, "{\"editor.fontSize\": 12, \"a b\": 1}", "{}");
        let paths: Vec<_> = d.decisions.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["\"editor.fontSize\"", "\"a b\""]);
    }

    // Mutations: (1) both_allowed set true; (2) a side missing from the key
    // counts as unchanged (`ov.is_some() && ov != bv`).
    #[test]
    fn delete_versus_modify_is_a_conflict_without_both() {
        let d = doc(Some("{\"a\":1}"), "{}", "{\"a\":2}");
        assert_eq!(
            kinds(&d),
            vec![row("a", RegionKind::Conflict, Change::Removed)]
        );
        assert!(d.decisions[0].ours.is_none());
        assert!(!d.decisions[0].both_allowed);
    }

    // Mutation: has_base always returns true.
    #[test]
    fn a_key_added_on_both_sides_with_different_values_conflicts_without_last_written() {
        let d = doc(Some("{}"), "{\"a\":1}", "{\"a\":2}");
        assert_eq!(
            kinds(&d),
            vec![row("a", RegionKind::Conflict, Change::Value)]
        );
        assert!(!d.has_base(0));
    }

    // Mutation: use the disk text as the two-way base.
    #[test]
    fn two_way_makes_every_difference_disk_sided() {
        let d = doc(None, "{\"a\":2,\"n\":1}", "{\"a\":1}");
        assert!(d.degraded_base);
        assert_eq!(
            kinds(&d),
            vec![
                row("a", RegionKind::OursOnly, Change::Value),
                row("n", RegionKind::OursOnly, Change::Added)
            ]
        );
    }

    // Mutation: an unparseable base returns an error instead of degrading.
    #[test]
    fn an_unparseable_base_degrades_to_two_way() {
        let d = doc(Some("{"), "{\"a\":2}", "{\"a\":1}");
        assert!(d.degraded_base);
        assert_eq!(
            kinds(&d),
            vec![row("a", RegionKind::OursOnly, Change::Value)]
        );
        assert_eq!(d.side_text(Side::Base), "{\"a\":1}");
    }

    // Mutation: ignore `duplicates` -> the source case produces no decision.
    #[test]
    fn duplicates_flag_the_row_and_use_the_last_value() {
        let d = doc(None, "{\"a\":1,\"a\":2}", "{\"a\":2}");
        assert!(d.decisions.is_empty());

        let d = doc(None, "{\"a\":1,\"a\":3}", "{\"a\":2}");
        assert_eq!(
            kinds(&d),
            vec![row("a", RegionKind::OursOnly, Change::Value)]
        );
        assert_eq!(d.decisions[0].duplicated, Some(Side::Ours));
        assert_eq!(d.decisions[0].ours.as_ref().unwrap().text, "3");

        let d = doc(None, "{\"a\":2}", "{\"a\":1,\"a\":2}");
        assert_eq!(
            kinds(&d),
            vec![row("a", RegionKind::BothSame, Change::Value)]
        );
        assert_eq!(d.decisions[0].duplicated, Some(Side::Theirs));
        assert_eq!(d.auto_pick(0), Some(Pick::Source));
    }

    // Mutation: forget the disk predecessor (`previous = None`), so every
    // disk-only key goes to the end with no anchor.
    #[test]
    fn disk_only_keys_sit_after_their_disk_predecessor() {
        let d = doc(None, "{\"a\":1,\"n\":9,\"b\":2}", "{\"a\":1,\"b\":2}");
        assert_eq!(
            kinds(&d),
            vec![row("n", RegionKind::OursOnly, Change::Added)]
        );
        assert_eq!(d.decisions[0].site.after, Some(0));
        assert_eq!(d.decisions[0].site.entry, None);
    }

    // Mutation: walk the keys in reverse.
    #[test]
    fn decisions_follow_source_order_with_disk_only_keys_in_place() {
        let d = doc(
            None,
            "{\"x\":1,\"n\":9,\"a\":2}",
            "{\"a\":1,\"b\":1,\"x\":1}",
        );
        let paths: Vec<_> = d.decisions.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["a", "b", "n"]);
        // "n" follows "x" on disk, and "x" is last in source.
        assert_eq!(d.decisions[2].site.after, Some(2));
    }

    // Mutation: treat a base of another root shape as empty (the filter
    // accepts any root): degraded_base stays false and a Conflict appears.
    #[test]
    fn a_base_of_another_root_shape_degrades_to_two_way() {
        let d = doc(Some("[1]"), "{\"a\":2}", "{\"a\":1}");
        assert!(d.degraded_base);
        assert_eq!(
            kinds(&d),
            vec![row("a", RegionKind::OursOnly, Change::Value)]
        );
        assert_eq!(d.side_text(Side::Base), "{\"a\":1}");
    }

    fn text(side: &Option<SideText>) -> Option<&str> {
        side.as_ref().map(|s| s.text.as_str())
    }

    // Mutation: compare arrays whole → one Value decision instead of
    // per-element rows.
    #[test]
    fn one_sided_array_changes_split_into_one_row_per_element() {
        let d = doc(
            Some("{\"p\":[\"a\",\"b\"]}"),
            "{\"p\":[\"a\",\"x\",\"b\"]}",
            "{\"p\":[\"b\"]}",
        );
        // A removed element keeps its place; an added one follows its
        // predecessor.
        assert_eq!(
            kinds(&d),
            vec![
                row("p[0]", RegionKind::TheirsOnly, Change::Removed),
                row("p[1]", RegionKind::OursOnly, Change::Added)
            ]
        );
        assert!(d.decisions.iter().all(|x| !x.both_allowed));
        // Neither element is in source: both sit before source's "b".
        assert_eq!(d.decisions[0].site.elements, 0..0);
        assert_eq!(d.decisions[1].site.elements, 0..0);
        assert_eq!(text(&d.decisions[0].base), Some("\"a\","));
        assert_eq!(text(&d.decisions[0].ours), Some("\"a\","));
        assert_eq!(text(&d.decisions[0].theirs), None);
        assert_eq!(text(&d.decisions[1].ours), Some("\"x\","));
        assert_eq!(text(&d.decisions[1].base), None);
    }

    // Mutation: emit only the Added row for an edited element (skip the base
    // elements of a one-sided region) — the Removed row is missing.
    #[test]
    fn an_edited_element_is_a_removed_row_and_an_added_row() {
        let d = doc(
            Some("{\"p\":[\"a\"]}"),
            "{\"p\":[\"A\"]}",
            "{\"p\":[\"a\"]}",
        );
        assert_eq!(
            kinds(&d),
            vec![
                row("p[0]", RegionKind::OursOnly, Change::Removed),
                row("p[1]", RegionKind::OursOnly, Change::Added)
            ]
        );
        // The removed "a" is source's element 0; "A" goes after it.
        assert_eq!(d.decisions[0].site.elements, 0..1);
        assert_eq!(d.decisions[1].site.elements, 1..1);
        assert_eq!(text(&d.decisions[0].theirs), Some("\"a\""));
        assert_eq!(text(&d.decisions[0].ours), None);
        assert!(d.has_base(0));
        assert!(!d.has_base(1));
        assert_eq!(d.auto_pick(0), Some(Pick::Disk));
    }

    // Mutation: both_allowed stays false on an Elements conflict.
    #[test]
    fn conflicting_element_runs_are_one_row_with_both() {
        let d = doc(
            Some("{\"p\":[\"a\"]}"),
            "{\"p\":[\"x\"]}",
            "{\"p\":[\"y\"]}",
        );
        assert_eq!(
            kinds(&d),
            vec![row("p[0]", RegionKind::Conflict, Change::Elements)]
        );
        let c = &d.decisions[0];
        assert!(c.both_allowed);
        assert_eq!(text(&c.base), Some("\"a\""));
        assert_eq!(text(&c.ours), Some("\"x\""));
        assert_eq!(text(&c.theirs), Some("\"y\""));
        assert_eq!(c.site.elements, 0..1);
        assert_eq!(d.auto_pick(0), None);
        assert_eq!(d.required_decisions(), vec![0]);

        // A run of more than one element spans its union indices, and its
        // text runs from the first element to the last.
        let d = doc(
            Some("{\"p\":[0, \"a\", 9]}"),
            "{\"p\":[0, \"x\", \"z\", 9]}",
            "{\"p\":[0, \"y\", 9]}",
        );
        assert_eq!(
            kinds(&d),
            vec![row("p[1..3]", RegionKind::Conflict, Change::Elements)]
        );
        assert_eq!(text(&d.decisions[0].ours), Some("\"x\", \"z\","));
    }

    // Mutations: (1) tokens are the element's text, not its canonical
    // value — disk's reformatted object is a change and the rows conflict;
    // (2) a one-element region whose elements are all objects recurses
    // (`walk_object` on the elements) — a "p[0].k" row appears.
    #[test]
    fn array_elements_compare_by_value_and_objects_do_not_recurse() {
        let d = doc(
            Some("{\"p\":[{\"k\":1}]}"),
            "{\"p\":[{ \"k\" : 1 }]}",
            "{\"p\":[{\"k\":2}]}",
        );
        assert_eq!(
            kinds(&d),
            vec![
                row("p[0]", RegionKind::TheirsOnly, Change::Removed),
                row("p[1]", RegionKind::TheirsOnly, Change::Added)
            ]
        );
    }

    // Mutation: join an element index like a key (`join_path(path, "0")`) —
    // the root array's rows read "0" instead of "[0]".
    #[test]
    fn a_root_array_file_has_bracket_paths() {
        let d = doc(None, "[\"x\", \"a\"]", "[\"a\"]");
        assert_eq!(
            kinds(&d),
            vec![row("[0]", RegionKind::OursOnly, Change::Added)]
        );
        let d = doc(Some("[0, 1, 9]"), "[0, 2, 3, 9]", "[0, 4, 9]");
        assert_eq!(
            kinds(&d),
            vec![row("[1..3]", RegionKind::Conflict, Change::Elements)]
        );
    }

    // Mutation: treat an empty base like a degraded one (filter drops Empty).
    #[test]
    fn an_empty_base_is_a_real_empty_base() {
        let d = doc(Some(""), "{\"a\":1}", "{\"a\":2}");
        assert!(!d.degraded_base);
        assert_eq!(
            kinds(&d),
            vec![row("a", RegionKind::Conflict, Change::Value)]
        );
        assert!(!d.has_base(0));
    }

    fn map_for(tmpl: &str, rendered: &str) -> SpanMap {
        use crate::template::anchor::anchor;
        use crate::template::lexer::lex;
        anchor(tmpl, &lex(tmpl).unwrap(), rendered)
    }

    const EMAIL_TMPL: &str = "{\n  \"email\": \"{{ .email }}\",\n  \"editor\": \"hx\"\n}\n";
    const EMAIL_THEIRS: &str = "{\n  \"email\": \"me@x.y\",\n  \"editor\": \"hx\"\n}\n";

    fn locked_doc(tmpl: &str, ours: &str, theirs: &str) -> StructuredDocument {
        let map = map_for(tmpl, theirs);
        StructuredDocument::compute(None, ours, theirs, Some(&map)).expect("structured")
    }

    // Mutation: lock only the value's own line, not every touched line -> the
    // removal case (c) stays unlocked.
    #[test]
    fn entries_whose_splice_touches_a_template_line_are_locked() {
        // (a) a templated value
        let ours = "{\n  \"email\": \"other@x.y\",\n  \"editor\": \"hx\"\n}\n";
        let d = locked_doc(EMAIL_TMPL, ours, EMAIL_THEIRS);
        assert_eq!(d.decisions.len(), 1);
        let lock = d.decisions[0].locked.expect("email is locked");
        let map = map_for(EMAIL_TMPL, EMAIL_THEIRS);
        assert!(matches!(
            map.spans[lock.span].origin,
            SpanOrigin::Action { .. }
        ));
        assert_eq!(d.auto_pick(0), Some(Pick::Source));
        assert!(d.required_decisions().is_empty());
        // (b) a literal value on another line
        let ours = "{\n  \"email\": \"me@x.y\",\n  \"editor\": \"zed\"\n}\n";
        let d = locked_doc(EMAIL_TMPL, ours, EMAIL_THEIRS);
        assert_eq!(d.decisions.len(), 1);
        assert!(d.decisions[0].locked.is_none());
        // (c) removing the last entry fixes the comma on the templated line
        let ours = "{\n  \"email\": \"me@x.y\"\n}\n";
        let d = locked_doc(EMAIL_TMPL, ours, EMAIL_THEIRS);
        assert_eq!(d.decisions.len(), 1);
        assert_eq!(d.decisions[0].path, "editor");
        assert!(d.decisions[0].locked.is_some());
        // (d) a templated key
        let tmpl = "{\n  \"{{ .k }}\": 1\n}\n";
        let theirs = "{\n  \"kk\": 1\n}\n";
        let d = locked_doc(tmpl, "{\n  \"kk\": 2\n}\n", theirs);
        assert_eq!(d.decisions.len(), 1);
        assert!(d.decisions[0].locked.is_some());
    }

    // Mutation: lock_against runs regardless of the map (treats None as
    // "everything protected") -> the plain doc is locked.
    #[test]
    fn without_a_span_map_nothing_is_locked() {
        let ours = "{\n  \"email\": \"other@x.y\",\n  \"editor\": \"hx\"\n}\n";
        let base = EMAIL_THEIRS.replace("me@", "old@");
        let d = doc(Some(&base), ours, EMAIL_THEIRS);
        assert!(d.decisions.iter().all(|d| d.locked.is_none()));
        assert_eq!(d.required_decisions(), vec![0]);
    }
}
