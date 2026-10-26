//! Pure planner for Structure mode's tree (spec 2026-10-25 §3.1–3.2): the
//! source file in order, one row per key, a chip per side on each row the
//! engine has a decision for. No gpui; the view renders the rows as given.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use tomte_core::merge::RegionKind;
use tomte_core::structured::jsonc::{Document, NodeId, NodeKind};
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
    /// An unfolded container chip's read-only rows. Task 3 plans them.
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
    /// Container chips the user unfolded (Task 3).
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
    /// A container node → the container holding it.
    parent: HashMap<NodeId, NodeId>,
}

impl<'a> SideIndex<'a> {
    fn new(doc: &'a Document) -> Self {
        let mut by_start = HashMap::new();
        let mut entry_of = HashMap::new();
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
                NodeKind::Array { elements } => elements.iter().map(|e| e.value).collect(),
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
            let slot = match container.map(|c| (c, &source.node(c).kind)) {
                Some((c, NodeKind::Object { entries })) => match (d.site.entry, d.site.after) {
                    (Some(i), _) if i < entries.len() => Some((c, Slot::At(i))),
                    (None, None) => Some((c, Slot::After(None))),
                    (None, Some(a)) if a < entries.len() => Some((c, Slot::After(Some(a)))),
                    _ => None,
                },
                // Element sites are Task 3's; until then they land at the end.
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
        if side == Side::Theirs {
            self.children(rows, root, 1);
        } else {
            for d in &self.state.doc.decisions {
                self.decision(rows, d, 1);
            }
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
                for element in elements {
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

    /// A decision's comments (from the side whose text the result carries)
    /// and its row.
    fn decision(&self, rows: &mut Vec<TreeRow>, d: &Decision, depth: usize) {
        let owner = [Side::Theirs, Side::Ours, Side::Base]
            .into_iter()
            .find_map(|side| {
                let st = side_text(d, side)?;
                let (object, i) = self.index(side).entry_at(&st.value)?;
                Some((side, object, i))
            });
        let entry = owner.map(|(side, object, i)| {
            let doc = self.index(side).doc;
            let NodeKind::Object { entries } = &doc.node(object).kind else {
                unreachable!("entry_of maps only into objects")
            };
            (side, doc, &entries[i])
        });
        if let Some((side, doc, entry)) = entry
            && side != Side::Base
        {
            push_comments(rows, doc, &entry.leading, depth);
        }
        let locked = d.locked.is_some();
        let effective = self
            .state
            .resolution
            .get(d.id)
            .or(self.state.doc.auto_pick(d.id));
        rows.push(TreeRow::Decision {
            depth,
            id: d.id,
            key: entry.map(|(_, doc, entry)| doc.slice(&entry.key_range).to_string()),
            chips: self.chips(d, effective),
            open: !locked && d.kind == RegionKind::Conflict && effective.is_none(),
            locked,
        });
    }

    fn chips(&self, d: &Decision, effective: Option<Pick>) -> Vec<Chip> {
        if d.locked.is_some() {
            return vec![Chip {
                picked: true,
                ..self.chip(d, ChipSide::Source, None)
            }];
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
            .map(|side| Chip {
                agreed: agreed && side == ChipSide::Source,
                ..self.chip(d, side, effective)
            })
            .collect()
    }

    fn chip(&self, d: &Decision, side: ChipSide, effective: Option<Pick>) -> Chip {
        let (text, container) = match side.side() {
            Some(doc_side) => self.side_value(d, doc_side),
            None => {
                let disk = self.side_value(d, Side::Ours).0;
                let source = self.side_value(d, Side::Theirs).0;
                (format!("{disk}, {source}"), false)
            }
        };
        Chip {
            side,
            text,
            picked: effective == Some(side.pick()),
            agreed: false,
            container,
            unfolded: false,
        }
    }

    /// One side's chip text and whether the value is a container.
    fn side_value(&self, d: &Decision, side: Side) -> (String, bool) {
        let Some(st) = side_text(d, side) else {
            let missing = if d.base.is_some() {
                "removed"
            } else {
                "absent"
            };
            return (missing.to_string(), false);
        };
        let index = self.index(side);
        let node = index.node_at(&st.value);
        let (mut text, container) = match node.and_then(|id| summary(index.doc, id)) {
            Some((Bracket::Object, count)) => (format!("{{ {count} }}"), true),
            Some((Bracket::Array, count)) => (format!("[ {count} ]"), true),
            None => (one_line(index.doc.slice(&st.value)), false),
        };
        if d.duplicated == Some(side)
            && let Some(copies) = self.copies(index, &st.value)
        {
            text.push_str(&format!(" ×{copies}"));
        }
        (text, container)
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

/// Where a decision goes inside a source object.
enum Slot {
    At(usize),
    After(Option<usize>),
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

    use super::{Bracket, Chip, ChipSide, NodeKey, TreeRow, TreeView, plan};
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
}
