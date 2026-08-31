use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::cmd::{CommandError, CommandOutput, CommandRequest, CommandRunner};

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

/// Outcome of starting `git merge <upstream>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeStart {
    /// Fast-forward or auto-merged and committed.
    Clean,
    /// Mid-merge with these paths conflicted; resolve or abort.
    Conflicts(Vec<PathBuf>),
}

/// Outcome of the virtual merge probe [`GitClient::merge_tree_conflicts`].
/// The working tree, the index, and HEAD are never touched producing this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeTreeOutcome {
    /// The merge would be clean (git exited 0).
    Clean,
    /// The merge would conflict (git exited 1). `paths` lists every
    /// individual file git could name — but per `git-merge-tree(1)`'s
    /// MISTAKES TO AVOID, an EMPTY list here does NOT mean clean: some
    /// conflicts (directory rename conflicts, and others git may add in
    /// the future) don't reduce to a single file. The exit status alone
    /// is what makes this `Conflicts`, never the length of `paths` — treat
    /// `Conflicts(vec![])` as "conflicted, no individually-listed file".
    Conflicts(Vec<PathBuf>),
    /// merge-tree could not produce an answer at all: git predates
    /// `--write-tree` (added 2.38), or it refused outright (e.g. unrelated
    /// histories — both surface as the same non-0/1 exit status git itself
    /// does not further distinguish). The caller must fall back to not
    /// auto-joining rather than guessing which case it was.
    Unknown,
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

    /// Like [`Self::run`] but does not turn a non-zero exit into
    /// `GitError::Exit` — for probes where a specific non-zero code IS the
    /// answer (e.g. [`Self::merge_tree_conflicts`]'s "conflicts found").
    /// `failure_is_expected` makes the command chokepoint log a non-zero
    /// exit at info, not error — an expected outcome must never read like a
    /// fault in the logs.
    fn run_raw(&self, args: &[&str], timeout: Duration) -> Result<CommandOutput, GitError> {
        Ok(self.runner.run(
            CommandRequest::new("git")
                .args(args.iter().copied())
                .cwd(&self.repo)
                .timeout(timeout)
                .failure_is_expected(),
        )?)
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

    /// Fast-forward the current branch to `upstream`; refuses to create a
    /// merge commit (`--ff-only`), so it is lossless by construction.
    pub fn ff_merge(&self, upstream: &str) -> Result<(), GitError> {
        self.run(&["merge", "--ff-only", upstream], Duration::from_secs(30))?;
        Ok(())
    }

    /// Replay local commits on top of `upstream`. On failure the caller
    /// MUST call [`Self::rebase_abort`] to restore the branch.
    pub fn rebase(&self, upstream: &str) -> Result<(), GitError> {
        self.run(&["rebase", upstream], Duration::from_secs(60))?;
        Ok(())
    }

    /// Best-effort abort of an in-progress rebase.
    pub fn rebase_abort(&self) {
        let _ = self.run(&["rebase", "--abort"], Duration::from_secs(30));
    }

    /// Merge `upstream` into the current branch. Clean merges commit
    /// immediately (default message); a conflicted merge leaves the index
    /// mid-merge and reports the conflicted paths — the caller resolves
    /// them (or [`Self::merge_abort`]s).
    pub fn merge(&self, upstream: &str) -> Result<MergeStart, GitError> {
        match self.run(&["merge", "--no-edit", upstream], Duration::from_secs(60)) {
            Ok(_) => Ok(MergeStart::Clean),
            Err(e) => {
                let conflicts = self.conflicted_files()?;
                if conflicts.is_empty() {
                    // Not a content conflict (dirty tree, unrelated
                    // histories…): surface the original failure.
                    return Err(e);
                }
                Ok(MergeStart::Conflicts(conflicts))
            }
        }
    }

    /// Paths with unresolved merge conflicts (index stage entries).
    pub fn conflicted_files(&self) -> Result<Vec<PathBuf>, GitError> {
        Ok(self
            .run_utf8(&["diff", "--name-only", "--diff-filter=U"])?
            .lines()
            .map(PathBuf::from)
            .collect())
    }

    /// One side of a conflicted path: stage 1 = common ancestor, 2 = ours
    /// (local branch), 3 = theirs (the merged-in upstream). `None` when the
    /// side has no version (added on one side only).
    pub fn stage_blob(&self, stage: u8, rel_path: &Path) -> Result<Option<Vec<u8>>, GitError> {
        let spec = format!(":{stage}:{}", rel_path.display());
        match self.run(&["show", &spec], Duration::from_secs(30)) {
            Ok(out) => Ok(Some(out)),
            Err(_) => Ok(None),
        }
    }

    /// Mark a conflicted path resolved (after the caller wrote the file).
    pub fn add_path(&self, rel_path: &Path) -> Result<(), GitError> {
        let path = rel_path.display().to_string();
        self.run(&["add", "--", &path], Duration::from_secs(30))?;
        Ok(())
    }

    /// Conclude an in-progress (fully resolved) merge with the default
    /// merge-commit message.
    pub fn merge_commit(&self) -> Result<String, GitError> {
        self.run(&["commit", "--no-edit"], Duration::from_secs(30))?;
        self.rev_parse("HEAD")
    }

    /// A merge is mid-flight (MERGE_HEAD exists) — e.g. the app died
    /// between resolving conflicts and concluding.
    pub fn merge_in_progress(&self) -> bool {
        self.run(
            &["rev-parse", "-q", "--verify", "MERGE_HEAD"],
            Duration::from_secs(10),
        )
        .is_ok()
    }

    /// Best-effort abort of an in-progress merge.
    pub fn merge_abort(&self) {
        let _ = self.run(&["merge", "--abort"], Duration::from_secs(30));
    }

    /// Probe whether a merge with `upstream` would conflict, computed
    /// entirely in the object database via `git merge-tree --write-tree` —
    /// the working tree, the index, and HEAD are never touched. See
    /// [`MergeTreeOutcome`] for how to read the result; in particular, the
    /// exit status decides `Clean` vs `Conflicts`, NOT whether any path was
    /// parsed out of stdout.
    pub fn merge_tree_conflicts(&self, upstream: &str) -> Result<MergeTreeOutcome, GitError> {
        let out = self.run_raw(
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                "HEAD",
                upstream,
            ],
            Duration::from_secs(30),
        )?;
        match out.exit_code {
            0 => Ok(MergeTreeOutcome::Clean),
            1 if !out.stdout.is_empty() => {
                // Documented layout (verified against git 2.50.1):
                //   <tree-oid>\n<conflicted path>\n...\n\n<free-form messages>
                // The blank line is the real separator between the path list
                // and human-readable prose ("Auto-merging x", "CONFLICT
                // ...") — that prose must NOT be parsed as more paths. The
                // path list itself may legitimately be empty (see
                // `MergeTreeOutcome::Conflicts` doc) — exit 1 alone is what
                // makes this a conflict, so it is never treated as clean
                // regardless of how many paths came out of parsing.
                let text = out.stdout_utf8();
                let paths = text
                    .lines()
                    .skip(1)
                    .take_while(|l| !l.is_empty())
                    .map(PathBuf::from)
                    .collect();
                Ok(MergeTreeOutcome::Conflicts(paths))
            }
            1 => {
                // Exit 1 normally means "conflicts, see stdout" — but git
                // also uses it for outright failures (e.g. an unresolvable
                // `upstream`) where stdout is empty and the reason is on
                // stderr. That is not an answer; treating it as a clean
                // merge would be silently wrong, so surface it.
                Err(GitError::Exit {
                    code: out.exit_code,
                    stderr: out.stderr_utf8(),
                })
            }
            _ => Ok(MergeTreeOutcome::Unknown),
        }
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
    use crate::cmd::fake::FakeRunner;
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
    fn merge_tree_conflicts_surfaces_an_unresolvable_upstream_instead_of_reporting_clean() {
        // git 2.50.1 exits 1 for BOTH "conflicts found" (stdout carries the
        // tree oid + paths) and "couldn't even attempt the merge" (e.g. a
        // ref that doesn't exist — empty stdout, reason on stderr). A naive
        // "exit 1 => Some(vec![...])" reading of stdout would silently
        // report the latter as a clean merge; it must surface as an error.
        let (_g, work) = scratch();
        let err = client(&work)
            .merge_tree_conflicts("no-such-branch")
            .unwrap_err();
        assert!(matches!(err, GitError::Exit { .. }), "{err:?}");
    }

    #[test]
    fn merge_tree_exit_status_is_authoritative_over_an_empty_parsed_path_list() {
        // git-merge-tree(1), MISTAKES TO AVOID (verbatim): "Do NOT interpret
        // an empty Conflicted file info list as a clean merge; check the
        // exit status. A merge can have conflicts without having individual
        // files conflict (directory rename conflicts fall into this
        // category, and others may be added in the future)." Real git
        // 2.50.1 would not reproduce this from three tried directory-rename
        // shapes, so exercise the decision logic directly: fake exit 1 with
        // stdout that carries a tree oid and the messages section but NO
        // conflicted-path line at all.
        let fake = FakeRunner::new();
        fake.push_ok(
            1,
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n\nAuto-merging x\nCONFLICT (rename/rename): x renamed on both sides\n",
            "",
        );
        let git = GitClient::new(Arc::new(fake), PathBuf::from("/does-not-matter"));
        assert_eq!(
            git.merge_tree_conflicts("upstream").unwrap(),
            MergeTreeOutcome::Conflicts(vec![]),
            "exit 1 must report Conflicts, never Clean, regardless of the parsed path list"
        );
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
