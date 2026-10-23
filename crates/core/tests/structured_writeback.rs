//! The lock rule and the template write-back agree: every decision the lock
//! pass leaves free writes back into the template and re-renders.

use std::sync::Arc;

use tomte_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use tomte_core::cmd::SystemRunner;
use tomte_core::structured::{Pick, Resolution, StructuredDocument};
use tomte_core::template::anchor::anchor;
use tomte_core::template::lexer::lex;
use tomte_core::template::verify::verify_write_back;
use tomte_core::template::writeback::write_back;

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

/// Picks Disk on every decision the lock pass left free, requires the
/// write-back and the re-render to succeed, and only then checks the locked
/// paths and the free count.
fn round_trip(template: &str, ours: &str, locked: &[&str], free: usize) {
    let (_g, chezmoi) = scratch_chezmoi();
    let rendered =
        String::from_utf8(chezmoi.execute_template(template.as_bytes()).unwrap()).unwrap();
    let map = anchor(template, &lex(template).unwrap(), &rendered);
    let doc = StructuredDocument::compute(None, ours, &rendered, Some(&map)).unwrap();
    let mut res = Resolution::default();
    for d in doc.decisions.iter().filter(|d| d.locked.is_none()) {
        res.set(d.id, Pick::Disk);
    }
    let result = doc.assemble(&res).unwrap();
    let new_template = write_back(template, &map, &rendered, &result)
        .unwrap_or_else(|e| panic!("an unlocked Disk pick was refused: {e}"));
    verify_write_back(&chezmoi, &new_template, &result).unwrap();
    let mut locked_paths: Vec<&str> = doc
        .decisions
        .iter()
        .filter(|d| d.locked.is_some())
        .map(|d| d.path.as_str())
        .collect();
    locked_paths.sort_unstable();
    assert_eq!(locked_paths, locked);
    assert_eq!(doc.decisions.len() - locked_paths.len(), free);
}

// Mutation: make touched_lines drop the previous entry's comma line for a
// removal. "tabs" in the second case then stays unlocked, its Disk pick drops
// the comma on the templated "name" line, and write_back refuses with
// ProtectedSpanTouched inside this test's write_back call, before any lock
// list is compared. The first case is the brief's: value edits plus one
// locked templated entry.
#[test]
fn every_unlocked_disk_pick_writes_back_and_re_renders() {
    round_trip(
        "{\n  \"name\": \"{{ .name }}\",\n  \"editor\": \"hx\",\n  \"tabs\": 2\n}\n",
        "{\n  \"name\": \"X\",\n  \"editor\": \"zed\",\n  \"tabs\": 4\n}\n",
        &["name"],
        2,
    );
    round_trip(
        "{\n  \"editor\": \"hx\",\n  \"name\": \"{{ .name }}\",\n  \"tabs\": 2\n}\n",
        "{\n  \"editor\": \"zed\",\n  \"name\": \"X\"\n}\n",
        &["name", "tabs"],
        1,
    );
}

// `{{- if }}` / `{{- end }}` trim the newline before them, so the line above
// each directive belongs to two literal spans and write_back refuses any
// edit of it with EditOutsideLiteral (ruling R27). Mutation: drop
// `.or_else(|| hit.get(1).copied())` from merge.rs's offending_span -> the
// split lines stay unlocked and this test's write_back call refuses their
// Disk picks.
#[test]
fn block_directives_lock_the_lines_they_split() {
    // The reviewer's probe: every entry sits on a split line or reaches one.
    round_trip(
        "{\n  \"editor\": \"hx\",\n  {{- if .work }}\n  \"proxy\": \"corp\",\n  {{- end }}\n  \"tabs\": 2\n}\n",
        "{\n  \"editor\": \"vi\",\n  \"proxy\": \"home\",\n  \"tabs\": 4\n}\n",
        &["editor", "proxy"],
        1,
    );
    // Removing the entry after the block rewrites the comma on a split line.
    round_trip(
        "{\n  \"editor\": \"hx\",\n  {{- if .work }}\n  \"proxy\": \"corp\",\n  {{- end }}\n  \"tabs\": 2\n}\n",
        "{\n  \"editor\": \"hx\",\n  \"proxy\": \"corp\"\n}\n",
        &["tabs"],
        0,
    );
    // Lines away from the directives stay free on both sides of them.
    round_trip(
        "{\n  \"editor\": \"hx\",\n  \"font\": 12,\n  {{- if .work }}\n  \"proxy\": \"corp\",\n  \"retries\": 3,\n  {{- end }}\n  \"tabs\": 2,\n  \"theme\": \"dark\"\n}\n",
        "{\n  \"editor\": \"vi\",\n  \"font\": 14,\n  \"proxy\": \"home\",\n  \"retries\": 5,\n  \"tabs\": 4,\n  \"theme\": \"light\"\n}\n",
        &["font", "retries"],
        4,
    );
}
