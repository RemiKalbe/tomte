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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tomte_core::chezmoi::{ChezmoiClient, ChezmoiError};
use tomte_core::git::{GitClient, GitError};
use tomte_core::template::anchor::SpanMap;
use tomte_core::template::verify::{VerifyError, verify_write_back};
use tomte_core::template::writeback::write_back;
use tomte_journal::{Journal, JournalError};
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
    /// Templated source: one-click is unsafe (chezmoi re-add silently ignores
    /// templates); the merge editor (Plan 7) is required.
    NeedsMergeEditor,
    /// Merge-editor write-back rejected: the resolved text touches a
    /// protected template span (or the new template failed re-render
    /// verification). Nothing was mutated — the rejection itself is
    /// journaled. UI copy: "this change touches a templated value — open in
    /// editor".
    ProtectedSpan { detail: String },
}

#[derive(Clone)]
pub struct ResolveEngine {
    // Arc'd / cheaply clonable so background closures can own clones.
    pub chezmoi: ChezmoiClient,
    pub git: GitClient,
    pub ipc: Arc<IpcClient>,
    /// Read-only opens for undo (spec §8: single writer = daemon).
    pub journal_path: PathBuf,
}

impl ResolveEngine {
    /// Keep the on-disk version: re-add it into the source state.
    ///
    /// Templated sources return `Ok(NeedsMergeEditor)` without touching
    /// anything — `chezmoi re-add` silently ignores templates, and pretending
    /// success would be a lie.
    pub fn keep_disk(&self, target: &Path) -> Result<ResolveOutcome, ResolveError> {
        tomte_core::log_info!("resolve", "keep_disk {}", target.display());
        let source = self.chezmoi.source_path(target)?;
        if is_templated(&source) {
            return Ok(ResolveOutcome::NeedsMergeEditor);
        }
        let session = self.session_start()?;
        let hashes = self.snapshot_blobs(vec![target.to_path_buf(), source.clone()])?;
        let [dest_blob, source_blob] = hashes.as_slice() else {
            return Err(ResolveError::Failed(format!(
                "snapshot returned {} blobs for 2 paths",
                hashes.len()
            )));
        };
        let decision = build_decision("keep_disk", target, dest_blob, Some(source_blob));
        self.session_decision(session, decision)?;
        self.expect_changes(vec![target.to_path_buf(), source.clone()])?;
        // The user has committed to an answer, so a diverged repo may now be
        // put into a real merge (spec §6.1). `re_add` writes the source from
        // the DESTINATION, so it overwrites whatever the merge left in that
        // file — conflict markers included — which is exactly what "keep
        // disk" means for this conflict.
        let merging = self.ensure_merge_started()?;
        let (committed, pushed, note) = if merging {
            // NOT `chezmoi re-add` here. A chezmoi configured with autoCommit
            // commits from inside re-add, and git refuses to commit with
            // unmerged paths — so mid-merge that call fails outright. It
            // cannot be suppressed per-invocation either (CHEZMOI_GIT_* env
            // overrides are ignored). Copying the destination over the source
            // is exactly what re-add does for a non-templated file, which is
            // all that reaches here — `is_templated` returned early above.
            std::fs::copy(target, &source)?;
            self.stage_resolution(&source)?;
            self.conclude_merge()
        } else {
            self.chezmoi.re_add(target)?;
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
        if self.git.merge_in_progress() {
            let source = self.chezmoi.source_path(target)?;
            if self.is_mid_merge_conflict(&source)? {
                return Err(ResolveError::Failed(format!(
                    "{} is mid-merge with origin — its source file holds conflict markers; \
                     resolve the origin difference in the merge editor first",
                    display_name(target)
                )));
            }
        }
        let session = self.session_start()?;
        let hashes = self.snapshot_blobs(vec![target.to_path_buf()])?;
        let [dest_blob] = hashes.as_slice() else {
            return Err(ResolveError::Failed(format!(
                "snapshot returned {} blobs for 1 path",
                hashes.len()
            )));
        };
        let decision = build_decision("keep_source", target, dest_blob, None);
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
        // A merge already running with THIS file conflicted means its
        // working-tree source is git's conflict-marker text, and `inputs`
        // — read from that file — describes nothing that exists. Writing a
        // resolution derived from it would persist the markers. The way in
        // is the merge editor's repo-conflict mode, which reads the index
        // stages instead (see [`Self::repo_conflict_for`]).
        if self.is_mid_merge_conflict(&inputs.source_path)? {
            return Err(ResolveError::Failed(format!(
                "{} is mid-merge with origin — reopen it in the merge editor, \
                 which reads the two sides from the merge itself",
                display_name(target)
            )));
        }
        let session = self.session_start()?;
        let hashes =
            self.snapshot_blobs(vec![inputs.target.clone(), inputs.source_path.clone()])?;
        let [dest_blob, source_blob] = hashes.as_slice() else {
            return Err(ResolveError::Failed(format!(
                "snapshot returned {} blobs for 2 paths",
                hashes.len()
            )));
        };
        let decision = build_decision(action, target, dest_blob, Some(source_blob));
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
                    )?;
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

    /// Restore the destination files of the LAST finished session from their
    /// journaled blobs, journal the undo as a new session, and rescan.
    /// Returns the id of the session that was undone, or `Ok(None)` when no
    /// finished session exists.
    ///
    /// Source-side revert (rolling back the source-repo commit a `keep_disk`
    /// produced) is deferred to Plan 7's merge tooling — this only restores
    /// destination files.
    pub fn undo_last(&self) -> Result<Option<i64>, ResolveError> {
        tomte_core::log_info!("resolve", "undo_last");
        let journal = Journal::open_read_only(&self.journal_path, RO_MACHINE)?;
        let Some((of, decisions)) = journal.last_finished_session()? else {
            return Ok(None);
        };
        let restores = parse_undo_restores(&decisions);
        for (target, dest_blob) in &restores {
            let bytes = journal.get_blob(dest_blob)?.ok_or_else(|| {
                ResolveError::Failed(format!(
                    "undo: blob {dest_blob} for {} is missing from the journal",
                    target.display()
                ))
            })?;
            self.expect_changes(vec![target.clone()])?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(target, &bytes)?;
        }
        let session = self.session_start()?;
        self.session_decision(session, serde_json::json!({ "action": "undo", "of": of }))?;
        self.session_end(
            session,
            &format!("undo session {of} — restored {} files", restores.len()),
        )?;
        self.rescan()?;
        Ok(Some(of))
    }

    /// Shared commit phase: when the source repo is left dirty (chezmoi
    /// autoCommit off or failed), stage everything and commit; then push.
    /// Both are best-effort — a locked 1Password failing the signed commit
    /// must not fail the resolution, only report honestly (spec §10).
    fn commit_phase(&self, action: &str, target: &Path) -> (bool, bool, Option<String>) {
        let mut note = None;
        let committed = match self.git.dirty_files() {
            // Clean repo: chezmoi autoCommit already committed (or there was
            // nothing to commit) — a preexisting commit counts as committed.
            Ok(dirty) if dirty.is_empty() => true,
            Ok(_) => {
                let message = commit_message(action, target);
                match self.git.add_all().and_then(|()| self.git.commit(&message)) {
                    Ok(_sha) => true,
                    Err(e) => {
                        tomte_core::log_error!("resolve", "commit failed: {e}");
                        append_note(&mut note, format!("commit failed: {e}"));
                        false
                    }
                }
            }
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
    pub fn abort_repo_reconcile(&self) {
        tomte_core::log_info!("reconcile", "aborted");
        self.git.merge_abort();
    }

    /// Ask the daemon to rescan. Every mutating path here already ends with
    /// one; this exposes it for the paths that rewrite the source repo
    /// WITHOUT going through them — an abort, above all, whose whole effect
    /// is to make the daemon's last scan describe a repo that is gone.
    pub fn request_rescan(&self) -> Result<(), ResolveError> {
        self.rescan()
    }

    fn session_start(&self) -> Result<i64, ResolveError> {
        match self.ipc.request(Request::SessionStart { ts: now_ts() })? {
            Response::SessionStarted { session } => Ok(session),
            other => Err(unexpected("session start", other)),
        }
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
        match self.ipc.request(Request::SnapshotBlobs { paths })? {
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
        match self.ipc.request(request)? {
            Response::Ok => Ok(()),
            other => Err(unexpected(what, other)),
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

/// Whether a source path is a chezmoi template (`.tmpl` extension). Templated
/// sources must never be one-click re-added: `chezmoi re-add` silently
/// ignores them.
pub fn is_templated(source_path: &Path) -> bool {
    source_path.extension().is_some_and(|ext| ext == "tmpl")
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
/// `{action, target, dest_blob, source_blob?}`.
fn build_decision(
    action: &str,
    target: &Path,
    dest_blob: &str,
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

/// Extract `(target, dest_blob)` pairs from a session's decisions array.
/// Decisions without both fields (e.g. `sync_all`, `undo`) restore nothing.
fn parse_undo_restores(decisions: &serde_json::Value) -> Vec<(PathBuf, String)> {
    decisions
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|decision| {
                    let target = decision.get("target")?.as_str()?;
                    let dest_blob = decision.get("dest_blob")?.as_str()?;
                    Some((PathBuf::from(target), dest_blob.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
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
            "abc123",
            Some("def456"),
        );
        assert_eq!(with_source["action"], "keep_disk");
        assert_eq!(with_source["target"], "/Users/x/.zshrc");
        assert_eq!(with_source["dest_blob"], "abc123");
        assert_eq!(with_source["source_blob"], "def456");

        let without_source =
            build_decision("keep_source", Path::new("/Users/x/.testrc"), "abc123", None);
        assert!(without_source.get("source_blob").is_none());

        let decisions = serde_json::json!([
            with_source,
            without_source,
            { "action": "sync_all" },
        ]);
        assert_eq!(
            parse_undo_restores(&decisions),
            vec![
                (PathBuf::from("/Users/x/.zshrc"), "abc123".to_string()),
                (PathBuf::from("/Users/x/.testrc"), "abc123".to_string()),
            ]
        );
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
