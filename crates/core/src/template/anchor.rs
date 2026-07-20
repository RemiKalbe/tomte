//! Anchor template literal segments in rendered output → protected spans.

use std::ops::Range;

use super::lexer::{ActionClass, Segment, SegmentKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanOrigin {
    Literal {
        segment: usize,
        src: Range<usize>,
        repeated: bool,
    },
    Action {
        segment: usize,
    },
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
            .filter(|s| {
                matches!(
                    s.origin,
                    SpanOrigin::Literal {
                        repeated: false,
                        ..
                    }
                )
            })
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
    /// whether the literal sits inside a `range` control block
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
                let first = body.split_whitespace().next().unwrap_or("");
                match class {
                    ActionClass::ControlOpen => {
                        open_stack.push(if first == "range" { "range" } else { "other" })
                    }
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
                    SegmentKind::Action { trim_after, .. }
                    | SegmentKind::Comment { trim_after, .. } => Some(trim_after),
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
                    SegmentKind::Action { trim_before, .. }
                    | SegmentKind::Comment { trim_before, .. } => Some(trim_before),
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
                        in_range_block: open_stack.contains(&"range"),
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
fn gap_profile(
    segments: &[Segment],
    after: Option<usize>,
    before: Option<usize>,
) -> (usize, usize, Option<usize>) {
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
    (
        value_actions,
        control_actions,
        if value_actions == 1 { only_value } else { None },
    )
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
    let first_is_doc_start = segments
        .first()
        .map(|s| matches!(s.kind, SegmentKind::Literal))
        .unwrap_or(false);
    let last_is_doc_end = segments
        .last()
        .map(|s| matches!(s.kind, SegmentKind::Literal))
        .unwrap_or(false);
    let mut anchored: Vec<Anchored> = Vec::new();
    let mut cursor = 0usize;
    let mut failed = false;
    for (k, lit) in d0.iter().enumerate() {
        let needle = &template[lit.src.clone()];
        let is_first = k == 0 && first_is_doc_start && lit.segment == 0;
        let is_last = k == d0.len() - 1 && last_is_doc_end && lit.segment == segments.len() - 1;
        let at = if is_first {
            if rendered.starts_with(needle) {
                Some(0)
            } else {
                None
            }
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
                anchored.push(Anchored {
                    lit: (*lit).clone(),
                    at,
                });
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
            let prev_seg = if i == 0 {
                None
            } else {
                Some(anchored[i - 1].lit.segment)
            };
            fill_gap(
                template,
                segments,
                &lits,
                prev_seg,
                Some(a.lit.segment),
                pos..a.at,
                rendered,
                &mut spans,
            );
        }
        let needle_len = a.lit.src.len();
        spans.push(RenderedSpan {
            range: a.at..a.at + needle_len,
            origin: SpanOrigin::Literal {
                segment: a.lit.segment,
                src: a.lit.src.clone(),
                repeated: false,
            },
        });
        pos = a.at + needle_len;
    }
    if failed || pos < rendered.len() {
        let prev_seg = anchored.last().map(|a| a.lit.segment);
        if failed {
            // conservative: protect the whole tail
            spans.push(RenderedSpan {
                range: pos..rendered.len(),
                origin: SpanOrigin::Unmapped,
            });
        } else {
            fill_gap(
                template,
                segments,
                &lits,
                prev_seg,
                None,
                pos..rendered.len(),
                rendered,
                &mut spans,
            );
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
            spans.push(RenderedSpan {
                range: gap,
                origin: SpanOrigin::Action { segment: seg },
            });
        } else {
            spans.push(RenderedSpan {
                range: gap,
                origin: SpanOrigin::Unmapped,
            });
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
    let repeat_count = if inner.is_empty() {
        0
    } else {
        local.len() / inner.len()
    };
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
            spans.push(RenderedSpan {
                range: gap.start + pos..gap.start + r.start,
                origin,
            });
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
        spans.push(RenderedSpan {
            range: gap.start + pos..gap.end,
            origin,
        });
    }
}

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
        assert!(matches!(
            map.spans[0].origin,
            SpanOrigin::Literal {
                repeated: false,
                ..
            }
        ));
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
        assert!(
            map2.spans
                .iter()
                .all(|s| !matches!(s.origin, SpanOrigin::Unmapped))
        );
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
            assert!(matches!(
                s.origin,
                SpanOrigin::Literal { repeated: true, .. }
            ));
        }
    }

    #[test]
    fn unmatched_required_literal_protects_tail() {
        let tmpl = "aaa BBB ccc";
        let rendered = "aaa XXX zzz"; // literal template but rendered diverges
        let map = spans_of(tmpl, rendered);
        tiles(&map, rendered.len());
        assert!(matches!(
            map.spans.last().unwrap().origin,
            SpanOrigin::Unmapped
        ));
    }

    #[test]
    fn trim_markers_shrink_anchored_literals() {
        let tmpl = "a\n{{- if .x }}\nb{{ end }}\n";
        // {{- trims the newline after "a"; rendered with .x true:
        let rendered = "a\nb\n";
        let map = spans_of(tmpl, rendered);
        tiles(&map, rendered.len());
        assert!(
            map.spans
                .iter()
                .all(|s| !matches!(s.origin, SpanOrigin::Unmapped))
        );
    }

    #[test]
    fn coverage_metric() {
        let tmpl = "x = {{ .v }}\n";
        let map = spans_of(tmpl, "x = 1\n");
        let c = map.literal_coverage();
        assert!(c > 0.7 && c < 1.0, "coverage was {c}");
    }
}
