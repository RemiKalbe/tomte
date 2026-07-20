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
    /// Set when `chezmoi status` could not run (e.g. secret manager locked);
    /// the scan still covers destination- and git-side drift.
    pub degraded: Option<EvalFailure>,
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

        // (a) destination side, render-free
        for (target, entry) in &dump.entry_state {
            if entry.kind != "file" || !managed.contains(target) {
                continue;
            }
            managed_file_count += 1;
            let expected = entry
                .contents_sha256
                .as_deref()
                .and_then(ContentHash::from_hex);
            if hash_file(target)? != expected {
                candidates.insert(target.clone());
            }
        }

        // (b) source side via git
        let mut source_changed: BTreeSet<PathBuf> = BTreeSet::new();
        source_changed.extend(self.git.changed_files("HEAD", &self.remote_ref)?);
        source_changed.extend(self.git.dirty_files()?);
        // Map one source at a time: non-entry files (.chezmoiignore, README, …)
        // make `chezmoi target-path` fail, and must not kill the scan.
        for rel in &source_changed {
            let abs = source_dir.join(rel);
            let Ok(targets) = self.chezmoi.target_paths(std::slice::from_ref(&abs)) else {
                continue;
            };
            for target in targets {
                if managed.contains(&target) {
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

        let mut drifted = Vec::new();
        for target in candidates {
            let Some(entry) = dump.entry_state.get(&target) else {
                // Managed but never written by chezmoi (fresh from remote, or
                // ignored entry type) — probe with last_written = None.
                if let Some(d) = self.probe_file(&source_dir, &target, None)? {
                    drifted.push(d);
                }
                continue;
            };
            if entry.kind != "file" {
                continue; // symlink/dir probing: Plan 3
            }
            let last = entry
                .contents_sha256
                .as_deref()
                .and_then(ContentHash::from_hex);
            if let Some(d) = self.probe_file(&source_dir, &target, last)? {
                drifted.push(d);
            }
        }

        let in_sync_count = managed_file_count.saturating_sub(drifted.len());
        Ok(ScanReport {
            drifted,
            in_sync_count,
            degraded,
        })
    }

    fn probe_file(
        &self,
        source_dir: &Path,
        target: &Path,
        last_written: Option<ContentHash>,
    ) -> Result<Option<FileDrift>, ScanError> {
        let destination = hash_file(target)?;
        let rendered = match self.chezmoi.cat(target) {
            Ok(bytes) => Ok(Some(ContentHash::of(&bytes))),
            Err(ChezmoiError::Eval(f)) => Err(f),
            Err(other) => return Err(other.into()),
        };
        let source_rel = match self.chezmoi.source_path(target) {
            Ok(abs) => abs.strip_prefix(source_dir).ok().map(Path::to_path_buf),
            Err(_) => None,
        };
        let git = match &source_rel {
            Some(rel) => GitSignals {
                local_ahead: self
                    .git
                    .commits_touching(&format!("{}..HEAD", self.remote_ref), rel)?
                    > 0,
                remote_ahead: self
                    .git
                    .commits_touching(&format!("HEAD..{}", self.remote_ref), rel)?
                    > 0,
            },
            None => GitSignals::default(),
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
