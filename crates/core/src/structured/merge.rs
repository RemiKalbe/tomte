//! Per-key decisions for a three-way merge of JSON and JSONC files.
//!
//! [`StructuredDocument::compute`] walks the base, disk (`ours`) and source
//! (`theirs`) trees together and lists one [`Decision`] for every place where
//! the sides disagree. Equality is by value, never by text, so a reformatted
//! but equal value produces no decision. Arrays are compared whole for now.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;

use thiserror::Error;

use super::jsonc::{self, Document, NodeId, NodeKind, Value};
use crate::merge::RegionKind;
use crate::template::anchor::SpanMap;

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
// The splicer (Task 5) is the only reader of these fields.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Site {
    /// The source object that holds the key. `None` when the source is empty.
    pub(crate) container: Option<NodeId>,
    /// The index of the key's entry in the source container, the last copy
    /// when the key repeats. `None` when the key is absent from source.
    pub(crate) entry: Option<usize>,
    /// For a key absent from source: the source entry index to insert after.
    /// `None` means the end of the container.
    pub(crate) after: Option<usize>,
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
    // The splicer (Task 5) is the only reader.
    #[allow(dead_code)]
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
    base: Document,
    ours: Document,
    theirs: Document,
}

impl StructuredDocument {
    /// Compares `ours` (disk) and `theirs` (source) against `base`, the last
    /// written text. A missing or unparseable base degrades to two-way: base
    /// becomes source, so every difference is `OursOnly`.
    ///
    /// `span_map` is accepted for the locking rule and not used yet.
    pub fn compute(
        base: Option<&str>,
        ours: &str,
        theirs: &str,
        _span_map: Option<&SpanMap>,
    ) -> Result<Self, NotStructured> {
        let ours = parse_side(ours, Side::Ours)?;
        let theirs = parse_side(theirs, Side::Theirs)?;
        let root = check_roots(&ours, &theirs)?;
        let parsed_base = base.and_then(|text| jsonc::parse(text).ok());
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
        Ok(Self {
            decisions,
            degraded_base,
            base,
            ours,
            theirs,
        })
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
                && matches!(self.ours.node(of.node).kind, NodeKind::Object { .. })
                && matches!(self.theirs.node(tf.node).kind, NodeKind::Object { .. })
            {
                let base_object = bf
                    .filter(|f| matches!(self.base.node(f.node).kind, NodeKind::Object { .. }))
                    .map(|f| f.node);
                self.walk_object(&key_path, base_object, Some(of.node), Some(tf.node));
                continue;
            }
            let site = Site {
                container: theirs,
                entry: tf.as_ref().and_then(|f| f.entry),
                after: if tf.is_none() { slot.anchor } else { None },
            };
            self.decide(key_path, site, bf, of, tf);
        }
    }

    /// Roots that are both arrays are compared whole until arrays get their
    /// own element-wise walk.
    fn walk_root_array(&mut self) {
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
        };
        self.decide(
            String::new(),
            site,
            root_found(self.base),
            root_found(self.ours),
            root_found(self.theirs),
        );
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

    // Mutation: have detect skip the BothEmpty check (return Ok for two empty
    // texts), or report a mismatch before a parse error.
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

    // Mutation: drop the extension test, or the parsed-side test.
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

    // Mutation: label an absent side Value, or pick Added by the wrong side.
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

    // Mutation: recurse when the sides differ in type, or label it Value.
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

    // Mutation: set both_allowed true, or treat a missing side as unchanged.
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

    // Mutation: fill base from the other sides, or default has_base to true.
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

    // Mutation: return an error for an unparseable base instead of degrading.
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

    // Mutation: insert disk-only keys at the end instead of after their
    // disk predecessor.
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

    // Mutation: drop the decision order walk (emit disk-only keys first).
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
}
