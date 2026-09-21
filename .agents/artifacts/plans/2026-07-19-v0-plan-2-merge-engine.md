# chezmoi-ui v0 — Plan 2: Merge Engine & Template Span Mapping

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The pure merge engine (spec §6): structured 3-way merge regions (`MergeDocument`), resolution assembly, word-level highlighting support, and the template pipeline — Go-template lexer → rendered-span anchoring → verified write-back — plus a `template-spans` debug binary.

**Architecture:** All in `czui-core`, pure (no I/O) except the final verification helper and integration tests, which call `chezmoi execute-template` through the existing `ChezmoiClient`. Diffing uses `imara-diff` 0.2 (API verified against local crate source: `InternedInput::new` / `Diff::compute(Algorithm::Histogram, …)` / `diff.hunks()` yielding `Hunk { before: Range<u32>, after: Range<u32> }` in monotonic order; `TokenSource` trait = `{ type Token: Hash+Eq; type Tokenizer: Iterator; fn tokenize; fn estimate_tokens }`). We feed our own `split_inclusive('\n')` line slices so reassembly is byte-exact.

**Tech Stack:** Rust stable (edition 2024), imara-diff 0.2, existing czui-core modules from Plan 1.

**Prerequisites:** Plan 1 complete (`czui_core::{cmd, chezmoi, git, drift, scanner}` exist; 20 tests green).

## Global Constraints

Identical to Plan 1's (see `2026-07-19-v0-plan-1-foundation-core.md`), plus:

- New workspace dependency: `imara-diff = "0.2"` — no other new dependencies (no regex, no proptest).
- Engine code (`merge`, `template::lexer`, `template::anchor`, `template::writeback`) must be pure: no filesystem, no subprocess. Only `template::verify` and integration tests may touch `ChezmoiClient`.
- **Safety invariant (spec §6.2):** only bytes attributed to a *non-repeated Literal* span are ever editable via write-back; Action, Unmapped, and repeated-Literal spans are protected. Write-back output is never trusted without re-render verification.
- Integration tests must be hermetic: `chezmoi execute-template` runs with `--config` pointing at a scratch config in a temp dir (its `[data]` block supplies template variables); never the user's real config.

## File Structure

```
crates/core/src/lib.rs            # + pub mod merge; pub mod template;
crates/core/src/merge.rs          # MergeDocument, regions, Resolution, assemble, SliceTokens
crates/core/src/merge/worddiff.rs # word-level changed-range computation
crates/core/src/template.rs       # pub mod lexer; pub mod anchor; pub mod writeback; pub mod verify;
crates/core/src/template/lexer.rs
crates/core/src/template/anchor.rs
crates/core/src/template/writeback.rs
crates/core/src/template/verify.rs
crates/core/src/bin/template-spans.rs
crates/core/tests/templates/gitconfig.tmpl
crates/core/tests/templates/env-nu.tmpl
crates/core/tests/templates/aliases.tmpl
crates/core/tests/template_roundtrip.rs
```

---

### Task 1: MergeDocument region model

**Files:**
- Modify: `Cargo.toml` (workspace deps: add `imara-diff = "0.2"`), `crates/core/Cargo.toml` (add `imara-diff.workspace = true`), `crates/core/src/lib.rs` (add `pub mod merge;`)
- Create: `crates/core/src/merge.rs`

**Interfaces:**
- Consumes: imara-diff (verified API above).
- Produces (later tasks and the UI depend on these exact names):
  - `SliceTokens<'a>(pub &'a [&'a str])` implementing `imara_diff::intern::TokenSource` (reused by Task 3).
  - `MergeOptions { ignore_trailing_whitespace: bool }` (`Default`: false)
  - `RegionKind::{Unchanged, OursOnly, TheirsOnly, BothSame, Conflict}`
  - `MergeRegion { kind: RegionKind, base: Range<usize>, ours: Range<usize>, theirs: Range<usize> }` (line-index ranges)
  - `MergeDocument::compute(base: &str, ours: &str, theirs: &str, opts: MergeOptions) -> MergeDocument`
  - `MergeDocument { pub regions: Vec<MergeRegion>, … }` with `base_lines()/ours_lines()/theirs_lines() -> &[String]` accessors (lines keep their `\n`, split via `split_inclusive`)
  - `MergeDocument::required_decisions(&self) -> Vec<usize>` (indices of `Conflict` regions)

- [x] **Step 1: Write the failing tests** (`#[cfg(test)]` in `merge.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
                RegionKind::Unchanged, // a
                RegionKind::OursOnly,  // b -> B
                RegionKind::Unchanged, // c
                RegionKind::TheirsOnly, // d -> D
                RegionKind::Unchanged, // e
            ]
        );
        let ours_region = &doc.regions[1];
        assert_eq!(doc.ours_lines()[ours_region.ours.clone()], ["B\n".to_string()]);
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
        let doc = MergeDocument::compute("a\nz\n", "a\nours\nz\n", "a\ntheirs\nz\n", MergeOptions::default());
        assert_eq!(kinds(&doc), vec![RegionKind::Unchanged, RegionKind::Conflict, RegionKind::Unchanged]);
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
            MergeOptions { ignore_trailing_whitespace: true },
        );
        assert_eq!(kinds(&relaxed), vec![RegionKind::BothSame]);
    }

    #[test]
    fn missing_trailing_newline_roundtrips() {
        let doc = MergeDocument::compute("a\nb", "a\nb", "a\nb", MergeOptions::default());
        assert_eq!(doc.base_lines().concat(), "a\nb");
    }
}
```

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-core merge`
Expected: compile errors — types not defined.

- [x] **Step 3: Implement**

`crates/core/src/merge.rs`:
```rust
//! Structured 3-way merge (spec §6.1): no conflict markers, a region list.

use std::ops::Range;

use imara_diff::intern::{InternedInput, TokenSource};
use imara_diff::{Algorithm, Diff, Hunk};

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
        out.push(Cluster { base, ours: oi..i, theirs: ti..j });
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
                    if eq_lines(&doc_ours[ours_range.clone()], &doc_theirs[theirs_range.clone()], opts)
                    {
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
        Self { regions, base_lines: doc_base, ours_lines: doc_ours, theirs_lines: doc_theirs }
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
```

Note for the implementer: `pub mod worddiff;` requires the file to exist — create `crates/core/src/merge/worddiff.rs` containing only `//! see plan task 3` so the crate compiles (Task 3 fills it).

- [x] **Step 4: Run tests**

Run: `cargo test -p czui-core merge`
Expected: 7 passed.

- [x] **Step 5: Full gate + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green (27 tests total: 20 prior + 7 new).

```bash
git add Cargo.toml Cargo.lock crates/core/Cargo.toml crates/core/src/lib.rs crates/core/src/merge.rs crates/core/src/merge
git commit -m "feat(core): MergeDocument 3-way region model over imara-diff"
```

---

### Task 2: Resolution assembly

**Files:**
- Modify: `crates/core/src/merge.rs`

**Interfaces:**
- Produces:
  - `Choice::{Ours, Theirs, Base, Edited(String)}`
  - `Resolution` with `Resolution::new()`, `set(region_index: usize, choice: Choice)`, `get(region_index) -> Option<&Choice>`
  - `AssembleError::Unresolved { region: usize }` (thiserror)
  - `MergeDocument::assemble(&self, res: &Resolution) -> Result<String, AssembleError>` — defaults: `Unchanged`→base, `OursOnly`→ours, `TheirsOnly`→theirs, `BothSame`→ours; `Conflict` requires a choice; an explicit choice on ANY region overrides its default (spec §5: merge editor always available, including on auto regions).

- [x] **Step 1: Write the failing tests** (append to `merge.rs` tests)

```rust
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
```

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-core merge`
Expected: compile errors (`Resolution` not defined).

- [x] **Step 3: Implement** (append to `merge.rs`)

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Ours,
    Theirs,
    Base,
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
                (Some(Choice::Ours), _) => out.push_str(&self.ours_lines[region.ours.clone()].concat()),
                (Some(Choice::Theirs), _) => {
                    out.push_str(&self.theirs_lines[region.theirs.clone()].concat())
                }
                (Some(Choice::Base), _) => out.push_str(&self.base_lines[region.base.clone()].concat()),
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
                (None, RegionKind::Conflict) => return Err(AssembleError::Unresolved { region: idx }),
            }
        }
        Ok(out)
    }
}
```

- [x] **Step 4: Run tests**

Run: `cargo test -p czui-core merge`
Expected: 9 passed.

- [x] **Step 5: Full gate + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add crates/core/src/merge.rs
git commit -m "feat(core): resolution assembly with per-region choices and overrides"
```

---

### Task 3: Word-level diff spans

**Files:**
- Modify: `crates/core/src/merge/worddiff.rs`

**Interfaces:**
- Consumes: `SliceTokens` from Task 1.
- Produces:
  - `WordDiff { changed_a: Vec<Range<usize>>, changed_b: Vec<Range<usize>> }` — byte ranges of changed runs per side, for intra-region highlighting (spec §6.1).
  - `word_diff(a: &str, b: &str) -> WordDiff`
  - Tokens: maximal runs of either word bytes (`alphanumeric | '_' | '.' | '-'`) or non-word bytes; if either side exceeds 5000 tokens, fall back to whole-string ranges.

- [x] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_strings_have_no_changes() {
        let d = word_diff("alias g=git\n", "alias g=git\n");
        assert!(d.changed_a.is_empty() && d.changed_b.is_empty());
    }

    #[test]
    fn single_word_change_is_isolated() {
        let a = "profile = \"work\"\n";
        let b = "profile = \"personal\"\n";
        let d = word_diff(a, b);
        assert_eq!(d.changed_a.len(), 1);
        assert_eq!(&a[d.changed_a[0].clone()], "work");
        assert_eq!(&b[d.changed_b[0].clone()], "personal");
    }

    #[test]
    fn insertion_only_marks_b() {
        let a = "a c\n";
        let b = "a b c\n";
        let d = word_diff(a, b);
        assert!(d.changed_a.is_empty());
        assert_eq!(d.changed_b.len(), 1);
        // exact alignment (" b" vs "b ") is the diff algorithm's choice;
        // assert the inserted word is captured either way
        assert_eq!(b[d.changed_b[0].clone()].trim(), "b");
    }
}
```

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-core worddiff`
Expected: compile errors.

- [x] **Step 3: Implement**

`crates/core/src/merge/worddiff.rs`:
```rust
//! Word-level changed ranges inside a merge region (highlighting support).

use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput};

use super::SliceTokens;

const MAX_TOKENS: usize = 5000;

#[derive(Debug, Default)]
pub struct WordDiff {
    pub changed_a: Vec<Range<usize>>,
    pub changed_b: Vec<Range<usize>>,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.' || c == '-'
}

/// Maximal runs of word / non-word characters, with their byte offsets.
fn tokens(s: &str) -> (Vec<&str>, Vec<usize>) {
    let mut toks = Vec::new();
    let mut offs = Vec::new();
    let mut start = 0usize;
    let mut prev_word: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let w = is_word(c);
        if let Some(pw) = prev_word {
            if pw != w {
                toks.push(&s[start..i]);
                offs.push(start);
                start = i;
            }
        }
        prev_word = Some(w);
    }
    if prev_word.is_some() {
        toks.push(&s[start..]);
        offs.push(start);
    }
    (toks, offs)
}

pub fn word_diff(a: &str, b: &str) -> WordDiff {
    let (ta, oa) = tokens(a);
    let (tb, ob) = tokens(b);
    if ta.len() > MAX_TOKENS || tb.len() > MAX_TOKENS {
        return WordDiff {
            changed_a: if a.is_empty() { vec![] } else { vec![0..a.len()] },
            changed_b: if b.is_empty() { vec![] } else { vec![0..b.len()] },
        };
    }
    let input = InternedInput::new(SliceTokens(&ta), SliceTokens(&tb));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut out = WordDiff::default();
    let byte_range = |toks: &[&str], offs: &[usize], r: std::ops::Range<u32>| -> Option<Range<usize>> {
        if r.is_empty() {
            return None;
        }
        let (s, e) = (r.start as usize, r.end as usize - 1);
        Some(offs[s]..offs[e] + toks[e].len())
    };
    for h in diff.hunks() {
        if let Some(r) = byte_range(&ta, &oa, h.before.clone()) {
            out.changed_a.push(r);
        }
        if let Some(r) = byte_range(&tb, &ob, h.after.clone()) {
            out.changed_b.push(r);
        }
    }
    out
}
```

- [x] **Step 4: Run tests**

Run: `cargo test -p czui-core worddiff`
Expected: 3 passed.

- [x] **Step 5: Full gate + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add crates/core/src/merge/worddiff.rs
git commit -m "feat(core): word-level diff spans for intra-region highlighting"
```

---

### Task 4: Go-template lexer

**Files:**
- Modify: `crates/core/src/lib.rs` (add `pub mod template;`)
- Create: `crates/core/src/template.rs` (module root: `pub mod lexer;` plus placeholder files for `anchor`, `writeback`, `verify` as `//! see plan task N`)
- Create: `crates/core/src/template/lexer.rs`

**Interfaces:**
- Produces (Tasks 5–6 depend on exact names):
  - `ActionClass::{Value, ControlOpen, ControlElse, ControlClose}`
  - `SegmentKind::{Literal, Action { class: ActionClass, trim_before: bool, trim_after: bool }, Comment { trim_before: bool, trim_after: bool }}`
  - `Segment { kind: SegmentKind, src: Range<usize>, depth: u32 }` — `src` indexes the template source. For `Literal`, `depth` = control-flow nesting it sits in; for actions, the depth *inside which the action token itself sits* (an `end` has the depth of the block it closes minus… see rule below).
  - Depth rule: `Literal.depth` = number of unclosed `ControlOpen` actions before it. `ControlOpen` carries the depth *before* opening; `ControlClose` carries the depth *after* closing. So a literal inside one `if` has depth 1; top-level literals have depth 0.
  - `LexError::{UnclosedAction { pos: usize }, UnbalancedEnd { pos: usize }}` (thiserror)
  - `lex(src: &str) -> Result<Vec<Segment>, LexError>` — segments tile `0..src.len()` exactly; empty literals are omitted.
  - Control keywords (first word of action body): `if`, `range`, `with`, `block`, `define` → `ControlOpen`; `else` → `ControlElse`; `end` → `ControlClose`; everything else (including `template`, variables, pipelines) → `Value`.
  - String handling inside actions: `"…"` with `\` escapes and `` `…` `` raw strings — `}}` inside either does not close the action.

- [x] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn lex_ok(src: &str) -> Vec<Segment> {
        lex(src).unwrap()
    }
    fn text<'a>(src: &'a str, seg: &Segment) -> &'a str {
        &src[seg.src.clone()]
    }

    #[test]
    fn plain_text_is_one_literal() {
        let src = "hello\nworld\n";
        let segs = lex_ok(src);
        assert_eq!(segs.len(), 1);
        assert!(matches!(segs[0].kind, SegmentKind::Literal));
        assert_eq!(text(src, &segs[0]), src);
    }

    #[test]
    fn value_action_between_literals() {
        let src = "email = {{ .email }}!\n";
        let segs = lex_ok(src);
        assert_eq!(segs.len(), 3);
        assert!(matches!(segs[0].kind, SegmentKind::Literal));
        assert!(
            matches!(segs[1].kind, SegmentKind::Action { class: ActionClass::Value, trim_before: false, trim_after: false })
        );
        assert_eq!(text(src, &segs[1]), "{{ .email }}");
        assert_eq!(text(src, &segs[2]), "!\n");
    }

    #[test]
    fn trim_markers_are_recorded() {
        let src = "a\n{{- if .x -}}\nb\n{{- end }}\n";
        let segs = lex_ok(src);
        let SegmentKind::Action { class, trim_before, trim_after } = segs[1].kind else {
            panic!("expected action");
        };
        assert_eq!(class, ActionClass::ControlOpen);
        assert!(trim_before && trim_after);
    }

    #[test]
    fn depths_track_nesting() {
        let src = "t{{ if .a }}x{{ if .b }}y{{ end }}z{{ end }}u";
        let segs = lex_ok(src);
        let lit_depths: Vec<(String, u32)> = segs
            .iter()
            .filter(|s| matches!(s.kind, SegmentKind::Literal))
            .map(|s| (text(src, s).to_string(), s.depth))
            .collect();
        assert_eq!(
            lit_depths,
            vec![
                ("t".to_string(), 0),
                ("x".to_string(), 1),
                ("y".to_string(), 2),
                ("z".to_string(), 1),
                ("u".to_string(), 0),
            ]
        );
    }

    #[test]
    fn strings_hide_closers_and_comments_lex() {
        let src = r#"{{ printf "}}" }}{{/* note }} here */}}end"#;
        let segs = lex_ok(src);
        assert!(matches!(segs[0].kind, SegmentKind::Action { class: ActionClass::Value, .. }));
        assert!(matches!(segs[1].kind, SegmentKind::Comment { .. }));
        assert_eq!(text(src, &segs[2]), "end");
    }

    #[test]
    fn errors_are_reported() {
        assert!(matches!(lex("a {{ .x "), Err(LexError::UnclosedAction { .. })));
        assert!(matches!(lex("a {{ end }}"), Err(LexError::UnbalancedEnd { .. })));
    }
}
```

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-core template::lexer`
Expected: compile errors.

- [x] **Step 3: Implement**

`crates/core/src/template.rs`:
```rust
//! Template span pipeline (spec §6.2): lex → anchor → write-back → verify.

pub mod anchor;
pub mod lexer;
pub mod verify;
pub mod writeback;
```

`crates/core/src/template/lexer.rs`:
```rust
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionClass {
    Value,
    ControlOpen,
    ControlElse,
    ControlClose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Literal,
    Action { class: ActionClass, trim_before: bool, trim_after: bool },
    Comment { trim_before: bool, trim_after: bool },
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub kind: SegmentKind,
    pub src: Range<usize>,
    pub depth: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum LexError {
    #[error("unclosed template action starting at byte {pos}")]
    UnclosedAction { pos: usize },
    #[error("unbalanced {{{{ end }}}} at byte {pos}")]
    UnbalancedEnd { pos: usize },
}

fn classify(body: &str) -> ActionClass {
    let first = body.trim_start().split_whitespace().next().unwrap_or("");
    match first {
        "if" | "range" | "with" | "block" | "define" => ActionClass::ControlOpen,
        "else" => ActionClass::ControlElse,
        "end" => ActionClass::ControlClose,
        _ => ActionClass::Value,
    }
}

/// Find the end of an action body starting right after `{{` (or `{{-`),
/// honoring double-quoted (with escapes) and backquoted strings.
/// Returns byte offset of the closing `}}` and whether `-}}` was used.
fn find_close(src: &str, from: usize) -> Option<(usize, bool)> {
    let bytes = src.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'`' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'`' {
                    i += 1;
                }
                i += 1;
            }
            b'}' if bytes.get(i + 1) == Some(&b'}') => {
                let trim_after = i >= 1 && bytes[i - 1] == b'-' && i >= 2 && bytes[i - 2] == b' ';
                return Some((i, trim_after));
            }
            _ => i += 1,
        }
    }
    None
}

pub fn lex(src: &str) -> Result<Vec<Segment>, LexError> {
    let mut segs = Vec::new();
    let mut depth: u32 = 0;
    let mut lit_start = 0usize;
    let mut i = 0usize;
    let bytes = src.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'{' && bytes.get(i + 1) == Some(&b'{') {
            if i > lit_start {
                segs.push(Segment { kind: SegmentKind::Literal, src: lit_start..i, depth });
            }
            let action_start = i;
            let mut body_from = i + 2;
            let trim_before = bytes.get(body_from) == Some(&b'-')
                && bytes.get(body_from + 1) == Some(&b' ');
            if trim_before {
                body_from += 1;
            }
            let rest = &src[body_from..];
            let is_comment = rest.trim_start().starts_with("/*");
            let (close, trim_after) = if is_comment {
                // A comment is the entire action: {{/* … */}} (or trim variants).
                // `}}` inside the comment must NOT close it — scan for `*/` first.
                let comment_open =
                    body_from + (rest.len() - rest.trim_start().len());
                let rel = src[comment_open + 2..]
                    .find("*/")
                    .ok_or(LexError::UnclosedAction { pos: action_start })?;
                let mut k = comment_open + 2 + rel + 2;
                while src.as_bytes().get(k) == Some(&b' ') {
                    k += 1;
                }
                let trim = src.as_bytes().get(k) == Some(&b'-');
                if trim {
                    k += 1;
                }
                if !src[k..].starts_with("}}") {
                    return Err(LexError::UnclosedAction { pos: action_start });
                }
                (k, trim)
            } else {
                find_close(src, body_from).ok_or(LexError::UnclosedAction { pos: action_start })?
            };
            let end = close + 2;
            let body = &src[body_from..close.min(src.len())];
            if is_comment {
                segs.push(Segment {
                    kind: SegmentKind::Comment { trim_before, trim_after },
                    src: action_start..end,
                    depth,
                });
            } else {
                let class = classify(body);
                match class {
                    ActionClass::ControlOpen => {
                        segs.push(Segment {
                            kind: SegmentKind::Action { class, trim_before, trim_after },
                            src: action_start..end,
                            depth,
                        });
                        depth += 1;
                    }
                    ActionClass::ControlClose => {
                        if depth == 0 {
                            return Err(LexError::UnbalancedEnd { pos: action_start });
                        }
                        depth -= 1;
                        segs.push(Segment {
                            kind: SegmentKind::Action { class, trim_before, trim_after },
                            src: action_start..end,
                            depth,
                        });
                    }
                    _ => {
                        segs.push(Segment {
                            kind: SegmentKind::Action { class, trim_before, trim_after },
                            src: action_start..end,
                            depth,
                        });
                    }
                }
            }
            i = end;
            lit_start = end;
        } else {
            i += 1;
        }
    }
    if lit_start < src.len() {
        segs.push(Segment { kind: SegmentKind::Literal, src: lit_start..src.len(), depth });
    }
    Ok(segs)
}
```

Implementer note on `find_close`'s `trim_after` detection: Go's syntax for a right trim marker is `` `-}}` preceded by a space `` (i.e. ``" -}}"``), which is what the two-byte lookback checks. If the trim-marker tests fail against `chezmoi execute-template` behavior in Task 7, adjust here — the contract is "record what Go text/template would trim".

- [x] **Step 4: Run tests**

Run: `cargo test -p czui-core template::lexer`
Expected: 6 passed.

- [x] **Step 5: Full gate + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add crates/core/src/lib.rs crates/core/src/template.rs crates/core/src/template
git commit -m "feat(core): Go-template lexer with trim markers, strings, and nesting depth"
```

---

### Task 5: Span anchoring

**Files:**
- Modify: `crates/core/src/template/anchor.rs`

**Interfaces:**
- Consumes: `lexer::{Segment, SegmentKind, ActionClass, lex}`.
- Produces (Task 6 and the UI depend on these):
  - `SpanOrigin::{Literal { segment: usize, src: Range<usize>, repeated: bool }, Action { segment: usize }, Unmapped}`
  - `RenderedSpan { range: Range<usize>, origin: SpanOrigin }`
  - `SpanMap { spans: Vec<RenderedSpan> }` with `SpanMap::literal_coverage(&self) -> f32` (fraction of rendered bytes in non-repeated Literal spans) and `SpanMap::span_at(&self, byte: usize) -> Option<&RenderedSpan>`
  - `anchor(template: &str, segments: &[Segment], rendered: &str) -> SpanMap`
  - **Invariants (tested):** spans tile `0..rendered.len()` in order with no gaps/overlaps; every `Literal` span's rendered bytes equal the (trim-adjusted) template bytes of its `src`.

**Algorithm (conservative by construction — spec §6.2 steps 2–3):**
1. Compute *effective literals*: for each `Literal` segment, trim its text per neighboring trim markers (previous action/comment `trim_after` ⇒ trim leading whitespace; next action/comment `trim_before` ⇒ trim trailing whitespace); record the shrunken `src` range. Drop empties.
2. **Phase A:** anchor depth-0 effective literals in order by first occurrence at/after the cursor. A depth-0 literal that cannot be found ⇒ everything from the cursor to `rendered.len()` becomes one `Unmapped` span; stop.
3. **Phase B:** for each gap between consecutive depth-0 anchors (and before the first / after the last):
   - Collect the segments lying between the two bounding literals in segment order.
   - If the gap contains **no control-flow action** and exactly **one value action**: the whole gap is one `Action` span.
   - If the gap contains **control flow**: match the gap's depth>0 effective literals sequentially *within the gap bounds*; if the innermost enclosing `ControlOpen` keyword is `range`, retry the block's literal subsequence repeatedly while it continues to match (marking those literal spans `repeated: true` from the second pass onward — and retroactively marking the first pass `repeated: true` too, since editing any iteration is unsafe). Bytes in the gap not covered by a matched literal become `Action` if exactly one value action is the nearest unconsumed segment between the surrounding matches, else `Unmapped`.
   - Anything else (multiple adjacent value actions, unmatched conditional literals, leftovers) ⇒ `Unmapped`.
4. Comments produce no rendered bytes and are skipped everywhere.

- [x] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::lexer::lex;

    fn spans_of(tmpl: &str, rendered: &str) -> SpanMap {
        anchor(tmpl, &lex(tmpl).unwrap(), rendered)
    }

    fn tiles(map: &SpanMap, len: usize) {
        let mut pos = 0;
        for s in &map.spans {
            assert_eq!(s.range.start, pos, "gap/overlap at {pos}");
            pos = s.range.end;
        }
        assert_eq!(pos, len, "spans must cover the full rendered text");
    }

    #[test]
    fn pure_value_template_maps_fully() {
        let tmpl = "email = {{ .email }}!\n";
        let rendered = "email = a@b.c!\n";
        let map = spans_of(tmpl, rendered);
        tiles(&map, rendered.len());
        assert_eq!(map.spans.len(), 3);
        assert!(matches!(map.spans[0].origin, SpanOrigin::Literal { repeated: false, .. }));
        assert!(matches!(map.spans[1].origin, SpanOrigin::Action { .. }));
        assert_eq!(&rendered[map.spans[1].range.clone()], "a@b.c");
        assert!(matches!(map.spans[2].origin, SpanOrigin::Literal { .. }));
    }

    #[test]
    fn if_block_present_and_absent() {
        let tmpl = "start\n{{ if .x }}inside\n{{ end }}end\n";
        let present = "start\ninside\nend\n";
        let map = spans_of(tmpl, present);
        tiles(&map, present.len());
        let inside = map
            .spans
            .iter()
            .find(|s| &present[s.range.clone()] == "inside\n")
            .unwrap();
        assert!(matches!(inside.origin, SpanOrigin::Literal { .. }));
        let absent = "start\nend\n";
        let map2 = spans_of(tmpl, absent);
        tiles(&map2, absent.len());
        assert!(map2.spans.iter().all(|s| !matches!(s.origin, SpanOrigin::Unmapped)));
    }

    #[test]
    fn range_blocks_mark_repeated_literals() {
        let tmpl = "# top\n{{ range .shells }}alias {{ . }}\n{{ end }}# bottom\n";
        let rendered = "# top\nalias zsh\nalias nu\n# bottom\n";
        let map = spans_of(tmpl, rendered);
        tiles(&map, rendered.len());
        let alias_spans: Vec<_> = map
            .spans
            .iter()
            .filter(|s| &rendered[s.range.clone()] == "alias ")
            .collect();
        assert_eq!(alias_spans.len(), 2);
        for s in alias_spans {
            assert!(matches!(s.origin, SpanOrigin::Literal { repeated: true, .. }));
        }
    }

    #[test]
    fn unmatched_required_literal_protects_tail() {
        let tmpl = "aaa BBB ccc";
        let rendered = "aaa XXX zzz"; // literal template but rendered diverges
        let map = spans_of(tmpl, rendered);
        tiles(&map, rendered.len());
        assert!(matches!(map.spans.last().unwrap().origin, SpanOrigin::Unmapped));
    }

    #[test]
    fn trim_markers_shrink_anchored_literals() {
        let tmpl = "a\n{{- if .x }}\nb{{ end }}\n";
        // {{- trims the newline after "a"; rendered with .x true:
        let rendered = "a\nb\n";
        let map = spans_of(tmpl, rendered);
        tiles(&map, rendered.len());
        assert!(map.spans.iter().all(|s| !matches!(s.origin, SpanOrigin::Unmapped)));
    }

    #[test]
    fn coverage_metric() {
        let tmpl = "x = {{ .v }}\n";
        let map = spans_of(tmpl, "x = 1\n");
        let c = map.literal_coverage();
        assert!(c > 0.7 && c < 1.0, "coverage was {c}");
    }
}
```

Note: in `if_block_present_and_absent`, the second assertion block contains a deliberately trivial `|| true` guard in the first `assert!` — remove that line entirely when implementing; the meaningful assertions are the `inside` span lookup and the `map2` all-mapped check. (Do not ship an `assert!(… || true)`.)

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-core template::anchor`
Expected: compile errors.

- [x] **Step 3: Implement**

`crates/core/src/template/anchor.rs`:
```rust
//! Anchor template literal segments in rendered output → protected spans.

use std::ops::Range;

use super::lexer::{ActionClass, Segment, SegmentKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanOrigin {
    Literal { segment: usize, src: Range<usize>, repeated: bool },
    Action { segment: usize },
    Unmapped,
}

#[derive(Debug, Clone)]
pub struct RenderedSpan {
    pub range: Range<usize>,
    pub origin: SpanOrigin,
}

#[derive(Debug)]
pub struct SpanMap {
    pub spans: Vec<RenderedSpan>,
}

impl SpanMap {
    pub fn literal_coverage(&self) -> f32 {
        let total: usize = self.spans.iter().map(|s| s.range.len()).sum();
        if total == 0 {
            return 1.0;
        }
        let lit: usize = self
            .spans
            .iter()
            .filter(|s| matches!(s.origin, SpanOrigin::Literal { repeated: false, .. }))
            .map(|s| s.range.len())
            .sum();
        lit as f32 / total as f32
    }

    pub fn span_at(&self, byte: usize) -> Option<&RenderedSpan> {
        self.spans.iter().find(|s| s.range.contains(&byte))
    }
}

/// A literal with trim-adjusted source range.
#[derive(Debug, Clone)]
struct EffLit {
    segment: usize,
    src: Range<usize>,
    depth: u32,
    /// segment index of the innermost enclosing ControlOpen, if any
    in_range_block: bool,
}

fn effective_literals(template: &str, segments: &[Segment]) -> Vec<EffLit> {
    let mut out = Vec::new();
    // Track enclosing ControlOpen keyword stack to know range-blocks.
    let mut open_stack: Vec<&str> = Vec::new();
    for (idx, seg) in segments.iter().enumerate() {
        match &seg.kind {
            SegmentKind::Action { class, .. } => {
                let body = template[seg.src.clone()]
                    .trim_start_matches("{{")
                    .trim_start_matches('-')
                    .trim_end_matches("}}")
                    .trim_end_matches('-');
                let first = body.trim().split_whitespace().next().unwrap_or("");
                match class {
                    ActionClass::ControlOpen => open_stack.push(if first == "range" { "range" } else { "other" }),
                    ActionClass::ControlClose => {
                        open_stack.pop();
                    }
                    _ => {}
                }
            }
            SegmentKind::Literal => {
                let mut src = seg.src.clone();
                // previous non-literal segment trims our leading whitespace?
                let trim_lead = segments[..idx].iter().rev().find_map(|s| match s.kind {
                    SegmentKind::Action { trim_after, .. } | SegmentKind::Comment { trim_after, .. } => {
                        Some(trim_after)
                    }
                    SegmentKind::Literal => None,
                });
                if trim_lead == Some(true) {
                    while src.start < src.end
                        && template.as_bytes()[src.start].is_ascii_whitespace()
                    {
                        src.start += 1;
                    }
                }
                let trim_trail = segments[idx + 1..].iter().find_map(|s| match s.kind {
                    SegmentKind::Action { trim_before, .. } | SegmentKind::Comment { trim_before, .. } => {
                        Some(trim_before)
                    }
                    SegmentKind::Literal => None,
                });
                if trim_trail == Some(true) {
                    while src.end > src.start
                        && template.as_bytes()[src.end - 1].is_ascii_whitespace()
                    {
                        src.end -= 1;
                    }
                }
                if !src.is_empty() {
                    out.push(EffLit {
                        segment: idx,
                        src,
                        depth: seg.depth,
                        in_range_block: open_stack.iter().any(|k| *k == "range"),
                    });
                }
            }
            SegmentKind::Comment { .. } => {}
        }
    }
    out
}

/// Segments strictly between two segment indices that are Value actions /
/// control actions (comments ignored).
fn gap_profile(segments: &[Segment], after: Option<usize>, before: Option<usize>) -> (usize, usize, Option<usize>) {
    let start = after.map(|i| i + 1).unwrap_or(0);
    let end = before.unwrap_or(segments.len());
    let mut value_actions = 0usize;
    let mut control_actions = 0usize;
    let mut only_value: Option<usize> = None;
    for (i, seg) in segments[start..end].iter().enumerate() {
        if let SegmentKind::Action { class, .. } = seg.kind {
            match class {
                ActionClass::Value => {
                    value_actions += 1;
                    only_value = Some(start + i);
                }
                _ => control_actions += 1,
            }
        }
    }
    (value_actions, control_actions, if value_actions == 1 { only_value } else { None })
}

pub fn anchor(template: &str, segments: &[Segment], rendered: &str) -> SpanMap {
    let lits = effective_literals(template, segments);
    let mut spans: Vec<RenderedSpan> = Vec::new();

    // Phase A: anchor depth-0 literals. The template's first/last literal
    // segments pin to the rendered document's start/end — greedy-first alone
    // mis-anchors short literals (a final "\n" would match far too early).
    struct Anchored {
        lit: EffLit,
        at: usize,
    }
    let d0: Vec<&EffLit> = lits.iter().filter(|l| l.depth == 0).collect();
    let first_is_doc_start =
        segments.first().map(|s| matches!(s.kind, SegmentKind::Literal)).unwrap_or(false);
    let last_is_doc_end =
        segments.last().map(|s| matches!(s.kind, SegmentKind::Literal)).unwrap_or(false);
    let mut anchored: Vec<Anchored> = Vec::new();
    let mut cursor = 0usize;
    let mut failed = false;
    for (k, lit) in d0.iter().enumerate() {
        let needle = &template[lit.src.clone()];
        let is_first = k == 0 && first_is_doc_start && lit.segment == 0;
        let is_last =
            k == d0.len() - 1 && last_is_doc_end && lit.segment == segments.len() - 1;
        let at = if is_first {
            if rendered.starts_with(needle) { Some(0) } else { None }
        } else if is_last {
            match rendered.len().checked_sub(needle.len()) {
                Some(s) if s >= cursor && rendered.ends_with(needle) => Some(s),
                _ => None,
            }
        } else {
            rendered[cursor..].find(needle).map(|off| cursor + off)
        };
        match at {
            Some(at) => {
                cursor = at + needle.len();
                anchored.push(Anchored { lit: (*lit).clone(), at });
            }
            None => {
                failed = true;
                break;
            }
        }
    }

    // Emit spans gap-by-gap.
    let mut pos = 0usize;
    for (i, a) in anchored.iter().enumerate() {
        if a.at > pos {
            let prev_seg = if i == 0 { None } else { Some(anchored[i - 1].lit.segment) };
            fill_gap(template, segments, &lits, prev_seg, Some(a.lit.segment), pos..a.at, rendered, &mut spans);
        }
        let needle_len = a.lit.src.len();
        spans.push(RenderedSpan {
            range: a.at..a.at + needle_len,
            origin: SpanOrigin::Literal { segment: a.lit.segment, src: a.lit.src.clone(), repeated: false },
        });
        pos = a.at + needle_len;
    }
    if failed || pos < rendered.len() {
        let prev_seg = anchored.last().map(|a| a.lit.segment);
        if failed {
            // conservative: protect the whole tail
            spans.push(RenderedSpan { range: pos..rendered.len(), origin: SpanOrigin::Unmapped });
        } else {
            fill_gap(template, segments, &lits, prev_seg, None, pos..rendered.len(), rendered, &mut spans);
        }
    }
    SpanMap { spans }
}

/// Attribute one gap between two depth-0 anchors (or document edges).
#[allow(clippy::too_many_arguments)]
fn fill_gap(
    template: &str,
    segments: &[Segment],
    lits: &[EffLit],
    after_seg: Option<usize>,
    before_seg: Option<usize>,
    gap: std::ops::Range<usize>,
    rendered: &str,
    spans: &mut Vec<RenderedSpan>,
) {
    let (value_actions, control_actions, only_value) = gap_profile(segments, after_seg, before_seg);
    if control_actions == 0 {
        if let Some(seg) = only_value {
            spans.push(RenderedSpan { range: gap, origin: SpanOrigin::Action { segment: seg } });
        } else {
            spans.push(RenderedSpan { range: gap, origin: SpanOrigin::Unmapped });
        }
        return;
    }
    let _ = value_actions;

    // Control flow in the gap: try to match inner (depth>0) literals inside it.
    let start_seg = after_seg.map(|i| i + 1).unwrap_or(0);
    let end_seg = before_seg.unwrap_or(segments.len());
    let inner: Vec<&EffLit> = lits
        .iter()
        .filter(|l| l.depth > 0 && l.segment >= start_seg && l.segment < end_seg)
        .collect();

    let gap_text = &rendered[gap.clone()];
    let mut local: Vec<(std::ops::Range<usize>, &EffLit)> = Vec::new(); // gap-relative
    let mut cur = 0usize;
    // repeated passes: keep matching the inner sequence while progress is made
    let mut any_repeat = false;
    loop {
        let mut matched_this_pass = 0usize;
        for lit in &inner {
            let needle = &template[lit.src.clone()];
            if let Some(off) = gap_text[cur..].find(needle) {
                let at = cur + off;
                local.push((at..at + needle.len(), lit));
                cur = at + needle.len();
                matched_this_pass += 1;
            }
        }
        if matched_this_pass == 0 || cur >= gap_text.len() {
            break;
        }
        // Only range blocks legitimately repeat; a second successful pass
        // means repetition happened.
        if matched_this_pass > 0 && local.len() > inner.len() {
            any_repeat = true;
        }
        if inner.is_empty() || !inner.iter().any(|l| l.in_range_block) {
            break;
        }
    }
    let repeat_count = if inner.is_empty() { 0 } else { local.len() / inner.len() };
    let repeated = any_repeat || repeat_count > 1;

    // Emit: unmatched stretches → Action (single value action) or Unmapped.
    let mut pos = 0usize;
    let single_value_gap = |a: usize, b: usize| -> SpanOrigin {
        // between two matched inner literals: attribute to a value action only
        // if exactly one value action sits between those segments
        let (_, c, only) = gap_profile(segments, Some(a), Some(b));
        match only {
            Some(seg) if c == 0 => SpanOrigin::Action { segment: seg },
            _ => SpanOrigin::Unmapped,
        }
    };
    let mut prev_seg = after_seg;
    for (r, lit) in &local {
        if r.start > pos {
            let origin = match prev_seg {
                Some(p) => single_value_gap(p, lit.segment),
                None => SpanOrigin::Unmapped,
            };
            spans.push(RenderedSpan { range: gap.start + pos..gap.start + r.start, origin });
        }
        spans.push(RenderedSpan {
            range: gap.start + r.start..gap.start + r.end,
            origin: SpanOrigin::Literal {
                segment: lit.segment,
                src: lit.src.clone(),
                repeated: repeated || lit.in_range_block,
            },
        });
        pos = r.end;
        prev_seg = Some(lit.segment);
    }
    if pos < gap_text.len() {
        let origin = match (prev_seg, before_seg) {
            (Some(p), Some(b)) => single_value_gap(p, b),
            _ => SpanOrigin::Unmapped,
        };
        spans.push(RenderedSpan { range: gap.start + pos..gap.end, origin });
    }
}
```

Implementer note: this algorithm is deliberately conservative — when in doubt, `Unmapped` (protected). The tests define the contract; if an assertion about *which* protected origin (Action vs Unmapped) fails, prefer adjusting toward `Unmapped` rather than loosening protection. The tiling invariant and the Literal-bytes-equality invariant are non-negotiable.

- [x] **Step 4: Run tests**

Run: `cargo test -p czui-core template::anchor`
Expected: 6 passed.

- [x] **Step 5: Full gate + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add crates/core/src/template/anchor.rs
git commit -m "feat(core): rendered-span anchoring with conservative protection"
```

---

### Task 6: Write-back

**Files:**
- Modify: `crates/core/src/template/writeback.rs`

**Interfaces:**
- Consumes: `anchor::{SpanMap, RenderedSpan, SpanOrigin}`, `merge::{split_lines, SliceTokens}` diff machinery via imara-diff.
- Produces:
  - `WriteBackError::{ProtectedSpanTouched { rendered_range: Range<usize> }, RepeatedLiteral { rendered_range: Range<usize> }, EditOutsideLiteral { rendered_range: Range<usize> }}` (thiserror)
  - `write_back(template: &str, map: &SpanMap, rendered: &str, resolved: &str) -> Result<String, WriteBackError>`
  - Semantics: line-diff `rendered → resolved`; convert line hunks to byte hunks; each hunk must fall strictly inside a single non-repeated `Literal` span (insertions must land strictly inside one, or at the very start/end of the document when that edge byte belongs to a literal span); map the hunk through the span's `src` range and splice into the template (back-to-front). Any hunk overlapping an `Action`/`Unmapped` span ⇒ `ProtectedSpanTouched`; overlapping a repeated literal ⇒ `RepeatedLiteral`; anything else unplaceable ⇒ `EditOutsideLiteral`.

- [x] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::anchor::anchor;
    use crate::template::lexer::lex;

    fn map_for(tmpl: &str, rendered: &str) -> crate::template::anchor::SpanMap {
        anchor(tmpl, &lex(tmpl).unwrap(), rendered)
    }

    #[test]
    fn identity_writeback_returns_template_unchanged() {
        let tmpl = "email = {{ .email }}\neditor = hx\n";
        let rendered = "email = a@b.c\neditor = hx\n";
        let map = map_for(tmpl, rendered);
        assert_eq!(write_back(tmpl, &map, rendered, rendered).unwrap(), tmpl);
    }

    #[test]
    fn literal_edit_lands_in_template() {
        let tmpl = "email = {{ .email }}\neditor = hx\n";
        let rendered = "email = a@b.c\neditor = hx\n";
        let resolved = "email = a@b.c\neditor = nvim\n";
        let map = map_for(tmpl, rendered);
        let out = write_back(tmpl, &map, rendered, resolved).unwrap();
        assert_eq!(out, "email = {{ .email }}\neditor = nvim\n");
    }

    #[test]
    fn edit_touching_action_output_is_rejected() {
        let tmpl = "email = {{ .email }}\neditor = hx\n";
        let rendered = "email = a@b.c\neditor = hx\n";
        let resolved = "email = x@y.z\neditor = hx\n";
        let map = map_for(tmpl, rendered);
        assert!(matches!(
            write_back(tmpl, &map, rendered, resolved),
            Err(WriteBackError::ProtectedSpanTouched { .. })
        ));
    }

    #[test]
    fn edit_in_repeated_literal_is_rejected() {
        let tmpl = "{{ range .shells }}alias {{ . }}\n{{ end }}";
        let rendered = "alias zsh\nalias nu\n";
        let resolved = "alia zsh\nalias nu\n"; // edits first "alias " occurrence
        let map = map_for(tmpl, rendered);
        assert!(matches!(
            write_back(tmpl, &map, rendered, resolved),
            Err(WriteBackError::RepeatedLiteral { .. }) | Err(WriteBackError::ProtectedSpanTouched { .. })
        ));
    }

    #[test]
    fn multi_line_literal_edit_including_insertion() {
        let tmpl = "# header\nline1\nline2\n";
        let rendered = tmpl; // no actions at all
        let resolved = "# header\nline1\nline1.5\nline2 changed\n";
        let map = map_for(tmpl, rendered);
        assert_eq!(write_back(tmpl, &map, rendered, resolved).unwrap(), resolved);
    }
}
```

- [x] **Step 2: Run to verify failure**

Run: `cargo test -p czui-core template::writeback`
Expected: compile errors.

- [x] **Step 3: Implement**

`crates/core/src/template/writeback.rs`:
```rust
//! Map resolved-text edits back into the template source (spec §6.2 step 4).

use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput};

use crate::merge::{split_lines, SliceTokens};
use crate::template::anchor::{SpanMap, SpanOrigin};

#[derive(Debug, thiserror::Error)]
pub enum WriteBackError {
    #[error("edit at rendered bytes {rendered_range:?} touches a protected template span")]
    ProtectedSpanTouched { rendered_range: Range<usize> },
    #[error("edit at rendered bytes {rendered_range:?} touches a repeated (range-block) literal")]
    RepeatedLiteral { rendered_range: Range<usize> },
    #[error("edit at rendered bytes {rendered_range:?} cannot be placed in a literal span")]
    EditOutsideLiteral { rendered_range: Range<usize> },
}

struct ByteHunk {
    rendered: Range<usize>,
    replacement: String,
}

fn line_starts(lines: &[&str]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(lines.len() + 1);
    let mut pos = 0;
    for l in lines {
        starts.push(pos);
        pos += l.len();
    }
    starts.push(pos);
    starts
}

fn byte_hunks(rendered: &str, resolved: &str) -> Vec<ByteHunk> {
    let r_lines = split_lines(rendered);
    let s_lines = split_lines(resolved);
    let r_starts = line_starts(&r_lines);
    let s_starts = line_starts(&s_lines);
    let input = InternedInput::new(SliceTokens(&r_lines), SliceTokens(&s_lines));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    diff.hunks()
        .map(|h| ByteHunk {
            rendered: r_starts[h.before.start as usize]..r_starts[h.before.end as usize],
            replacement: resolved[s_starts[h.after.start as usize]..s_starts[h.after.end as usize]]
                .to_string(),
        })
        .collect()
}

pub fn write_back(
    template: &str,
    map: &SpanMap,
    rendered: &str,
    resolved: &str,
) -> Result<String, WriteBackError> {
    let hunks = byte_hunks(rendered, resolved);
    // template edits collected as (template_range, replacement)
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();

    for h in &hunks {
        // find the single literal span containing this hunk
        let mut owner: Option<&crate::template::anchor::RenderedSpan> = None;
        for span in &map.spans {
            let overlaps = span.range.start < h.rendered.end && h.rendered.start < span.range.end;
            let insertion_inside = h.rendered.is_empty()
                && span.range.start <= h.rendered.start
                && h.rendered.start <= span.range.end
                && !span.range.is_empty();
            if overlaps || insertion_inside {
                match &span.origin {
                    SpanOrigin::Literal { repeated: true, .. } => {
                        return Err(WriteBackError::RepeatedLiteral {
                            rendered_range: h.rendered.clone(),
                        })
                    }
                    SpanOrigin::Literal { .. } => {
                        if owner.is_some() && overlaps {
                            // spans two literals — cannot split the replacement
                            return Err(WriteBackError::EditOutsideLiteral {
                                rendered_range: h.rendered.clone(),
                            });
                        }
                        if owner.is_none() {
                            owner = Some(span);
                        }
                    }
                    SpanOrigin::Action { .. } | SpanOrigin::Unmapped => {
                        if overlaps {
                            return Err(WriteBackError::ProtectedSpanTouched {
                                rendered_range: h.rendered.clone(),
                            });
                        }
                    }
                }
            }
        }
        let Some(owner) = owner else {
            return Err(WriteBackError::EditOutsideLiteral { rendered_range: h.rendered.clone() });
        };
        // hunk must be fully inside the owner span
        if h.rendered.start < owner.range.start || h.rendered.end > owner.range.end {
            return Err(WriteBackError::ProtectedSpanTouched { rendered_range: h.rendered.clone() });
        }
        let SpanOrigin::Literal { src, .. } = &owner.origin else { unreachable!() };
        let off_start = h.rendered.start - owner.range.start;
        let off_end = h.rendered.end - owner.range.start;
        edits.push((src.start + off_start..src.start + off_end, h.replacement.clone()));
    }

    // apply back-to-front so earlier offsets stay valid
    edits.sort_by(|a, b| a.0.start.cmp(&b.0.start));
    let mut out = template.to_string();
    for (range, replacement) in edits.into_iter().rev() {
        out.replace_range(range, &replacement);
    }
    Ok(out)
}
```

- [x] **Step 4: Run tests**

Run: `cargo test -p czui-core template::writeback`
Expected: 5 passed.

- [x] **Step 5: Full gate + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add crates/core/src/template/writeback.rs
git commit -m "feat(core): span-mapped write-back with protected-span rejection"
```

---

### Task 7: Verification helper + corpus round-trip integration tests

**Files:**
- Modify: `crates/core/src/template/verify.rs`
- Create: `crates/core/tests/templates/gitconfig.tmpl`, `crates/core/tests/templates/env-nu.tmpl`, `crates/core/tests/templates/aliases.tmpl`, `crates/core/tests/template_roundtrip.rs`

**Interfaces:**
- Consumes: `ChezmoiClient::execute_template` (Plan 1), lexer/anchor/writeback (Tasks 4–6).
- Produces:
  - `VerifyError::{Chezmoi(ChezmoiError), Mismatch { expected: Vec<u8>, actual: Vec<u8> }}`
  - `verify_write_back(chezmoi: &ChezmoiClient, new_template: &str, expected: &str) -> Result<(), VerifyError>` — re-renders and byte-compares (spec §6.2 step 4: never trust the mapping blindly).

- [x] **Step 1: Create the corpus fixtures**

`crates/core/tests/templates/gitconfig.tmpl`:
```
[user]
    name = {{ .name }}
    email = {{ .email }}
[core]
    editor = hx
```

`crates/core/tests/templates/env-nu.tmpl`:
```
$env.EDITOR = "hx"
{{ if .work }}$env.AWS_PROFILE = "work"
{{ end }}$env.HOSTNAME = "{{ .hostname }}"
```

`crates/core/tests/templates/aliases.tmpl`:
```
# generated
{{ range .shells }}alias run-{{ . }}="echo {{ . }}"
{{ end }}# end
```

- [x] **Step 2: Implement the verify helper**

`crates/core/src/template/verify.rs`:
```rust
//! Re-render verification: the runtime twin of the tests' round-trip invariant.

use crate::chezmoi::{ChezmoiClient, ChezmoiError};

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error(transparent)]
    Chezmoi(#[from] ChezmoiError),
    #[error("re-rendered template does not match the resolved text")]
    Mismatch { expected: Vec<u8>, actual: Vec<u8> },
}

pub fn verify_write_back(
    chezmoi: &ChezmoiClient,
    new_template: &str,
    expected: &str,
) -> Result<(), VerifyError> {
    let actual = chezmoi.execute_template(new_template.as_bytes())?;
    if actual != expected.as_bytes() {
        return Err(VerifyError::Mismatch { expected: expected.as_bytes().to_vec(), actual });
    }
    Ok(())
}
```

- [x] **Step 3: Write the integration tests**

`crates/core/tests/template_roundtrip.rs`:
```rust
//! Round-trip invariant (spec §11): render → anchor → write-back → re-render.
//! Uses the real `chezmoi execute-template` with a hermetic scratch config.

use std::path::PathBuf;
use std::sync::Arc;

use czui_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use czui_core::cmd::SystemRunner;
use czui_core::template::anchor::anchor;
use czui_core::template::lexer::lex;
use czui_core::template::verify::verify_write_back;
use czui_core::template::writeback::{write_back, WriteBackError};

fn scratch_chezmoi() -> (tempfile::TempDir, ChezmoiClient) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("chezmoi.toml");
    std::fs::write(
        &cfg,
        r#"
[data]
name = "Test User"
email = "t@example.com"
hostname = "testbox"
work = true
shells = ["zsh", "nu"]
"#,
    )
    .unwrap();
    let source = dir.path().join("source");
    let dest = dir.path().join("home");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    let opts = ChezmoiOptions {
        base_args: vec![
            "--config".into(),
            cfg.to_string_lossy().into_owned(),
            "--source".into(),
            source.to_string_lossy().into_owned(),
            "--destination".into(),
            dest.to_string_lossy().into_owned(),
            "--no-tty".into(),
            "--no-pager".into(),
        ],
        ..ChezmoiOptions::default()
    };
    (dir, ChezmoiClient::new(Arc::new(SystemRunner), opts))
}

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/templates").join(name);
    std::fs::read_to_string(p).unwrap()
}

const CORPUS: &[&str] = &["gitconfig.tmpl", "env-nu.tmpl", "aliases.tmpl"];

#[test]
fn identity_roundtrip_leaves_template_unchanged() {
    let (_g, chezmoi) = scratch_chezmoi();
    for name in CORPUS {
        let tmpl = fixture(name);
        let rendered = String::from_utf8(chezmoi.execute_template(tmpl.as_bytes()).unwrap()).unwrap();
        let map = anchor(&tmpl, &lex(&tmpl).unwrap(), &rendered);
        let out = write_back(&tmpl, &map, &rendered, &rendered)
            .unwrap_or_else(|e| panic!("{name}: identity write-back failed: {e}"));
        assert_eq!(out, tmpl, "{name}: identity write-back must not change the template");
        verify_write_back(&chezmoi, &out, &rendered).unwrap();
    }
}

#[test]
fn literal_mutation_roundtrips_through_rerender() {
    let (_g, chezmoi) = scratch_chezmoi();
    let tmpl = fixture("gitconfig.tmpl");
    let rendered = String::from_utf8(chezmoi.execute_template(tmpl.as_bytes()).unwrap()).unwrap();
    let resolved = rendered.replace("editor = hx", "editor = nvim");
    assert_ne!(rendered, resolved);
    let map = anchor(&tmpl, &lex(&tmpl).unwrap(), &rendered);
    let new_tmpl = write_back(&tmpl, &map, &rendered, &resolved).unwrap();
    assert!(new_tmpl.contains("editor = nvim"));
    assert!(new_tmpl.contains("{{ .email }}"), "template expressions must survive");
    verify_write_back(&chezmoi, &new_tmpl, &resolved).unwrap();
}

#[test]
fn if_block_template_supports_literal_edit_outside_block() {
    let (_g, chezmoi) = scratch_chezmoi();
    let tmpl = fixture("env-nu.tmpl");
    let rendered = String::from_utf8(chezmoi.execute_template(tmpl.as_bytes()).unwrap()).unwrap();
    let resolved = rendered.replace("\"hx\"", "\"nvim\"");
    let map = anchor(&tmpl, &lex(&tmpl).unwrap(), &rendered);
    let new_tmpl = write_back(&tmpl, &map, &rendered, &resolved).unwrap();
    verify_write_back(&chezmoi, &new_tmpl, &resolved).unwrap();
}

#[test]
fn editing_action_output_is_rejected_not_clobbered() {
    let (_g, chezmoi) = scratch_chezmoi();
    let tmpl = fixture("gitconfig.tmpl");
    let rendered = String::from_utf8(chezmoi.execute_template(tmpl.as_bytes()).unwrap()).unwrap();
    let resolved = rendered.replace("t@example.com", "evil@example.com");
    let map = anchor(&tmpl, &lex(&tmpl).unwrap(), &rendered);
    match write_back(&tmpl, &map, &rendered, &resolved) {
        Err(WriteBackError::ProtectedSpanTouched { .. }) => {}
        other => panic!("expected ProtectedSpanTouched, got {other:?}"),
    }
}

#[test]
fn range_block_output_edits_are_rejected() {
    let (_g, chezmoi) = scratch_chezmoi();
    let tmpl = fixture("aliases.tmpl");
    let rendered = String::from_utf8(chezmoi.execute_template(tmpl.as_bytes()).unwrap()).unwrap();
    // editing one iteration's literal text must not silently edit the template
    let resolved = rendered.replacen("alias run-", "alias go-", 1);
    let map = anchor(&tmpl, &lex(&tmpl).unwrap(), &rendered);
    assert!(write_back(&tmpl, &map, &rendered, &resolved).is_err());
}
```

- [x] **Step 4: Run the integration tests**

Run: `cargo test -p czui-core --test template_roundtrip`
Expected: 5 passed. These exercise real `chezmoi execute-template`; if any anchoring/trim assumption disagrees with chezmoi's actual rendering, fix the lexer/anchorer (Tasks 4–5), NOT the test.

- [x] **Step 5: Full gate + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add crates/core/src/template/verify.rs crates/core/tests/templates crates/core/tests/template_roundtrip.rs
git commit -m "feat(core): re-render verification and corpus round-trip integration tests"
```

---

### Task 8: `template-spans` debug binary

**Files:**
- Create: `crates/core/src/bin/template-spans.rs`

**Interfaces:**
- Consumes: `ChezmoiClient` (real), lexer/anchor.
- Produces: `cargo run -p czui-core --bin template-spans -- <target-path>` — resolves the target's source via `chezmoi source-path`, renders via `chezmoi cat`, prints the rendered text with protected spans wrapped in `⟦…⟧` and a coverage summary. Read-only; Plan 2's real-machine smoke deliverable.

- [x] **Step 1: Implement**

```rust
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use czui_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use czui_core::cmd::SystemRunner;
use czui_core::template::anchor::{anchor, SpanOrigin};
use czui_core::template::lexer::lex;

fn main() -> ExitCode {
    let Some(target) = std::env::args().nth(1) else {
        eprintln!("usage: template-spans <target-path>");
        return ExitCode::FAILURE;
    };
    let target = PathBuf::from(target);
    let chezmoi = ChezmoiClient::new(Arc::new(SystemRunner), ChezmoiOptions::default());

    let source = match chezmoi.source_path(&target) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: source-path failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let template = match std::fs::read_to_string(&source) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", source.display());
            return ExitCode::FAILURE;
        }
    };
    if source.extension().and_then(|e| e.to_str()) != Some("tmpl") {
        println!("{} is not a template — every byte is editable.", source.display());
        return ExitCode::SUCCESS;
    }
    let rendered = match chezmoi.cat(&target) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            eprintln!("error: chezmoi cat failed (secret manager?): {e}");
            return ExitCode::FAILURE;
        }
    };
    let segments = match lex(&template) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: lex failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let map = anchor(&template, &segments, &rendered);
    let mut out = String::new();
    for span in &map.spans {
        let text = &rendered[span.range.clone()];
        match span.origin {
            SpanOrigin::Literal { repeated: false, .. } => out.push_str(text),
            SpanOrigin::Literal { repeated: true, .. } => {
                out.push_str(&format!("⟦R:{text}⟧"));
            }
            SpanOrigin::Action { .. } => out.push_str(&format!("⟦A:{text}⟧")),
            SpanOrigin::Unmapped => out.push_str(&format!("⟦U:{text}⟧")),
        }
    }
    println!("{out}");
    println!(
        "-- literal coverage: {:.0}% editable ({} spans)",
        map.literal_coverage() * 100.0,
        map.spans.len()
    );
    ExitCode::SUCCESS
}
```

- [x] **Step 2: Smoke-run on the real machine (read-only)**

Run: `chezmoi managed -i templates --path-style=absolute | head -5` and pick a template target, preferring one without secret functions. Then:
`cargo run -p czui-core --bin template-spans -- <picked-target>`
Expected: the rendered file with `⟦A:…⟧` around template-derived values (or an `EvalFailed`-style error mentioning the secret manager for 1Password-dependent templates — that is also a correct outcome; try another target then). No panic either way.

- [x] **Step 3: Full gate + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add crates/core/src/bin/template-spans.rs
git commit -m "feat(core): template-spans debug binary showing protected spans"
```

---

## Self-Review Notes (completed during plan writing)

- **Spec coverage for this plan's slice:** §6.1 MergeDocument/regions/normalization/word-level ✓ (Tasks 1–3); §6.2 lex/anchor/write-back/verify + conservative control-flow handling ✓ (Tasks 4–7); §11 round-trip invariant as both runtime check and test ✓ (Task 7). §6.3 undo/session snapshots need the journal → Plan 3. Assisted-manual mode is UI → Plan 5.
- **Type consistency:** `SliceTokens` shared by Tasks 1/3/6; `SpanOrigin::Literal { segment, src, repeated }` consistent across Tasks 5/6/8; `split_lines` public in merge.rs, used by writeback.
- **Known simplifications (accepted for v0, all fail toward protection):** write-back requires each hunk inside a single literal span (comment-adjacent literal runs and cross-span edits are rejected → assisted manual); anchoring attributes multi-value-action gaps as Unmapped; `else` branches are handled by the generic conditional-literal matching, not modeled specially; range-repetition detection marks ALL literals in a range block repeated (even single-iteration renders), which is conservative.
- **Fixes applied during review:** (1) lexer comment handling scans for `*/` so `}}` inside comments can't close the action; (2) Phase A anchoring pins the template's first/last literals to the rendered document's edges, killing the short-literal mis-anchor (`"\n"` matching too early); (3) removed an `expect()` from lib code; (4) de-brittled the word-diff insertion assertion and the if-block anchor test.
