//! Round-trip invariant (spec §11): render → anchor → write-back → re-render.
//! Uses the real `chezmoi execute-template` with a hermetic scratch config.

use std::path::PathBuf;
use std::sync::Arc;

use czui_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use czui_core::cmd::SystemRunner;
use czui_core::template::anchor::anchor;
use czui_core::template::lexer::lex;
use czui_core::template::verify::verify_write_back;
use czui_core::template::writeback::{WriteBackError, write_back};

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
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/templates")
        .join(name);
    std::fs::read_to_string(p).unwrap()
}

const CORPUS: &[&str] = &["gitconfig.tmpl", "env-nu.tmpl", "aliases.tmpl"];

#[test]
fn identity_roundtrip_leaves_template_unchanged() {
    let (_g, chezmoi) = scratch_chezmoi();
    for name in CORPUS {
        let tmpl = fixture(name);
        let rendered =
            String::from_utf8(chezmoi.execute_template(tmpl.as_bytes()).unwrap()).unwrap();
        let map = anchor(&tmpl, &lex(&tmpl).unwrap(), &rendered);
        let out = write_back(&tmpl, &map, &rendered, &rendered)
            .unwrap_or_else(|e| panic!("{name}: identity write-back failed: {e}"));
        assert_eq!(
            out, tmpl,
            "{name}: identity write-back must not change the template"
        );
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
    assert!(
        new_tmpl.contains("{{ .email }}"),
        "template expressions must survive"
    );
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
