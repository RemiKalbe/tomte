//! A small CommonMark subset, parsed and rendered — enough for release
//! notes and CHANGELOG.md, which is all this app ever displays.
//!
//! Deliberately not a full engine: no tables, no reference links, no nested
//! block containers. What it does cover it covers properly — headings,
//! nested bullet and ordered lists, fenced code, block quotes, rules,
//! paragraphs, and inline `code` / **bold** / *italic* / [links](url) —
//! instead of the previous "strip the `**` and hope" pass that also gave up
//! after fourteen lines and sent you to GitHub.
//!
//! Parsing is pure and tested; rendering is a straight fold over the blocks.

use std::ops::Range;

use gpui::{
    AnyElement, Div, FontStyle, FontWeight, HighlightStyle, SharedString, StyledText,
    UnderlineStyle, div, prelude::*, px,
};

use crate::theme::Theme;

/// An inline run's meaning. Rendering maps these onto text styles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mark {
    Bold,
    Italic,
    Code,
    /// Link text; the destination rides along for the tooltip/underline.
    Link(String),
}

/// A line of inline content: the plain text plus the marked-up ranges.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inline {
    pub text: String,
    pub marks: Vec<(Range<usize>, Mark)>,
}

/// One block-level element.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading {
        level: u8,
        content: Inline,
    },
    Paragraph(Inline),
    /// `depth` is the nesting level (0 = top). `marker` is the rendered
    /// bullet or number.
    ListItem {
        depth: usize,
        marker: String,
        content: Inline,
    },
    Code {
        text: String,
    },
    Quote(Inline),
    Rule,
}

/// Parse inline markup. Code spans win over everything (their content is
/// literal); the rest nest naively, which is all release notes need.
pub fn parse_inline(src: &str) -> Inline {
    let mut out = String::with_capacity(src.len());
    let mut marks: Vec<(Range<usize>, Mark)> = Vec::new();
    let bytes: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // `code`
        if c == '`'
            && let Some(end) = (i + 1..bytes.len()).find(|&j| bytes[j] == '`')
        {
            let start = out.len();
            out.extend(&bytes[i + 1..end]);
            marks.push((start..out.len(), Mark::Code));
            i = end + 1;
            continue;
        }
        // **bold**
        if c == '*'
            && i + 1 < bytes.len()
            && bytes[i + 1] == '*'
            && let Some(end) = (i + 2..bytes.len().saturating_sub(1))
                .find(|&j| bytes[j] == '*' && bytes[j + 1] == '*')
        {
            let start = out.len();
            let inner: String = bytes[i + 2..end].iter().collect();
            let nested = parse_inline(&inner);
            out.push_str(&nested.text);
            marks.push((start..out.len(), Mark::Bold));
            marks.extend(
                nested
                    .marks
                    .into_iter()
                    .map(|(r, m)| (start + r.start..start + r.end, m)),
            );
            i = end + 2;
            continue;
        }
        // *italic* / _italic_
        if (c == '*' || c == '_')
            && let Some(end) = (i + 1..bytes.len()).find(|&j| bytes[j] == c)
            && end > i + 1
        {
            let start = out.len();
            let inner: String = bytes[i + 1..end].iter().collect();
            let nested = parse_inline(&inner);
            out.push_str(&nested.text);
            marks.push((start..out.len(), Mark::Italic));
            marks.extend(
                nested
                    .marks
                    .into_iter()
                    .map(|(r, m)| (start + r.start..start + r.end, m)),
            );
            i = end + 1;
            continue;
        }
        // [text](url)
        if c == '['
            && let Some(close) = (i + 1..bytes.len()).find(|&j| bytes[j] == ']')
            && bytes.get(close + 1) == Some(&'(')
            && let Some(paren) = (close + 2..bytes.len()).find(|&j| bytes[j] == ')')
        {
            let start = out.len();
            let label: String = bytes[i + 1..close].iter().collect();
            let url: String = bytes[close + 2..paren].iter().collect();
            out.push_str(&label);
            marks.push((start..out.len(), Mark::Link(url)));
            i = paren + 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    Inline { text: out, marks }
}

/// Parse a markdown document into blocks.
pub fn parse(src: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut para: Vec<String> = Vec::new();
    // The open list item, accumulating its continuation lines. Markdown
    // wraps a bullet across source lines by indenting the rest; without
    // this, every wrapped bullet in CHANGELOG.md broke into a bullet plus
    // an orphan paragraph.
    let mut item: Option<(usize, String, Vec<String>)> = None;
    let mut code: Option<Vec<String>> = None;

    fn flush_item(item: &mut Option<(usize, String, Vec<String>)>, blocks: &mut Vec<Block>) {
        if let Some((depth, marker, lines)) = item.take() {
            blocks.push(Block::ListItem {
                depth,
                marker,
                content: parse_inline(&lines.join(" ")),
            });
        }
    }
    fn flush_para(para: &mut Vec<String>, blocks: &mut Vec<Block>) {
        if !para.is_empty() {
            blocks.push(Block::Paragraph(parse_inline(&para.join(" "))));
            para.clear();
        }
    }

    for raw in src.lines() {
        // Fenced code swallows everything until the closing fence.
        if raw.trim_start().starts_with("```") {
            match code.take() {
                Some(lines) => blocks.push(Block::Code {
                    text: lines.join("\n"),
                }),
                None => {
                    flush_item(&mut item, &mut blocks);
                    flush_para(&mut para, &mut blocks);
                    code = Some(Vec::new());
                }
            }
            continue;
        }
        if let Some(lines) = code.as_mut() {
            lines.push(raw.to_string());
            continue;
        }

        let trimmed = raw.trim_end();
        if trimmed.trim().is_empty() {
            flush_item(&mut item, &mut blocks);
            flush_para(&mut para, &mut blocks);
            continue;
        }
        // Rule: --- / *** / ___
        let compact: String = trimmed
            .trim()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if compact.len() >= 3
            && (compact.chars().all(|c| c == '-')
                || compact.chars().all(|c| c == '*')
                || compact.chars().all(|c| c == '_'))
        {
            flush_item(&mut item, &mut blocks);
            flush_para(&mut para, &mut blocks);
            blocks.push(Block::Rule);
            continue;
        }
        // Heading
        if let Some(rest) = trimmed.strip_prefix('#') {
            let extra = rest.chars().take_while(|&c| c == '#').count();
            let level = (1 + extra).min(6) as u8;
            let text = trimmed[(level as usize)..].trim();
            flush_item(&mut item, &mut blocks);
            flush_para(&mut para, &mut blocks);
            blocks.push(Block::Heading {
                level,
                content: parse_inline(text),
            });
            continue;
        }
        // Quote
        if let Some(rest) = trimmed.trim_start().strip_prefix("> ") {
            flush_item(&mut item, &mut blocks);
            flush_para(&mut para, &mut blocks);
            blocks.push(Block::Quote(parse_inline(rest)));
            continue;
        }
        // List item (bullet or ordered), nesting by leading spaces.
        let indent = trimmed.len() - trimmed.trim_start().len();
        let body = trimmed.trim_start();
        let bullet = ["- ", "* ", "+ "]
            .iter()
            .find_map(|p| body.strip_prefix(p))
            .map(|rest| ("•".to_string(), rest));
        let ordered = body
            .find(". ")
            .filter(|&dot| dot > 0 && body[..dot].chars().all(|c| c.is_ascii_digit()))
            .map(|dot| (format!("{}.", &body[..dot]), &body[dot + 2..]));
        if let Some((marker, rest)) = bullet.or(ordered) {
            flush_item(&mut item, &mut blocks);
            flush_para(&mut para, &mut blocks);
            item = Some((indent / 2, marker, vec![rest.to_string()]));
            continue;
        }
        // Plain text: continues the open list item, else the paragraph.
        match item.as_mut() {
            Some((_, _, lines)) => lines.push(body.to_string()),
            None => para.push(body.to_string()),
        }
    }
    flush_item(&mut item, &mut blocks);
    flush_para(&mut para, &mut blocks);
    if let Some(lines) = code {
        blocks.push(Block::Code {
            text: lines.join("\n"),
        });
    }
    blocks
}

/// Render one inline run with its marks applied.
fn inline_el(inline: &Inline, theme: Theme, base: Option<FontWeight>) -> AnyElement {
    if inline.marks.is_empty() {
        return div()
            .when_some(base, |el, w| el.font_weight(w))
            .child(SharedString::from(inline.text.clone()))
            .into_any_element();
    }
    let highlights = inline.marks.iter().map(|(range, mark)| {
        let style = match mark {
            Mark::Bold => HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
            Mark::Italic => HighlightStyle {
                font_style: Some(FontStyle::Italic),
                ..Default::default()
            },
            Mark::Code => HighlightStyle {
                background_color: Some(Theme::wash(theme.text_muted, 0.15).into()),
                color: Some(theme.text.into()),
                ..Default::default()
            },
            Mark::Link(_) => HighlightStyle {
                color: Some(theme.accent.into()),
                underline: Some(UnderlineStyle {
                    color: Some(Theme::wash(theme.accent, 0.5).into()),
                    thickness: px(1.),
                    wavy: false,
                }),
                ..Default::default()
            },
        };
        (range.clone(), style)
    });
    div()
        .when_some(base, |el, w| el.font_weight(w))
        .child(StyledText::new(SharedString::from(inline.text.clone())).with_highlights(highlights))
        .into_any_element()
}

/// Render parsed markdown as a column of blocks.
pub fn render(blocks: &[Block], theme: Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .text_sm()
        .text_color(theme.text)
        .children(blocks.iter().map(|block| {
            match block {
                Block::Heading { level, content } => div()
                    .when(*level <= 2, |el| {
                        el.pt_3()
                            .pb_1()
                            .border_b_1()
                            .border_color(Theme::wash(theme.border, 0.8))
                    })
                    .when(*level > 2, |el| el.pt_1())
                    .map(|el| match level {
                        1 => el.text_xl(),
                        2 => el.text_lg(),
                        3 => el.text_base(),
                        _ => el.text_sm(),
                    })
                    .child(inline_el(content, theme, Some(FontWeight::SEMIBOLD)))
                    .into_any_element(),
                Block::Paragraph(content) => div()
                    .child(inline_el(content, theme, None))
                    .into_any_element(),
                Block::ListItem {
                    depth,
                    marker,
                    content,
                } => div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .pl(px(16. * *depth as f32))
                    .child(
                        div()
                            .flex_none()
                            .w_4()
                            .text_color(theme.text_muted)
                            .child(SharedString::from(marker.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(inline_el(content, theme, None)),
                    )
                    .into_any_element(),
                Block::Code { text } => div()
                    .p_2()
                    .rounded_md()
                    .bg(Theme::wash(theme.text_muted, 0.08))
                    .border_1()
                    .border_color(Theme::wash(theme.border, 0.8))
                    .font_family("Menlo")
                    .text_xs()
                    .child(SharedString::from(text.clone()))
                    .into_any_element(),
                Block::Quote(content) => div()
                    .pl_3()
                    .border_l_2()
                    .border_color(Theme::wash(theme.accent, 0.5))
                    .text_color(theme.text_muted)
                    .child(inline_el(content, theme, None))
                    .into_any_element(),
                Block::Rule => div()
                    .h_px()
                    .my_2()
                    .bg(Theme::wash(theme.border, 0.8))
                    .into_any_element(),
            }
        }))
}

#[cfg(test)]
mod tests {
    use super::{Block, Mark, parse, parse_inline};

    #[test]
    fn inline_marks_cover_the_right_ranges() {
        let i = parse_inline("plain **bold** and `code` and [link](http://x) end");
        assert_eq!(i.text, "plain bold and code and link end");
        let marked: Vec<(&str, &Mark)> = i
            .marks
            .iter()
            .map(|(r, m)| (&i.text[r.clone()], m))
            .collect();
        assert_eq!(
            marked,
            vec![
                ("bold", &Mark::Bold),
                ("code", &Mark::Code),
                ("link", &Mark::Link("http://x".into())),
            ]
        );
    }

    #[test]
    fn code_spans_are_literal() {
        // The `**` inside a code span must survive as text, not become bold.
        let i = parse_inline("use `a ** b` here");
        assert_eq!(i.text, "use a ** b here");
        assert_eq!(i.marks.len(), 1);
        assert_eq!(i.marks[0].1, Mark::Code);
    }

    #[test]
    fn italics_do_not_swallow_bold() {
        let i = parse_inline("**b** then *i*");
        assert_eq!(i.text, "b then i");
        assert_eq!(i.marks[0].1, Mark::Bold);
        assert_eq!(i.marks[1].1, Mark::Italic);
    }

    #[test]
    fn blocks_cover_a_changelog_section() {
        let src = "## 0.1.13 — 2026-08-20\n\n\
                   ### Changed\n\n\
                   - Editing is whole-file now\n\
                   - Save is disabled while editing\n  \
                     - nested detail\n\n\
                   1. first\n\
                   2. second\n\n\
                   Some paragraph that\nwraps two lines.\n\n\
                   ```\ncode block\n```\n\n\
                   ---\n\n\
                   > a quote\n";
        let blocks = parse(src);
        assert!(matches!(blocks[0], Block::Heading { level: 2, .. }));
        assert!(matches!(blocks[1], Block::Heading { level: 3, .. }));
        let items: Vec<(&usize, &String)> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::ListItem { depth, marker, .. } => Some((depth, marker)),
                _ => None,
            })
            .collect();
        assert_eq!(
            items
                .iter()
                .map(|(d, m)| (**d, m.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "•"), (0, "•"), (1, "•"), (0, "1."), (0, "2.")],
            "bullets nest by indent and ordered markers keep their number"
        );
        // The wrapped paragraph is joined into one block.
        assert!(blocks.iter().any(|b| matches!(
            b,
            Block::Paragraph(i) if i.text == "Some paragraph that wraps two lines."
        )));
        assert!(blocks.iter().any(|b| matches!(
            b, Block::Code { text } if text == "code block"
        )));
        assert!(blocks.iter().any(|b| matches!(b, Block::Rule)));
        assert!(blocks.iter().any(|b| matches!(
            b, Block::Quote(i) if i.text == "a quote"
        )));
    }

    #[test]
    fn a_wrapped_bullet_stays_one_bullet() {
        // CHANGELOG.md wraps long entries with an indented continuation.
        // Treating those as paragraphs split every long entry in two.
        let blocks = parse("- first line\n  continues here\n  and here\n- second\n");
        let items: Vec<&str> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::ListItem { content, .. } => Some(content.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(items, vec!["first line continues here and here", "second"]);
        assert!(
            !blocks.iter().any(|b| matches!(b, Block::Paragraph(_))),
            "a continuation must not become its own paragraph"
        );
    }

    #[test]
    fn a_dashed_rule_is_not_mistaken_for_a_bullet() {
        let blocks = parse("- item\n\n---\n");
        assert!(matches!(blocks[0], Block::ListItem { .. }));
        assert!(matches!(blocks[1], Block::Rule));
    }
}
