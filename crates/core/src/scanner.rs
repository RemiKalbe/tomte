//! Point-in-time drift scan composing ChezmoiClient + GitClient + classify().
//!
//! Candidate discovery is layered so template failures cannot hide drift:
//!   a) destination-vs-last-written via state dump hashes (no rendering),
//!   b) source-side via git (remote diff + dirty worktree) mapped through
//!      `chezmoi target-path`,
//!   c) `chezmoi status` when it works; if it fails with an EvalFailure the
//!      scan continues from (a)+(b) and reports `degraded`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::chezmoi::{ChezmoiClient, ChezmoiError, EvalFailure};
use crate::drift::{ContentHash, DriftClass, GitSignals, StateProbe, classify};
use crate::git::{GitClient, GitError};

#[derive(Clone)]
pub struct DriftScanner {
    chezmoi: ChezmoiClient,
    git: GitClient,
    remote_ref: String,
}

#[derive(Debug)]
pub struct FileDrift {
    pub target: PathBuf,
    pub source_rel: Option<PathBuf>,
    pub class: DriftClass,
    pub probe: StateProbe,
}

#[derive(Debug)]
pub struct ScanReport {
    pub drifted: Vec<FileDrift>,
    pub in_sync_count: usize,
    pub degraded: Option<EvalFailure>,
    /// Targets the scan could not check, and why. A probe that fails used to
    /// abort the WHOLE scan through `?`, which froze the daemon's status at
    /// its initial "0 drifted, 0 in sync" indefinitely (2026-08-27). A single
    /// bad target is now skipped and recorded — never silently: these are
    /// logged by the daemon and excluded from `in_sync_count`, so the count
    /// never claims a file is fine when it was never looked at.
    pub unprobed: Vec<UnprobedTarget>,
    /// Managed TARGET paths (not source paths) whose origin source differs
    /// from local's, regardless of which side moved: decision 6.4 is
    /// "surface origin whenever its content differs, without distinguishing
    /// why". A file only this machine changed still has an origin version
    /// that differs from ours, and belongs here too.
    pub origin_differs: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct UnprobedTarget {
    pub target: PathBuf,
    pub reason: String,
}

/// Where `probe_file` reads a target's ahead/behind from.
enum GitSignalSource<'a> {
    /// Two `rev-list --count` for THIS file: the per-event probe of one
    /// target, where two subprocesses are cheap and a whole-history walk
    /// per file event would not be.
    PerFile,
    /// Computed once per scan (spec 2026-10-11 §2.3): the paths touched by
    /// `remote..HEAD` and by `HEAD..remote`.
    Sets {
        local: &'a BTreeSet<PathBuf>,
        remote: &'a BTreeSet<PathBuf>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error(transparent)]
    Chezmoi(#[from] ChezmoiError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("i/o error during scan: {0}")]
    Io(#[from] std::io::Error),
}

fn hash_file(path: &Path) -> Result<Option<ContentHash>, std::io::Error> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(ContentHash::of(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

impl DriftScanner {
    pub fn new(chezmoi: ChezmoiClient, git: GitClient, remote_ref: String) -> Self {
        Self {
            chezmoi,
            git,
            remote_ref,
        }
    }

    pub fn scan(&self) -> Result<ScanReport, ScanError> {
        let source_dir = self.chezmoi.source_dir()?;
        let dump = self.chezmoi.state_dump()?;
        let managed: BTreeSet<PathBuf> = self.chezmoi.managed()?.into_iter().collect();

        let mut candidates: BTreeSet<PathBuf> = BTreeSet::new();
        let mut managed_file_count = 0usize;
        // The targets behind `managed_file_count`, so an unprobed one can be
        // taken back out of the in-sync tally.
        let mut counted: BTreeSet<PathBuf> = BTreeSet::new();
        let mut unprobed: Vec<UnprobedTarget> = Vec::new();

        // (a) destination side, render-free
        for (target, entry) in &dump.entry_state {
            if entry.kind != "file" || !managed.contains(target) {
                continue;
            }
            managed_file_count += 1;
            counted.insert(target.clone());
            let expected = entry
                .contents_sha256
                .as_deref()
                .and_then(ContentHash::from_hex);
            match hash_file(target) {
                Ok(actual) => {
                    if actual != expected {
                        candidates.insert(target.clone());
                    }
                }
                Err(e) => unprobed.push(UnprobedTarget {
                    target: target.clone(),
                    reason: e.to_string(),
                }),
            }
        }

        // (b) source side via git
        //
        // NOTE: changed_files runs `git diff --name-only HEAD..<remote>`, which lists
        // every file whose CONTENT differs between the two — regardless of which side
        // moved. That is deliberately what we want: decision 6.4 is "surface origin
        // whenever its content differs, without distinguishing why", so a file only
        // this machine changed still has an origin version that differs from ours.
        // Do NOT narrow this to an ahead/behind split.
        let differs_from_origin: BTreeSet<PathBuf> = self
            .git
            .changed_files("HEAD", &self.remote_ref)?
            .into_iter()
            .collect();
        let mut source_changed: BTreeSet<PathBuf> = differs_from_origin.clone();
        source_changed.extend(self.git.dirty_files()?);

        // Which managed TARGETS origin has its own version of. Recorded as the
        // existing mapping runs, so the scan costs exactly what it did before.
        let mut origin_differs: BTreeSet<PathBuf> = BTreeSet::new();

        // Map one source at a time: non-entry files (.chezmoiignore, README, …)
        // make `chezmoi target-path` fail, and must not kill the scan.
        for rel in &source_changed {
            let abs = source_dir.join(rel);
            let Ok(targets) = self.chezmoi.target_paths(std::slice::from_ref(&abs)) else {
                continue;
            };
            for target in targets {
                if managed.contains(&target) {
                    if differs_from_origin.contains(rel) {
                        origin_differs.insert(target.clone());
                    }
                    candidates.insert(target);
                }
            }
        }

        // (c) rendered side via status, degrading on eval failure
        let mut degraded = None;
        match self.chezmoi.status() {
            Ok(entries) => {
                for e in entries {
                    if managed.contains(&e.path) {
                        candidates.insert(e.path);
                    }
                }
            }
            Err(ChezmoiError::Eval(f)) => degraded = Some(f),
            Err(other) => return Err(other.into()),
        }

        // The probe loop's two git questions and its source-path lookup are
        // answered once for the whole scan (spec 2026-10-11 §2.3): three
        // subprocesses instead of three per candidate. A set that cannot be
        // computed degrades to the per-file questions, logged — never to a
        // guessed "not ahead".
        let local_range = format!("{}..HEAD", self.remote_ref);
        let remote_range = format!("HEAD..{}", self.remote_ref);
        let sets = match (
            self.git.has_merges(&local_range),
            self.git.has_merges(&remote_range),
        ) {
            (Ok(false), Ok(false)) => {
                match (
                    self.git.files_touched_in(&local_range),
                    self.git.files_touched_in(&remote_range),
                ) {
                    (Ok(local), Ok(remote)) => Some((local, remote)),
                    (l, r) => {
                        let why = l
                            .err()
                            .or(r.err())
                            .map(|e| e.to_string())
                            .unwrap_or_default();
                        crate::log_warn!(
                            "scan",
                            "git ahead/behind sets unavailable ({why}); asking per file"
                        );
                        None
                    }
                }
            }
            (Ok(l), Ok(r)) => {
                let range = if l { &local_range } else { &remote_range };
                let _ = r;
                crate::log_info!(
                    "scan",
                    "{range} contains merge commits; asking git per file"
                );
                None
            }
            (l, r) => {
                let why = l
                    .err()
                    .or(r.err())
                    .map(|e| e.to_string())
                    .unwrap_or_default();
                crate::log_warn!(
                    "scan",
                    "git ahead/behind sets unavailable ({why}); asking per file"
                );
                None
            }
        };
        let probe_targets: Vec<PathBuf> = candidates
            .iter()
            // Already known unreadable from the pass above — don't spend a
            // subprocess re-confirming it, and don't record it twice.
            .filter(|t| !unprobed.iter().any(|u| u.target == **t))
            // A recorded entry that isn't a file: symlink/dir probing is
            // out of scope, so skip without ceremony.
            .filter(|t| !matches!(dump.entry_state.get(*t), Some(entry) if entry.kind != "file"))
            .cloned()
            .collect();
        let source_abs = self.chezmoi.source_paths(&probe_targets)?;
        let mut drifted = Vec::new();
        for (target, abs) in probe_targets.into_iter().zip(source_abs) {
            // A target with no state entry was never written by chezmoi
            // (fresh from remote, or an ignored entry type): probe with
            // last_written = None. Nothing says it IS a file: a managed
            // directory that was never applied lands here, and `chezmoi cat`
            // on it fails with NotAFile, handled below — it used to travel
            // out through `?` and kill the entire scan.
            let last_written = dump
                .entry_state
                .get(&target)
                .and_then(|entry| entry.contents_sha256.as_deref())
                .and_then(ContentHash::from_hex);
            let source_rel =
                abs.and_then(|a| a.strip_prefix(&source_dir).ok().map(Path::to_path_buf));
            let signals = match &sets {
                Some((local, remote)) => GitSignalSource::Sets { local, remote },
                None => GitSignalSource::PerFile,
            };
            match self.probe_file(&target, last_written, source_rel, &signals) {
                Ok(Some(d)) => drifted.push(d),
                Ok(None) => {}
                // Chezmoi's own verdict that the target is not a regular
                // file. Expected, and out of scope — not a failure.
                Err(ScanError::Chezmoi(ChezmoiError::NotAFile { .. })) => {}
                Err(e) => unprobed.push(UnprobedTarget {
                    target: target.clone(),
                    reason: e.to_string(),
                }),
            }
        }

        // "In sync" = managed files chezmoi has written that were CHECKED and
        // found clean. Only subtract targets that were actually counted:
        // a drifted candidate can come from git or status without ever being
        // in the managed-file tally (a source file not yet applied), and
        // subtracting those under-reported the count. Unchecked targets are
        // unknown, not fine, so they come out too.
        let counted_out = drifted
            .iter()
            .map(|d| &d.target)
            .chain(unprobed.iter().map(|u| &u.target))
            .filter(|t| counted.contains(*t))
            .collect::<BTreeSet<_>>()
            .len();
        let in_sync_count = managed_file_count.saturating_sub(counted_out);
        Ok(ScanReport {
            drifted,
            in_sync_count,
            degraded,
            unprobed,
            origin_differs,
        })
    }

    /// Probe a single target on demand (daemon per-event path). `None` = in sync.
    pub fn probe_one(&self, target: &Path) -> Result<Option<FileDrift>, ScanError> {
        let source_dir = self.chezmoi.source_dir()?;
        let dump = self.chezmoi.state_dump()?;
        let last = dump
            .entry_state
            .get(target)
            .and_then(|e| e.contents_sha256.as_deref())
            .and_then(ContentHash::from_hex);
        let source_rel = self
            .chezmoi
            .source_path(target)
            .ok()
            .and_then(|abs| abs.strip_prefix(&source_dir).ok().map(Path::to_path_buf));
        self.probe_file(target, last, source_rel, &GitSignalSource::PerFile)
    }

    fn probe_file(
        &self,
        target: &Path,
        last_written: Option<ContentHash>,
        source_rel: Option<PathBuf>,
        signals: &GitSignalSource<'_>,
    ) -> Result<Option<FileDrift>, ScanError> {
        let destination = hash_file(target)?;
        let rendered = match self.chezmoi.cat_probe(target) {
            Ok(bytes) => Ok(Some(ContentHash::of(&bytes))),
            Err(ChezmoiError::Eval(f)) => Err(f),
            Err(other) => return Err(other.into()),
        };
        let git = match (&source_rel, signals) {
            (Some(rel), GitSignalSource::PerFile) => GitSignals {
                local_ahead: self
                    .git
                    .commits_touching(&format!("{}..HEAD", self.remote_ref), rel)?
                    > 0,
                remote_ahead: self
                    .git
                    .commits_touching(&format!("HEAD..{}", self.remote_ref), rel)?
                    > 0,
            },
            (Some(rel), GitSignalSource::Sets { local, remote }) => GitSignals {
                local_ahead: local.contains(rel),
                remote_ahead: remote.contains(rel),
            },
            (None, _) => GitSignals::default(),
        };
        let probe = StateProbe {
            destination,
            rendered,
            last_written,
            git,
        };
        let class = classify(&probe);
        if class == DriftClass::InSync {
            return Ok(None);
        }
        Ok(Some(FileDrift {
            target: target.to_path_buf(),
            source_rel,
            class,
            probe,
        }))
    }
}
