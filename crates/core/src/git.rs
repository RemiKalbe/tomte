use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::cmd::{CommandError, CommandRequest, CommandRunner};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divergence {
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error("git exited with code {code}: {stderr}")]
    Exit { code: i32, stderr: String },
    #[error("failed to parse git output ({context}): {detail}")]
    Parse {
        context: &'static str,
        detail: String,
    },
}

#[derive(Clone)]
pub struct GitClient {
    runner: Arc<dyn CommandRunner>,
    repo: PathBuf,
}

impl GitClient {
    pub fn new(runner: Arc<dyn CommandRunner>, repo: PathBuf) -> Self {
        Self { runner, repo }
    }

    fn run(&self, args: &[&str], timeout: Duration) -> Result<Vec<u8>, GitError> {
        let out = self.runner.run(
            CommandRequest::new("git")
                .args(args.iter().copied())
                .cwd(&self.repo)
                .timeout(timeout),
        )?;
        if out.success() {
            Ok(out.stdout)
        } else {
            Err(GitError::Exit {
                code: out.exit_code,
                stderr: out.stderr_utf8(),
            })
        }
    }

    fn run_utf8(&self, args: &[&str]) -> Result<String, GitError> {
        Ok(String::from_utf8_lossy(&self.run(args, Duration::from_secs(30))?).into_owned())
    }

    pub fn fetch(&self, remote: &str) -> Result<(), GitError> {
        // A locked 1Password SSH agent leaves ssh sitting on a GUI prompt;
        // without these, a post-sleep fetch hangs the full timeout while
        // HOLDING THE DAEMON'S CORE LOCK, which the app then misreads as a
        // stuck daemon (2026-08-02 sleep/wake incident). BatchMode + no
        // terminal prompt turn a locked agent into a fast, honest failure
        // surfaced as a degraded hint.
        let out = self.runner.run(
            CommandRequest::new("git")
                .args(["fetch", "--quiet", remote])
                .cwd(&self.repo)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env(
                    "GIT_SSH_COMMAND",
                    "ssh -o BatchMode=yes -o ConnectTimeout=10",
                )
                .timeout(Duration::from_secs(30)),
        )?;
        if out.success() {
            Ok(())
        } else {
            Err(GitError::Exit {
                code: out.exit_code,
                stderr: out.stderr_utf8(),
            })
        }
    }

    pub fn rev_parse(&self, rev: &str) -> Result<String, GitError> {
        Ok(self.run_utf8(&["rev-parse", rev])?.trim().to_string())
    }

    pub fn head_branch(&self) -> Result<String, GitError> {
        Ok(self
            .run_utf8(&["rev-parse", "--abbrev-ref", "HEAD"])?
            .trim()
            .to_string())
    }

    pub fn divergence(&self, upstream: &str) -> Result<Divergence, GitError> {
        let range = format!("{upstream}...HEAD");
        let text = self.run_utf8(&["rev-list", "--left-right", "--count", &range])?;
        // output: "<behind>\t<ahead>" (left = upstream-only, right = HEAD-only)
        let parts: Vec<&str> = text.split_whitespace().collect();
        let [behind, ahead] = parts.as_slice() else {
            return Err(GitError::Parse {
                context: "divergence",
                detail: text,
            });
        };
        let parse = |s: &str| {
            s.parse::<u32>().map_err(|e| GitError::Parse {
                context: "divergence",
                detail: e.to_string(),
            })
        };
        Ok(Divergence {
            behind: parse(behind)?,
            ahead: parse(ahead)?,
        })
    }

    pub fn changed_files(&self, from: &str, to: &str) -> Result<Vec<PathBuf>, GitError> {
        let range = format!("{from}..{to}");
        Ok(self
            .run_utf8(&["diff", "--name-only", &range])?
            .lines()
            .map(PathBuf::from)
            .collect())
    }

    pub fn blob_at(&self, rev: &str, rel_path: &Path) -> Result<Option<Vec<u8>>, GitError> {
        let spec = format!("{rev}:{}", rel_path.to_string_lossy());
        match self.run(&["cat-file", "blob", &spec], Duration::from_secs(30)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(GitError::Exit { .. }) => Ok(None), // path absent at rev
            Err(e) => Err(e),
        }
    }

    pub fn commits_touching(&self, range: &str, rel_path: &Path) -> Result<u32, GitError> {
        let text = self.run_utf8(&[
            "rev-list",
            "--count",
            range,
            "--",
            &rel_path.to_string_lossy(),
        ])?;
        text.trim()
            .parse()
            .map_err(|e: std::num::ParseIntError| GitError::Parse {
                context: "commits_touching",
                detail: e.to_string(),
            })
    }

    pub fn add_all(&self) -> Result<(), GitError> {
        self.run(&["add", "-A"], Duration::from_secs(30))?;
        Ok(())
    }

    /// Commit staged changes and return the new HEAD sha. A signing failure
    /// (e.g. locked 1Password) surfaces as `GitError::Exit` with git's stderr.
    pub fn commit(&self, message: &str) -> Result<String, GitError> {
        self.run(&["commit", "-m", message], Duration::from_secs(30))?;
        self.rev_parse("HEAD")
    }

    pub fn push(&self, remote: &str) -> Result<(), GitError> {
        // network op: same generous timeout as fetch
        self.run(&["push", "--quiet", remote], Duration::from_secs(120))?;
        Ok(())
    }

    pub fn head_sha(&self) -> Result<String, GitError> {
        self.rev_parse("HEAD")
    }

    pub fn dirty_files(&self) -> Result<Vec<PathBuf>, GitError> {
        Ok(self
            .run_utf8(&["status", "--porcelain"])?
            .lines()
            .filter(|l| l.len() > 3)
            .map(|l| PathBuf::from(l[3..].trim_start()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CommandRequest, CommandRunner, SystemRunner};
    use std::path::Path;
    use std::sync::Arc;

    fn sh(cwd: &Path, program: &str, args: &[&str]) {
        let out = SystemRunner
            .run(
                CommandRequest::new(program)
                    .args(args.iter().copied())
                    .cwd(cwd),
            )
            .unwrap();
        assert!(
            out.success(),
            "{program} {args:?} failed: {}",
            out.stderr_utf8()
        );
    }

    fn git(cwd: &Path, args: &[&str]) {
        // -c: identity without touching global config
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

    /// repo with an `origin` bare remote and one pushed commit (file `f.txt` = "one\n")
    fn scratch() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("origin.git");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        sh(
            dir.path(),
            "git",
            &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        git(&work, &["init", "-b", "main"]);
        // Local config: GitClient::commit runs WITHOUT the -c overrides above,
        // so the repo itself must carry identity and disable signing (the
        // machine's global config may have 1Password SSH signing enabled).
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["config", "commit.gpgsign", "false"]);
        std::fs::write(work.join("f.txt"), "one\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "c1"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&work, &["push", "-u", "origin", "main"]);
        (dir, work)
    }

    fn client(repo: &Path) -> GitClient {
        GitClient::new(Arc::new(SystemRunner), repo.to_path_buf())
    }

    #[test]
    fn head_branch_and_clean_divergence() {
        let (_g, work) = scratch();
        let c = client(&work);
        assert_eq!(c.head_branch().unwrap(), "main");
        let d = c.divergence("origin/main").unwrap();
        assert_eq!((d.ahead, d.behind), (0, 0));
    }

    #[test]
    fn detects_remote_ahead_after_fetch() {
        let (guard, work) = scratch();
        // second clone pushes a change
        let other = guard.path().join("other");
        sh(
            guard.path(),
            "git",
            &[
                "clone",
                work.join("../origin.git").to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        std::fs::write(other.join("f.txt"), "two\n").unwrap();
        git(&other, &["add", "."]);
        git(&other, &["commit", "-m", "c2"]);
        git(&other, &["push"]);

        let c = client(&work);
        c.fetch("origin").unwrap();
        let d = c.divergence("origin/main").unwrap();
        assert_eq!((d.ahead, d.behind), (0, 1));
        assert_eq!(
            c.changed_files("HEAD", "origin/main").unwrap(),
            vec![std::path::PathBuf::from("f.txt")]
        );
        assert_eq!(
            c.commits_touching("HEAD..origin/main", Path::new("f.txt"))
                .unwrap(),
            1
        );
        assert_eq!(
            c.blob_at("origin/main", Path::new("f.txt"))
                .unwrap()
                .unwrap(),
            b"two\n"
        );
        assert_eq!(
            c.blob_at("origin/main", Path::new("missing.txt")).unwrap(),
            None
        );
    }

    #[test]
    fn rev_parse_returns_sha() {
        let (_g, work) = scratch();
        let sha = client(&work).rev_parse("HEAD").unwrap();
        assert_eq!(sha.len(), 40);
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn add_all_commit_push_roundtrip_lands_on_origin() {
        let (guard, work) = scratch();
        let c = client(&work);
        let first = c.head_sha().unwrap();

        std::fs::write(work.join("f.txt"), "two\n").unwrap();
        std::fs::write(work.join("new.txt"), "fresh\n").unwrap();
        c.add_all().unwrap();
        let second = c.commit("tomte: keep_disk f.txt").unwrap();

        assert_ne!(second, first, "commit must move HEAD");
        assert_eq!(second.len(), 40);
        assert_eq!(second, c.head_sha().unwrap());
        // the new commit sits on top of the old head → a second commit exists
        assert_eq!(c.rev_parse("HEAD~1").unwrap(), first);

        c.push("origin").unwrap();
        let origin = GitClient::new(Arc::new(SystemRunner), guard.path().join("origin.git"));
        assert_eq!(
            origin.rev_parse("HEAD").unwrap(),
            second,
            "push must land the new commit on the bare origin"
        );
    }

    #[test]
    fn commit_failure_surfaces_exit_with_stderr() {
        // Nothing staged → git commit exits non-zero. Signing failures (locked
        // 1Password) arrive through the exact same GitError::Exit surface.
        let (_g, work) = scratch();
        let err = client(&work).commit("nothing to do").unwrap_err();
        match err {
            GitError::Exit { code, .. } => assert_ne!(code, 0),
            other => panic!("expected GitError::Exit, got {other:?}"),
        }
    }

    #[test]
    fn dirty_files_lists_worktree_changes() {
        let (_g, work) = scratch();
        std::fs::write(work.join("f.txt"), "dirty\n").unwrap();
        assert_eq!(
            client(&work).dirty_files().unwrap(),
            vec![std::path::PathBuf::from("f.txt")]
        );
    }
}
