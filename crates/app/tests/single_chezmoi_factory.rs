//! Structural gate: every app-side chezmoi client is built by
//! `app_chezmoi_client()` in main.rs — the ONLY place allowed to construct
//! `ChezmoiOptions`. Three separate call sites forgot OP_ACCOUNT before
//! this existed (0.1.4 fixed one, 2026-08-17 found three more).

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn chezmoi_clients_only_come_from_the_factory() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let mut offenders = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let allowed = file.ends_with("src/main.rs");
        if !allowed && (text.contains("ChezmoiOptions") || text.contains("ChezmoiClient::new")) {
            // Test modules may build fake-runner clients; only flag
            // non-test occurrences (before any `mod tests`).
            let live = text.split("mod tests").next().unwrap_or("");
            if live.contains("ChezmoiOptions") || live.contains("ChezmoiClient::new") {
                offenders.push(file.display().to_string());
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "chezmoi clients must come from app_chezmoi_client() (main.rs), \
         or OP_ACCOUNT is silently lost. Offenders: {offenders:?}"
    );
}
