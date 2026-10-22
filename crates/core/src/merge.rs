//! Structured 3-way merge (spec §6.1): no conflict markers, a region list.

use std::hash::Hash;
use std::ops::Range;

use imara_diff::{Algorithm, Diff, Hunk, InternedInput, TokenSource};

pub mod worddiff;

/// Tokens we control, so reassembly is byte-exact (lines keep `\n`). Each
/// token is a reference to a slice element; `T = &str` is the line case.
pub struct SliceTokens<'a, T>(pub &'a [T]);

impl<'a, T: Hash + Eq> TokenSource for SliceTokens<'a, T> {
    type Token = &'a T;
    type Tokenizer = std::slice::Iter<'a, T>;
    fn tokenize(&self) -> Self::Tokenizer {
        self.0.iter()
    }
    fn estimate_tokens(&self) -> u32 {
        self.0.len() as u32
    }
}

pub fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MergeOptions {
    /// Treat lines differing only in trailing whitespace as equal when
    /// deciding `BothSame` (spec §6.1 normalization toggle).
    pub ignore_trailing_whitespace: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    Unchanged,
    OursOnly,
    TheirsOnly,
    BothSame,
    Conflict,
}

#[derive(Debug, Clone)]
pub struct MergeRegion {
    pub kind: RegionKind,
    pub base: Range<usize>,
    pub ours: Range<usize>,
    pub theirs: Range<usize>,
}

#[derive(Debug)]
pub struct MergeDocument {
    pub regions: Vec<MergeRegion>,
    base_lines: Vec<String>,
    ours_lines: Vec<String>,
    theirs_lines: Vec<String>,
}

/// One diff side: sorted hunks (base range × side range).
fn side_hunks<T: Hash + Eq>(base: &[T], side: &[T]) -> Vec<Hunk> {
    let input = InternedInput::new(SliceTokens(base), SliceTokens(side));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    diff.hunks().collect()
}

/// Line-index hunks between two texts (`before` → `after`), for callers
/// that must project positions across a free-form edit — the merge editor's
/// whole-document Apply maps region boundaries through these.
pub fn line_hunks(before: &str, after: &str) -> Vec<(Range<usize>, Range<usize>)> {
    let b = split_lines(before);
    let a = split_lines(after);
    let input = InternedInput::new(SliceTokens(&b), SliceTokens(&a));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    diff.hunks()
        .map(|h| {
            (
                h.before.start as usize..h.before.end as usize,
                h.after.start as usize..h.after.end as usize,
            )
        })
        .collect()
}

/// Ranges "touch" if they overlap or are adjacent — adjacency clusters
/// same-point insertions from both sides into a conflict (conservative).
fn touches(a: &Range<u32>, b: &Range<u32>) -> bool {
    a.start <= b.end && b.start <= a.end
}

/// Which pairs of hunks from the two sides fall into one cluster.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Join {
    /// The line rule: overlapping or adjacent ([`touches`]).
    Adjacent,
    /// The element rule ([`shares`]): adjacent hunks stay apart.
    Shared,
}

/// Hunks "share" base when they cover a common base element, insert at the
/// same base position, or one inserts strictly inside the other's range.
/// An insertion at a deletion's boundary does not share.
fn shares(a: &Range<u32>, b: &Range<u32>) -> bool {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => a.start == b.start,
        (true, false) => b.start < a.start && a.start < b.end,
        (false, true) => a.start < b.start && b.start < a.end,
        (false, false) => a.start < b.end && b.start < a.end,
    }
}

struct Cluster {
    base: Range<u32>,
    /// index ranges into the per-side hunk vectors
    ours: Range<usize>,
    theirs: Range<usize>,
}

fn clusters(hunks_o: &[Hunk], hunks_t: &[Hunk], join: Join) -> Vec<Cluster> {
    let joins = |a: &Range<u32>, b: &Range<u32>| match join {
        Join::Adjacent => touches(a, b),
        Join::Shared => shares(a, b),
    };
    let (mut i, mut j) = (0usize, 0usize);
    let mut out = Vec::new();
    while i < hunks_o.len() || j < hunks_t.len() {
        let take_ours = match (hunks_o.get(i), hunks_t.get(j)) {
            // Under the element rule an insertion at a deletion's start is
            // its own cluster, so it must come first to keep base in order.
            (Some(o), Some(t)) if join == Join::Shared => {
                (o.before.start, !o.before.is_empty()) <= (t.before.start, !t.before.is_empty())
            }
            (Some(o), Some(t)) => o.before.start <= t.before.start,
            (Some(_), None) => true,
            _ => false,
        };
        let mut base = if take_ours {
            hunks_o[i].before.clone()
        } else {
            hunks_t[j].before.clone()
        };
        let (oi, ti) = (i, j);
        loop {
            let mut grew = false;
            while let Some(h) = hunks_o.get(i) {
                if joins(&h.before, &base) {
                    base.start = base.start.min(h.before.start);
                    base.end = base.end.max(h.before.end);
                    i += 1;
                    grew = true;
                } else {
                    break;
                }
            }
            while let Some(h) = hunks_t.get(j) {
                if joins(&h.before, &base) {
                    base.start = base.start.min(h.before.start);
                    base.end = base.end.max(h.before.end);
                    j += 1;
                    grew = true;
                } else {
                    break;
                }
            }
            if !grew {
                break;
            }
        }
        out.push(Cluster {
            base,
            ours: oi..i,
            theirs: ti..j,
        });
    }
    out
}

/// Map a base line position to the side position, given all hunks strictly
/// before `first_cluster_hunk` (cluster boundaries never split hunks).
fn map_pos(hunks: &[Hunk], upto: usize, base_pos: u32) -> usize {
    let mut delta: i64 = 0;
    for h in &hunks[..upto] {
        delta += h.after.len() as i64 - h.before.len() as i64;
    }
    (base_pos as i64 + delta) as usize
}

fn eq_lines(a: &[&str], b: &[&str], opts: MergeOptions) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).all(|(x, y)| {
        if opts.ignore_trailing_whitespace {
            x.trim_end() == y.trim_end()
        } else {
            x == y
        }
    })
}

/// Three-way region clustering over any token sequence (lines today, array
/// elements for the structured merge). Regions partition each side in order.
/// `same` decides `BothSame` versus `Conflict` between the two sides' changed
/// slices; diffing against base uses `T`'s own `Hash + Eq`.
pub fn three_way<T: Hash + Eq>(
    base: &[T],
    ours: &[T],
    theirs: &[T],
    same: impl Fn(&[T], &[T]) -> bool,
) -> Vec<MergeRegion> {
    three_way_by(base, ours, theirs, same, Join::Adjacent)
}

/// [`three_way`] for array elements: changes on the two sides conflict only
/// when they share base (a common base element, insertions at the same
/// position, or an insertion strictly inside the other side's range). A
/// change next to the other side's change stays one-sided, so one side
/// deleting an element while the other inserts right after it gives a
/// `TheirsOnly` and an `OursOnly` region rather than a conflict. Lines keep
/// the conservative adjacency rule.
pub fn three_way_elements<T: Hash + Eq>(
    base: &[T],
    ours: &[T],
    theirs: &[T],
    same: impl Fn(&[T], &[T]) -> bool,
) -> Vec<MergeRegion> {
    three_way_by(base, ours, theirs, same, Join::Shared)
}

fn three_way_by<T: Hash + Eq>(
    base: &[T],
    ours: &[T],
    theirs: &[T],
    same: impl Fn(&[T], &[T]) -> bool,
    join: Join,
) -> Vec<MergeRegion> {
    let hunks_o = side_hunks(base, ours);
    let hunks_t = side_hunks(base, theirs);

    let mut regions = Vec::new();
    let mut cursor: u32 = 0;
    for c in clusters(&hunks_o, &hunks_t, join) {
        if c.base.start > cursor {
            let (bs, be) = (cursor as usize, c.base.start as usize);
            regions.push(MergeRegion {
                kind: RegionKind::Unchanged,
                base: bs..be,
                ours: map_pos(&hunks_o, c.ours.start, cursor)
                    ..map_pos(&hunks_o, c.ours.start, c.base.start),
                theirs: map_pos(&hunks_t, c.theirs.start, cursor)
                    ..map_pos(&hunks_t, c.theirs.start, c.base.start),
            });
        }
        let ours_range = map_pos(&hunks_o, c.ours.start, c.base.start)
            ..map_pos(&hunks_o, c.ours.end, c.base.end);
        let theirs_range = map_pos(&hunks_t, c.theirs.start, c.base.start)
            ..map_pos(&hunks_t, c.theirs.end, c.base.end);
        let ours_changed = !c.ours.is_empty();
        let theirs_changed = !c.theirs.is_empty();
        let kind = match (ours_changed, theirs_changed) {
            (true, false) => RegionKind::OursOnly,
            (false, true) => RegionKind::TheirsOnly,
            (true, true) => {
                if same(&ours[ours_range.clone()], &theirs[theirs_range.clone()]) {
                    RegionKind::BothSame
                } else {
                    RegionKind::Conflict
                }
            }
            (false, false) => RegionKind::Unchanged, // unreachable: clusters come from hunks
        };
        regions.push(MergeRegion {
            kind,
            base: c.base.start as usize..c.base.end as usize,
            ours: ours_range,
            theirs: theirs_range,
        });
        cursor = c.base.end;
    }
    if (cursor as usize) < base.len() || regions.is_empty() {
        regions.push(MergeRegion {
            kind: RegionKind::Unchanged,
            base: cursor as usize..base.len(),
            ours: map_pos(&hunks_o, hunks_o.len(), cursor)..ours.len(),
            theirs: map_pos(&hunks_t, hunks_t.len(), cursor)..theirs.len(),
        });
    }
    regions
}

impl MergeDocument {
    pub fn compute(base: &str, ours: &str, theirs: &str, opts: MergeOptions) -> Self {
        let b = split_lines(base);
        let o = split_lines(ours);
        let t = split_lines(theirs);
        let regions = three_way(&b, &o, &t, |x, y| eq_lines(x, y, opts));

        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        Self {
            regions,
            base_lines: own(&b),
            ours_lines: own(&o),
            theirs_lines: own(&t),
        }
    }

    pub fn base_lines(&self) -> &[String] {
        &self.base_lines
    }
    pub fn ours_lines(&self) -> &[String] {
        &self.ours_lines
    }
    pub fn theirs_lines(&self) -> &[String] {
        &self.theirs_lines
    }

    pub fn required_decisions(&self) -> Vec<usize> {
        self.regions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == RegionKind::Conflict)
            .map(|(i, _)| i)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Ours,
    Theirs,
    Base,
    /// Keep both sides: ours (disk) first, then theirs (source) — the order
    /// git and Zed's "Use Both" use. First-class rather than
    /// `Edited(concat)` so provenance survives (each half keeps its tint and
    /// the decision stays revisitable without loss).
    Both,
    Edited(String),
    /// Origin's version of this region. Assembles exactly like `Edited`;
    /// separate so the strip label and result-row tint can say where the
    /// text came from.
    Origin(String),
}

#[derive(Debug, Default)]
pub struct Resolution {
    choices: std::collections::HashMap<usize, Choice>,
}

impl Resolution {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&mut self, region_index: usize, choice: Choice) {
        self.choices.insert(region_index, choice);
    }
    pub fn get(&self, region_index: usize) -> Option<&Choice> {
        self.choices.get(&region_index)
    }
    /// Remove a recorded choice (undo of a first-time pick).
    pub fn unset(&mut self, region_index: usize) {
        self.choices.remove(&region_index);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AssembleError {
    #[error("region {region} is a conflict with no choice made")]
    Unresolved { region: usize },
}

impl MergeDocument {
    pub fn assemble(&self, res: &Resolution) -> Result<String, AssembleError> {
        let mut out = String::new();
        for (idx, region) in self.regions.iter().enumerate() {
            let choice = res.get(idx);
            match (choice, region.kind) {
                (Some(Choice::Ours), _) => {
                    out.push_str(&self.ours_lines[region.ours.clone()].concat())
                }
                (Some(Choice::Theirs), _) => {
                    out.push_str(&self.theirs_lines[region.theirs.clone()].concat())
                }
                (Some(Choice::Base), _) => {
                    out.push_str(&self.base_lines[region.base.clone()].concat())
                }
                (Some(Choice::Both), _) => {
                    out.push_str(&self.ours_lines[region.ours.clone()].concat());
                    out.push_str(&self.theirs_lines[region.theirs.clone()].concat());
                }
                (Some(Choice::Edited(text)), _) => out.push_str(text),
                (Some(Choice::Origin(text)), _) => out.push_str(text),
                (None, RegionKind::Unchanged) => {
                    out.push_str(&self.base_lines[region.base.clone()].concat())
                }
                (None, RegionKind::OursOnly | RegionKind::BothSame) => {
                    out.push_str(&self.ours_lines[region.ours.clone()].concat())
                }
                (None, RegionKind::TheirsOnly) => {
                    out.push_str(&self.theirs_lines[region.theirs.clone()].concat())
                }
                (None, RegionKind::Conflict) => {
                    return Err(AssembleError::Unresolved { region: idx });
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_choice_concatenates_ours_then_theirs() {
        // base "x" -> ours "disk", theirs "src": a true conflict.
        let doc = MergeDocument::compute("x\n", "disk\n", "src\n", MergeOptions::default());
        let conflicts = doc.required_decisions();
        assert_eq!(conflicts.len(), 1);
        let mut res = Resolution::new();
        res.set(conflicts[0], Choice::Both);
        assert_eq!(doc.assemble(&res).unwrap(), "disk\nsrc\n");
    }

    #[test]
    fn both_choice_on_one_sided_region_duplicates_nothing_missing() {
        // theirs added a line; ours == base. Both on that region = ours part
        // (empty side tolerated) + theirs part.
        let doc = MergeDocument::compute("a\n", "a\n", "a\nb\n", MergeOptions::default());
        let region_ix = doc
            .regions
            .iter()
            .position(|r| r.kind == RegionKind::TheirsOnly)
            .expect("theirs-only region");
        let mut res = Resolution::new();
        res.set(region_ix, Choice::Both);
        let out = doc.assemble(&res).unwrap();
        assert_eq!(out, "a\nb\n");
    }

    #[test]
    fn both_choice_with_empty_ours_side_in_conflict() {
        // ours deleted the line, theirs rewrote it: conflict with empty ours.
        let doc = MergeDocument::compute("x\n", "", "y\n", MergeOptions::default());
        let conflicts = doc.required_decisions();
        assert_eq!(conflicts.len(), 1);
        let mut res = Resolution::new();
        res.set(conflicts[0], Choice::Both);
        assert_eq!(doc.assemble(&res).unwrap(), "y\n");
    }

    fn kinds(doc: &MergeDocument) -> Vec<RegionKind> {
        doc.regions.iter().map(|r| r.kind).collect()
    }

    #[test]
    fn identical_inputs_are_one_unchanged_region() {
        let doc = MergeDocument::compute("a\nb\n", "a\nb\n", "a\nb\n", MergeOptions::default());
        assert_eq!(kinds(&doc), vec![RegionKind::Unchanged]);
        assert_eq!(doc.regions[0].base, 0..2);
    }

    #[test]
    fn one_sided_changes_classify_by_side() {
        // ours edits line b; theirs edits line d — two independent regions
        let base = "a\nb\nc\nd\ne\n";
        let ours = "a\nB\nc\nd\ne\n";
        let theirs = "a\nb\nc\nD\ne\n";
        let doc = MergeDocument::compute(base, ours, theirs, MergeOptions::default());
        assert_eq!(
            kinds(&doc),
            vec![
                RegionKind::Unchanged,  // a
                RegionKind::OursOnly,   // b -> B
                RegionKind::Unchanged,  // c
                RegionKind::TheirsOnly, // d -> D
                RegionKind::Unchanged,  // e
            ]
        );
        let ours_region = &doc.regions[1];
        assert_eq!(
            doc.ours_lines()[ours_region.ours.clone()],
            ["B\n".to_string()]
        );
    }

    #[test]
    fn same_change_on_both_sides_is_both_same() {
        let doc = MergeDocument::compute("x\n", "y\n", "y\n", MergeOptions::default());
        assert_eq!(kinds(&doc), vec![RegionKind::BothSame]);
    }

    #[test]
    fn overlapping_different_changes_conflict() {
        let doc = MergeDocument::compute("v = 1\n", "v = 2\n", "v = 3\n", MergeOptions::default());
        assert_eq!(kinds(&doc), vec![RegionKind::Conflict]);
        assert_eq!(doc.required_decisions(), vec![0]);
    }

    #[test]
    fn adjacent_insertions_from_both_sides_cluster_into_conflict() {
        // both sides insert different lines at the same spot — must NOT interleave silently
        let doc = MergeDocument::compute(
            "a\nz\n",
            "a\nours\nz\n",
            "a\ntheirs\nz\n",
            MergeOptions::default(),
        );
        assert_eq!(
            kinds(&doc),
            vec![
                RegionKind::Unchanged,
                RegionKind::Conflict,
                RegionKind::Unchanged
            ]
        );
    }

    #[test]
    fn whitespace_only_difference_is_both_same_with_option() {
        let base = "k: v\n";
        let ours = "k: v  \n";
        let theirs = "k: v\t\n";
        let strict = MergeDocument::compute(base, ours, theirs, MergeOptions::default());
        assert_eq!(kinds(&strict), vec![RegionKind::Conflict]);
        let relaxed = MergeDocument::compute(
            base,
            ours,
            theirs,
            MergeOptions {
                ignore_trailing_whitespace: true,
            },
        );
        assert_eq!(kinds(&relaxed), vec![RegionKind::BothSame]);
    }

    #[test]
    fn missing_trailing_newline_roundtrips() {
        let doc = MergeDocument::compute("a\nb", "a\nb", "a\nb", MergeOptions::default());
        assert_eq!(doc.base_lines().concat(), "a\nb");
    }

    #[test]
    fn assemble_applies_defaults_and_choices() {
        let base = "a\nv = 1\nz\n";
        let ours = "a\nv = 2\nz\n";
        let theirs = "a\nv = 3\nz\n";
        let doc = MergeDocument::compute(base, ours, theirs, MergeOptions::default());
        let conflict = doc.required_decisions()[0];

        let mut res = Resolution::new();
        assert!(matches!(
            doc.assemble(&res),
            Err(AssembleError::Unresolved { region }) if region == conflict
        ));

        res.set(conflict, Choice::Theirs);
        assert_eq!(doc.assemble(&res).unwrap(), "a\nv = 3\nz\n");

        res.set(conflict, Choice::Edited("v = 23\n".to_string()));
        assert_eq!(doc.assemble(&res).unwrap(), "a\nv = 23\nz\n");
    }

    #[test]
    fn assemble_override_beats_default() {
        // OursOnly region defaults to ours, but an explicit Base choice wins
        let doc = MergeDocument::compute("old\n", "new\n", "old\n", MergeOptions::default());
        assert_eq!(doc.assemble(&Resolution::new()).unwrap(), "new\n");
        let mut res = Resolution::new();
        res.set(0, Choice::Base);
        assert_eq!(doc.assemble(&res).unwrap(), "old\n");
    }

    #[test]
    fn origin_choice_assembles_verbatim_and_keeps_its_provenance() {
        let doc = MergeDocument::compute("x\n", "disk\n", "src\n", MergeOptions::default());
        let mut res = Resolution::new();
        res.set(0, Choice::Origin("from origin\n".into()));
        assert_eq!(doc.assemble(&res).unwrap(), "from origin\n");
        // Provenance is the whole point: it must not be an `Edited`.
        assert!(matches!(res.get(0), Some(Choice::Origin(_))));
    }

    // Mutation: three_way ignores `theirs` -> the TheirsOnly region disappears.
    #[test]
    fn three_way_over_integers_matches_the_line_engine_shapes() {
        let same = |a: &[i32], b: &[i32]| a == b;
        let regions = three_way(&[1, 2, 3, 4, 5], &[1, 9, 3, 4, 5], &[1, 2, 3, 8, 5], same);
        let kinds: Vec<RegionKind> = regions.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                RegionKind::Unchanged,
                RegionKind::OursOnly,
                RegionKind::Unchanged,
                RegionKind::TheirsOnly,
                RegionKind::Unchanged
            ]
        );
        assert_eq!(regions[1].ours, 1..2);
        let kinds = |o: &[i32], t: &[i32]| -> Vec<RegionKind> {
            three_way(&[1], o, t, same).iter().map(|r| r.kind).collect()
        };
        assert_eq!(kinds(&[2], &[3]), [RegionKind::Conflict]);
        assert_eq!(kinds(&[2], &[2]), [RegionKind::BothSame]);
    }

    // Mutations: (1) use the line rule (adjacency conflicts) — the delete
    // next to the insert becomes one Conflict region; (2) order hunks by
    // start alone (the line rule's tie-break) — disk's deletion of "b" is
    // clustered before source's insertion in front of it, and the regions
    // come out of base order.
    #[test]
    fn element_clustering_conflicts_only_on_shared_base() {
        let same = |a: &[&str], b: &[&str]| a == b;
        let kinds = |b: &[&str], o: &[&str], t: &[&str]| -> Vec<RegionKind> {
            three_way_elements(b, o, t, same)
                .iter()
                .map(|r| r.kind)
                .filter(|k| *k != RegionKind::Unchanged)
                .collect()
        };
        // Source deletes "a"; disk inserts "x" right after it.
        assert_eq!(
            kinds(&["a", "b"], &["a", "x", "b"], &["b"]),
            [RegionKind::TheirsOnly, RegionKind::OursOnly]
        );
        // Source inserts "y" in front of the "b" disk deletes: the
        // insertion comes first.
        assert_eq!(
            kinds(&["a", "b"], &["a"], &["a", "y", "b"]),
            [RegionKind::TheirsOnly, RegionKind::OursOnly]
        );
        // The line engine calls the same input one conflict.
        let lines = three_way(&["a", "b"], &["a", "x", "b"], &["b"], same);
        assert!(lines.iter().any(|r| r.kind == RegionKind::Conflict));
        // Different insertions at one position conflict.
        assert_eq!(
            kinds(&["a"], &["a", "x"], &["a", "y"]),
            [RegionKind::Conflict]
        );
        // Both deleting the same element agree.
        assert_eq!(kinds(&["a", "b"], &["a"], &["a"]), [RegionKind::BothSame]);
    }
}
