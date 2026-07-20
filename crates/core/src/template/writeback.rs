//! Map resolved-text edits back into the template source (spec §6.2 step 4).

use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput};

use crate::merge::{SliceTokens, split_lines};
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
                        });
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
            return Err(WriteBackError::EditOutsideLiteral {
                rendered_range: h.rendered.clone(),
            });
        };
        // hunk must be fully inside the owner span
        if h.rendered.start < owner.range.start || h.rendered.end > owner.range.end {
            return Err(WriteBackError::ProtectedSpanTouched {
                rendered_range: h.rendered.clone(),
            });
        }
        let SpanOrigin::Literal { src, .. } = &owner.origin else {
            unreachable!()
        };
        let off_start = h.rendered.start - owner.range.start;
        let off_end = h.rendered.end - owner.range.start;
        edits.push((
            src.start + off_start..src.start + off_end,
            h.replacement.clone(),
        ));
    }

    // apply back-to-front so earlier offsets stay valid
    edits.sort_by_key(|e| e.0.start);
    let mut out = template.to_string();
    for (range, replacement) in edits.into_iter().rev() {
        out.replace_range(range, &replacement);
    }
    Ok(out)
}

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
            Err(WriteBackError::RepeatedLiteral { .. })
                | Err(WriteBackError::ProtectedSpanTouched { .. })
        ));
    }

    #[test]
    fn multi_line_literal_edit_including_insertion() {
        let tmpl = "# header\nline1\nline2\n";
        let rendered = tmpl; // no actions at all
        let resolved = "# header\nline1\nline1.5\nline2 changed\n";
        let map = map_for(tmpl, rendered);
        assert_eq!(
            write_back(tmpl, &map, rendered, resolved).unwrap(),
            resolved
        );
    }
}
