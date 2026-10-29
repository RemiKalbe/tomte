//! ResolveEngine (spec §5, §6.3): user-initiated drift resolution.
//!
//! Orchestrates every mutation path the same way: journal session via IPC
//! (SessionStart → SnapshotBlobs → decisions → SessionEnd), pre-announce via
//! ExpectChanges, mutate via chezmoi/git, request Rescan after. Blocking —
//! callers run it on the background executor, never the main thread.
//!
//! Failures are outcomes, not errors: a locked 1Password failing the source
//! commit still leaves the resolution DONE locally, reported honestly via
//! `ResolveOutcome::Done { committed: false, note, .. }` (spec §10).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tomte_core::chezmoi::{ChezmoiClient, ChezmoiError};
use tomte_core::drift::ContentHash;
use tomte_core::git::{GitClient, GitError};
use tomte_core::template::anchor::SpanMap;
use tomte_core::template::verify::{VerifyError, verify_write_back};
use tomte_core::template::writeback::write_back;
use tomte_core::type_change::{
    EntryKind, FolderComparison, TypeChange, compare_folder, disk_kind, type_change,
};
use tomte_journal::{BLOB_CAP, Journal, JournalError};
use tomte_proto::{Request, Response};

use crate::ipc::{IpcClient, IpcError};
use crate::merge_inputs::{MergeInputs, MergeInputsError};

/// TTL for ExpectChanges pre-announcements: generously covers a slow chezmoi
/// invocation without leaving stale suppressions around for long.
const EXPECT_TTL_SECS: u32 = 60;

/// Machine label for the app's read-only journal handle. Only stamped on
/// writes, which a read-only handle rejects — it never reaches the database.
pub(crate) const RO_MACHINE: &str = "tomte-app";

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error(transparent)]
    Chezmoi(#[from] ChezmoiError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Ipc(#[from] IpcError),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Loading the merge inputs failed (binary content, a lex failure, a
    /// chezmoi/journal error) — [`ResolveEngine::keep_source_origin`] needs
    /// them before it can decide anything.
    #[error(transparent)]
    MergeInputs(#[from] MergeInputsError),
    /// Semantic failure: an unexpected IPC reply, a missing undo blob, or
    /// `chezmoi update` failing (its stderr).
    #[error("{0}")]
    Failed(String),
}

/// Outcome of [`ResolveEngine::finish_merge_if_resolved`]: what the merge
/// with origin looked like after one more per-file decision landed in it.
#[derive(Debug)]
pub enum MergeConclusion {
    /// No merge was running — the decision stood on its own.
    NotMerging,
    /// The merge stays open: these paths still need a decision. Not an
    /// error — each one is an ordinary review row, and the last decision
    /// to land commits the merge.
    StillConflicted(Vec<PathBuf>),
    /// Nothing was left conflicted, so the merge commit was made (and
    /// pushed unless `note` says why not).
    Committed { pushed: bool, note: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    Done {
        session: i64,
        committed: bool,
        pushed: bool,
        note: Option<String>,
    },
    /// Template write-back rejected — from the merge editor's Save, "keep
    /// origin", or a one-click keep-disk on a templated file: the wanted
    /// text touches a protected template span, or cannot be placed in the
    /// template's literal text (a repeated block, an edit with no literal
    /// home), or the new template failed re-render verification. `detail`
    /// says which. Nothing was mutated — the rejection itself is journaled.
    /// UI copy: "this change touches a templated value — open in editor".
    ProtectedSpan { detail: String },
}

/// Which one-click decision a batch item carries.
///
/// `KeepSourceLocal` is the LOCAL source (`chezmoi apply`). Keeping ORIGIN's
/// version is deliberately absent: [`ResolveEngine::keep_source_origin`]
/// rewrites the source file through the write-after-verify path and can
/// reject the whole thing (`ProtectedSpan`), so it has no batchable tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchDecision {
    KeepDisk,
    KeepSourceLocal,
}

#[derive(Debug, Clone)]
pub struct BatchItem {
    pub target: PathBuf,
    pub decision: BatchDecision,
    /// Keep disk on a folder replaced by a link: files (relative to
    /// `target`) whose source version is copied into the link target first,
    /// as [`ResolveEngine::keep_disk_with`] takes them. Empty otherwise.
    pub copies: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct BatchOutcome {
    /// One entry per item, in input order. `Done` carries that file's OWN
    /// session id, so per-file undo is unchanged.
    pub per_target: Vec<(PathBuf, Result<ResolveOutcome, ResolveError>)>,
}

/// A batch item that survived [`ResolveEngine::resolve_batch`]'s pre-flight:
/// its source path is known. Not yet journaled — nothing has been touched.
struct Candidate {
    target: PathBuf,
    source: PathBuf,
    decision: BatchDecision,
    /// `source` is a `.tmpl`. A templated keep-disk cannot ride `re-add`
    /// (chezmoi ignores templates there); its source is written by the
    /// template write-back instead, and it still shares the batch's commit.
    templated: bool,
    /// The target is gone from disk. Keep disk then means "remove it from the
    /// source": it rides `chezmoi forget`, not `re-add`, and journals no disk
    /// blob. Always `templated: false` alongside, since forget removes a
    /// template as readily as a plain file.
    deleted: bool,
    /// The disk holds another kind of entry than the source (a folder that
    /// became a link, …). `re-add` silently does nothing for one, so a
    /// keep-disk rides `chezmoi add` instead (unless `deleted`, which is
    /// still a forget), and both decisions journal the disk entry exactly as
    /// the single-file paths do. Always `templated: false` alongside.
    type_change: Option<TypeChange>,
    /// The item's copies, relative to `target`; only ever non-empty for a
    /// keep-disk of a folder replaced by a link (pre-flight refuses others).
    copies: Vec<PathBuf>,
    /// Index of this item's entry in [`BatchOutcome::per_target`], carried so
    /// its result lands in INPUT order however many siblings were refused.
    slot: usize,
}

impl Candidate {
    /// A keep disk that swaps the source entry for another kind via
    /// `chezmoi add`.
    fn swaps_entry(&self) -> bool {
        self.decision == BatchDecision::KeepDisk && !self.deleted && self.type_change.is_some()
    }

    /// The copies as absolute paths inside the link target.
    fn copy_paths(&self) -> Vec<PathBuf> {
        self.copies
            .iter()
            .map(|rel| self.target.join(rel))
            .collect()
    }
}

/// A candidate whose own journal session is open and whose decision (with the
/// blobs undo needs) is journaled, so only the shared tail and `session_end`
/// remain.
struct Prepared {
    candidate: Candidate,
    session: i64,
}

#[derive(Clone)]
pub struct ResolveEngine {
    // Arc'd / cheaply clonable so background closures can own clones.
    pub chezmoi: ChezmoiClient,
    pub git: GitClient,
    pub ipc: Arc<IpcClient>,
    /// Read-only opens for undo (spec §8: single writer = daemon).
    pub journal_path: PathBuf,
    /// How long a decision keeps retrying a daemon that answers "busy" (or
    /// "starting") before that becomes the row's failure. [`DAEMON_PATIENCE`]
    /// in the app; tests shrink it.
    pub daemon_patience: Duration,
}

pub use crate::ipc::DAEMON_PATIENCE;

impl ResolveEngine {
    /// [`Self::keep_disk_with`], copying nothing.
    pub fn keep_disk(&self, target: &Path) -> Result<ResolveOutcome, ResolveError> {
        self.keep_disk_with(target, &[])
    }

    /// Keep the on-disk version: re-add it into the source state.
    ///
    /// A templated source cannot be re-added — `chezmoi re-add` silently
    /// ignores templates — so it takes the merge editor's own route instead:
    /// the on-disk text is written back into the template's literal segments
    /// and verified by re-render ([`Self::resolve_merged`], spec §6.2), which
    /// is exactly what saving the editor with "disk" chosen on every region
    /// does, minus the clicks. The one thing that path refuses is a disk edit
    /// inside a templated VALUE, and it says so (`ProtectedSpan`) rather than
    /// sending every templated file to the editor unasked.
    ///
    /// A type change (a folder that became a link, a file that became a
    /// link, …) is checked FIRST: `re-add` silently does nothing for one, so
    /// the row came back after a "successful" decision. It goes through
    /// [`Self::keep_disk_type_change`] instead. `copies` (paths relative to
    /// `target`) are only for a folder replaced by a link.
    pub fn keep_disk_with(
        &self,
        target: &Path,
        copies: &[PathBuf],
    ) -> Result<ResolveOutcome, ResolveError> {
        tomte_core::log_info!("resolve", "keep_disk {}", target.display());
        let source = self.chezmoi.source_path(target)?;
        if let Some(change) = self.type_change_at(target, &source)? {
            return self.keep_disk_type_change(target, &source, &change, copies);
        }
        if !copies.is_empty() {
            return Err(copy_picks_refused());
        }
        if is_absent(target) {
            return self.keep_deletion(target, &source);
        }
        if is_templated(&source) {
            // Checked here as well as inside `resolve_merged_as`: a source
            // full of conflict markers may not even render, and that raw
            // template error would hide the one answer that helps.
            self.refuse_mid_merge_conflict(&source, target)?;
            let inputs = self.load_inputs(target)?;
            return self.resolve_merged_as("keep_disk", &inputs, &inputs.ours);
        }
        let session = self.session_start()?;
        let hashes = self.snapshot_blobs(vec![target.to_path_buf(), source.clone()])?;
        let [dest_blob, source_blob] = hashes.as_slice() else {
            return Err(ResolveError::Failed(format!(
                "snapshot returned {} blobs for 2 paths",
                hashes.len()
            )));
        };
        let decision = build_decision("keep_disk", target, Some(dest_blob), Some(source_blob));
        self.session_decision(session, decision)?;
        self.expect_changes(vec![target.to_path_buf(), source.clone()])?;
        // The user has committed to an answer, so a diverged repo may now be
        // put into a real merge (spec §6.1). `re_add` writes the source from
        // the DESTINATION, so it overwrites whatever the merge left in that
        // file — conflict markers included — which is exactly what "keep
        // disk" means for this conflict.
        let merging = self.ensure_merge_started()?;
        let (committed, pushed, note) = if merging {
            // NOT `chezmoi re-add` here — but not because re-add cannot run
            // mid-merge. It can: with autoCommit on, its own `git add`
            // resolves the very file it just overwrote, so its commit
            // succeeds. That is the problem. `git add` stages the WHOLE
            // index, so re-add's commit would sweep up every OTHER file the
            // merge left conflicted — markers and all — and conclude a merge
            // the user only answered one row of. Copying the destination over
            // the source is exactly what re-add does for a non-templated
            // file, which is all that reaches here (`is_templated` returned
            // early above), and it leaves the staging to
            // `stage_resolution`, which stages THIS file only.
            std::fs::copy(target, &source)?;
            self.stage_resolution(&source)?;
            self.conclude_merge()
        } else {
            self.chezmoi
                .re_add(std::slice::from_ref(&target.to_path_buf()))?;
            self.commit_phase("keep_disk", target)
        };
        self.session_end(session, &format!("keep_disk {}", display_name(target)))?;
        self.rescan()?;
        Ok(ResolveOutcome::Done {
            session,
            committed,
            pushed,
            note,
        })
    }

    /// Keep disk for a target that is gone from disk: remove it from the
    /// source. The journal records the source blob and no disk blob, so Undo
    /// leaves the target absent, as it found it. Mid-merge the source file is
    /// deleted and staged by hand, for the reason `keep_disk` copies instead
    /// of re-adding there: forget's own commit would sweep up every other
    /// file the merge left conflicted.
    fn keep_deletion(&self, target: &Path, source: &Path) -> Result<ResolveOutcome, ResolveError> {
        let session = self.session_start()?;
        let source_blob = self.snapshot_source(source)?;
        let decision = build_decision("keep_disk", target, None, Some(&source_blob));
        self.session_decision(session, decision)?;
        self.expect_changes(vec![target.to_path_buf(), source.to_path_buf()])?;
        let merging = self.ensure_merge_started()?;
        let (committed, pushed, note) = if merging {
            std::fs::remove_file(source)?;
            self.stage_resolution(source)?;
            self.conclude_merge()
        } else {
            self.chezmoi
                .forget(std::slice::from_ref(&target.to_path_buf()))?;
            self.commit_phase("keep_disk", target)
        };
        self.session_end(session, &format!("keep_disk {}", display_name(target)))?;
        self.rescan()?;
        Ok(ResolveOutcome::Done {
            session,
            committed,
            pushed,
            note,
        })
    }

    /// The type change at `target`, or `None` when the disk holds the kind
    /// the source says. The scanner's check for one target: the source kind
    /// is read from the source path, the disk kind with [`disk_kind`]. Which
    /// side moved is not asked (`state_kind` is `None`): the engine acts the
    /// same either way.
    pub fn type_change_of(&self, target: &Path) -> Result<Option<TypeChange>, ResolveError> {
        let source = self.chezmoi.source_path(target)?;
        self.type_change_at(target, &source)
    }

    fn type_change_at(
        &self,
        target: &Path,
        source: &Path,
    ) -> Result<Option<TypeChange>, ResolveError> {
        let source_kind = source_kind(&self.chezmoi, target, source)?;
        let disk = disk_kind(target)?;
        Ok(type_change(&source_kind, &disk, None))
    }

    /// Keep disk for a type change: the source entry becomes what the disk
    /// holds, via `chezmoi add`, which replaces an entry of any kind. A
    /// removed link is a deletion ([`Self::keep_deletion`]).
    ///
    /// Copies (folder → link only) write the source's version of the picked
    /// files into the link target BEFORE the swap: `add` removes those source
    /// files. Each copy is journaled with what it overwrote, so Undo takes it
    /// back out. The keep-disk decision itself changes nothing on disk, so
    /// it records no disk entry and Undo restores nothing for it, as for a
    /// file.
    fn keep_disk_type_change(
        &self,
        target: &Path,
        source: &Path,
        change: &TypeChange,
        copies: &[PathBuf],
    ) -> Result<ResolveOutcome, ResolveError> {
        refuse_stray_copies(BatchDecision::KeepDisk, Some(change), copies)?;
        if change.disk == EntryKind::Absent {
            return self.keep_deletion(target, source);
        }
        let copies: Vec<PathBuf> = copies.iter().map(|rel| target.join(rel)).collect();
        // Joined before journaling, so a copy whose source file the merge
        // left conflicted refuses before anything is recorded: its raw bytes
        // are conflict markers, and they would land in the link target.
        let merging = self.ensure_merge_started()?;
        if merging && !copies.is_empty() {
            let conflicted = self.git.conflicted_files()?;
            for file in &copies {
                let rel = self.repo_rel(&self.chezmoi.source_path(file)?)?;
                if conflicted.contains(&rel) {
                    return Err(conflict_refusal(&rel));
                }
            }
        }
        let session = self.session_start()?;
        self.journal_keep_disk_type_change(session, target, change, &copies)?;
        let mut expected = vec![target.to_path_buf(), source.to_path_buf()];
        expected.extend(copies.iter().cloned());
        self.expect_changes(expected)?;
        self.write_copies(&copies)?;
        let (committed, pushed, note) = if merging {
            self.swap_source_entry(target, source)?;
            self.conclude_merge()
        } else {
            self.chezmoi
                .add(std::slice::from_ref(&target.to_path_buf()))?;
            self.commit_phase("keep_disk", target)
        };
        self.session_end(session, &format!("keep_disk {}", display_name(target)))?;
        self.rescan()?;
        Ok(ResolveOutcome::Done {
            session,
            committed,
            pushed,
            note,
        })
    }

    /// Journal a type-change keep disk into `session`: one `copy_into_link`
    /// decision per copy (absolute paths) with what it will overwrite, then
    /// the keep-disk decision, which names the change and records no disk
    /// entry.
    fn journal_keep_disk_type_change(
        &self,
        session: i64,
        target: &Path,
        change: &TypeChange,
        copies: &[PathBuf],
    ) -> Result<(), ResolveError> {
        for file in copies {
            let dest_blob = self.snapshot_dest(file)?;
            let decision = build_decision("copy_into_link", file, dest_blob.as_deref(), None);
            self.session_decision(session, decision)?;
        }
        self.session_decision(
            session,
            serde_json::json!({
                "action": "keep_disk",
                "target": target.to_string_lossy(),
                "type_change": {
                    "source": kind_word(&change.source),
                    "disk": kind_word(&change.disk),
                },
            }),
        )
    }

    /// Write the source's version of each copy (absolute paths) into the
    /// link target. Runs before the swap: `add` removes those source files.
    fn write_copies(&self, copies: &[PathBuf]) -> Result<(), ResolveError> {
        for file in copies {
            let bytes = source_version(&self.chezmoi, &self.chezmoi.source_path(file)?, file)?;
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(file, bytes)?;
        }
        Ok(())
    }

    /// The mid-merge half of a type-change keep disk. `chezmoi add` cannot
    /// run here, for the reason `keep_disk` does not `re-add`: its autoCommit
    /// stages the whole index and would sweep up every other file the merge
    /// left conflicted. So chezmoi adds the target into a SCRATCH source with
    /// an empty config (no git), which encodes the new entry exactly as `add`
    /// would (`symlink_`, attributes and all); the old entry is `git rm`ed,
    /// the scratch leaf moved into its place, and only that leaf staged.
    fn swap_source_entry(&self, target: &Path, source: &Path) -> Result<(), ResolveError> {
        let scratch = tempfile::tempdir()?;
        let scratch_source = scratch.path().join("source");
        let config_dir = scratch.path().join("config");
        std::fs::create_dir_all(&scratch_source)?;
        std::fs::create_dir_all(&config_dir)?;
        let config = config_dir.join("chezmoi.toml");
        std::fs::write(&config, "")?;
        let leaf = self
            .chezmoi
            .add_into_source(target, &scratch_source, &config)?;
        let (Some(parent), Some(name)) = (source.parent(), leaf.file_name()) else {
            return Err(ResolveError::Failed(format!(
                "cannot place {} beside {}",
                leaf.display(),
                source.display()
            )));
        };
        let new_entry = parent.join(name);
        self.git.rm_path(&self.repo_rel(source)?)?;
        // An untracked file left under the old entry would keep it a source
        // entry beside the new one.
        remove_entry(source)?;
        move_entry(&leaf, &new_entry)?;
        self.git.add_path(&self.repo_rel(&new_entry)?)?;
        Ok(())
    }

    /// Keep source for a type change: `chezmoi apply` writes the source's
    /// entry over whatever the disk holds. The journal records what was on
    /// disk (`dest_entry`, a folder walked without following links) and, for
    /// a folder the source restores, the paths that apply writes into it
    /// (`applied`), so Undo can tell Tomte's own files from ones added later.
    fn keep_source_type_change(
        &self,
        target: &Path,
        change: &TypeChange,
    ) -> Result<ResolveOutcome, ResolveError> {
        let session = self.session_start()?;
        self.journal_keep_source_type_change(session, target, change)?;
        self.expect_changes(vec![target.to_path_buf()])?;
        self.chezmoi.apply(Some(target))?;
        self.session_end(session, &format!("keep_source {}", display_name(target)))?;
        self.rescan()?;
        Ok(ResolveOutcome::Done {
            session,
            committed: false,
            pushed: false,
            note: None,
        })
    }

    /// Journal a type-change keep source into `session`: the disk entry and,
    /// for a folder the source restores, the paths apply writes into it.
    fn journal_keep_source_type_change(
        &self,
        session: i64,
        target: &Path,
        change: &TypeChange,
    ) -> Result<(), ResolveError> {
        let applied = match change.source {
            EntryKind::Folder => Some(
                self.chezmoi
                    .managed_under(target, None)?
                    .iter()
                    .map(|path| path.strip_prefix(target).unwrap_or(path).to_path_buf())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        let dest_entry = self.snapshot_entry(target, &change.disk)?;
        let decision = build_entry_decision("keep_source", target, dest_entry, applied.as_deref());
        self.session_decision(session, decision)
    }

    /// What is at `target` now, as the journal's `dest_entry`, its file
    /// contents snapshotted as blobs.
    fn snapshot_entry(
        &self,
        target: &Path,
        disk: &EntryKind,
    ) -> Result<serde_json::Value, ResolveError> {
        Ok(match disk {
            EntryKind::Absent => serde_json::json!({ "kind": "absent" }),
            EntryKind::Link(text) => serde_json::json!({ "kind": "link", "text": text }),
            EntryKind::File => {
                let blob = self.snapshot_files(vec![target.to_path_buf()])?.remove(0);
                serde_json::json!({ "kind": "file", "blob": blob })
            }
            EntryKind::Folder => {
                let walked = walk_folder(target)?;
                let files: Vec<PathBuf> = walked
                    .iter()
                    .filter(|(_, kind)| *kind == Walked::File)
                    .map(|(rel, _)| target.join(rel))
                    .collect();
                let mut blobs = self.snapshot_files(files)?.into_iter();
                let mut entries = Vec::new();
                for (rel, kind) in walked {
                    let path = rel.to_string_lossy().into_owned();
                    match kind {
                        Walked::Folder => {}
                        Walked::File => {
                            let blob = blobs.next().unwrap_or_default();
                            entries.push(serde_json::json!({ "path": path, "blob": blob }));
                        }
                        Walked::Link(text) => {
                            entries.push(serde_json::json!({ "path": path, "link": text }));
                        }
                    }
                }
                serde_json::json!({ "kind": "folder", "entries": entries })
            }
        })
    }

    /// Snapshot `paths` (files that exist), one blob each, in order.
    fn snapshot_files(&self, paths: Vec<PathBuf>) -> Result<Vec<String>, ResolveError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let wanted = paths.len();
        let hashes = self.snapshot_blobs(paths)?;
        if hashes.len() != wanted {
            return Err(ResolveError::Failed(format!(
                "snapshot returned {} blobs for {wanted} paths",
                hashes.len()
            )));
        }
        Ok(hashes)
    }

    /// Restore chezmoi's version: apply the target. No commit phase — apply
    /// never touches the source repo, so it decides nothing about a
    /// divergence with origin and never starts a merge.
    ///
    /// That cuts the other way once a merge IS running: a source file git
    /// left conflicted holds conflict markers, and applying it would copy
    /// them into the user's destination file. Refused, loudly — the origin
    /// difference has to be answered first, in the merge editor.
    pub fn keep_source(&self, target: &Path) -> Result<ResolveOutcome, ResolveError> {
        tomte_core::log_info!("resolve", "keep_source {}", target.display());
        let source = self.chezmoi.source_path(target)?;
        let change = self.type_change_at(target, &source)?;
        if let Some(change) = &change {
            refuse_unrestorable(target, change)?;
        }
        // Catch up first, exactly as `keep_disk` does. This one never FAILED
        // while behind — `chezmoi apply` does not dirty the source, so there
        // was nothing to push and `commit_phase`'s rebase-and-retry recovery
        // never fired — but it quietly left the repo behind origin. Origin's
        // changes then never arrived, and the next decision that DID need to
        // push walked into the non-fast-forward wall.
        //
        // The mid-merge guard below runs after this deliberately, so it sees
        // the state the join actually produced.
        self.ensure_merge_started()?;
        if let Some(change) = change {
            // Apply writes every file under the entry, conflict markers
            // included, so any conflicted path under it refuses.
            if self.git.merge_in_progress() {
                let rel = self.repo_rel(&source)?;
                if let Some(hit) = self
                    .git
                    .conflicted_files()?
                    .into_iter()
                    .find(|path| path.starts_with(&rel))
                {
                    return Err(conflict_refusal(&hit));
                }
            }
            return self.keep_source_type_change(target, &change);
        }
        if self.is_mid_merge_conflict(&source)? {
            return Err(ResolveError::Failed(format!(
                "{} is mid-merge with origin — its source file holds conflict markers; \
                 resolve the origin difference in the merge editor first",
                display_name(target)
            )));
        }
        let session = self.session_start()?;
        let dest_blob = self.snapshot_dest(target)?;
        let decision = build_decision("keep_source", target, dest_blob.as_deref(), None);
        self.session_decision(session, decision)?;
        self.expect_changes(vec![target.to_path_buf()])?;
        self.chezmoi.apply(Some(target))?;
        self.session_end(session, &format!("keep_source {}", display_name(target)))?;
        self.rescan()?;
        Ok(ResolveOutcome::Done {
            session,
            committed: false,
            pushed: false,
            note: None,
        })
    }

    /// Resolve several one-click decisions with ONE shared tail.
    ///
    /// Per file: its own journal session, snapshot and decision, exactly as
    /// [`Self::keep_disk`] / [`Self::keep_source`] do — so Undo is unchanged.
    /// Shared: one `expect_changes`, one `chezmoi re-add` for every keep-disk
    /// target (one `chezmoi add` for the type changes among them, which
    /// re-add silently skips), one `chezmoi apply` for every keep-source
    /// target, type changes included, one commit
    /// and one push for the keep-disk group, one rescan. Those are the
    /// seconds (re-add ~2.5s, push ~1.5s, per INVOCATION, not per file); the
    /// sessions are milliseconds.
    ///
    /// The two decision groups are reported SEPARATELY, because they mutate
    /// different things and one failing says nothing about the other. Only
    /// keep-disk reaches the source repo, so only it can be committed and
    /// pushed; keep-source reports `committed: false, pushed: false`, exactly
    /// as [`Self::keep_source`] does. A keep-source-only batch touches the
    /// repo not at all.
    ///
    /// Mid-merge the shared tail is not available, and the reason is sharper
    /// than "git dislikes unmerged paths": the tail stages the WHOLE index
    /// and commits it. Run inside a merge it would commit the conflicts the
    /// user never answered — conflict markers and all — and call the merge
    /// concluded. So items fall back to their single-file paths, one at a
    /// time, where each answer is staged into the merge on its own and the
    /// merge only concludes once nothing is left conflicted. Slower, and the
    /// only correct answer. Pinned by `story_o` in `resolve_e2e`.
    pub fn resolve_batch(&self, items: Vec<BatchItem>) -> BatchOutcome {
        tomte_core::log_info!("resolve", "resolve_batch of {} files", items.len());
        let mut per_target: Vec<(PathBuf, Result<ResolveOutcome, ResolveError>)> =
            Vec::with_capacity(items.len());

        // Pre-flight — resolve every item's source path BEFORE touching the
        // repo, so a batch that cannot even name its files touches nothing.
        let mut candidates: Vec<Candidate> = Vec::new();
        for item in items {
            let slot = per_target.len();
            let preflight = self
                .chezmoi
                .source_path(&item.target)
                .map_err(ResolveError::from)
                .and_then(|source| {
                    let change = self.type_change_at(&item.target, &source)?;
                    refuse_stray_copies(item.decision, change.as_ref(), &item.copies)?;
                    if let (BatchDecision::KeepSourceLocal, Some(change)) = (item.decision, &change)
                    {
                        refuse_unrestorable(&item.target, change)?;
                    }
                    Ok((source, change))
                });
            match preflight {
                Ok((source, type_change)) => {
                    // Overwritten below for EVERY candidate. An honest error
                    // so that if a future edit ever let one leak, the caller
                    // reads a failure instead of a bogus success.
                    per_target.push((
                        item.target.clone(),
                        Err(ResolveError::Failed(
                            "internal: the batch never reported this file".into(),
                        )),
                    ));
                    let deleted = is_absent(&item.target);
                    let templated = !deleted && type_change.is_none() && is_templated(&source);
                    candidates.push(Candidate {
                        target: item.target,
                        source,
                        decision: item.decision,
                        templated,
                        deleted,
                        type_change,
                        copies: item.copies,
                        slot,
                    });
                }
                Err(e) => per_target.push((item.target, Err(e))),
            }
        }
        if candidates.is_empty() {
            return BatchOutcome { per_target };
        }

        // Catch up with origin once for the whole batch. If that opens a
        // conflicted merge, the batch tail cannot be used — run singly.
        match self.ensure_merge_started() {
            Ok(true) => {
                for c in candidates {
                    per_target[c.slot].1 = match c.decision {
                        BatchDecision::KeepDisk => self.keep_disk_with(&c.target, &c.copies),
                        BatchDecision::KeepSourceLocal => self.keep_source(&c.target),
                    };
                }
                return BatchOutcome { per_target };
            }
            Ok(false) => {}
            Err(e) => {
                // Nothing was journaled or mutated for anyone: the batch
                // could not even reach the repo.
                let msg = e.to_string();
                for c in candidates {
                    per_target[c.slot].1 = Err(ResolveError::Failed(msg.clone()));
                }
                return BatchOutcome { per_target };
            }
        }

        // Phase 1 — per file, cheap: session, snapshot, decision. AFTER the
        // catch-up on purpose: a merge rewrites source files, and a snapshot
        // taken before it would journal content undo must not restore.
        // A daemon that stayed busy past the whole patience budget for one
        // file is not asked again for the next: the budget is per request in
        // `IpcClient`, so it is made per BATCH here — a ten-file batch must
        // not hold the queue for ten budgets while every row sits at
        // "running…". `stuck` is set by the first exhausted budget, in this
        // loop or in Phase 2a's rejection journaling or in Phase 3's session
        // close, and every later daemon call in the batch is skipped: the
        // file is reported as not attempted (nothing happened to it), or —
        // after its work landed — as resolved with its session left open.
        let mut prepared: Vec<Prepared> = Vec::new();
        let mut stuck: Option<String> = None;
        for candidate in candidates {
            if let Some(why) = &stuck {
                per_target[candidate.slot].1 = Err(not_attempted(why));
                continue;
            }
            match self.open_batch_session(&candidate) {
                Ok(session) => prepared.push(Prepared { candidate, session }),
                Err(e) => {
                    if let Some(why) = stuck_phrase(&e) {
                        stuck = Some(why);
                    }
                    per_target[candidate.slot].1 = Err(e);
                }
            }
        }
        if prepared.is_empty() {
            return BatchOutcome { per_target };
        }
        if let Some(why) = &stuck {
            // Nothing below has run for anyone: no announcement, no write.
            for p in &prepared {
                per_target[p.candidate.slot].1 = Err(not_attempted(why));
            }
            return BatchOutcome { per_target };
        }

        // Phase 2 — the shared tail, the whole point of the method.
        //
        // The two decision groups are evaluated SEPARATELY. They mutate
        // different things — `re-add` rewrites source files, `apply` rewrites
        // destination files — so one failing says nothing about the other. A
        // single shared verdict would have reported "this file was not
        // resolved" for keep-disk files whose sources had already been
        // rewritten, skipped their `session_end`, and so put those writes
        // beyond `undo_last` (which reads the last FINISHED session).
        let mut expect: Vec<PathBuf> = Vec::new();
        for p in &prepared {
            expect.push(p.candidate.target.clone());
            if p.candidate.decision == BatchDecision::KeepDisk {
                expect.push(p.candidate.source.clone());
            }
            expect.extend(p.candidate.copy_paths());
        }
        if let Err(e) = self.expect_changes(expect) {
            // Pre-announcing is the first thing the tail does, so nothing has
            // been mutated for anyone yet.
            for p in &prepared {
                per_target[p.candidate.slot].1 = Err(ResolveError::Failed(format!(
                    "batch failed before this file could land: {e}"
                )));
            }
            return BatchOutcome { per_target };
        }

        // Phase 2a — templated keep-disks write their SOURCE first. `re-add`
        // ignores templates, so each of these goes through the template
        // write-back instead: the disk text is mapped into the literal
        // segments and proven to re-render (spec §6.2), then written. A
        // verified write is an ordinary dirty file the shared commit below
        // picks up — still one commit for the batch. A rejection (a disk
        // edit inside a templated VALUE) ends that file's own session right
        // here, attempt and rejection journaled exactly as `resolve_merged`
        // journals them; the file drops out of the tail and its slot says
        // `ProtectedSpan`.
        let mut written: Vec<usize> = Vec::new(); // indices into `prepared`
        for (ix, p) in prepared.iter().enumerate() {
            if !(p.candidate.decision == BatchDecision::KeepDisk && p.candidate.templated) {
                continue;
            }
            let target = p.candidate.target.as_path();
            if let Some(why) = &stuck {
                per_target[p.candidate.slot].1 = Err(not_attempted(why));
                continue;
            }
            per_target[p.candidate.slot].1 = match self.write_back_disk_text(p) {
                Ok(WriteBackAttempt::Verified(new_template)) => {
                    match std::fs::write(&p.candidate.source, new_template) {
                        Ok(()) => {
                            written.push(ix);
                            continue;
                        }
                        Err(e) => Err(e.into()),
                    }
                }
                Ok(WriteBackAttempt::Rejected(detail)) => {
                    match self.journal_rejection(p.session, "keep_disk", target, &detail) {
                        Ok(()) => Ok(ResolveOutcome::ProtectedSpan { detail }),
                        Err(e) => {
                            if let Some(why) = stuck_phrase(&e) {
                                stuck = Some(why);
                            }
                            Err(e)
                        }
                    }
                }
                Err(e) => Err(e),
            };
        }

        // Phase 2b — copies for a kept link that replaced a folder, written
        // into the link target BEFORE the shared `add` removes their source
        // files. A failed write fails that file alone: it drops out of the
        // add, and its session stays open as a failed single-file keep disk
        // leaves its own.
        let mut copy_failed: Vec<usize> = Vec::new(); // indices into `prepared`
        for (ix, p) in prepared.iter().enumerate() {
            if !p.candidate.swaps_entry() || p.candidate.copies.is_empty() {
                continue;
            }
            if let Err(e) = self.write_copies(&p.candidate.copy_paths()) {
                per_target[p.candidate.slot].1 = Err(e);
                copy_failed.push(ix);
            }
        }

        let readds: Vec<PathBuf> = prepared
            .iter()
            .filter(|p| {
                p.candidate.decision == BatchDecision::KeepDisk
                    && !p.candidate.templated
                    && !p.candidate.deleted
                    && p.candidate.type_change.is_none()
            })
            .map(|p| p.candidate.target.clone())
            .collect();
        let adds: Vec<PathBuf> = prepared
            .iter()
            .enumerate()
            .filter(|(ix, p)| p.candidate.swaps_entry() && !copy_failed.contains(ix))
            .map(|(_, p)| p.candidate.target.clone())
            .collect();
        let forgets: Vec<PathBuf> = prepared
            .iter()
            .filter(|p| p.candidate.decision == BatchDecision::KeepDisk && p.candidate.deleted)
            .map(|p| p.candidate.target.clone())
            .collect();
        // Guarded, never called empty: an empty target list keeps chezmoi's
        // own meaning — EVERY modified file — and both `re_add` and
        // `apply_many` refuse one outright rather than let that happen.
        //
        // These three run with chezmoi's git automation OFF. With autoCommit
        // and autoPush on (the user's setup) each of them would commit and
        // push by itself, so a mixed batch made up to three commits and three
        // pushes; off, they only change the source and `commit_batch` below
        // makes the batch's one commit and one push.
        // Derived on first use, so a batch with none of the three never asks.
        let quiet = std::cell::OnceCell::new();
        let quietly = |run: &dyn Fn(&ChezmoiClient) -> Result<(), ChezmoiError>| {
            let client = quiet.get_or_init(|| {
                self.chezmoi.without_git_automation().map_err(|e| {
                    tomte_core::log_error!("resolve", "batch config without git automation: {e}");
                    format!("could not turn off chezmoi's git automation for the batch: {e}")
                })
            });
            match client {
                Ok(client) => run(client),
                Err(why) => Err(ChezmoiError::Failed(why.clone())),
            }
        };

        let readd = (!readds.is_empty()).then(|| quietly(&|c| c.re_add(&readds)));
        let added = (!adds.is_empty()).then(|| quietly(&|c| c.add(&adds)));
        let forgot = (!forgets.is_empty()).then(|| quietly(&|c| c.forget(&forgets)));

        let applies = self.batch_targets(&prepared, BatchDecision::KeepSourceLocal);
        let applied = (!applies.is_empty()).then(|| self.chezmoi.apply_many(&applies));
        // A verified template write leaves the destination already saying
        // what the template now renders; this apply is for chezmoi's own
        // record of what it last wrote, exactly as `resolve_merged` applies
        // after its write. Its own invocation, so a failure here is charged
        // to these files and not to the keep-source group.
        let tmpl_targets: Vec<PathBuf> = written
            .iter()
            .map(|&ix| prepared[ix].candidate.target.clone())
            .collect();
        let applied_tmpl =
            (!tmpl_targets.is_empty()).then(|| self.chezmoi.apply_many(&tmpl_targets));

        // Only keep-disk writes reach the SOURCE repo — `re-add` for the plain
        // files, the write-back for the templated ones — so the commit and
        // push belong to those and run if EITHER landed: a failed re-add must
        // not leave verified template writes sitting dirty for the next
        // catch-up to sweep into a commit nobody named. A keep-source-only
        // batch commits nothing and pushes nothing, and reports `false/false`
        // exactly as single-file `keep_source` does — rather than counting a
        // clean repo as "committed" and pushing a no-op.
        let plain_landed = matches!(readd, Some(Ok(())));
        let forgot_landed = matches!(forgot, Some(Ok(())));
        let added_landed = matches!(added, Some(Ok(())));
        let landed_disk: Vec<usize> = prepared
            .iter()
            .enumerate()
            .filter(|(ix, p)| {
                p.candidate.decision == BatchDecision::KeepDisk
                    && if p.candidate.templated {
                        written.contains(ix)
                    } else if p.candidate.deleted {
                        forgot_landed
                    } else if p.candidate.swaps_entry() {
                        added_landed && !copy_failed.contains(ix)
                    } else {
                        plain_landed
                    }
            })
            .map(|(ix, _)| ix)
            .collect();
        let commit: (bool, bool, Option<String>) = if landed_disk.is_empty() {
            // Nothing reached the source repo, so there is nothing to commit.
            // The value is inert: Phase 3 only reads it for landed keep-disk
            // files, and there are none.
            (false, false, None)
        } else {
            let targets: Vec<&Path> = landed_disk
                .iter()
                .map(|&ix| prepared[ix].candidate.target.as_path())
                .collect();
            self.commit_batch(&targets)
        };

        // Phase 3 — per file again: end its own session, report its own
        // group's result.
        for (ix, p) in prepared.iter().enumerate() {
            let landed: Result<(bool, bool, Option<String>), String> = match p.candidate.decision {
                BatchDecision::KeepDisk if p.candidate.templated => {
                    // Rejected or failed in Phase 2a: its slot is already
                    // reported and its session already closed (or, on an
                    // error, left as the other groups leave theirs).
                    if !written.contains(&ix) {
                        continue;
                    }
                    match &applied_tmpl {
                        Some(Err(e)) => Err(format!("the batch apply failed: {e}")),
                        _ => Ok(commit.clone()),
                    }
                }
                BatchDecision::KeepDisk if p.candidate.deleted => match &forgot {
                    Some(Err(e)) => Err(format!("the batch forget failed: {e}")),
                    _ => Ok(commit.clone()),
                },
                BatchDecision::KeepDisk if p.candidate.swaps_entry() => {
                    // A failed copy is already reported, its session open.
                    if copy_failed.contains(&ix) {
                        continue;
                    }
                    match &added {
                        Some(Err(e)) => Err(format!("the batch add failed: {e}")),
                        _ => Ok(commit.clone()),
                    }
                }
                BatchDecision::KeepDisk => match &readd {
                    Some(Err(e)) => Err(format!("the batch re-add failed: {e}")),
                    _ => Ok(commit.clone()),
                },
                BatchDecision::KeepSourceLocal => match &applied {
                    Some(Err(e)) => Err(format!("the batch apply failed: {e}")),
                    _ => Ok((false, false, None)),
                },
            };
            let label = match p.candidate.decision {
                BatchDecision::KeepDisk => "keep_disk",
                BatchDecision::KeepSourceLocal => "keep_source",
            };
            per_target[p.candidate.slot].1 = close_landed(
                p.session,
                landed,
                || {
                    self.session_end(
                        p.session,
                        &format!("{label} {}", display_name(&p.candidate.target)),
                    )
                },
                &mut stuck,
            );
        }
        if let Some(why) = &stuck {
            // The last daemon call of the batch is skipped like the others:
            // a stuck daemon is not asked once more for a second budget. Its
            // own next scan covers what this one would have.
            tomte_core::log_warn!("resolve", "rescan after batch skipped: {why}");
        } else if let Err(e) = self.rescan() {
            // Every file already landed; a missed rescan only means the
            // daemon's view is stale, which the next scan fixes.
            tomte_core::log_warn!("resolve", "rescan after batch failed: {e}");
        }
        BatchOutcome { per_target }
    }

    /// The targets of one decision group, in input order — the list a single
    /// `re-add` or `apply` invocation is handed.
    fn batch_targets(&self, prepared: &[Prepared], decision: BatchDecision) -> Vec<PathBuf> {
        prepared
            .iter()
            .filter(|p| p.candidate.decision == decision)
            .map(|p| p.candidate.target.clone())
            .collect()
    }

    /// Open a batch item's OWN journal session and record its decision with
    /// the blobs undo will need. One session per file, never one per batch:
    /// that is what keeps Undo per file.
    ///
    /// The keep-disk blob count is checked as strictly as the single-file
    /// path checks it: `SnapshotBlobs` silently omits a path it could not
    /// snapshot, and a keep-disk decision journaled without its
    /// `source_blob` is an undo that cannot restore the source. Keep source
    /// goes through [`Self::snapshot_dest`], which knows a target absent on
    /// disk from a snapshot that failed.
    fn open_batch_session(&self, c: &Candidate) -> Result<i64, ResolveError> {
        let session = self.session_start()?;
        // A type change journals exactly what its single-file path does. A
        // removed link (keep disk, `deleted`) is a deletion, as it is there.
        if let Some(change) = &c.type_change {
            match c.decision {
                BatchDecision::KeepDisk if c.deleted => {}
                BatchDecision::KeepDisk => {
                    self.journal_keep_disk_type_change(
                        session,
                        &c.target,
                        change,
                        &c.copy_paths(),
                    )?;
                    return Ok(session);
                }
                BatchDecision::KeepSourceLocal => {
                    self.journal_keep_source_type_change(session, &c.target, change)?;
                    return Ok(session);
                }
            }
        }
        let (action, dest_blob, source_blob) = match c.decision {
            BatchDecision::KeepDisk if c.deleted => {
                ("keep_disk", None, Some(self.snapshot_source(&c.source)?))
            }
            BatchDecision::KeepDisk => {
                let hashes = self.snapshot_blobs(vec![c.target.clone(), c.source.clone()])?;
                let [dest, source] = hashes.as_slice() else {
                    return Err(ResolveError::Failed(format!(
                        "snapshot returned {} blobs for 2 paths",
                        hashes.len()
                    )));
                };
                ("keep_disk", Some(dest.clone()), Some(source.clone()))
            }
            BatchDecision::KeepSourceLocal => ("keep_source", self.snapshot_dest(&c.target)?, None),
        };
        let decision = build_decision(
            action,
            &c.target,
            dest_blob.as_deref(),
            source_blob.as_deref(),
        );
        self.session_decision(session, decision)?;
        Ok(session)
    }

    /// The target's disk content, snapshotted for undo — or `None` when the
    /// target does not exist on disk: a source file this machine has never
    /// applied (2026-10-14: a 427-file skill folder, every Keep source
    /// refused with "snapshot returned 0 blobs for 1 paths"). Undo of such a
    /// decision removes what `apply` created. The absence is established
    /// HERE, on the same filesystem, before asking the daemon: `SnapshotBlobs`
    /// omits a path it cannot read without saying which, so a blob-count
    /// mismatch for a target that DOES exist stays the error it always was.
    fn snapshot_source(&self, source: &Path) -> Result<String, ResolveError> {
        let hashes = self.snapshot_blobs(vec![source.to_path_buf()])?;
        match hashes.as_slice() {
            [one] => Ok(one.clone()),
            _ => Err(ResolveError::Failed(format!(
                "snapshot of the source {} returned {} blobs",
                source.display(),
                hashes.len()
            ))),
        }
    }

    fn snapshot_dest(&self, target: &Path) -> Result<Option<String>, ResolveError> {
        match std::fs::metadata(target) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let hashes = self.snapshot_blobs(vec![target.to_path_buf()])?;
        match hashes.as_slice() {
            [one] => Ok(Some(one.clone())),
            _ => Err(ResolveError::Failed(format!(
                "snapshot returned {} blobs for 1 path",
                hashes.len()
            ))),
        }
    }

    /// Keep ORIGIN's version of the source: origin's rendered content becomes
    /// the source content, then the target is applied from it.
    ///
    /// [`Self::keep_source`] cannot serve this — it never touches the source
    /// file, it applies what the LOCAL source already says. For origin to win,
    /// origin's content has to become the source content first, which is
    /// exactly what [`Self::resolve_merged`] does: verbatim for a plain file,
    /// write-back plus `execute-template` verification for a templated one
    /// (spec §6.2). So this loads the merge inputs and hands origin's rendered
    /// text to that already-tested write-after-verify path rather than
    /// inventing a second one — including its rejection: if origin's version
    /// disagrees inside a protected template span, the caller gets
    /// `ProtectedSpan` and nothing is mutated.
    ///
    /// The inputs are loaded HERE, at click time, not taken from the caller:
    /// origin's text the UI loaded when the file was selected may be minutes
    /// old, and writing a stale blob into the source is not a resolution.
    /// `origin_seen` is what the caller actually SHOWED the user (the text its
    /// diff was built from) and the reload must still agree with it — a fetch
    /// between selecting and clicking would otherwise write content the user
    /// never reviewed into their dotfiles. On the one path here that rewrites
    /// a source file, "what you approved is what gets written" outranks
    /// convenience: a refusal the user retries costs a click, an unreviewed
    /// write costs trust.
    ///
    /// A file with no origin blob at all (added locally, never pushed) is an
    /// error, not a silent no-op — the UI only offers this action for files
    /// whose origin content actually loaded, so reaching here means the world
    /// changed underneath it.
    ///
    /// Both refusals happen BEFORE `session_start`: nothing is snapshotted,
    /// journaled or mutated when either fires.
    pub fn keep_source_origin(
        &self,
        target: &Path,
        origin_seen: &str,
    ) -> Result<ResolveOutcome, ResolveError> {
        tomte_core::log_info!("resolve", "keep_source_origin {}", target.display());
        let (inputs, remote_ref) = self.load_inputs_with_origin(target)?;
        let Some(origin) = inputs.origin.clone() else {
            return Err(ResolveError::Failed(format!(
                "{} has no version at {remote_ref} that differs from this machine's source",
                display_name(target)
            )));
        };
        if origin != origin_seen {
            return Err(ResolveError::Failed(format!(
                "{}'s version at {remote_ref} changed since it was shown — \
                 reload the file and look at the new one before keeping it",
                display_name(target)
            )));
        }
        self.resolve_merged_as("keep_source_origin", &inputs, &origin)
    }

    /// The merge editor's inputs for `target`, loaded NOW — `chezmoi cat`, the
    /// destination, the journal's last-written base, and the template span
    /// map when the source is a `.tmpl` — WITHOUT origin's pane. Every
    /// one-click path that rewrites a SOURCE file through the write-after-
    /// verify route loads them here rather than trusting content the UI
    /// loaded earlier. Origin is left out because these callers never read
    /// it, and it costs a `git cat-file` plus a render per file.
    fn load_inputs(&self, target: &Path) -> Result<MergeInputs, ResolveError> {
        Ok(crate::merge_inputs::load_local(
            &self.chezmoi,
            &self.journal_path,
            target,
        )?)
    }

    /// [`Self::load_inputs`] plus origin's pane, for the one caller that acts
    /// on it. Also returns the `origin/<branch>` ref it was loaded against,
    /// for the messages that name it.
    fn load_inputs_with_origin(
        &self,
        target: &Path,
    ) -> Result<(MergeInputs, String), ResolveError> {
        // Same derivation as the daemon's and `recover_rejected_push`'s: an
        // unreadable branch degrades to "main" rather than failing.
        let branch = self.git.head_branch().unwrap_or_else(|_| "main".into());
        let remote_ref = format!("origin/{branch}");
        let inputs = crate::merge_inputs::load(
            &self.chezmoi,
            &self.git,
            &remote_ref,
            &self.journal_path,
            target,
        )?;
        Ok((inputs, remote_ref))
    }

    /// A merge already running with THIS file conflicted means its
    /// working-tree source is git's conflict-marker text, and anything read
    /// from that file describes nothing that exists. Writing a resolution
    /// derived from it would persist the markers. The way in is the merge
    /// editor's repo-conflict mode, which reads the index stages instead
    /// (see [`Self::repo_conflict_for`]).
    fn refuse_mid_merge_conflict(
        &self,
        source_path: &Path,
        target: &Path,
    ) -> Result<(), ResolveError> {
        if self.is_mid_merge_conflict(source_path)? {
            return Err(ResolveError::Failed(format!(
                "{} is mid-merge with origin — reopen it in the merge editor, \
                 which reads the two sides from the merge itself",
                display_name(target)
            )));
        }
        Ok(())
    }

    /// Journal a write-back rejection and close its session: the attempt
    /// happened, nothing was mutated (spec §8: the journal is truth).
    fn journal_rejection(
        &self,
        session: i64,
        action: &str,
        target: &Path,
        detail: &str,
    ) -> Result<(), ResolveError> {
        self.session_decision(
            session,
            serde_json::json!({
                "action": format!("{action}_rejected"),
                "target": target.to_string_lossy(),
                "detail": detail,
            }),
        )?;
        self.session_end(
            session,
            &format!(
                "{action} rejected (protected span) {}",
                display_name(target)
            ),
        )
    }

    /// The in-memory write-back of one templated keep-disk in a batch: load
    /// the merge inputs (the render, the span map) and map the destination's
    /// text into the template's literal segments. Nothing is written here.
    fn write_back_disk_text(&self, p: &Prepared) -> Result<WriteBackAttempt, ResolveError> {
        let inputs = self.load_inputs(&p.candidate.target)?;
        let Some(span_map) = &inputs.span_map else {
            return Err(ResolveError::Failed(format!(
                "templated merge inputs for {} carry no span map",
                display_name(&p.candidate.target)
            )));
        };
        let template = std::fs::read_to_string(&inputs.source_path)?;
        write_back_verified(
            &self.chezmoi,
            &template,
            span_map,
            &inputs.theirs,
            &inputs.ours,
        )
    }

    /// Persist a merge-editor resolution: write the resolved text into the
    /// source (plain file: verbatim; templated: write_back + verify), then
    /// apply the target so all states converge. Full session/undo plumbing.
    ///
    /// Templated sources are WRITE-AFTER-VERIFY (spec §6.2): the new template
    /// text is computed in memory and re-rendered via `chezmoi
    /// execute-template` first; the source file is written only once the
    /// render matches `resolved`. A protected-span touch or a verification
    /// mismatch ends the session with a rejection decision (journal truth:
    /// the attempt happened; nothing mutated) and returns
    /// `Ok(ProtectedSpan { detail })` with the source file untouched.
    pub fn resolve_merged(
        &self,
        inputs: &MergeInputs,
        resolved: &str,
    ) -> Result<ResolveOutcome, ResolveError> {
        self.resolve_merged_as("merge", inputs, resolved)
    }

    /// [`Self::resolve_merged`] under a caller-chosen action name. The name
    /// is what the journal decision, the session summary and the fallback
    /// commit message all say happened, so an origin keep does not have to
    /// file itself as a hand-made merge (spec §8: the journal is truth).
    fn resolve_merged_as(
        &self,
        action: &str,
        inputs: &MergeInputs,
        resolved: &str,
    ) -> Result<ResolveOutcome, ResolveError> {
        tomte_core::log_info!("resolve", "{action} {}", inputs.target.display());
        let target = inputs.target.as_path();
        // `inputs` was read from the working-tree source: refuse if that is
        // git's conflict-marker text (see `refuse_mid_merge_conflict`).
        self.refuse_mid_merge_conflict(&inputs.source_path, target)?;
        let session = self.session_start()?;
        let hashes =
            self.snapshot_blobs(vec![inputs.target.clone(), inputs.source_path.clone()])?;
        let [dest_blob, source_blob] = hashes.as_slice() else {
            return Err(ResolveError::Failed(format!(
                "snapshot returned {} blobs for 2 paths",
                hashes.len()
            )));
        };
        let decision = build_decision(action, target, Some(dest_blob), Some(source_blob));
        self.session_decision(session, decision)?;
        self.expect_changes(vec![inputs.target.clone(), inputs.source_path.clone()])?;

        // The new SOURCE text is computed FIRST, before any merge starts:
        // the templated path reads the current source file as its template,
        // and a merge started ahead of it would hand `write_back` git's
        // conflict markers instead of a template.
        let new_source = if inputs.templated {
            let Some(span_map) = &inputs.span_map else {
                return Err(ResolveError::Failed(format!(
                    "templated merge inputs for {} carry no span map",
                    display_name(target)
                )));
            };
            let template = std::fs::read_to_string(&inputs.source_path)?;
            let attempt =
                write_back_verified(&self.chezmoi, &template, span_map, &inputs.theirs, resolved)?;
            match attempt {
                WriteBackAttempt::Verified(new_template) => new_template,
                WriteBackAttempt::Rejected(detail) => {
                    self.journal_rejection(session, action, target, &detail)?;
                    return Ok(ResolveOutcome::ProtectedSpan { detail });
                }
            }
        } else {
            resolved.to_string()
        };

        // The decision is made, so a diverged repo may now be put into a
        // real merge and this file's answer recorded IN it (spec §6.1):
        // `resolve_repo_conflict` writes the source and stages it, which is
        // how git learns the conflict is answered.
        let merging = self.ensure_merge_started()?;
        if merging {
            self.resolve_repo_conflict(&inputs.source_path, &new_source)?;
        } else {
            std::fs::write(&inputs.source_path, &new_source)?;
        }

        self.chezmoi.apply(Some(target))?;
        let (committed, pushed, note) = if merging {
            self.conclude_merge()
        } else {
            self.commit_phase(action, target)
        };
        self.session_end(session, &format!("{action} {}", display_name(target)))?;
        self.rescan()?;
        Ok(ResolveOutcome::Done {
            session,
            committed,
            pushed,
            note,
        })
    }

    /// Pull + apply (`chezmoi update`). Caller guarantees zero pending
    /// decisions (menu gating). Its applies make targets equal the rendered
    /// state, which the daemon probes as InSync — no ExpectChanges needed.
    pub fn sync_all(&self) -> Result<ResolveOutcome, ResolveError> {
        tomte_core::log_info!("resolve", "sync_all");
        let session = self.session_start()?;
        self.session_decision(session, serde_json::json!({ "action": "sync_all" }))?;
        // Network/merge failures are semantic (Plan 7 owns conflicts): report
        // chezmoi's stderr, leaving the session unfinished so it can never
        // become undoable state.
        self.chezmoi
            .update()
            .map_err(|e| ResolveError::Failed(e.to_string()))?;
        self.session_end(session, "sync_all")?;
        self.rescan()?;
        Ok(ResolveOutcome::Done {
            session,
            committed: false,
            pushed: false,
            note: None,
        })
    }

    /// Restore the destination files of the last session that LANDED from
    /// their journaled blobs, journal the undo as a new session, and rescan.
    /// Returns the id of the session that was undone, or `Ok(None)` when no
    /// such session exists.
    ///
    /// "Landed" excludes a finished session that recorded a rejection: the
    /// write-back refused before any write, so it mutated nothing by
    /// construction and reverting it would rewrite the same bytes — while
    /// shadowing the real last landing. The journal orders sessions by id,
    /// which is START order, and a batch starts every item's session before
    /// any of them lands, so a rejected last item would otherwise outrank a
    /// landed sibling and Undo would revert something other than what the
    /// banner names.
    ///
    /// Source-side revert (rolling back the source-repo commit a `keep_disk`
    /// produced) is deferred to Plan 7's merge tooling — this only restores
    /// destination files.
    pub fn undo_last(&self) -> Result<Option<i64>, ResolveError> {
        tomte_core::log_info!("resolve", "undo_last");
        let journal = Journal::open_read_only(&self.journal_path, RO_MACHINE)?;
        let Some((of, decisions)) = journal
            .finished_sessions_desc(UNDO_LOOKBACK)?
            .into_iter()
            .find(|(_, decisions)| session_landed(decisions))
        else {
            return Ok(None);
        };
        let steps = parse_undo_restores(&decisions);
        // Refusals first, so a refused undo touches nothing.
        for step in steps.iter().filter(|step| step.replace) {
            refuse_foreign_folder(step)?;
        }
        // Every blob is read BEFORE anything at a target is removed: a
        // missing one (a file over the journal's cap) refuses the whole undo
        // with the disk untouched, never half restored.
        let mut blobs: HashMap<String, Vec<u8>> = HashMap::new();
        for step in &steps {
            let needed: Vec<(&str, PathBuf)> = match &step.restore {
                UndoRestore::Content(hash) => vec![(hash.as_str(), step.target.clone())],
                UndoRestore::Folder(entries) => entries
                    .iter()
                    .filter_map(|entry| match entry {
                        FolderEntry::File { blob, .. } => {
                            Some((blob.as_str(), step.target.join(entry.path())))
                        }
                        FolderEntry::Link { .. } => None,
                    })
                    .collect(),
                UndoRestore::Absent | UndoRestore::Link(_) => Vec::new(),
            };
            for (hash, path) in needed {
                if blobs.contains_key(hash) {
                    continue;
                }
                let bytes = journal.get_blob(hash)?.ok_or_else(|| {
                    ResolveError::Failed(format!(
                        "undo: blob {hash} for {} is missing from the journal",
                        path.display()
                    ))
                })?;
                blobs.insert(hash.to_owned(), bytes);
            }
        }
        let blob = |hash: &str| -> &[u8] { blobs.get(hash).map(Vec::as_slice).unwrap_or_default() };
        let (mut restored, mut removed) = (0usize, 0usize);
        for step in &steps {
            let target = &step.target;
            let mut expected = vec![target.clone()];
            if let UndoRestore::Folder(entries) = &step.restore {
                expected.extend(entries.iter().map(|entry| target.join(entry.path())));
            }
            self.expect_changes(expected)?;
            if step.replace {
                remove_entry(target)?;
            }
            match &step.restore {
                UndoRestore::Content(dest_blob) => {
                    write_creating_parents(target, blob(dest_blob))?;
                    restored += 1;
                }
                // The file was not on disk before the decision: back to that.
                UndoRestore::Absent => {
                    match std::fs::remove_file(target) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                    removed += 1;
                }
                UndoRestore::Link(text) => {
                    link_creating_parents(text, target)?;
                    restored += 1;
                }
                UndoRestore::Folder(entries) => {
                    std::fs::create_dir_all(target)?;
                    for entry in entries {
                        let path = target.join(entry.path());
                        match entry {
                            FolderEntry::File { blob: hash, .. } => {
                                write_creating_parents(&path, blob(hash))?;
                            }
                            FolderEntry::Link { text, .. } => link_creating_parents(text, &path)?,
                        }
                        restored += 1;
                    }
                }
            }
        }
        let session = self.session_start()?;
        self.session_decision(session, serde_json::json!({ "action": "undo", "of": of }))?;
        self.session_end(
            session,
            &format!("undo session {of} — restored {restored} files, removed {removed}"),
        )?;
        self.rescan()?;
        Ok(Some(of))
    }

    /// Commit phase for ONE resolved file: `tomte: <action> <file-name>`.
    fn commit_phase(&self, action: &str, target: &Path) -> (bool, bool, Option<String>) {
        self.commit_with_message(&commit_message(action, target))
    }

    /// Commit phase for a whole batch: ONE commit naming the batch, ONE push,
    /// the same recovery. A batch of one still gets a message a human can
    /// read in `git log`.
    fn commit_batch(&self, targets: &[&Path]) -> (bool, bool, Option<String>) {
        let message = match targets {
            [one] => commit_message("resolve", one),
            many => format!("Resolve {} files via tomte", many.len()),
        };
        self.commit_with_message(&message)
    }

    /// Shared commit phase: when the source repo is left dirty (chezmoi
    /// autoCommit off or failed), stage everything and commit; then push.
    /// Both are best-effort — a locked 1Password failing the signed commit
    /// must not fail the resolution, only report honestly (spec §10).
    ///
    /// The message is the caller's, single file or batch, so the
    /// rejected-push rebase-and-retry recovery below exists exactly ONCE.
    fn commit_with_message(&self, message: &str) -> (bool, bool, Option<String>) {
        let mut note = None;
        let committed = match self.git.dirty_files() {
            // Clean repo: chezmoi autoCommit already committed (or there was
            // nothing to commit) — a preexisting commit counts as committed.
            Ok(dirty) if dirty.is_empty() => true,
            Ok(_) => match self.git.add_all().and_then(|()| self.git.commit(message)) {
                Ok(_sha) => true,
                Err(e) => {
                    tomte_core::log_error!("resolve", "commit failed: {e}");
                    append_note(&mut note, format!("commit failed: {e}"));
                    false
                }
            },
            Err(e) => {
                append_note(&mut note, format!("could not check source repo: {e}"));
                false
            }
        };
        // Push only on top of a successful/preexisting commit.
        let pushed = committed
            && match self.git.push("origin") {
                Ok(()) => true,
                Err(e) if e.to_string().contains("rejected") => {
                    // Origin moved since our last integration: replay our
                    // commit(s) on top and push again (2026-08-10: a clean
                    // auto-merge ended in "couldn't push — conflict").
                    tomte_core::log_warn!(
                        "resolve",
                        "push rejected (origin ahead) — fetching, rebasing, retrying"
                    );
                    self.recover_rejected_push(&mut note)
                }
                Err(e) => {
                    tomte_core::log_error!("resolve", "push failed: {e}");
                    append_note(&mut note, format!("push failed: {e}"));
                    false
                }
            };
        (committed, pushed, note)
    }

    /// Push bounced non-fast-forward: refresh origin, rebase our commits on
    /// top, push once more. A rebase that conflicts is aborted and reported
    /// honestly — repo-level divergence is the user's call, never ours.
    fn recover_rejected_push(&self, note: &mut Option<String>) -> bool {
        if let Err(e) = self.git.fetch("origin") {
            append_note(note, format!("push rejected and re-fetch failed: {e}"));
            return false;
        }
        let branch = self.git.head_branch().unwrap_or_else(|_| "main".into());
        let upstream = format!("origin/{branch}");
        match self.git.rebase(&upstream) {
            Ok(()) => match self.git.push("origin") {
                Ok(()) => {
                    tomte_core::log_info!("resolve", "rebased onto {upstream} and pushed");
                    append_note(
                        note,
                        format!("origin had new commits — rebased onto {upstream} and pushed"),
                    );
                    true
                }
                Err(e) => {
                    tomte_core::log_error!("resolve", "push failed after rebase: {e}");
                    append_note(note, format!("push failed even after rebase: {e}"));
                    false
                }
            },
            Err(e) => {
                self.git.rebase_abort();
                tomte_core::log_error!("resolve", "rebase onto {upstream} conflicted: {e}");
                append_note(
                    note,
                    format!(
                        "origin has diverging commits that conflict with this change — \
                         resolve in the dotfiles repo manually (rebase aborted): {e}"
                    ),
                );
                false
            }
        }
    }

    /// Start the real merge if this file's decision needs one and one is not
    /// already running. Called from the resolve path the moment the user
    /// commits to a decision — never speculatively, so the repo is only ever
    /// put into a merge state in response to an explicit choice.
    ///
    /// The decision has to be about a file that actually needs the merge,
    /// not merely about any file while the repo happens to be behind. A
    /// merge writes conflict markers into EVERY file the two sides disagree
    /// on, so starting one because the user answered a purely local drift
    /// would scribble on sources they never selected. `merge_tree_conflicts`
    /// answers that question in the object database — the working tree, the
    /// index and HEAD are never touched producing it — so the probe itself
    /// can never be the thing that damages the repo.
    ///
    /// Everything it does not positively attribute to THIS file falls back to
    /// the ordinary path (commit, push, and `commit_phase`'s rejected-push
    /// rebase recovery): a clean divergence, which the daemon auto-joins
    /// anyway; a conflict git could not reduce to named files; and a
    /// merge-tree that could not answer at all. None of those is a licence to
    /// put the repo into a merge on this file's behalf.
    ///
    /// Replaces the old separate reconcile flow (2026-08-18: local unpushed
    /// commits + origin commits conflicted on the same file, and EVERY
    /// resolve's push was hostage to it). The mechanism is unchanged — one
    /// abortable merge whose stage 2/3 map onto the editor's disk/source
    /// vocabulary — but a file now enters it from an ordinary review row
    /// instead of a queue behind its own button.
    ///
    /// No fetch of its own: `divergence` reads the `origin/…` ref the daemon
    /// keeps current, and fetching here would mean a decision could act on
    /// commits the user never saw. A divergence that appeared since the last
    /// fetch still gets caught downstream, by `commit_phase`'s rejected-push
    /// recovery.
    /// Catch the source repo up with origin so a decision can actually reach
    /// it, returning whether a CONFLICTED merge is now open.
    ///
    /// Every caller follows this with something that can push. Being behind
    /// origin makes that push non-fast-forward, so there is nothing to decide
    /// here: if we are behind, we integrate. A clean join is silent; a
    /// conflicting one opens a merge whose conflicts are ordinary review rows.
    ///
    /// It deliberately does NOT ask whether *this* file is one of the
    /// conflicts. 0.1.15 did, and skipped the join for everything else — so a
    /// decision ran `chezmoi re-add`, chezmoi's autoCommit made a local commit,
    /// its autoPush was rejected for being behind, and "behind" became
    /// "diverged", breaking every later decision too. Whose file conflicts has
    /// no bearing on whether the repo needs origin's commits.
    pub fn ensure_merge_started(&self) -> Result<bool, ResolveError> {
        if self.git.merge_in_progress() {
            return Ok(true);
        }
        let branch = self.git.head_branch()?;
        let upstream = format!("origin/{branch}");
        if self.git.divergence(&upstream)?.behind == 0 {
            return Ok(false);
        }
        // `git merge` refuses to run over local modifications it would
        // overwrite, so stray source edits become a commit first (the policy
        // the old reconcile entry point carried). Tolerant like
        // `commit_phase`: a commit that cannot be signed is reported, not
        // fatal, and `git merge` still gets its say — it only objects to the
        // files it would actually touch.
        if !self.git.dirty_files()?.is_empty()
            && let Err(e) = self
                .git
                .add_all()
                .and_then(|()| self.git.commit("Update dotfiles (pre-merge)"))
        {
            tomte_core::log_warn!(
                "reconcile",
                "could not commit stray source edits before merging: {e}"
            );
        }
        match self.git.merge(&upstream)? {
            tomte_core::git::MergeStart::Clean => {
                tomte_core::log_info!("reconcile", "joined {upstream} — nothing to answer");
                Ok(false)
            }
            // Expected, and not a failure: these files are review rows now.
            // Each is answerable on its own, in any order; the merge commits
            // when the last one lands.
            tomte_core::git::MergeStart::Conflicts(rels) => {
                tomte_core::log_info!(
                    "reconcile",
                    "joined {upstream} with {} file(s) to answer: {rels:?}",
                    rels.len()
                );
                Ok(true)
            }
        }
    }

    /// A merge with origin that every conflict has been answered for, but
    /// whose merge commit did not happen — the state a failed `merge_commit`
    /// strands the repo in, and the one state nothing else can dig it out of.
    ///
    /// A merge with conflicts LEFT recovers by itself: each conflicted file
    /// is still a review row, and answering the last one commits the merge.
    /// This one has no rows left to answer, so unless something asks the
    /// question the repo sits mid-merge until someone opens a terminal.
    /// A locked 1Password signing key is the everyday cause, so the app
    /// probes this on connect and offers [`Self::finish_merge_if_resolved`].
    pub fn pending_merge_commit(&self) -> Result<bool, ResolveError> {
        if !self.git.merge_in_progress() {
            return Ok(false);
        }
        Ok(self.git.conflicted_files()?.is_empty())
    }

    /// The source paths an abort would roll back to their pre-merge content:
    /// everything the merge has staged, both the conflicts the user answered
    /// and the files git auto-merged. What Cancel actually discards, so it
    /// can be said out loud before it happens.
    pub fn merge_rollback_files(&self) -> Result<Vec<PathBuf>, ResolveError> {
        if !self.git.merge_in_progress() {
            return Ok(Vec::new());
        }
        Ok(self.git.staged_files()?)
    }

    /// Conclude the merge the per-file decisions are resolving, if the one
    /// that just landed was the last. Anything still conflicted keeps the
    /// merge open on purpose: those files are ordinary review rows, each
    /// resolvable on its own, and the last decision commits the merge.
    ///
    /// Also the recovery entry point: called with nothing conflicted and no
    /// decision in hand, it simply commits a merge an earlier failure left
    /// hanging (see [`Self::pending_merge_commit`]).
    pub fn finish_merge_if_resolved(&self) -> Result<MergeConclusion, ResolveError> {
        if !self.git.merge_in_progress() {
            return Ok(MergeConclusion::NotMerging);
        }
        let leftover = self.git.conflicted_files()?;
        if !leftover.is_empty() {
            tomte_core::log_info!(
                "reconcile",
                "{} conflict(s) still unanswered — merge stays open",
                leftover.len()
            );
            return Ok(MergeConclusion::StillConflicted(leftover));
        }
        let (pushed, note) = self.finish_repo_reconcile()?;
        Ok(MergeConclusion::Committed { pushed, note })
    }

    /// [`Self::finish_merge_if_resolved`] rendered as the `(committed,
    /// pushed, note)` triple every resolve outcome reports, so a decision
    /// taken inside a merge says exactly as much about itself as one taken
    /// outside — including when it is only a step towards the commit.
    fn conclude_merge(&self) -> (bool, bool, Option<String>) {
        match self.finish_merge_if_resolved() {
            Ok(MergeConclusion::Committed { pushed, note }) => (true, pushed, note),
            Ok(MergeConclusion::StillConflicted(leftover)) => (
                false,
                false,
                Some(format!(
                    "recorded — {} more file(s) still differ from origin; \
                     resolving them finishes the merge",
                    leftover.len()
                )),
            ),
            // Unreachable in practice (the caller only gets here while a
            // merge is running), but a stale claim of "committed" would be a
            // lie, and this path must not invent one.
            Ok(MergeConclusion::NotMerging) => (
                false,
                false,
                Some("the merge with origin ended before this decision could conclude it".into()),
            ),
            // Typically the merge commit could not be signed. Everything is
            // resolved and staged, so NO review row is left to re-enter the
            // merge through — say that it is still open and finishable, or
            // the repo sits mid-merge in silence.
            Err(e) => {
                tomte_core::log_error!("reconcile", "could not conclude the merge: {e}");
                (
                    false,
                    false,
                    Some(format!(
                        "your answer was recorded, but the merge with origin could not be \
                         committed ({e}) — it is still open; finish it from the dashboard"
                    )),
                )
            }
        }
    }

    /// Merge-editor inputs for `target` when an in-progress merge left its
    /// source file conflicted; `Ok(None)` otherwise. This is how a file
    /// enters the editor's repo-conflict mode now that there is no reconcile
    /// queue: mid-merge the working-tree source is git's conflict-marker
    /// text, so the panes have to come from the index stages rather than
    /// from chezmoi's view of a file git has scribbled on.
    pub fn repo_conflict_for(
        &self,
        target: &Path,
    ) -> Result<Option<std::sync::Arc<MergeInputs>>, ResolveError> {
        if !self.git.merge_in_progress() {
            return Ok(None);
        }
        let source = self.chezmoi.source_path(target)?;
        let rel = self.repo_rel(&source)?;
        if !self.git.conflicted_files()?.contains(&rel) {
            return Ok(None);
        }
        match self.conflict_inputs(vec![rel.clone()])?.pop() {
            Some(inputs) => Ok(Some(inputs)),
            None => Err(ResolveError::Failed(format!(
                "{} is conflicted in the merge but produced no merge inputs",
                rel.display()
            ))),
        }
    }

    /// Whether `source_path` is one of the paths an in-progress merge left
    /// conflicted. Cheap when no merge is running: `merge_in_progress` is a
    /// `rev-parse MERGE_HEAD`, and nothing else runs.
    fn is_mid_merge_conflict(&self, source_path: &Path) -> Result<bool, ResolveError> {
        if !self.git.merge_in_progress() {
            return Ok(false);
        }
        let rel = self.repo_rel(source_path)?;
        Ok(self.git.conflicted_files()?.contains(&rel))
    }

    /// A source path as the source repo names it.
    fn repo_rel(&self, source_path: &Path) -> Result<PathBuf, ResolveError> {
        let source_dir = self.chezmoi.source_dir()?;
        Ok(source_path
            .strip_prefix(&source_dir)
            .unwrap_or(source_path)
            .to_path_buf())
    }

    /// Build merge-editor inputs for mid-merge conflicted paths from the
    /// index stages (1 = ancestor, 2 = local commits, 3 = origin).
    fn conflict_inputs(
        &self,
        rels: Vec<PathBuf>,
    ) -> Result<Vec<std::sync::Arc<MergeInputs>>, ResolveError> {
        let source_dir = self.chezmoi.source_dir()?;
        let mut conflicts = Vec::new();
        {
            for rel in rels {
                let read = |stage: u8| -> String {
                    self.git
                        .stage_blob(stage, &rel)
                        .ok()
                        .flatten()
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default()
                };
                let abs = source_dir.join(&rel);
                // Display identity: the chezmoi target when resolvable,
                // else the repo-relative path (e.g. .chezmoiignore).
                let target = self
                    .chezmoi
                    .target_paths(std::slice::from_ref(&abs))
                    .ok()
                    .and_then(|mut t| t.pop())
                    .unwrap_or_else(|| rel.clone());
                conflicts.push(std::sync::Arc::new(MergeInputs {
                    target,
                    ours: read(2),   // our local commits ("on disk" side)
                    theirs: read(3), // origin's commits ("source" side)
                    base: Some(read(1)),
                    source_path: abs,
                    // Repo-level resolution edits SOURCE TEXT directly —
                    // template text merges as text; no span protection.
                    templated: false,
                    template: None,
                    span_map: None,
                    // Repo-level conflicts already ARE origin's version
                    // (stage 3, `read(3)` above as `theirs`) — a separate
                    // origin pane has nothing to add here.
                    origin: None,
                }));
            }
        }
        Ok(conflicts)
    }

    /// Persist one resolved repo conflict: write the source file, mark it
    /// resolved in the index. The merge concludes in
    /// [`Self::finish_repo_reconcile`] once every conflict is resolved.
    pub fn resolve_repo_conflict(
        &self,
        source_path: &Path,
        resolved: &str,
    ) -> Result<(), ResolveError> {
        tomte_core::log_info!("reconcile", "resolved {}", source_path.display());
        std::fs::write(source_path, resolved)?;
        self.stage_resolution(source_path)
    }

    /// Tell git one conflicted path is answered, the file already holding the
    /// answer. Split out of [`Self::resolve_repo_conflict`] for `keep_disk`,
    /// whose answer is written by `chezmoi re-add` rather than by us.
    fn stage_resolution(&self, source_path: &Path) -> Result<(), ResolveError> {
        let rel = self.repo_rel(source_path)?;
        self.git.add_path(&rel)?;
        Ok(())
    }

    /// Conclude the merge (all conflicts resolved), push, rescan.
    pub fn finish_repo_reconcile(&self) -> Result<(bool, Option<String>), ResolveError> {
        let leftover = self.git.conflicted_files()?;
        if !leftover.is_empty() {
            return Err(ResolveError::Failed(format!(
                "still conflicted: {leftover:?}"
            )));
        }
        self.git.merge_commit()?;
        tomte_core::log_info!("reconcile", "merge committed");
        let mut note = None;
        let pushed = match self.git.push("origin") {
            Ok(()) => true,
            Err(e) => {
                tomte_core::log_error!("reconcile", "push failed: {e}");
                append_note(&mut note, format!("reconciled, but push failed: {e}"));
                false
            }
        };
        self.rescan()?;
        Ok((pushed, note))
    }

    /// Bail out of an in-progress reconcile; the repo returns to pre-merge.
    ///
    /// IDEMPOTENT, THEN HONEST — in that order, and they are different
    /// promises:
    ///
    /// - **Nothing to abort is not a failure.** Cancel is reachable when no
    ///   merge is running (the editor is open on a file the merge already
    ///   concluded, a second Discard lands behind the first), and `git merge
    ///   --abort` answers that with a hard error. Saying `Ok(())` for a repo
    ///   that is ALREADY where the caller wants it is the truthful answer,
    ///   and it is what keeps a doubled press from producing a red banner
    ///   about a merge that is simply gone.
    /// - **A merge that refuses to roll back IS a failure.** This used to be
    ///   unreportable: `merge_abort` swallowed its result, so every caller
    ///   was told the roll-back happened. An `index.lock` held past
    ///   `GitClient::run`'s retries (the contention `85fec09` met) leaves the
    ///   merge open with the user's answers still staged in it, and the one
    ///   thing they must not be told is that the repo is back where it
    ///   started.
    pub fn abort_repo_reconcile(&self) -> Result<(), ResolveError> {
        if !self.git.merge_in_progress() {
            tomte_core::log_info!("reconcile", "nothing to abort: no merge is in progress");
            return Ok(());
        }
        if let Err(e) = self.git.merge_abort() {
            tomte_core::log_error!("reconcile", "the merge could not be rolled back: {e}");
            return Err(e.into());
        }
        tomte_core::log_info!("reconcile", "aborted");
        Ok(())
    }

    /// Ask the daemon to rescan. Every mutating path here already ends with
    /// one; this exposes it for the paths that rewrite the source repo
    /// WITHOUT going through them — an abort, above all, whose whole effect
    /// is to make the daemon's last scan describe a repo that is gone.
    pub fn request_rescan(&self) -> Result<(), ResolveError> {
        self.rescan()
    }

    fn session_start(&self) -> Result<i64, ResolveError> {
        match self.request_patiently(Request::SessionStart { ts: now_ts() }, "session start")? {
            Response::SessionStarted { session } => Ok(session),
            other => Err(unexpected("session start", other)),
        }
    }

    /// One daemon request, retried across busy bounces for up to
    /// [`Self::daemon_patience`] ([`IpcClient::request_patiently`]). A busy
    /// budget that runs out is [`IpcError::Busy`], which the batch reads to
    /// stop asking a stuck daemon once per file.
    fn request_patiently(
        &self,
        request: Request,
        what: &'static str,
    ) -> Result<Response, ResolveError> {
        Ok(self
            .ipc
            .request_patiently(request, self.daemon_patience, what)?)
    }
    fn session_decision(
        &self,
        session: i64,
        decision: serde_json::Value,
    ) -> Result<(), ResolveError> {
        self.ipc_ok(
            Request::SessionDecision { session, decision },
            "session decision",
        )
    }

    fn session_end(&self, session: i64, summary: &str) -> Result<(), ResolveError> {
        self.ipc_ok(
            Request::SessionEnd {
                session,
                ts: now_ts(),
                summary: summary.to_string(),
            },
            "session end",
        )
    }

    fn snapshot_blobs(&self, paths: Vec<PathBuf>) -> Result<Vec<String>, ResolveError> {
        match self.request_patiently(Request::SnapshotBlobs { paths }, "snapshot blobs")? {
            Response::Blobs { hashes } => Ok(hashes),
            other => Err(unexpected("snapshot blobs", other)),
        }
    }

    fn expect_changes(&self, paths: Vec<PathBuf>) -> Result<(), ResolveError> {
        self.ipc_ok(
            Request::ExpectChanges {
                paths,
                ttl_secs: EXPECT_TTL_SECS,
            },
            "expect changes",
        )
    }

    fn rescan(&self) -> Result<(), ResolveError> {
        self.ipc_ok(Request::Rescan, "rescan")
    }

    fn ipc_ok(&self, request: Request, what: &'static str) -> Result<(), ResolveError> {
        match self.request_patiently(request, what)? {
            Response::Ok => Ok(()),
            other => Err(unexpected(what, other)),
        }
    }
}

/// The batch's answer for a file it never asked the daemon about, because an
/// earlier file already exhausted the busy budget: nothing happened to it, so
/// trying again is exactly right.
fn not_attempted(why: &str) -> ResolveError {
    ResolveError::Failed(format!("not attempted — {why} — try again"))
}

/// What a batch remembers about a stuck daemon: the busy error's own words
/// WITHOUT its "try again". Each row adds the guidance that is true for it —
/// a file nothing happened to may be tried again ([`not_attempted`]); a file
/// whose work landed must not be ([`close_landed`]).
fn stuck_phrase(e: &ResolveError) -> Option<String> {
    match e {
        ResolveError::Ipc(IpcError::Busy {
            waited_secs,
            message,
        }) => Some(format!(
            "the daemon stayed busy for {waited_secs:.1}s ({message})"
        )),
        _ => None,
    }
}

/// Phase 3 for one file of the batch's tail. `landed` is its group's result
/// (its `re-add`/write-back/`apply` ran, or failed as a group); `end` closes
/// its journal session. The one thing this must never do is tell the user to
/// try again a file whose work has already landed: whatever goes wrong with
/// the close — the daemon known stuck already (`stuck`, then `end` is not even
/// called), or `end` itself failing — the row says the file is resolved and
/// only Undo is unavailable for it. A close that exhausted the busy budget
/// sets `stuck` for the files after it.
fn close_landed(
    session: i64,
    landed: Result<(bool, bool, Option<String>), String>,
    end: impl FnOnce() -> Result<(), ResolveError>,
    stuck: &mut Option<String>,
) -> Result<ResolveOutcome, ResolveError> {
    let (committed, pushed, note) = match landed {
        Ok(landed) => landed,
        Err(msg) => return Err(ResolveError::Failed(msg)),
    };
    let unclosed = |why: &str| {
        ResolveError::Failed(format!(
            "resolved, but its journal session was not closed (Undo unavailable for it): {why}"
        ))
    };
    if let Some(why) = stuck.as_deref() {
        return Err(unclosed(why));
    }
    match end() {
        Ok(()) => Ok(ResolveOutcome::Done {
            session,
            committed,
            pushed,
            note,
        }),
        Err(e) => {
            let why = stuck_phrase(&e);
            if let Some(why) = &why {
                *stuck = Some(why.clone());
            }
            Err(unclosed(&why.unwrap_or_else(|| e.to_string())))
        }
    }
}

fn unexpected(what: &str, response: Response) -> ResolveError {
    match response {
        Response::Error { message } => ResolveError::Failed(format!("{what}: {message}")),
        other => ResolveError::Failed(format!("{what}: unexpected reply {other:?}")),
    }
}

/// Outcome of the in-memory templated write-back attempt: the verified new
/// template text, or the human-readable detail of a semantic rejection.
#[derive(Debug, PartialEq, Eq)]
enum WriteBackAttempt {
    Verified(String),
    Rejected(String),
}

/// WRITE-AFTER-VERIFY core: map the resolved text back into the template via
/// `write_back` (in memory), then prove the new template re-renders to
/// exactly `resolved` via `chezmoi execute-template`. Only a `Verified`
/// template may be persisted. Every write-back placement failure
/// (protected span, repeated literal, unplaceable edit) and a render
/// mismatch are semantic rejections; a chezmoi failure during verification
/// is a real error.
fn write_back_verified(
    chezmoi: &ChezmoiClient,
    template: &str,
    span_map: &SpanMap,
    theirs: &str,
    resolved: &str,
) -> Result<WriteBackAttempt, ResolveError> {
    let new_template = match write_back(template, span_map, theirs, resolved) {
        Ok(t) => t,
        Err(e) => return Ok(WriteBackAttempt::Rejected(e.to_string())),
    };
    match verify_write_back(chezmoi, &new_template, resolved) {
        Ok(()) => Ok(WriteBackAttempt::Verified(new_template)),
        Err(e @ VerifyError::Mismatch { .. }) => Ok(WriteBackAttempt::Rejected(e.to_string())),
        Err(VerifyError::Chezmoi(e)) => Err(ResolveError::Chezmoi(e)),
    }
}

/// Whether a source path is a chezmoi template (`.tmpl` extension). A
/// templated source cannot be re-added — `chezmoi re-add` silently ignores
/// them — so a one-click keep-disk on one goes through the template
/// write-back instead of `re-add`.
pub fn is_templated(source_path: &Path) -> bool {
    source_path.extension().is_some_and(|ext| ext == "tmpl")
}

/// The source's version of one managed file, as `chezmoi apply` would write
/// it: the raw source bytes, or `chezmoi cat` when those bytes are not what
/// lands on disk (a template, an encrypted file, a `modify_` script).
pub fn source_version(
    chezmoi: &ChezmoiClient,
    source_file: &Path,
    target_file: &Path,
) -> Result<Vec<u8>, ResolveError> {
    let name = source_file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    // chezmoi's file prefixes come in a fixed order: `create_` or `modify_`
    // first, then `encrypted_`.
    let after_create = name.strip_prefix("create_").unwrap_or(&name);
    if is_templated(source_file)
        || name.starts_with("modify_")
        || after_create.starts_with("encrypted_")
    {
        return Ok(chezmoi.cat(target_file)?);
    }
    Ok(std::fs::read(source_file)?)
}

/// How the folder the source holds at `target` compares with what the disk
/// holds there now (followed through a link): every managed file under it,
/// hashed from [`source_version`], against the disk.
pub fn folder_comparison(
    chezmoi: &ChezmoiClient,
    target: &Path,
) -> Result<FolderComparison, ResolveError> {
    let files = chezmoi.managed_under(target, Some("files"))?;
    let sources = chezmoi.source_paths(&files)?;
    let mut hashed = Vec::with_capacity(files.len());
    for (file, source) in files.iter().zip(sources) {
        let source = source.ok_or_else(|| {
            ResolveError::Failed(format!("{} has no source path", file.display()))
        })?;
        let rel = file.strip_prefix(target).unwrap_or(file).to_path_buf();
        hashed.push((
            rel,
            ContentHash::of(&source_version(chezmoi, &source, file)?),
        ));
    }
    Ok(compare_folder(&hashed, target)?)
}

/// What the source says `target` is, read from its source path: a
/// `symlink_` file is a link (its text rendered by chezmoi), a directory a
/// folder, anything else a file.
fn source_kind(
    chezmoi: &ChezmoiClient,
    target: &Path,
    source: &Path,
) -> Result<EntryKind, ResolveError> {
    let is_link = source
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("symlink_"));
    if is_link {
        return Ok(EntryKind::Link(chezmoi.link_text(target)?));
    }
    if std::fs::symlink_metadata(source)?.is_dir() {
        return Ok(EntryKind::Folder);
    }
    Ok(EntryKind::File)
}

fn kind_word(kind: &EntryKind) -> &'static str {
    match kind {
        EntryKind::Absent => "absent",
        EntryKind::File => "file",
        EntryKind::Folder => "folder",
        EntryKind::Link(_) => "link",
    }
}

/// Keep source's refusal for a source path the merge left conflicted, named
/// by its path in the source repo.
fn conflict_refusal(rel: &Path) -> ResolveError {
    ResolveError::Failed(format!(
        "{} is mid-merge with origin — its source file holds conflict markers; \
         resolve the origin difference in the merge editor first",
        rel.display()
    ))
}

/// Keep source on a type change deletes whatever the disk holds at
/// `target`, and Undo can only bring back what the journal stored. A file
/// over [`BLOB_CAP`] is journaled by hash alone, so a decision that would
/// delete one refuses before anything is journaled or touched, naming the
/// file and its size.
fn refuse_unrestorable(target: &Path, change: &TypeChange) -> Result<(), ResolveError> {
    let files: Vec<PathBuf> = match change.disk {
        EntryKind::File => vec![target.to_path_buf()],
        EntryKind::Folder => walk_folder(target)?
            .into_iter()
            .filter(|(_, kind)| *kind == Walked::File)
            .map(|(rel, _)| target.join(rel))
            .collect(),
        EntryKind::Absent | EntryKind::Link(_) => Vec::new(),
    };
    for file in files {
        let size = std::fs::symlink_metadata(&file)?.len();
        if size > BLOB_CAP as u64 {
            return Err(ResolveError::Failed(format!(
                "{} is {:.1} MB, over the {} MB Undo can bring back — restoring would delete \
                 it for good; move it out of {} first",
                file.display(),
                size as f64 / (1024.0 * 1024.0),
                BLOB_CAP / (1024 * 1024),
                display_name(target)
            )));
        }
    }
    Ok(())
}

fn copy_picks_refused() -> ResolveError {
    ResolveError::Failed("copy picks apply only to a folder replaced by a link".into())
}

/// Copies are only for keeping a link that replaced a folder; any other
/// decision or change carrying them is refused before anything happens.
fn refuse_stray_copies(
    decision: BatchDecision,
    change: Option<&TypeChange>,
    copies: &[PathBuf],
) -> Result<(), ResolveError> {
    let link_over_folder = matches!(
        change.map(|c| (&c.source, &c.disk)),
        Some((EntryKind::Folder, EntryKind::Link(_)))
    );
    if copies.is_empty() || (decision == BatchDecision::KeepDisk && link_over_folder) {
        Ok(())
    } else {
        Err(copy_picks_refused())
    }
}

/// One path found by [`walk_folder`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Walked {
    Folder,
    File,
    Link(String),
}

/// Every path under `root`, relative to it, sorted, parents before their
/// children. Links are read, never followed.
fn walk_folder(root: &Path) -> std::io::Result<Vec<(PathBuf, Walked)>> {
    fn walk(root: &Path, rel: &Path, out: &mut Vec<(PathBuf, Walked)>) -> std::io::Result<()> {
        let mut items: Vec<_> = std::fs::read_dir(root.join(rel))?.collect::<Result<_, _>>()?;
        items.sort_by_key(|item| item.file_name());
        for item in items {
            let path = rel.join(item.file_name());
            let kind = item.file_type()?;
            if kind.is_symlink() {
                let text = std::fs::read_link(item.path())?;
                out.push((path, Walked::Link(text.to_string_lossy().into_owned())));
            } else if kind.is_dir() {
                out.push((path.clone(), Walked::Folder));
                walk(root, &path, out)?;
            } else {
                out.push((path, Walked::File));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, Path::new(""), &mut out)?;
    Ok(out)
}

/// Remove whatever is at `path`, a folder with everything in it; nothing
/// there is fine. A link is removed, never followed.
fn remove_entry(path: &Path) -> std::io::Result<()> {
    match disk_kind(path)? {
        EntryKind::Absent => Ok(()),
        EntryKind::Folder => std::fs::remove_dir_all(path),
        EntryKind::File | EntryKind::Link(_) => std::fs::remove_file(path),
    }
}

/// Move a file or folder, copying when the two sit on different volumes.
fn move_entry(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            copy_entry(from, to)?;
            remove_entry(from)
        }
        other => other,
    }
}

fn copy_entry(from: &Path, to: &Path) -> std::io::Result<()> {
    if !std::fs::symlink_metadata(from)?.is_dir() {
        return std::fs::copy(from, to).map(|_| ());
    }
    std::fs::create_dir_all(to)?;
    for item in std::fs::read_dir(from)? {
        let item = item?;
        copy_entry(&item.path(), &to.join(item.file_name()))?;
    }
    Ok(())
}

fn write_creating_parents(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

fn link_creating_parents(text: &str, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::os::unix::fs::symlink(text, path)
}

/// The target is gone from disk. `symlink_metadata`, so a dangling symlink
/// still counts as present: that is a link to re-add, not a deletion.
fn is_absent(target: &Path) -> bool {
    matches!(std::fs::symlink_metadata(target), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Message for the engine's fallback source-repo commit:
/// `tomte: <action> <file-name>`.
pub fn commit_message(action: &str, target: &Path) -> String {
    format!("tomte: {action} {}", display_name(target))
}

fn display_name(target: &Path) -> String {
    target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.to_string_lossy().into_owned())
}

/// Decision JSON journaled with each resolution. Carries the blob hashes
/// returned by SnapshotBlobs so undo can find them:
/// `{action, target, dest_blob, source_blob?}`. `dest_blob` is `null` when
/// the target was not on disk at decision time (see
/// [`ResolveEngine::snapshot_dest`]): the key is still present, so
/// `session_landed` counts the decision, and undo reads it as "remove".
fn build_decision(
    action: &str,
    target: &Path,
    dest_blob: Option<&str>,
    source_blob: Option<&str>,
) -> serde_json::Value {
    let mut decision = serde_json::json!({
        "action": action,
        "target": target.to_string_lossy(),
        "dest_blob": dest_blob,
    });
    if let (Some(map), Some(source)) = (decision.as_object_mut(), source_blob) {
        map.insert(
            "source_blob".to_string(),
            serde_json::Value::String(source.to_string()),
        );
    }
    decision
}

/// A type-change decision: `dest_entry` is what was on disk (see
/// [`ResolveEngine::snapshot_entry`]), `applied` the paths a restored folder
/// receives. `dest_blob: null` is still written so `session_landed` counts
/// the decision; undo prefers `dest_entry`.
fn build_entry_decision(
    action: &str,
    target: &Path,
    dest_entry: serde_json::Value,
    applied: Option<&[PathBuf]>,
) -> serde_json::Value {
    let mut decision = build_decision(action, target, None, None);
    if let Some(map) = decision.as_object_mut() {
        map.insert("dest_entry".to_string(), dest_entry);
        if let Some(applied) = applied {
            let paths: Vec<String> = applied
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            map.insert("applied".to_string(), serde_json::json!(paths));
        }
    }
    decision
}

/// How many finished sessions `undo_last` walks back looking for one that
/// landed. Rejections come one per refused attempt, so a handful is plenty;
/// the bound keeps a pathological journal from being read whole.
const UNDO_LOOKBACK: usize = 64;

/// Whether a finished session's decisions describe something that was
/// actually done. A `<action>_rejected` decision is the journaled record of
/// a refused write-back — the attempt, then the refusal, nothing mutated for
/// that file — so a session is skipped only when EVERY file it decided was
/// refused. A session that refused one file and landed another still landed;
/// one with no rejection at all (an undo session included) is taken as is.
fn session_landed(decisions: &serde_json::Value) -> bool {
    let Some(arr) = decisions.as_array() else {
        return true;
    };
    let rejected: std::collections::BTreeSet<&str> = arr
        .iter()
        .filter(|d| decision_action(d).is_some_and(|a| a.ends_with("_rejected")))
        .filter_map(decision_target)
        .collect();
    if rejected.is_empty() {
        return true;
    }
    arr.iter().any(|d| {
        d.get("dest_blob").is_some() && decision_target(d).is_some_and(|t| !rejected.contains(t))
    })
}

fn decision_action(d: &serde_json::Value) -> Option<&str> {
    d.get("action").and_then(|a| a.as_str())
}

fn decision_target(d: &serde_json::Value) -> Option<&str> {
    d.get("target").and_then(|t| t.as_str())
}

/// What undo does to one target: put its journaled content back, or remove
/// it — a `keep_source` on a file that was not on disk journals
/// `dest_blob: null`, and the only way back is to delete what `apply` made.
/// A type change puts back the link or the folder that was there.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UndoRestore {
    Content(String),
    Absent,
    Link(String),
    Folder(Vec<FolderEntry>),
}

/// One file or link inside a journaled folder, by path relative to it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FolderEntry {
    File { path: PathBuf, blob: String },
    Link { path: PathBuf, text: String },
}

impl FolderEntry {
    fn path(&self) -> &Path {
        match self {
            Self::File { path, .. } | Self::Link { path, .. } => path,
        }
    }
}

/// One target's undo.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UndoStep {
    target: PathBuf,
    restore: UndoRestore,
    /// The decision journaled a `dest_entry` (a type change): whatever is at
    /// the target now is removed before the entry is written back. A folder
    /// is removed only when every path in it is in `applied`.
    replace: bool,
    /// The paths, relative to the target, that the decision's apply wrote
    /// into a folder.
    applied: Option<BTreeSet<PathBuf>>,
}

/// Extract the undo steps from a session's decisions array. A `dest_entry`
/// is preferred over `dest_blob`. Decisions with neither, or without a
/// target (e.g. `sync_all`, `undo`), restore nothing; a `dest_blob` of
/// `null` is [`UndoRestore::Absent`].
fn parse_undo_restores(decisions: &serde_json::Value) -> Vec<UndoStep> {
    decisions
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|decision| {
                    let target = PathBuf::from(decision.get("target")?.as_str()?);
                    if let Some(entry) = decision.get("dest_entry") {
                        let applied =
                            decision
                                .get("applied")
                                .and_then(|a| a.as_array())
                                .map(|paths| {
                                    paths
                                        .iter()
                                        .filter_map(|p| p.as_str())
                                        .map(PathBuf::from)
                                        .collect()
                                });
                        return Some(UndoStep {
                            target,
                            restore: entry_restore(entry)?,
                            replace: true,
                            applied,
                        });
                    }
                    let restore = match decision.get("dest_blob")? {
                        serde_json::Value::Null => UndoRestore::Absent,
                        blob => UndoRestore::Content(blob.as_str()?.to_string()),
                    };
                    Some(UndoStep {
                        target,
                        restore,
                        replace: false,
                        applied: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A journaled `dest_entry` as the restore it asks for.
fn entry_restore(entry: &serde_json::Value) -> Option<UndoRestore> {
    let text = |value: &serde_json::Value, key: &str| -> Option<String> {
        Some(value.get(key)?.as_str()?.to_string())
    };
    match entry.get("kind")?.as_str()? {
        "absent" => Some(UndoRestore::Absent),
        "file" => Some(UndoRestore::Content(text(entry, "blob")?)),
        "link" => Some(UndoRestore::Link(text(entry, "text")?)),
        "folder" => entry
            .get("entries")?
            .as_array()?
            .iter()
            .map(|item| {
                let path = PathBuf::from(text(item, "path")?);
                match (text(item, "blob"), text(item, "link")) {
                    (Some(blob), _) => Some(FolderEntry::File { path, blob }),
                    (None, Some(text)) => Some(FolderEntry::Link { path, text }),
                    (None, None) => None,
                }
            })
            .collect::<Option<Vec<_>>>()
            .map(UndoRestore::Folder),
        _ => None,
    }
}

/// Refuse to undo over a folder that holds something the decision's apply
/// did not write: removing it would delete the user's own later file.
fn refuse_foreign_folder(step: &UndoStep) -> Result<(), ResolveError> {
    if disk_kind(&step.target)? != EntryKind::Folder {
        return Ok(());
    }
    let Some(applied) = &step.applied else {
        return Err(ResolveError::Failed(format!(
            "{} is a folder Tomte did not write; move it away and undo again",
            step.target.display()
        )));
    };
    match walk_folder(&step.target)?
        .into_iter()
        .find(|(rel, _)| !applied.contains(rel))
    {
        Some((rel, _)) => Err(ResolveError::Failed(format!(
            "{}/{} was added after the decision; move it away and undo again",
            step.target.display(),
            rel.display()
        ))),
        None => Ok(()),
    }
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn append_note(note: &mut Option<String>, message: String) {
    match note {
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(&message);
        }
        None => *note = Some(message),
    }
}

#[cfg(test)]
mod tests {
    use tomte_core::chezmoi::ChezmoiOptions;
    use tomte_core::cmd::fake::FakeRunner;
    use tomte_core::template::{anchor::anchor, lexer::lex};

    use super::*;

    /// A templated line plus a literal line: edits to `editor` are legal,
    /// edits to the rendered `email` value touch a protected span.
    const TMPL: &str = "email = {{ .email }}\neditor = hx\n";
    const RENDERED: &str = "email = a@b.c\neditor = hx\n";

    fn span_map() -> SpanMap {
        anchor(TMPL, &lex(TMPL).unwrap(), RENDERED)
    }

    fn fake_chezmoi() -> (Arc<FakeRunner>, ChezmoiClient) {
        let fake = Arc::new(FakeRunner::new());
        let client = ChezmoiClient::new(fake.clone(), ChezmoiOptions::default());
        (fake, client)
    }

    #[test]
    fn write_back_verified_literal_edit_verifies_and_returns_new_template() {
        let resolved = "email = a@b.c\neditor = nvim\n";
        let (fake, chezmoi) = fake_chezmoi();
        // execute-template renders the NEW template to exactly `resolved`.
        fake.push_ok(0, resolved, "");
        let attempt = write_back_verified(&chezmoi, TMPL, &span_map(), RENDERED, resolved).unwrap();
        assert_eq!(
            attempt,
            WriteBackAttempt::Verified("email = {{ .email }}\neditor = nvim\n".into())
        );
        // The verification ran execute-template with the new template on stdin.
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].args.contains(&"execute-template".to_string()));
        assert_eq!(
            calls[0].stdin.as_deref(),
            Some("email = {{ .email }}\neditor = nvim\n".as_bytes())
        );
    }

    #[test]
    fn write_back_verified_protected_touch_rejects_before_any_subprocess() {
        // Editing the rendered value of {{ .email }} touches a protected span.
        let resolved = "email = x@y.z\neditor = hx\n";
        let (fake, chezmoi) = fake_chezmoi();
        let attempt = write_back_verified(&chezmoi, TMPL, &span_map(), RENDERED, resolved).unwrap();
        let WriteBackAttempt::Rejected(detail) = attempt else {
            panic!("expected Rejected, got {attempt:?}");
        };
        assert!(detail.contains("protected"), "detail: {detail}");
        assert!(
            fake.calls().is_empty(),
            "a placement rejection must not spawn chezmoi"
        );
    }

    #[test]
    fn write_back_verified_repeated_literal_rejects() {
        let tmpl = "{{ range .shells }}alias {{ . }}\n{{ end }}";
        let rendered = "alias zsh\nalias nu\n";
        let resolved = "alia zsh\nalias nu\n";
        let map = anchor(tmpl, &lex(tmpl).unwrap(), rendered);
        let (fake, chezmoi) = fake_chezmoi();
        let attempt = write_back_verified(&chezmoi, tmpl, &map, rendered, resolved).unwrap();
        assert!(
            matches!(attempt, WriteBackAttempt::Rejected(_)),
            "got {attempt:?}"
        );
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn write_back_verified_render_mismatch_rejects() {
        let resolved = "email = a@b.c\neditor = nvim\n";
        let (fake, chezmoi) = fake_chezmoi();
        // The re-render disagrees with the resolved text → semantic rejection.
        fake.push_ok(0, "email = SOMETHING ELSE\neditor = nvim\n", "");
        let attempt = write_back_verified(&chezmoi, TMPL, &span_map(), RENDERED, resolved).unwrap();
        let WriteBackAttempt::Rejected(detail) = attempt else {
            panic!("expected Rejected, got {attempt:?}");
        };
        assert!(detail.contains("does not match"), "detail: {detail}");
    }

    #[test]
    fn write_back_verified_chezmoi_failure_is_an_error_not_a_rejection() {
        let resolved = "email = a@b.c\neditor = nvim\n";
        let (fake, chezmoi) = fake_chezmoi();
        fake.push_ok(1, "", "boom");
        let err = write_back_verified(&chezmoi, TMPL, &span_map(), RENDERED, resolved).unwrap_err();
        assert!(matches!(err, ResolveError::Chezmoi(_)), "got {err:?}");
    }

    #[test]
    fn is_templated_detects_tmpl_extension_only() {
        assert!(is_templated(Path::new("/src/dot_zshrc.tmpl")));
        assert!(is_templated(Path::new("/src/dot_config/env.nu.tmpl")));
        assert!(!is_templated(Path::new("/src/dot_zshrc")));
        assert!(!is_templated(Path::new("/src/settings.json")));
        // ".tmpl" alone is a hidden file with no extension in Path's model —
        // chezmoi never produces it, and it must not count as a template.
        assert!(!is_templated(Path::new("/src/.tmpl")));
    }

    #[test]
    fn commit_message_uses_action_and_file_name() {
        assert_eq!(
            commit_message("keep_disk", Path::new("/Users/x/.zshrc")),
            "tomte: keep_disk .zshrc"
        );
        assert_eq!(
            commit_message("keep_disk", Path::new("/Users/x/.config/starship.toml")),
            "tomte: keep_disk starship.toml"
        );
    }

    #[test]
    fn commit_message_falls_back_to_full_path_without_file_name() {
        assert_eq!(
            commit_message("keep_source", Path::new("/")),
            "tomte: keep_source /"
        );
    }

    #[test]
    fn decision_json_roundtrips_through_undo_parsing() {
        let with_source = build_decision(
            "keep_disk",
            Path::new("/Users/x/.zshrc"),
            Some("abc123"),
            Some("def456"),
        );
        assert_eq!(with_source["action"], "keep_disk");
        assert_eq!(with_source["target"], "/Users/x/.zshrc");
        assert_eq!(with_source["dest_blob"], "abc123");
        assert_eq!(with_source["source_blob"], "def456");

        let without_source = build_decision(
            "keep_source",
            Path::new("/Users/x/.testrc"),
            Some("abc123"),
            None,
        );
        assert!(without_source.get("source_blob").is_none());
        // Absent on disk at decision time: the key is there (it landed), the
        // value is null (undo removes). MUTATION: omit the key for `None` →
        // `session_landed` no longer counts it and the restore list below
        // lacks the removal.
        let absent = build_decision("keep_source", Path::new("/Users/x/.newrc"), None, None);
        assert!(absent["dest_blob"].is_null());
        assert!(session_landed(&serde_json::json!([
            absent.clone(),
            { "action": "keep_disk_rejected", "target": "/Users/x/.other" },
        ])));

        let decisions = serde_json::json!([
            with_source,
            without_source,
            absent,
            { "action": "sync_all" },
        ]);
        let restores: Vec<(PathBuf, UndoRestore)> = parse_undo_restores(&decisions)
            .into_iter()
            .map(|step| (step.target, step.restore))
            .collect();
        assert_eq!(
            restores,
            vec![
                (
                    PathBuf::from("/Users/x/.zshrc"),
                    UndoRestore::Content("abc123".to_string())
                ),
                (
                    PathBuf::from("/Users/x/.testrc"),
                    UndoRestore::Content("abc123".to_string())
                ),
                (PathBuf::from("/Users/x/.newrc"), UndoRestore::Absent),
            ]
        );
    }

    /// A type change's `dest_entry` survives the journal round trip: a link,
    /// a folder of files and links, and `applied`.
    ///
    /// MUTATION: ignore `dest_entry` in `parse_undo_restores` (read
    /// `dest_blob` only) → both decisions parse as `Absent`, not replacing.
    #[test]
    fn decision_json_roundtrips_link_and_folder_entries() {
        let link = build_entry_decision(
            "keep_source",
            Path::new("/Users/x/.d"),
            serde_json::json!({ "kind": "link", "text": ".real" }),
            Some(&[
                PathBuf::from("one"),
                PathBuf::from("sub"),
                PathBuf::from("sub/b"),
            ]),
        );
        assert!(link["dest_blob"].is_null(), "still counted as landed");
        assert!(session_landed(&serde_json::json!([
            link.clone(),
            { "action": "keep_disk_rejected", "target": "/Users/x/.other" },
        ])));
        let folder = build_entry_decision(
            "keep_source",
            Path::new("/Users/x/.l"),
            serde_json::json!({ "kind": "folder", "entries": [
                { "path": "a", "blob": "h1" },
                { "path": "sub/c", "link": "x" },
            ] }),
            None,
        );
        assert!(folder.get("applied").is_none());
        let steps = parse_undo_restores(&serde_json::json!([link, folder]));
        assert_eq!(
            steps,
            vec![
                UndoStep {
                    target: PathBuf::from("/Users/x/.d"),
                    restore: UndoRestore::Link(".real".into()),
                    replace: true,
                    applied: Some(
                        ["one", "sub", "sub/b"]
                            .into_iter()
                            .map(PathBuf::from)
                            .collect()
                    ),
                },
                UndoStep {
                    target: PathBuf::from("/Users/x/.l"),
                    restore: UndoRestore::Folder(vec![
                        FolderEntry::File {
                            path: PathBuf::from("a"),
                            blob: "h1".into()
                        },
                        FolderEntry::Link {
                            path: PathBuf::from("sub/c"),
                            text: "x".into()
                        },
                    ]),
                    replace: true,
                    applied: None,
                },
            ]
        );
    }

    /// MUTATION: make `session_landed` return `true` for a rejected session
    /// (drop the `_rejected` check) → the second and third assertions fail;
    /// make it `false` for ANY session with a rejection → the mixed one fails.
    #[test]
    fn session_landed_is_false_for_a_rejected_attempt_and_true_otherwise() {
        assert!(session_landed(&serde_json::json!([
            { "action": "keep_disk", "target": "/a", "dest_blob": "h" }
        ])));
        // Refused one file, landed another: the session landed.
        assert!(session_landed(&serde_json::json!([
            { "action": "keep_disk", "target": "/a", "dest_blob": "h" },
            { "action": "keep_disk_rejected", "target": "/a", "detail": "protected" },
            { "action": "keep_disk", "target": "/b", "dest_blob": "k" }
        ])));
        assert!(!session_landed(&serde_json::json!([
            { "action": "keep_disk", "target": "/a", "dest_blob": "h" },
            { "action": "keep_disk_rejected", "target": "/a", "detail": "protected" }
        ])));
        assert!(!session_landed(&serde_json::json!([
            { "action": "merge", "target": "/a", "dest_blob": "h" },
            { "action": "merge_rejected", "target": "/a", "detail": "protected" }
        ])));
        // An undo session and a malformed one are not rejections.
        assert!(session_landed(
            &serde_json::json!([{ "action": "undo", "of": 3 }])
        ));
        assert!(session_landed(&serde_json::json!({"not": "array"})));
    }

    fn busy() -> ResolveError {
        ResolveError::Ipc(IpcError::Busy {
            waited_secs: 90.0,
            message: tomte_proto::BUSY_REPLY.into(),
        })
    }

    /// A landed file is never told "try again": the close failing, or being
    /// skipped because the daemon is already known stuck, both read as
    /// resolved-with-Undo-unavailable, and only a busy close sets `stuck`.
    /// MUTATION: return `Err(e)` unchanged from the `end` failure → the
    /// second case fails; drop the `stuck` early return → the third case
    /// panics inside `end`; set `stuck` on every error → the fourth fails.
    #[test]
    fn close_landed_never_asks_the_user_to_redo_landed_work() {
        let landed = || Ok((true, true, None));
        let mut stuck = None;
        let ok = close_landed(7, landed(), || Ok(()), &mut stuck).unwrap();
        assert!(matches!(
            ok,
            ResolveOutcome::Done {
                session: 7,
                committed: true,
                pushed: true,
                ..
            }
        ));
        assert!(stuck.is_none());

        let mut stuck = None;
        let err = close_landed(7, landed(), || Err(busy()), &mut stuck).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("resolved, but its journal session was not closed"),
            "{err}"
        );
        assert!(err.to_string().contains("stayed busy"), "{err}");
        assert!(
            stuck.is_some(),
            "a busy close marks the daemon stuck for the files after it"
        );
        assert!(
            !err.to_string().contains("try again"),
            "landed work is never retried: {err}"
        );
        assert!(!stuck.as_deref().unwrap_or_default().contains("try again"));

        let mut stuck = Some("the daemon stayed busy for 90.0s".to_string());
        let err = close_landed(
            7,
            landed(),
            || panic!("must not ask a stuck daemon"),
            &mut stuck,
        )
        .unwrap_err();
        assert!(err.to_string().starts_with("resolved, but"), "{err}");

        let mut stuck = None;
        let err = close_landed(
            7,
            landed(),
            || Err(ResolveError::Failed("session end: no such session".into())),
            &mut stuck,
        )
        .unwrap_err();
        assert!(err.to_string().starts_with("resolved, but"), "{err}");
        assert!(
            stuck.is_none(),
            "an ordinary close failure is not a stuck daemon"
        );

        let mut stuck = None;
        let err = close_landed(
            7,
            Err("the batch re-add failed: x".into()),
            || Ok(()),
            &mut stuck,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "the batch re-add failed: x");
    }

    #[test]
    fn parse_undo_restores_skips_decisions_without_target_and_blob() {
        // not an array at all
        assert!(parse_undo_restores(&serde_json::json!({"not": "array"})).is_empty());
        let decisions = serde_json::json!([
            { "action": "keep_source", "target": "/a" },   // no dest_blob
            { "action": "keep_disk", "dest_blob": "h" },    // no target
            { "action": "undo", "of": 3 },
        ]);
        assert!(parse_undo_restores(&decisions).is_empty());
    }
}
