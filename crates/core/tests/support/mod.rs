#![allow(dead_code)] // shared by several test binaries, each using a subset
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tomte_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use tomte_core::cmd::{CommandRequest, CommandRunner, SystemRunner};
use tomte_core::git::GitClient;
use tomte_core::scanner::DriftScanner;

pub struct Scratch {
    pub root: tempfile::TempDir,
    pub home: PathBuf,
    pub source: PathBuf,
    pub bare: PathBuf,
}

pub fn sh(cwd: &Path, program: &str, args: &[&str]) {
    let out = SystemRunner
        .run(
            CommandRequest::new(program)
                .args(args.iter().copied())
                .cwd(cwd),
        )
        .unwrap();
    assert!(out.success(), "{program} {args:?}: {}", out.stderr_utf8());
}

pub fn git(cwd: &Path, args: &[&str]) {
    let mut full = vec![
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "-c",
        "commit.gpgsign=false",
    ];
    full.extend_from_slice(args);
    sh(cwd, "git", &full);
}

impl Scratch {
    /// chezmoi home with one managed file `~/.testrc` (source `dot_testrc` = "a=1\n"),
    /// applied, committed, and pushed to a local bare `origin`.
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let source = root.path().join("source");
        let bare = root.path().join("origin.git");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        sh(
            root.path(),
            "git",
            &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        git(&source, &["init", "-b", "main"]);
        std::fs::write(source.join("dot_testrc"), "a=1\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "init"]);
        git(
            &source,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        );
        git(&source, &["push", "-u", "origin", "main"]);
        let s = Self {
            root,
            home,
            source,
            bare,
        };
        s.chezmoi()
            .apply(None)
            .expect("initial apply (is chezmoi installed?)");
        s
    }

    pub fn chezmoi(&self) -> ChezmoiClient {
        let opts = ChezmoiOptions {
            base_args: vec![
                "--source".into(),
                self.source.to_string_lossy().into_owned(),
                "--destination".into(),
                self.home.to_string_lossy().into_owned(),
                "--config".into(),
                self.config_path().to_string_lossy().into_owned(),
                "--no-tty".into(),
                "--no-pager".into(),
            ],
            ..ChezmoiOptions::default()
        };
        ChezmoiClient::new(Arc::new(SystemRunner), opts)
    }

    fn config_path(&self) -> PathBuf {
        let p = self.root.path().join("chezmoi.toml");
        if !p.exists() {
            std::fs::write(&p, "").unwrap();
        }
        p
    }

    pub fn scanner(&self) -> DriftScanner {
        DriftScanner::new(
            self.chezmoi(),
            GitClient::new(Arc::new(SystemRunner), self.source.clone()),
            "origin/main".to_string(),
        )
    }
}
