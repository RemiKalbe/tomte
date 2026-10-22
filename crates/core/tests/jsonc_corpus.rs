//! The hand-written settings corpus: every file parses, its ranges tile the
//! text, and the strict `.json` files agree with `serde_json` on their values.

use std::ops::Range;
use std::path::PathBuf;

use tomte_core::structured::jsonc::{Document, NodeId, NodeKind, Num, Value, parse};

const JSONC_FILES: [&str; 3] = [
    "zed-settings.jsonc",
    "vscode-settings.jsonc",
    "opencode.jsonc",
];
const STRICT_FILES: [&str; 2] = ["claude-settings.json", "skill-lock.json"];

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/jsonc")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every range the document hands out as leaf spans: container brackets,
/// scalars, keys, commas, comments.
fn spans(doc: &Document, root: NodeId) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let node = doc.node(id);
        match &node.kind {
            NodeKind::Object { entries } => {
                out.push(node.range.start..node.range.start + 1);
                out.push(node.range.end - 1..node.range.end);
                for e in entries {
                    if !e.leading.is_empty() {
                        out.push(e.leading.clone());
                    }
                    out.push(e.key_range.clone());
                    out.extend(e.comma.clone());
                    out.extend(e.trailing_comment.clone());
                    stack.push(e.value);
                }
            }
            NodeKind::Array { elements } => {
                out.push(node.range.start..node.range.start + 1);
                out.push(node.range.end - 1..node.range.end);
                for e in elements {
                    if !e.leading.is_empty() {
                        out.push(e.leading.clone());
                    }
                    out.extend(e.comma.clone());
                    out.extend(e.trailing_comment.clone());
                    stack.push(e.value);
                }
            }
            _ => out.push(node.range.clone()),
        }
    }
    out
}

/// True when the spans do not overlap and everything between them is
/// whitespace, a key/value colon, or a comment-free gap.
fn tiles(doc: &Document) -> bool {
    let Some(root) = doc.root else { return false };
    let mut all = spans(doc, root);
    all.sort_by_key(|r| r.start);
    let mut cursor = 0;
    for r in &all {
        if r.start < cursor {
            return false;
        }
        if !doc.text[cursor..r.start]
            .chars()
            .all(|c| c.is_whitespace() || c == ':')
        {
            return false;
        }
        cursor = r.end;
    }
    doc.text[cursor..].chars().all(char::is_whitespace)
}

fn to_serde(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Number(Num::Float(f)) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                serde_json::Value::from(*f as i64)
            } else {
                serde_json::Value::from(*f)
            }
        }
        Value::Number(Num::Text(raw)) => serde_json::from_str(raw).unwrap(),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Array(items) => serde_json::Value::Array(items.iter().map(to_serde).collect()),
        Value::Object(map) => {
            serde_json::Value::Object(map.iter().map(|(k, v)| (k.clone(), to_serde(v))).collect())
        }
    }
}

// Mutation: drop a comment's range from the tiling (never record trailing
// comments or leading blocks in the parser) -> the comment text lands in a
// gap and this fails on the JSONC files.
#[test]
fn every_corpus_file_parses_and_tiles() {
    for name in JSONC_FILES.iter().chain(STRICT_FILES.iter()) {
        let text = fixture(name);
        let doc = parse(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(tiles(&doc), "{name}: ranges do not tile the text");
    }
}

// Mutation: unescape strings wrongly or map every number to Float(0.0) in
// Document::value -> the converted value differs from serde_json's and this fails.
#[test]
fn corpus_values_round_trip_through_serde_json() {
    for name in STRICT_FILES {
        let text = fixture(name);
        let expected: serde_json::Value = serde_json::from_str(&text).unwrap();
        let doc = parse(&text).unwrap();
        let actual = to_serde(&doc.value(doc.root.unwrap()));
        assert_eq!(actual, expected, "{name}");
    }
}
