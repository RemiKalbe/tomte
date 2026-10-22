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

/// Asserts which decisions lock, picks Disk on the rest, writes back, re-renders.
fn round_trip(template: &str, ours: &str, locked: &[&str], free: usize) {
    let (_g, chezmoi) = scratch_chezmoi();
    let rendered =
        String::from_utf8(chezmoi.execute_template(template.as_bytes()).unwrap()).unwrap();
    let map = anchor(template, &lex(template).unwrap(), &rendered);
    let doc = StructuredDocument::compute(None, ours, &rendered, Some(&map)).unwrap();
    let mut res = Resolution::default();
    let mut unlocked = 0;
    for d in &doc.decisions {
        if locked.contains(&d.path.as_str()) {
            assert!(d.locked.is_some(), "{} is locked", d.path);
        } else {
            assert!(d.locked.is_none(), "{} is free", d.path);
            unlocked += 1;
            res.set(d.id, Pick::Disk);
        }
    }
    assert_eq!(unlocked, free);
    let result = doc.assemble(&res).unwrap();
    let new_template = write_back(template, &map, &rendered, &result).unwrap();
    verify_write_back(&chezmoi, &new_template, &result).unwrap();
}

// Mutation: make touched_lines return only the value's range (drop the
// previous entry's comma line for a removal). The second case then leaves
// "tabs" unlocked, and write_back fails with ProtectedSpanTouched because the
// comma it must drop sits on the templated "name" line. The first case is the
// brief's: value edits plus one locked templated entry.
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
