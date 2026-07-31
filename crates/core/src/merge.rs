//! Structured 3-way merge (spec §6.1): no conflict markers, a region list.

use std::ops::Range;

use imara_diff::{Algorithm, Diff, Hunk, InternedInput, TokenSource};

pub mod worddiff;

/// Line tokens we control, so reassembly is byte-exact (lines keep `\n`).
pub struct SliceTokens<'a>(pub &'a [&'a str]);

impl<'a> TokenSource for SliceTokens<'a> {
    type Token = &'a str;
    type Tokenizer = std::iter::Copied<std::slice::Iter<'a, &'a str>>;
    fn tokenize(&self) -> Self::Tokenizer {
        self.0.iter().copied()
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
fn side_hunks(base: &[&str], side: &[&str]) -> Vec<Hunk> {
    let input = InternedInput::new(SliceTokens(base), SliceTokens(side));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    diff.hunks().collect()
}

/// Ranges "touch" if they overlap or are adjacent — adjacency clusters
/// same-point insertions from both sides into a conflict (conservative).
fn touches(a: &Range<u32>, b: &Range<u32>) -> bool {
    a.start <= b.end && b.start <= a.end
}

struct Cluster {
    base: Range<u32>,
    /// index ranges into the per-side hunk vectors
    ours: Range<usize>,
    theirs: Range<usize>,
}

fn clusters(hunks_o: &[Hunk], hunks_t: &[Hunk]) -> Vec<Cluster> {
    let (mut i, mut j) = (0usize, 0usize);
    let mut out = Vec::new();
    while i < hunks_o.len() || j < hunks_t.len() {
        let take_ours = match (hunks_o.get(i), hunks_t.get(j)) {
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
                if touches(&h.before, &base) {
                    base.start = base.start.min(h.before.start);
                    base.end = base.end.max(h.before.end);
                    i += 1;
                    grew = true;
                } else {
                    break;
                }
            }
            while let Some(h) = hunks_t.get(j) {
                if touches(&h.before, &base) {
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

fn eq_lines(a: &[String], b: &[String], opts: MergeOptions) -> bool {
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

impl MergeDocument {
    pub fn compute(base: &str, ours: &str, theirs: &str, opts: MergeOptions) -> Self {
        let b = split_lines(base);
        let o = split_lines(ours);
        let t = split_lines(theirs);
        let hunks_o = side_hunks(&b, &o);
        let hunks_t = side_hunks(&b, &t);

        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let doc_base = own(&b);
        let doc_ours = own(&o);
        let doc_theirs = own(&t);

        let mut regions = Vec::new();
        let mut cursor: u32 = 0;
        for c in clusters(&hunks_o, &hunks_t) {
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
                    if eq_lines(
                        &doc_ours[ours_range.clone()],
                        &doc_theirs[theirs_range.clone()],
                        opts,
                    ) {
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
        if (cursor as usize) < doc_base.len() || regions.is_empty() {
            regions.push(MergeRegion {
                kind: RegionKind::Unchanged,
                base: cursor as usize..doc_base.len(),
                ours: map_pos(&hunks_o, hunks_o.len(), cursor)..doc_ours.len(),
                theirs: map_pos(&hunks_t, hunks_t.len(), cursor)..doc_theirs.len(),
            });
        }
        Self {
            regions,
            base_lines: doc_base,
            ours_lines: doc_ours,
            theirs_lines: doc_theirs,
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
}
