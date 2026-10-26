//! Pure planner for Structure mode's tree (spec 2026-10-25 §3.1–3.2): the
//! source file in order, one row per key, a chip per side on each row the
//! engine has a decision for. No gpui; the view renders the rows as given.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use tomte_core::merge::RegionKind;
use tomte_core::structured::jsonc::{Document, Element, NodeId, NodeKind};
use tomte_core::structured::{Decision, Pick, Side, SideText};

use crate::structured_state::StructuredState;

/// The side a chip offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChipSide {
    Disk,
    Source,
    LastWritten,
    Both,
}

impl ChipSide {
    /// The pick a click on this chip makes.
    pub fn pick(self) -> Pick {
        match self {
            ChipSide::Disk => Pick::Disk,
            ChipSide::Source => Pick::Source,
            ChipSide::LastWritten => Pick::LastWritten,
            ChipSide::Both => Pick::Both,
        }
    }

    /// The chip's label as the user reads it.
    pub fn label(self) -> &'static str {
        match self {
            ChipSide::Disk => "disk",
            ChipSide::Source => "source",
            ChipSide::LastWritten => "last written",
            ChipSide::Both => "both",
        }
    }

    /// The chip's id fragment for element ids and tests.
    pub fn selector(self) -> &'static str {
        match self {
            ChipSide::Disk => "disk",
            ChipSide::Source => "source",
            ChipSide::LastWritten => "last-written",
            ChipSide::Both => "both",
        }
    }

    /// The document whose text the chip shows; `Both` has none of its own.
    fn side(self) -> Option<Side> {
        match self {
            ChipSide::Disk => Some(Side::Ours),
            ChipSide::Source => Some(Side::Theirs),
            ChipSide::LastWritten => Some(Side::Base),
            ChipSide::Both => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bracket {
    Object,
    Array,
}

/// What a `Fold` row stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FoldWhat {
    Keys,
    Elements,
}

/// A container node on one side, the key for its fold state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeKey {
    pub side: Side,
    pub node: NodeId,
}

/// A run of identical rows inside `container`, starting at child `first`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FoldKey {
    pub container: NodeKey,
    pub first: usize,
}

/// One side's value on a decision row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    pub side: ChipSide,
    pub text: String,
    /// This side is the row's effective pick.
    pub picked: bool,
    /// Disk and source agree: one chip the view labels "source · disk".
    pub agreed: bool,
    /// The value is an object or an array, shown as a count.
    pub container: bool,
    pub unfolded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRow {
    /// `"key": {` / `[`, or the root's bracket (`key: None`).
    Open {
        depth: usize,
        key: Option<String>,
        bracket: Bracket,
        folded: bool,
        /// "3 keys" | "12 elements".
        summary: String,
        /// Nothing inside differs between the sides.
        same: bool,
        node: NodeKey,
    },
    /// `}` / `]`, with the entry's trailing comma when it has one.
    Close {
        depth: usize,
        bracket: Bracket,
        comma: bool,
    },
    /// One line of an entry's leading comment block, trimmed.
    Comment { depth: usize, text: String },
    /// An entry or element equal on every side, on one line.
    Same { depth: usize, text: String },
    /// A decision: the key as written (`None` for an element) and its chips.
    Decision {
        depth: usize,
        id: usize,
        key: Option<String>,
        chips: Vec<Chip>,
        /// An unlocked conflict with no pick yet.
        open: bool,
        locked: bool,
    },
    /// One entry or element of an unfolded container chip, read-only.
    Preview { depth: usize, text: String },
    /// A run of identical rows behind the fold toggle. Task 4 plans them.
    Fold {
        depth: usize,
        count: usize,
        what: FoldWhat,
        key: FoldKey,
    },
}

/// The view's fold and unfold choices, kept for the editor session.
#[derive(Debug, Clone, Default)]
pub struct TreeView {
    /// "Fold identical" (Task 4).
    pub fold_identical: bool,
    /// A container the user folded (`true`) or opened (`false`), overriding
    /// its default.
    pub folded_overrides: HashMap<NodeKey, bool>,
    /// Container chips the user unfolded.
    pub unfolded_chips: HashSet<(usize, ChipSide)>,
    /// Fold runs the user expanded (Task 4).
    pub expanded_folds: HashSet<FoldKey>,
}

/// `text` on one line: every run of whitespace, line breaks included,
/// becomes one space, and the ends are trimmed.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The rows of Structure mode's tree for `state`, in source order.
pub fn plan(state: &StructuredState, view: &TreeView) -> Vec<TreeRow> {
    let planner = Planner::new(state, view);
    let mut rows = Vec::new();
    planner.root(&mut rows);
    rows
}

/// Lookups over one side's document, built once per plan.
struct SideIndex<'a> {
    doc: &'a Document,
    /// A node by its first byte; no two nodes start at the same byte.
    by_start: HashMap<usize, NodeId>,
    /// An object entry's value node → (object, entry index).
    entry_of: HashMap<NodeId, (NodeId, usize)>,
    /// An array element's value node → (array, element index).
    element_of: HashMap<NodeId, (NodeId, usize)>,
    /// A container node → the container holding it.
    parent: HashMap<NodeId, NodeId>,
}

impl<'a> SideIndex<'a> {
    fn new(doc: &'a Document) -> Self {
        let mut by_start = HashMap::new();
        let mut entry_of = HashMap::new();
        let mut element_of = HashMap::new();
        let mut parent = HashMap::new();
        for (id, node) in doc.nodes.iter().enumerate() {
            by_start.insert(node.range.start, id);
            let children: Vec<NodeId> = match &node.kind {
                NodeKind::Object { entries } => {
                    for (i, entry) in entries.iter().enumerate() {
                        entry_of.insert(entry.value, (id, i));
                    }
                    entries.iter().map(|e| e.value).collect()
                }
                NodeKind::Array { elements } => {
                    for (i, element) in elements.iter().enumerate() {
                        element_of.insert(element.value, (id, i));
                    }
                    elements.iter().map(|e| e.value).collect()
                }
                _ => continue,
            };
            for child in children {
                if is_container(doc, child) {
                    parent.insert(child, id);
                }
            }
        }
        Self {
            doc,
            by_start,
            entry_of,
            element_of,
            parent,
        }
    }

    /// The node whose range is exactly `value`.
    fn node_at(&self, value: &Range<usize>) -> Option<NodeId> {
        self.by_start
            .get(&value.start)
            .copied()
            .filter(|&id| self.doc.node(id).range == *value)
    }

    /// The object entry whose value node is exactly `value`.
    fn entry_at(&self, value: &Range<usize>) -> Option<(NodeId, usize)> {
        self.node_at(value)
            .and_then(|id| self.entry_of.get(&id).copied())
    }

    /// The run of array elements whose ranges, first start to last end,
    /// are exactly `value`: an element row's text on this side.
    fn run_at(&self, value: &Range<usize>) -> Option<&'a [Element]> {
        let first = self.by_start.get(&value.start)?;
        let &(array, k) = self.element_of.get(first)?;
        let NodeKind::Array { elements } = &self.doc.node(array).kind else {
            unreachable!("element_of maps only into arrays")
        };
        let elements: &'a [Element] = elements;
        if elements[k].range.start != value.start {
            return None;
        }
        let len = elements[k..]
            .iter()
            .position(|e| e.range.end >= value.end)
            .filter(|&i| elements[k + i].range.end == value.end)?;
        Some(&elements[k..=k + len])
    }
}

fn is_container(doc: &Document, id: NodeId) -> bool {
    matches!(
        doc.node(id).kind,
        NodeKind::Object { .. } | NodeKind::Array { .. }
    )
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `(bracket, "n keys")` for a container node, `None` for a scalar.
fn summary(doc: &Document, id: NodeId) -> Option<(Bracket, String)> {
    match &doc.node(id).kind {
        NodeKind::Object { entries } => {
            Some((Bracket::Object, plural(entries.len(), "key", "keys")))
        }
        NodeKind::Array { elements } => Some((
            Bracket::Array,
            plural(elements.len(), "element", "elements"),
        )),
        _ => None,
    }
}

/// The text of one `Same` row: the item's bytes and its same-line trailing
/// comment when that comment lies after the comma.
fn same_text(doc: &Document, range: &Range<usize>, trailing: &Option<Range<usize>>) -> String {
    let mut text = one_line(doc.slice(range));
    if let Some(comment) = trailing.as_ref().filter(|c| c.start >= range.end) {
        text.push(' ');
        text.push_str(&one_line(doc.slice(comment)));
    }
    text
}

fn push_comments(rows: &mut Vec<TreeRow>, doc: &Document, leading: &Range<usize>, depth: usize) {
    for line in doc.slice(leading).lines() {
        let text = line.trim();
        if !text.is_empty() {
            rows.push(TreeRow::Comment {
                depth,
                text: text.to_string(),
            });
        }
    }
}

/// A container's chip text: `{ 2 keys }` / `[ 3 elements ]`.
fn counted(bracket: Bracket, count: &str) -> String {
    match bracket {
        Bracket::Object => format!("{{ {count} }}"),
        Bracket::Array => format!("[ {count} ]"),
    }
}

/// An element's text without its comma, on one line.
fn element_text(doc: &Document, element: &Element) -> String {
    let end = element
        .comma
        .as_ref()
        .map_or(element.range.end, |c| c.start);
    one_line(doc.slice(&(element.range.start..end)))
}

/// One side's value on a chip: its text, whether it is (or holds) a
/// container, and the rows it unfolds into.
struct SideValue {
    text: String,
    container: bool,
    previews: Vec<String>,
}

/// One child of a container in the walk: an object entry or an array element.
struct Item<'d> {
    key: Option<&'d Range<usize>>,
    value: NodeId,
    range: &'d Range<usize>,
    comma: bool,
    leading: &'d Range<usize>,
    trailing: &'d Option<Range<usize>>,
}

struct Planner<'a> {
    state: &'a StructuredState,
    view: &'a TreeView,
    base: SideIndex<'a>,
    ours: SideIndex<'a>,
    theirs: SideIndex<'a>,
    /// Decisions replacing source entry `(object, index)`.
    at_entry: HashMap<(NodeId, usize), Vec<usize>>,
    /// Decisions inserted after source entry `(object, Some(index))`, or at
    /// the end of the object (`None`), in id order.
    after_entry: HashMap<(NodeId, Option<usize>), Vec<usize>>,
    /// Decisions covering source elements `(array, k)..` and the end of the
    /// run they cover; the covered elements get no rows of their own.
    at_element: HashMap<(NodeId, usize), (usize, Vec<usize>)>,
    /// Decisions inserted before source element `(array, k)`, `k == len`
    /// after the last, in id order.
    before_element: HashMap<(NodeId, usize), Vec<usize>>,
    /// The decision for the whole root array, when a side has no root: the
    /// single row inside the source's root.
    whole_root: Option<usize>,
    /// Decisions the walk cannot place, appended at the end of a container.
    unplaced: HashMap<NodeId, Vec<usize>>,
    /// Containers holding a decision, themselves or in a descendant.
    holding: HashSet<NodeId>,
}

impl<'a> Planner<'a> {
    fn new(state: &'a StructuredState, view: &'a TreeView) -> Self {
        let doc = &state.doc;
        let mut planner = Self {
            state,
            view,
            base: SideIndex::new(doc.side_doc(Side::Base)),
            ours: SideIndex::new(doc.side_doc(Side::Ours)),
            theirs: SideIndex::new(doc.side_doc(Side::Theirs)),
            at_entry: HashMap::new(),
            after_entry: HashMap::new(),
            at_element: HashMap::new(),
            before_element: HashMap::new(),
            whole_root: None,
            unplaced: HashMap::new(),
            holding: HashSet::new(),
        };
        planner.place_decisions();
        planner
    }

    fn source(&self) -> &'a Document {
        self.theirs.doc
    }

    fn index(&self, side: Side) -> &SideIndex<'a> {
        match side {
            Side::Base => &self.base,
            Side::Ours => &self.ours,
            Side::Theirs => &self.theirs,
        }
    }

    /// Sorts every decision into the walk's slots by its `Site`.
    fn place_decisions(&mut self) {
        let source = self.source();
        let Some(root) = source.root else {
            // An empty source: `root` lists every decision from disk's root.
            return;
        };
        for d in &self.state.doc.decisions {
            let container = d
                .site
                .container
                .filter(|&c| c < source.nodes.len() && is_container(source, c));
            let elements = &d.site.elements;
            let slot = match container.map(|c| (c, &source.node(c).kind)) {
                Some((c, _)) if d.path.is_empty() && c == root => Some((c, Slot::WholeRoot)),
                Some((c, NodeKind::Object { entries })) => match (d.site.entry, d.site.after) {
                    (Some(i), _) if i < entries.len() => Some((c, Slot::At(i))),
                    (None, None) => Some((c, Slot::After(None))),
                    (None, Some(a)) if a < entries.len() => Some((c, Slot::After(Some(a)))),
                    _ => None,
                },
                Some((c, NodeKind::Array { elements: items })) if elements.end <= items.len() => {
                    if elements.is_empty() {
                        Some((c, Slot::Before(elements.start)))
                    } else {
                        Some((c, Slot::Elements(elements.clone())))
                    }
                }
                _ => None,
            };
            let holder = match slot {
                Some((c, Slot::At(i))) => {
                    self.at_entry.entry((c, i)).or_default().push(d.id);
                    c
                }
                Some((c, Slot::After(a))) => {
                    self.after_entry.entry((c, a)).or_default().push(d.id);
                    c
                }
                Some((c, Slot::Elements(run))) => {
                    let (end, ids) = self.at_element.entry((c, run.start)).or_default();
                    *end = (*end).max(run.end);
                    ids.push(d.id);
                    c
                }
                Some((c, Slot::Before(k))) => {
                    self.before_element.entry((c, k)).or_default().push(d.id);
                    c
                }
                Some((c, Slot::WholeRoot)) => {
                    self.whole_root = Some(d.id);
                    c
                }
                None => {
                    tomte_core::log_error!(
                        "structured_tree",
                        "decision {} at {} has no place in the source: {:?}",
                        d.id,
                        d.path,
                        d.site
                    );
                    let c = container.unwrap_or(root);
                    self.unplaced.entry(c).or_default().push(d.id);
                    c
                }
            };
            let mut node = Some(holder);
            while let Some(c) = node {
                if !self.holding.insert(c) {
                    break;
                }
                node = self.theirs.parent.get(&c).copied();
            }
        }
    }

    fn root(&self, rows: &mut Vec<TreeRow>) {
        let (side, index) = match self.source().root {
            Some(_) => (Side::Theirs, &self.theirs),
            None => (Side::Ours, &self.ours),
        };
        let Some(root) = index.doc.root else {
            return;
        };
        let Some((bracket, summary)) = summary(index.doc, root) else {
            return;
        };
        rows.push(TreeRow::Open {
            depth: 0,
            key: None,
            bracket,
            folded: false,
            summary,
            same: side == Side::Theirs && !self.holding.contains(&root),
            node: NodeKey { side, node: root },
        });
        if side == Side::Ours {
            for d in &self.state.doc.decisions {
                self.decision(rows, d, 1);
            }
        } else if let Some(id) = self.whole_root {
            self.decisions(rows, &[id], 1);
        } else {
            self.children(rows, root, 1);
        }
        rows.push(TreeRow::Close {
            depth: 0,
            bracket,
            comma: false,
        });
    }

    /// The rows inside source container `c`, at `depth`.
    fn children(&self, rows: &mut Vec<TreeRow>, c: NodeId, depth: usize) {
        let source = self.source();
        match &source.node(c).kind {
            NodeKind::Object { entries } => {
                let earlier_copies: HashSet<usize> = source
                    .duplicates
                    .iter()
                    .filter(|dup| dup.object == c)
                    .flat_map(|dup| dup.entries.split_last().map_or(&[][..], |(_, e)| e))
                    .copied()
                    .collect();
                for (i, entry) in entries.iter().enumerate() {
                    match self.at_entry.get(&(c, i)) {
                        Some(ids) => self.decisions(rows, ids, depth),
                        // The last copy of a repeated key stands for all of them.
                        None if earlier_copies.contains(&i) => {}
                        None => self.item(
                            rows,
                            Item {
                                key: Some(&entry.key_range),
                                value: entry.value,
                                range: &entry.range,
                                comma: entry.comma.is_some(),
                                leading: &entry.leading,
                                trailing: &entry.trailing_comment,
                            },
                            depth,
                        ),
                    }
                    if let Some(ids) = self.after_entry.get(&(c, Some(i))) {
                        self.decisions(rows, ids, depth);
                    }
                }
                if let Some(ids) = self.after_entry.get(&(c, None)) {
                    self.decisions(rows, ids, depth);
                }
            }
            NodeKind::Array { elements } => {
                // The first element not yet covered by a decision's run.
                let mut next = 0;
                for (k, element) in elements.iter().enumerate() {
                    if let Some(ids) = self.before_element.get(&(c, k)) {
                        self.decisions(rows, ids, depth);
                    }
                    if let Some((end, ids)) = self.at_element.get(&(c, k)) {
                        self.decisions(rows, ids, depth);
                        next = next.max(*end);
                    }
                    if k < next {
                        continue;
                    }
                    self.item(
                        rows,
                        Item {
                            key: None,
                            value: element.value,
                            range: &element.range,
                            comma: element.comma.is_some(),
                            leading: &element.leading,
                            trailing: &element.trailing_comment,
                        },
                        depth,
                    );
                }
                if let Some(ids) = self.before_element.get(&(c, elements.len())) {
                    self.decisions(rows, ids, depth);
                }
            }
            _ => {}
        }
        if let Some(ids) = self.unplaced.get(&c) {
            self.decisions(rows, ids, depth);
        }
    }

    /// A source entry or element no decision replaces: its comments, then
    /// one `Same` row, or an `Open`/children/`Close` group for a container.
    fn item(&self, rows: &mut Vec<TreeRow>, item: Item<'_>, depth: usize) {
        let source = self.source();
        push_comments(rows, source, item.leading, depth);
        let Some((bracket, summary)) = summary(source, item.value) else {
            rows.push(TreeRow::Same {
                depth,
                text: same_text(source, item.range, item.trailing),
            });
            return;
        };
        let node = NodeKey {
            side: Side::Theirs,
            node: item.value,
        };
        let same = !self.holding.contains(&item.value);
        let folded = match self.view.folded_overrides.get(&node) {
            Some(&folded) => folded,
            None => same,
        };
        rows.push(TreeRow::Open {
            depth,
            key: item.key.map(|range| source.slice(range).to_string()),
            bracket,
            folded,
            summary,
            same,
            node,
        });
        if !folded {
            self.children(rows, item.value, depth + 1);
        }
        rows.push(TreeRow::Close {
            depth,
            bracket,
            comma: item.comma,
        });
    }

    fn decisions(&self, rows: &mut Vec<TreeRow>, ids: &[usize], depth: usize) {
        for &id in ids {
            self.decision(rows, &self.state.doc.decisions[id], depth);
        }
    }

    /// A decision's comments (from the side whose text the result carries),
    /// its row, and the `Preview` rows of its unfolded chips.
    fn decision(&self, rows: &mut Vec<TreeRow>, d: &Decision, depth: usize) {
        // The entry or the run's first element that carries the decision on
        // the first side that has one: its key and its leading comments.
        let owner = [Side::Theirs, Side::Ours, Side::Base]
            .into_iter()
            .find_map(|side| {
                let st = side_text(d, side)?;
                let index = self.index(side);
                if let Some((object, i)) = index.entry_at(&st.value) {
                    let NodeKind::Object { entries } = &index.doc.node(object).kind else {
                        unreachable!("entry_of maps only into objects")
                    };
                    let entry = &entries[i];
                    return Some((side, Some(&entry.key_range), &entry.leading));
                }
                let run = index.run_at(&st.value)?;
                Some((side, None, &run[0].leading))
            });
        if let Some((side, _, leading)) = owner
            && side != Side::Base
        {
            push_comments(rows, self.index(side).doc, leading, depth);
        }
        let locked = d.locked.is_some();
        let effective = self
            .state
            .resolution
            .get(d.id)
            .or(self.state.doc.auto_pick(d.id));
        let (chips, previews): (Vec<Chip>, Vec<Vec<String>>) =
            self.chips(d, effective).into_iter().unzip();
        let unfolded: Vec<TreeRow> = chips
            .iter()
            .zip(previews)
            .filter(|(chip, _)| chip.unfolded)
            .flat_map(|(_, previews)| previews)
            .map(|text| TreeRow::Preview {
                depth: depth + 1,
                text,
            })
            .collect();
        rows.push(TreeRow::Decision {
            depth,
            id: d.id,
            key: owner.and_then(|(side, key, _)| {
                key.map(|range| self.index(side).doc.slice(range).to_string())
            }),
            chips,
            open: !locked && d.kind == RegionKind::Conflict && effective.is_none(),
            locked,
        });
        rows.extend(unfolded);
    }

    /// The row's chips, each with the rows it unfolds into.
    fn chips(&self, d: &Decision, effective: Option<Pick>) -> Vec<(Chip, Vec<String>)> {
        if d.locked.is_some() {
            let (chip, previews) = self.chip(d, ChipSide::Source, None);
            return vec![(
                Chip {
                    picked: true,
                    ..chip
                },
                previews,
            )];
        }
        let agreed = d.kind == RegionKind::BothSame && d.duplicated.is_none();
        let mut sides = if agreed {
            vec![ChipSide::Source]
        } else {
            vec![ChipSide::Source, ChipSide::Disk]
        };
        let base_offered = matches!(d.kind, RegionKind::Conflict | RegionKind::BothSame);
        if base_offered && self.state.doc.has_base(d.id) {
            sides.push(ChipSide::LastWritten);
        }
        if d.both_allowed {
            sides.push(ChipSide::Both);
        }
        sides
            .into_iter()
            .map(|side| {
                let (chip, previews) = self.chip(d, side, effective);
                (
                    Chip {
                        agreed: agreed && side == ChipSide::Source,
                        ..chip
                    },
                    previews,
                )
            })
            .collect()
    }

    fn chip(&self, d: &Decision, side: ChipSide, effective: Option<Pick>) -> (Chip, Vec<String>) {
        let value = match side.side() {
            Some(doc_side) => self.side_value(d, doc_side),
            None => {
                // Disk's run then source's, skipping a side with none.
                let text = [Side::Ours, Side::Theirs]
                    .into_iter()
                    .filter(|&s| side_text(d, s).is_some())
                    .map(|s| self.side_value(d, s).text)
                    .collect::<Vec<_>>()
                    .join(", ");
                SideValue {
                    text,
                    container: false,
                    previews: Vec::new(),
                }
            }
        };
        let chip = Chip {
            side,
            text: value.text,
            picked: effective == Some(side.pick()),
            agreed: false,
            container: value.container,
            unfolded: value.container && self.view.unfolded_chips.contains(&(d.id, side)),
        };
        (chip, value.previews)
    }

    /// One side's chip value. An element row's run reads element by
    /// element, a container element as its count; a container value reads
    /// as its count and unfolds into its entries or elements.
    fn side_value(&self, d: &Decision, side: Side) -> SideValue {
        let Some(st) = side_text(d, side) else {
            let missing = if d.base.is_some() {
                "removed"
            } else {
                "absent"
            };
            return SideValue {
                text: missing.to_string(),
                container: false,
                previews: Vec::new(),
            };
        };
        let index = self.index(side);
        let doc = index.doc;
        if let Some(run) = index.run_at(&st.value) {
            let texts: Vec<String> = run
                .iter()
                .map(|e| match summary(doc, e.value) {
                    Some((bracket, count)) => counted(bracket, &count),
                    None => element_text(doc, e),
                })
                .collect();
            return SideValue {
                text: texts.join(", "),
                container: run.iter().any(|e| is_container(doc, e.value)),
                previews: run.iter().map(|e| element_text(doc, e)).collect(),
            };
        }
        let node = index.node_at(&st.value);
        let mut value = match node.and_then(|id| summary(doc, id).map(|s| (id, s))) {
            Some((id, (bracket, count))) => SideValue {
                text: counted(bracket, &count),
                container: true,
                previews: match &doc.node(id).kind {
                    NodeKind::Object { entries } => entries
                        .iter()
                        .map(|e| one_line(doc.slice(&e.range)))
                        .collect(),
                    NodeKind::Array { elements } => elements
                        .iter()
                        .map(|e| one_line(doc.slice(&e.range)))
                        .collect(),
                    _ => Vec::new(),
                },
            },
            None => SideValue {
                text: one_line(doc.slice(&st.value)),
                container: false,
                previews: Vec::new(),
            },
        };
        if d.duplicated == Some(side)
            && let Some(copies) = self.copies(index, &st.value)
        {
            value.text.push_str(&format!(" ×{copies}"));
        }
        value
    }

    /// How many times the key whose value is `value` appears in its object.
    fn copies(&self, index: &SideIndex<'_>, value: &Range<usize>) -> Option<usize> {
        let (object, i) = index.entry_at(value)?;
        index
            .doc
            .duplicates
            .iter()
            .find(|dup| dup.object == object && dup.entries.contains(&i))
            .map(|dup| dup.entries.len())
    }
}

/// Where a decision goes inside a source container.
enum Slot {
    /// Replaces object entry `i`.
    At(usize),
    /// After object entry `i`, or at the object's end.
    After(Option<usize>),
    /// Replaces the array elements of the run.
    Elements(Range<usize>),
    /// Before array element `k`; `k == len` is after the last.
    Before(usize),
    /// The whole root array.
    WholeRoot,
}

fn side_text(d: &Decision, side: Side) -> Option<&SideText> {
    match side {
        Side::Base => d.base.as_ref(),
        Side::Ours => d.ours.as_ref(),
        Side::Theirs => d.theirs.as_ref(),
    }
}

#[cfg(test)]
mod tests {
    use tomte_core::structured::{Pick, Side, StructuredDocument};
    use tomte_core::template::anchor::anchor;
    use tomte_core::template::lexer::lex;

    use super::{Bracket, Chip, ChipSide, NodeKey, Planner, TreeRow, TreeView, plan};
    use crate::structured_state::StructuredState;

    fn state(base: Option<&str>, ours: &str, theirs: &str) -> StructuredState {
        let doc = StructuredDocument::compute(base, ours, theirs, None).expect("structured");
        StructuredState::new(doc)
    }

    fn rows(base: &str, ours: &str, theirs: &str) -> Vec<TreeRow> {
        plan(&state(Some(base), ours, theirs), &TreeView::default())
    }

    fn decision(rows: &[TreeRow], id: usize) -> &TreeRow {
        rows.iter()
            .find(|row| matches!(row, TreeRow::Decision { id: d, .. } if *d == id))
            .unwrap_or_else(|| panic!("no Decision row for {id} in {rows:#?}"))
    }

    fn chip(side: ChipSide, text: &str, picked: bool) -> Chip {
        Chip {
            side,
            text: text.to_string(),
            picked,
            agreed: false,
            container: false,
            unfolded: false,
        }
    }

    fn key(text: &str) -> Option<String> {
        Some(text.to_string())
    }

    /// The source root's `Open` row for a plan over `st`.
    fn root_open(st: &StructuredState, summary: &str, same: bool) -> TreeRow {
        let source = st.doc.side_doc(Side::Theirs);
        TreeRow::Open {
            depth: 0,
            key: None,
            bracket: Bracket::Object,
            folded: false,
            summary: summary.to_string(),
            same,
            node: NodeKey {
                side: Side::Theirs,
                node: source.root.expect("source root"),
            },
        }
    }

    fn root_close() -> TreeRow {
        TreeRow::Close {
            depth: 0,
            bracket: Bracket::Object,
            comma: false,
        }
    }

    fn same(depth: usize, text: &str) -> TreeRow {
        TreeRow::Same {
            depth,
            text: text.to_string(),
        }
    }

    /// The source node of entry `name` in the source root object.
    fn source_value(st: &StructuredState, name: &str) -> usize {
        let source = st.doc.side_doc(Side::Theirs);
        let root = source.node(source.root.expect("root"));
        match &root.kind {
            tomte_core::structured::jsonc::NodeKind::Object { entries } => {
                entries.iter().find(|e| e.key == name).expect("entry").value
            }
            _ => panic!("root is not an object"),
        }
    }

    /// MUTATION: chip text uses `st.range` instead of `st.value` → the added
    /// row's disk chip starts with `"vim_mode"`.
    #[test]
    fn a_key_added_on_disk_is_a_decision_row_with_absent_source() {
        let base = "{\n  \"a\": 1\n}\n";
        let ours = "{\n  \"a\": 1,\n  \"vim_mode\": true\n}\n";
        let st = state(Some(base), ours, base);
        let rows = plan(&st, &TreeView::default());
        assert_eq!(
            rows,
            vec![
                root_open(&st, "1 key", false),
                same(1, "\"a\": 1"),
                TreeRow::Decision {
                    depth: 1,
                    id: 0,
                    key: key("\"vim_mode\""),
                    chips: vec![
                        chip(ChipSide::Source, "absent", false),
                        chip(ChipSide::Disk, "true", true),
                    ],
                    open: false,
                    locked: false,
                },
                root_close(),
            ]
        );
    }

    /// MUTATION: missing side always reads `absent` → fails on "removed".
    #[test]
    fn a_key_removed_on_disk_reads_removed_and_is_picked_disk() {
        let base = "{\n  \"a\": 1,\n  \"b\": 2\n}\n";
        let ours = "{\n  \"a\": 1\n}\n";
        let st = state(Some(base), ours, base);
        let rows = plan(&st, &TreeView::default());
        assert_eq!(
            rows,
            vec![
                root_open(&st, "2 keys", false),
                same(1, "\"a\": 1,"),
                TreeRow::Decision {
                    depth: 1,
                    id: 0,
                    key: key("\"b\""),
                    chips: vec![
                        chip(ChipSide::Source, "2", false),
                        chip(ChipSide::Disk, "removed", true),
                    ],
                    open: false,
                    locked: false,
                },
                root_close(),
            ]
        );
    }

    /// MUTATION: `open` ignores the resolution → stays true after a pick.
    #[test]
    fn a_conflict_row_has_three_chips_no_pick_and_closes_once_picked() {
        let base = "{\n  \"theme\": \"Solarized\"\n}\n";
        let ours = "{\n  \"theme\": \"Ayu\"\n}\n";
        let theirs = "{\n  \"theme\": \"One Dark\"\n}\n";
        let mut st = state(Some(base), ours, theirs);
        let before = plan(&st, &TreeView::default());
        assert_eq!(
            decision(&before, 0),
            &TreeRow::Decision {
                depth: 1,
                id: 0,
                key: key("\"theme\""),
                chips: vec![
                    chip(ChipSide::Source, "\"One Dark\"", false),
                    chip(ChipSide::Disk, "\"Ayu\"", false),
                    chip(ChipSide::LastWritten, "\"Solarized\"", false),
                ],
                open: true,
                locked: false,
            }
        );
        st.resolution.set(0, Pick::Disk);
        let after = plan(&st, &TreeView::default());
        assert_eq!(
            decision(&after, 0),
            &TreeRow::Decision {
                depth: 1,
                id: 0,
                key: key("\"theme\""),
                chips: vec![
                    chip(ChipSide::Source, "\"One Dark\"", false),
                    chip(ChipSide::Disk, "\"Ayu\"", true),
                    chip(ChipSide::LastWritten, "\"Solarized\"", false),
                ],
                open: false,
                locked: false,
            }
        );
    }

    /// MUTATION: agreed chip emitted for every row → the changed row shows one chip.
    #[test]
    fn a_same_change_on_both_sides_is_one_agreed_chip_plus_last_written() {
        let base = "{\n  \"t\": 1,\n  \"u\": 1\n}\n";
        let ours = "{\n  \"t\": 2,\n  \"u\": 3\n}\n";
        let theirs = "{\n  \"t\": 2,\n  \"u\": 1\n}\n";
        let rows = rows(base, ours, theirs);
        assert_eq!(
            decision(&rows, 0),
            &TreeRow::Decision {
                depth: 1,
                id: 0,
                key: key("\"t\""),
                chips: vec![
                    Chip {
                        agreed: true,
                        ..chip(ChipSide::Source, "2", true)
                    },
                    chip(ChipSide::LastWritten, "1", false),
                ],
                open: false,
                locked: false,
            }
        );
        assert_eq!(
            decision(&rows, 1),
            &TreeRow::Decision {
                depth: 1,
                id: 1,
                key: key("\"u\""),
                chips: vec![
                    chip(ChipSide::Source, "1", false),
                    chip(ChipSide::Disk, "3", true),
                ],
                open: false,
                locked: false,
            }
        );
    }

    /// MUTATION: container chips read the raw text → "{ 2 keys }" missing.
    #[test]
    fn a_type_change_shows_the_object_as_a_count() {
        let base = "{\n  \"x\": 1\n}\n";
        let ours = "{\n  \"x\": {\n    \"a\": 1,\n    \"b\": 2\n  }\n}\n";
        let rows = rows(base, ours, base);
        assert_eq!(
            decision(&rows, 0),
            &TreeRow::Decision {
                depth: 1,
                id: 0,
                key: key("\"x\""),
                chips: vec![
                    chip(ChipSide::Source, "1", false),
                    Chip {
                        container: true,
                        ..chip(ChipSide::Disk, "{ 2 keys }", true)
                    },
                ],
                open: false,
                locked: false,
            }
        );
    }

    /// MUTATION: repeated-key suffix dropped.
    #[test]
    fn a_key_source_repeats_is_one_row_at_the_last_copy_with_x2() {
        let theirs = "{\n  \"m\": 1,\n  \"m\": 1\n}\n";
        let ours = "{\n  \"m\": 1\n}\n";
        let st = state(Some(ours), ours, theirs);
        let rows = plan(&st, &TreeView::default());
        assert_eq!(
            rows,
            vec![
                root_open(&st, "2 keys", false),
                TreeRow::Decision {
                    depth: 1,
                    id: 0,
                    key: key("\"m\""),
                    chips: vec![
                        chip(ChipSide::Source, "1 ×2", true),
                        chip(ChipSide::Disk, "1", false),
                        chip(ChipSide::LastWritten, "1", false),
                    ],
                    open: false,
                    locked: false,
                },
                root_close(),
            ]
        );
    }

    /// MUTATION: lock ignored → the disk chip renders.
    #[test]
    fn a_locked_row_has_only_its_source_chip() {
        let template = concat!(
            "{\n",
            "  \"email\": \"{{ .email }}\",\n",
            "  \"font_size\": 14\n",
            "}\n",
        );
        let theirs = concat!(
            "{\n",
            "  \"email\": \"me@example.com\",\n",
            "  \"font_size\": 14\n",
            "}\n",
        );
        let ours = concat!(
            "{\n",
            "  \"email\": \"other@example.com\",\n",
            "  \"font_size\": 15\n",
            "}\n",
        );
        let span_map = lex(template)
            .ok()
            .map(|segments| anchor(template, &segments, theirs));
        let doc = StructuredDocument::compute(Some(theirs), ours, theirs, span_map.as_ref())
            .expect("structured");
        let st = StructuredState::new(doc);
        let rows = plan(&st, &TreeView::default());
        assert_eq!(
            rows,
            vec![
                root_open(&st, "2 keys", false),
                TreeRow::Decision {
                    depth: 1,
                    id: 0,
                    key: key("\"email\""),
                    chips: vec![chip(ChipSide::Source, "\"me@example.com\"", true)],
                    open: false,
                    locked: true,
                },
                TreeRow::Decision {
                    depth: 1,
                    id: 1,
                    key: key("\"font_size\""),
                    chips: vec![
                        chip(ChipSide::Source, "14", false),
                        chip(ChipSide::Disk, "15", true),
                    ],
                    open: false,
                    locked: false,
                },
                root_close(),
            ]
        );
    }

    /// MUTATION: comments taken from source for a disk-only entry → the disk
    /// comment is missing.
    #[test]
    fn comment_rows_come_from_the_side_that_owns_the_entry() {
        let base = "{\n  \"a\": 1\n}\n";
        let theirs = "{\n  // source a\n  \"a\": 1\n}\n";
        let ours = concat!(
            "{\n",
            "  // disk a\n",
            "  \"a\": 1,\n",
            "  // disk b, line one\n",
            "  //   line two\n",
            "  \"b\": 2\n",
            "}\n",
        );
        let st = state(Some(base), ours, theirs);
        let rows = plan(&st, &TreeView::default());
        let comment = |text: &str| TreeRow::Comment {
            depth: 1,
            text: text.to_string(),
        };
        assert_eq!(
            rows,
            vec![
                root_open(&st, "1 key", false),
                comment("// source a"),
                same(1, "\"a\": 1"),
                comment("// disk b, line one"),
                comment("//   line two"),
                TreeRow::Decision {
                    depth: 1,
                    id: 0,
                    key: key("\"b\""),
                    chips: vec![
                        chip(ChipSide::Source, "absent", false),
                        chip(ChipSide::Disk, "2", true),
                    ],
                    open: false,
                    locked: false,
                },
                root_close(),
            ]
        );
    }

    /// MUTATION: key text uses `entry.key` → the escaped key shows unescaped.
    #[test]
    fn keys_show_as_written() {
        let theirs = "{\n  \"a\\\"b\": 1\n}\n";
        let ours = "{\n  \"a\\\"b\": 2\n}\n";
        let rows = rows(theirs, ours, theirs);
        let TreeRow::Decision { key, .. } = decision(&rows, 0) else {
            unreachable!()
        };
        assert_eq!(key, &Some("\"a\\\"b\"".to_string()));
    }

    /// MUTATION: added keys emitted in reverse → order flips.
    #[test]
    fn a_chain_of_disk_added_keys_keeps_disk_order() {
        let base = "{\n  \"a\": 1,\n  \"z\": 9\n}\n";
        let ours = "{\n  \"a\": 1,\n  \"x\": 2,\n  \"y\": 3,\n  \"z\": 9\n}\n";
        let st = state(Some(base), ours, base);
        let rows = plan(&st, &TreeView::default());
        let added = |id: usize, name: &str, value: &str| TreeRow::Decision {
            depth: 1,
            id,
            key: key(name),
            chips: vec![
                chip(ChipSide::Source, "absent", false),
                chip(ChipSide::Disk, value, true),
            ],
            open: false,
            locked: false,
        };
        assert_eq!(
            rows,
            vec![
                root_open(&st, "2 keys", false),
                same(1, "\"a\": 1,"),
                added(0, "\"x\"", "2"),
                added(1, "\"y\"", "3"),
                same(1, "\"z\": 9"),
                root_close(),
            ]
        );
    }

    /// MUTATION: `one_line` keeps `\r` → texts differ.
    #[test]
    fn crlf_plans_like_lf() {
        let base = "{\n  // keep\n  \"a\":\n    1, // note\n  \"b\": 1\n}\n";
        let ours = "{\n  // keep\n  \"a\":\n    1, // note\n  \"b\": 2\n}\n";
        let crlf = |text: &str| text.replace('\n', "\r\n");
        let lf = rows(base, ours, base);
        assert_eq!(lf[2], same(1, "\"a\": 1, // note"));
        assert_eq!(rows(&crlf(base), &crlf(ours), &crlf(base)), lf);
    }

    /// MUTATION: identical container opens by default → `folded` is false.
    #[test]
    fn an_identical_object_is_a_folded_open_row_with_a_summary() {
        let base = "{\n  \"o\": {\n    \"p\": 1,\n    \"q\": 2\n  },\n  \"a\": 1\n}\n";
        let ours = "{\n  \"o\": {\n    \"p\": 1,\n    \"q\": 2\n  },\n  \"a\": 2\n}\n";
        let st = state(Some(base), ours, base);
        let rows = plan(&st, &TreeView::default());
        assert_eq!(
            rows,
            vec![
                root_open(&st, "2 keys", false),
                TreeRow::Open {
                    depth: 1,
                    key: key("\"o\""),
                    bracket: Bracket::Object,
                    folded: true,
                    summary: "2 keys".to_string(),
                    same: true,
                    node: NodeKey {
                        side: Side::Theirs,
                        node: source_value(&st, "o"),
                    },
                },
                TreeRow::Close {
                    depth: 1,
                    bracket: Bracket::Object,
                    comma: true,
                },
                TreeRow::Decision {
                    depth: 1,
                    id: 0,
                    key: key("\"a\""),
                    chips: vec![
                        chip(ChipSide::Source, "1", false),
                        chip(ChipSide::Disk, "2", true),
                    ],
                    open: false,
                    locked: false,
                },
                root_close(),
            ]
        );
    }

    /// MUTATION: a container with a decision inside folds → the decision row vanishes.
    #[test]
    fn a_container_holding_a_decision_opens_by_default() {
        let base = "{\n  \"o\": {\n    \"p\": 1,\n    \"q\": 2\n  }\n}\n";
        let ours = "{\n  \"o\": {\n    \"p\": 1,\n    \"q\": 3\n  }\n}\n";
        let st = state(Some(base), ours, base);
        let rows = plan(&st, &TreeView::default());
        assert_eq!(
            rows,
            vec![
                root_open(&st, "1 key", false),
                TreeRow::Open {
                    depth: 1,
                    key: key("\"o\""),
                    bracket: Bracket::Object,
                    folded: false,
                    summary: "2 keys".to_string(),
                    same: false,
                    node: NodeKey {
                        side: Side::Theirs,
                        node: source_value(&st, "o"),
                    },
                },
                same(2, "\"p\": 1,"),
                TreeRow::Decision {
                    depth: 2,
                    id: 0,
                    key: key("\"q\""),
                    chips: vec![
                        chip(ChipSide::Source, "2", false),
                        chip(ChipSide::Disk, "3", true),
                    ],
                    open: false,
                    locked: false,
                },
                TreeRow::Close {
                    depth: 1,
                    bracket: Bracket::Object,
                    comma: false,
                },
                root_close(),
            ]
        );
    }

    /// MUTATION: an unplaceable site is skipped → the decision id has no row.
    #[test]
    fn an_unplaceable_site_lands_at_the_end_of_its_container() {
        let base = "{\n  \"o\": {\n    \"p\": 1,\n    \"q\": 2\n  },\n  \"a\": 1\n}\n";
        let ours = "{\n  \"o\": {\n    \"p\": 1,\n    \"q\": 3\n  },\n  \"a\": 1\n}\n";
        let mut st = state(Some(base), ours, base);
        st.doc.decisions[0].site.entry = Some(99);
        let rows = plan(&st, &TreeView::default());
        let o = source_value(&st, "o");
        assert_eq!(
            rows,
            vec![
                root_open(&st, "2 keys", false),
                TreeRow::Open {
                    depth: 1,
                    key: key("\"o\""),
                    bracket: Bracket::Object,
                    folded: false,
                    summary: "2 keys".to_string(),
                    same: false,
                    node: NodeKey {
                        side: Side::Theirs,
                        node: o,
                    },
                },
                same(2, "\"p\": 1,"),
                same(2, "\"q\": 2"),
                TreeRow::Decision {
                    depth: 2,
                    id: 0,
                    key: key("\"q\""),
                    chips: vec![
                        chip(ChipSide::Source, "2", false),
                        chip(ChipSide::Disk, "3", true),
                    ],
                    open: false,
                    locked: false,
                },
                TreeRow::Close {
                    depth: 1,
                    bracket: Bracket::Object,
                    comma: true,
                },
                same(1, "\"a\": 1"),
                root_close(),
            ]
        );
    }

    /// The `Open` row of source key `name`, an array holding a decision.
    fn array_open(st: &StructuredState, name: &str, summary: &str) -> TreeRow {
        TreeRow::Open {
            depth: 1,
            key: key(&format!("\"{name}\"")),
            bracket: Bracket::Array,
            folded: false,
            summary: summary.to_string(),
            same: false,
            node: NodeKey {
                side: Side::Theirs,
                node: source_value(st, name),
            },
        }
    }

    fn array_close(depth: usize) -> TreeRow {
        TreeRow::Close {
            depth,
            bracket: Bracket::Array,
            comma: false,
        }
    }

    fn preview(depth: usize, text: &str) -> TreeRow {
        TreeRow::Preview {
            depth,
            text: text.to_string(),
        }
    }

    const ABC: &str = "{\n  \"p\": [\n    \"a\",\n    \"b\",\n    \"c\"\n  ]\n}\n";
    const ACD: &str = "{\n  \"p\": [\n    \"a\",\n    \"c\",\n    \"d\"\n  ]\n}\n";
    const AZ: &str = "{\n  \"p\": [\n    \"a\",\n    \"z\"\n  ]\n}\n";
    const ABZ: &str = "{\n  \"p\": [\n    \"a\",\n    \"b\",\n    \"z\"\n  ]\n}\n";
    const ACZ: &str = "{\n  \"p\": [\n    \"a\",\n    \"c\",\n    \"z\"\n  ]\n}\n";
    const A: &str = "{\n  \"p\": [\n    \"a\"\n  ]\n}\n";
    const A_OBJ: &str = "{\n  \"p\": [\n    \"a\",\n    { \"x\": 1, \"y\": 2 }\n  ]\n}\n";
    const A_TWO_OBJS: &str = concat!(
        "{\n",
        "  \"p\": [\n",
        "    \"a\",\n",
        "    {\n",
        "      \"u\": 1,\n",
        "      \"v\": 2\n",
        "    },\n",
        "    { \"w\": 3 }\n",
        "  ]\n",
        "}\n",
    );
    const ROOT_12: &str = "[\n  1,\n  2\n]\n";
    const ROOT_13: &str = "[\n  1,\n  3\n]\n";

    /// MUTATION: removed elements emitted after added ones → order flips.
    /// (Applied as: the insertions after the last element are emitted
    /// before the element loop.)
    #[test]
    fn removed_and_added_elements_are_rows_at_their_source_positions() {
        let st = state(Some(ABC), ACD, ABC);
        let rows = plan(&st, &TreeView::default());
        assert_eq!(
            rows,
            vec![
                root_open(&st, "1 key", false),
                array_open(&st, "p", "3 elements"),
                same(2, "\"a\","),
                TreeRow::Decision {
                    depth: 2,
                    id: 0,
                    key: None,
                    chips: vec![
                        chip(ChipSide::Source, "\"b\"", false),
                        chip(ChipSide::Disk, "removed", true),
                    ],
                    open: false,
                    locked: false,
                },
                same(2, "\"c\""),
                TreeRow::Decision {
                    depth: 2,
                    id: 1,
                    key: None,
                    chips: vec![
                        chip(ChipSide::Source, "absent", false),
                        chip(ChipSide::Disk, "\"d\"", true),
                    ],
                    open: false,
                    locked: false,
                },
                array_close(1),
                root_close(),
            ]
        );
    }

    /// MUTATION: both chip joins source first → text order flips.
    #[test]
    fn a_conflicting_run_has_source_disk_and_both_chips() {
        let st = state(Some(AZ), ACZ, ABZ);
        let rows = plan(&st, &TreeView::default());
        assert_eq!(
            rows,
            vec![
                root_open(&st, "1 key", false),
                array_open(&st, "p", "3 elements"),
                same(2, "\"a\","),
                TreeRow::Decision {
                    depth: 2,
                    id: 0,
                    key: None,
                    chips: vec![
                        chip(ChipSide::Source, "\"b\"", false),
                        chip(ChipSide::Disk, "\"c\"", false),
                        chip(ChipSide::Both, "\"c\", \"b\"", false),
                    ],
                    open: true,
                    locked: false,
                },
                same(2, "\"z\""),
                array_close(1),
                root_close(),
            ]
        );
    }

    /// MUTATION: container elements in a run read raw → "{ 2 keys }" missing.
    #[test]
    fn a_run_of_object_elements_reads_as_counts_and_unfolds_per_element() {
        let st = state(Some(A), A_TWO_OBJS, A_OBJ);
        let view = TreeView {
            unfolded_chips: [(0, ChipSide::Disk)].into_iter().collect(),
            ..TreeView::default()
        };
        let rows = plan(&st, &view);
        assert_eq!(
            rows,
            vec![
                root_open(&st, "1 key", false),
                array_open(&st, "p", "2 elements"),
                same(2, "\"a\","),
                TreeRow::Decision {
                    depth: 2,
                    id: 0,
                    key: None,
                    chips: vec![
                        Chip {
                            container: true,
                            ..chip(ChipSide::Source, "{ 2 keys }", false)
                        },
                        Chip {
                            container: true,
                            unfolded: true,
                            ..chip(ChipSide::Disk, "{ 2 keys }, { 1 key }", false)
                        },
                        chip(ChipSide::Both, "{ 2 keys }, { 1 key }, { 2 keys }", false),
                    ],
                    open: true,
                    locked: false,
                },
                preview(3, "{ \"u\": 1, \"v\": 2 }"),
                preview(3, "{ \"w\": 3 }"),
                array_close(1),
                root_close(),
            ]
        );
    }

    /// MUTATION: `unfolded` ignored → no Preview rows.
    #[test]
    fn an_object_chip_unfolds_into_preview_rows_under_its_row() {
        let base = "{\n  \"x\": 1\n}\n";
        let ours = "{\n  \"x\": {\n    \"a\": 1,\n    \"b\": 2\n  }\n}\n";
        let st = state(Some(base), ours, base);
        let view = TreeView {
            unfolded_chips: [(0, ChipSide::Disk)].into_iter().collect(),
            ..TreeView::default()
        };
        let rows = plan(&st, &view);
        assert_eq!(
            rows,
            vec![
                root_open(&st, "1 key", false),
                TreeRow::Decision {
                    depth: 1,
                    id: 0,
                    key: key("\"x\""),
                    chips: vec![
                        chip(ChipSide::Source, "1", false),
                        Chip {
                            container: true,
                            unfolded: true,
                            ..chip(ChipSide::Disk, "{ 2 keys }", true)
                        },
                    ],
                    open: false,
                    locked: false,
                },
                preview(2, "\"a\": 1,"),
                preview(2, "\"b\": 2"),
                root_close(),
            ]
        );
    }

    /// MUTATION: root arrays walk as objects → panics/empty. (Applied as:
    /// the Array arm of `children` lists nothing.)
    #[test]
    fn a_root_array_lists_its_elements() {
        let st = state(Some(ROOT_12), ROOT_13, ROOT_12);
        let rows = plan(&st, &TreeView::default());
        let source = st.doc.side_doc(Side::Theirs);
        assert_eq!(
            rows,
            vec![
                TreeRow::Open {
                    depth: 0,
                    key: None,
                    bracket: Bracket::Array,
                    folded: false,
                    summary: "2 elements".to_string(),
                    same: false,
                    node: NodeKey {
                        side: Side::Theirs,
                        node: source.root.expect("root"),
                    },
                },
                same(1, "1,"),
                TreeRow::Decision {
                    depth: 1,
                    id: 0,
                    key: None,
                    chips: vec![
                        chip(ChipSide::Source, "2", false),
                        chip(ChipSide::Disk, "removed", true),
                    ],
                    open: false,
                    locked: false,
                },
                TreeRow::Decision {
                    depth: 1,
                    id: 1,
                    key: None,
                    chips: vec![
                        chip(ChipSide::Source, "absent", false),
                        chip(ChipSide::Disk, "3", true),
                    ],
                    open: false,
                    locked: false,
                },
                array_close(0),
            ]
        );
    }

    /// MUTATION: the whole-root decision falls into the array's insertion
    /// slots → the source's elements are listed beside it.
    #[test]
    fn a_whole_root_array_decision_is_the_single_row_inside_the_root() {
        let st = state(None, "", ROOT_12);
        let rows = plan(&st, &TreeView::default());
        let source = st.doc.side_doc(Side::Theirs);
        assert_eq!(
            rows,
            vec![
                TreeRow::Open {
                    depth: 0,
                    key: None,
                    bracket: Bracket::Array,
                    folded: false,
                    summary: "2 elements".to_string(),
                    same: false,
                    node: NodeKey {
                        side: Side::Theirs,
                        node: source.root.expect("root"),
                    },
                },
                TreeRow::Decision {
                    depth: 1,
                    id: 0,
                    key: None,
                    chips: vec![
                        Chip {
                            container: true,
                            ..chip(ChipSide::Source, "[ 2 elements ]", false)
                        },
                        chip(ChipSide::Disk, "removed", true),
                    ],
                    open: false,
                    locked: false,
                },
                array_close(0),
            ]
        );
    }

    /// MUTATION: restore Task 2's unplaced arm for arrays (`_ => None`) →
    /// every array decision lands in `unplaced`.
    #[test]
    fn an_ordinary_array_decision_never_takes_the_unplaced_path() {
        let view = TreeView::default();
        for (base, ours, theirs) in [
            (Some(ABC), ACD, ABC),
            (Some(AZ), ACZ, ABZ),
            (Some(A), A_TWO_OBJS, A_OBJ),
            (Some(ROOT_12), ROOT_13, ROOT_12),
            (None, "", ROOT_12),
        ] {
            let st = state(base, ours, theirs);
            assert!(!st.doc.decisions.is_empty(), "{ours:?} has decisions");
            let planner = Planner::new(&st, &view);
            assert!(
                planner.unplaced.is_empty(),
                "{ours:?}: unplaced {:?}",
                planner.unplaced
            );
        }
    }

    /// MUTATION: the array arm accepts any `elements` range → the decision
    /// is keyed past the last element and never emitted.
    #[test]
    fn an_element_site_past_the_array_lands_at_its_end() {
        let mut st = state(Some(ABC), ACD, ABC);
        st.doc.decisions[0].site.elements = 7..8;
        let rows = plan(&st, &TreeView::default());
        assert_eq!(
            rows,
            vec![
                root_open(&st, "1 key", false),
                array_open(&st, "p", "3 elements"),
                same(2, "\"a\","),
                same(2, "\"b\","),
                same(2, "\"c\""),
                TreeRow::Decision {
                    depth: 2,
                    id: 1,
                    key: None,
                    chips: vec![
                        chip(ChipSide::Source, "absent", false),
                        chip(ChipSide::Disk, "\"d\"", true),
                    ],
                    open: false,
                    locked: false,
                },
                TreeRow::Decision {
                    depth: 2,
                    id: 0,
                    key: None,
                    chips: vec![
                        chip(ChipSide::Source, "\"b\"", false),
                        chip(ChipSide::Disk, "removed", true),
                    ],
                    open: false,
                    locked: false,
                },
                array_close(1),
                root_close(),
            ]
        );
    }
}
