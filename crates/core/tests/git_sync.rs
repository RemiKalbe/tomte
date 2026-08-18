//! Integration primitives against real repos: fetch → fast-forward, and the
//! rejected-push → rebase → push recovery (the 2026-08-10 second-machine
//! failure: fetch observed origin moving but nothing ever integrated it).

use std::path::Path;
use std::sync::Arc;

use support::{Scratch, sh};
use tomte_core::cmd::SystemRunner;
use tomte_core::git::GitClient;

mod support;

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

/// Repo-local identity + no signing: the production GitClient inherits the
/// developer's global config (1Password SSH signing hangs headless tests).
fn unsigned(repo: &Path) {
    sh(repo, "git", &["config", "user.email", "t@t"]);
    sh(repo, "git", &["config", "user.name", "t"]);
    sh(repo, "git", &["config", "commit.gpgsign", "false"]);
}

/// A second clone of the scratch origin, for making origin move.
fn other_clone(scratch: &Scratch) -> std::path::PathBuf {
    let dir = scratch.root.path().join("other");
    sh(
        scratch.root.path(),
        "git",
        &[
            "clone",
            scratch.bare.to_str().unwrap(),
            dir.to_str().unwrap(),
        ],
    );
    sh(&dir, "git", &["config", "user.email", "t@t"]);
    sh(&dir, "git", &["config", "user.name", "t"]);
    sh(&dir, "git", &["config", "commit.gpgsign", "false"]);
    dir
}

#[test]
fn fetch_then_ff_merge_catches_up_losslessly() {
    let scratch = Scratch::new();
    let git = GitClient::new(Arc::new(SystemRunner), scratch.source.clone());
    let other = other_clone(&scratch);
    write(&other.join("newfile"), "from the other machine\n");
    sh(&other, "git", &["add", "-A"]);
    sh(&other, "git", &["commit", "-m", "remote change"]);
    sh(&other, "git", &["push", "origin", "main"]);

    git.fetch("origin").unwrap();
    let div = git.divergence("origin/main").unwrap();
    assert_eq!((div.behind, div.ahead), (1, 0));
    git.ff_merge("origin/main").unwrap();
    let div = git.divergence("origin/main").unwrap();
    assert_eq!((div.behind, div.ahead), (0, 0));
    assert!(scratch.source.join("newfile").exists());
}

#[test]
fn rejected_push_recovers_via_rebase() {
    let scratch = Scratch::new();
    let git = GitClient::new(Arc::new(SystemRunner), scratch.source.clone());
    unsigned(&scratch.source);
    // Origin moves…
    let other = other_clone(&scratch);
    write(&other.join("remote-only"), "x\n");
    sh(&other, "git", &["add", "-A"]);
    sh(&other, "git", &["commit", "-m", "remote"]);
    sh(&other, "git", &["push", "origin", "main"]);
    // …while we commit locally without knowing.
    write(&scratch.source.join("local-only"), "y\n");
    git.add_all().unwrap();
    git.commit("local").unwrap();

    // The push bounces exactly like the field failure…
    let err = git.push("origin").unwrap_err();
    assert!(err.to_string().contains("rejected"), "{err}");
    // …and the recovery sequence lands both histories.
    git.fetch("origin").unwrap();
    git.rebase("origin/main").unwrap();
    git.push("origin").unwrap();
    let div = git.divergence("origin/main").unwrap();
    assert_eq!((div.behind, div.ahead), (0, 0));
}

#[test]
fn conflicting_rebase_aborts_cleanly() {
    let scratch = Scratch::new();
    let git = GitClient::new(Arc::new(SystemRunner), scratch.source.clone());
    unsigned(&scratch.source);
    let other = other_clone(&scratch);
    write(&other.join("same-file"), "remote version\n");
    sh(&other, "git", &["add", "-A"]);
    sh(&other, "git", &["commit", "-m", "remote"]);
    sh(&other, "git", &["push", "origin", "main"]);
    write(&scratch.source.join("same-file"), "local version\n");
    git.add_all().unwrap();
    git.commit("local").unwrap();

    git.fetch("origin").unwrap();
    let sha_before = git.head_sha().unwrap();
    assert!(git.rebase("origin/main").is_err());
    git.rebase_abort();
    assert_eq!(
        git.head_sha().unwrap(),
        sha_before,
        "abort must restore HEAD"
    );
}

#[test]
fn conflicted_merge_resolves_through_stages_and_pushes() {
    // The 2026-08-18 field failure: local unpushed commits + origin commits
    // touching the same file. Recovery = merge, resolve via stages, commit,
    // push — the plumbing behind Tomte's repo-reconcile flow.
    let scratch = Scratch::new();
    let git = GitClient::new(Arc::new(SystemRunner), scratch.source.clone());
    unsigned(&scratch.source);
    let other = other_clone(&scratch);
    write(&other.join("dot_testrc"), "a=remote\n");
    sh(&other, "git", &["add", "-A"]);
    sh(&other, "git", &["commit", "-m", "remote"]);
    sh(&other, "git", &["push", "origin", "main"]);
    write(&scratch.source.join("dot_testrc"), "a=local\n");
    git.add_all().unwrap();
    git.commit("local").unwrap();

    git.fetch("origin").unwrap();
    let start = git.merge("origin/main").unwrap();
    let conflicts = match start {
        tomte_core::git::MergeStart::Conflicts(c) => c,
        other => panic!("expected conflicts, got {other:?}"),
    };
    assert_eq!(conflicts, vec![std::path::PathBuf::from("dot_testrc")]);

    // All three sides are readable mid-merge.
    let rel = Path::new("dot_testrc");
    let base = git.stage_blob(1, rel).unwrap().unwrap();
    let ours = git.stage_blob(2, rel).unwrap().unwrap();
    let theirs = git.stage_blob(3, rel).unwrap().unwrap();
    assert_eq!(String::from_utf8_lossy(&base), "a=1\n");
    assert_eq!(String::from_utf8_lossy(&ours), "a=local\n");
    assert_eq!(String::from_utf8_lossy(&theirs), "a=remote\n");

    // Resolve, conclude, push: both histories land.
    write(&scratch.source.join("dot_testrc"), "a=resolved\n");
    git.add_path(rel).unwrap();
    assert!(git.conflicted_files().unwrap().is_empty());
    git.merge_commit().unwrap();
    git.push("origin").unwrap();
    let div = git.divergence("origin/main").unwrap();
    assert_eq!((div.behind, div.ahead), (0, 0));
}

#[test]
fn merge_abort_restores_the_branch() {
    let scratch = Scratch::new();
    let git = GitClient::new(Arc::new(SystemRunner), scratch.source.clone());
    unsigned(&scratch.source);
    let other = other_clone(&scratch);
    write(&other.join("dot_testrc"), "a=remote\n");
    sh(&other, "git", &["add", "-A"]);
    sh(&other, "git", &["commit", "-m", "remote"]);
    sh(&other, "git", &["push", "origin", "main"]);
    write(&scratch.source.join("dot_testrc"), "a=local\n");
    git.add_all().unwrap();
    git.commit("local").unwrap();
    git.fetch("origin").unwrap();

    let sha = git.head_sha().unwrap();
    assert!(matches!(
        git.merge("origin/main").unwrap(),
        tomte_core::git::MergeStart::Conflicts(_)
    ));
    git.merge_abort();
    assert_eq!(git.head_sha().unwrap(), sha);
    assert!(git.conflicted_files().unwrap().is_empty());
}
