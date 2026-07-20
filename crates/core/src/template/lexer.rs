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
    Action {
        class: ActionClass,
        trim_before: bool,
        trim_after: bool,
    },
    Comment {
        trim_before: bool,
        trim_after: bool,
    },
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
    let first = body.split_whitespace().next().unwrap_or("");
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
                segs.push(Segment {
                    kind: SegmentKind::Literal,
                    src: lit_start..i,
                    depth,
                });
            }
            let action_start = i;
            let mut body_from = i + 2;
            let trim_before =
                bytes.get(body_from) == Some(&b'-') && bytes.get(body_from + 1) == Some(&b' ');
            if trim_before {
                body_from += 1;
            }
            let rest = &src[body_from..];
            let is_comment = rest.trim_start().starts_with("/*");
            let (close, trim_after) = if is_comment {
                // A comment is the entire action: {{/* … */}} (or trim variants).
                // `}}` inside the comment must NOT close it — scan for `*/` first.
                let comment_open = body_from + (rest.len() - rest.trim_start().len());
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
                    kind: SegmentKind::Comment {
                        trim_before,
                        trim_after,
                    },
                    src: action_start..end,
                    depth,
                });
            } else {
                let class = classify(body);
                match class {
                    ActionClass::ControlOpen => {
                        segs.push(Segment {
                            kind: SegmentKind::Action {
                                class,
                                trim_before,
                                trim_after,
                            },
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
                            kind: SegmentKind::Action {
                                class,
                                trim_before,
                                trim_after,
                            },
                            src: action_start..end,
                            depth,
                        });
                    }
                    _ => {
                        segs.push(Segment {
                            kind: SegmentKind::Action {
                                class,
                                trim_before,
                                trim_after,
                            },
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
        segs.push(Segment {
            kind: SegmentKind::Literal,
            src: lit_start..src.len(),
            depth,
        });
    }
    Ok(segs)
}

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
        assert!(matches!(
            segs[1].kind,
            SegmentKind::Action {
                class: ActionClass::Value,
                trim_before: false,
                trim_after: false
            }
        ));
        assert_eq!(text(src, &segs[1]), "{{ .email }}");
        assert_eq!(text(src, &segs[2]), "!\n");
    }

    #[test]
    fn trim_markers_are_recorded() {
        let src = "a\n{{- if .x -}}\nb\n{{- end }}\n";
        let segs = lex_ok(src);
        let SegmentKind::Action {
            class,
            trim_before,
            trim_after,
        } = segs[1].kind
        else {
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
        assert!(matches!(
            segs[0].kind,
            SegmentKind::Action {
                class: ActionClass::Value,
                ..
            }
        ));
        assert!(matches!(segs[1].kind, SegmentKind::Comment { .. }));
        assert_eq!(text(src, &segs[2]), "end");
    }

    #[test]
    fn errors_are_reported() {
        assert!(matches!(
            lex("a {{ .x "),
            Err(LexError::UnclosedAction { .. })
        ));
        assert!(matches!(
            lex("a {{ end }}"),
            Err(LexError::UnbalancedEnd { .. })
        ));
    }
}
