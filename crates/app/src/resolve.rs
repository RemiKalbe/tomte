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
use crate::merge_inputs::MergeInputs;

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
    /// Semantic failure: an unexpected IPC reply, a missing undo blob, or
    /// `chezmoi update` failing (its stderr).
    #[error("{0}")]
    Failed(String),
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
        self.expect_changes(vec![target.to_path_buf(), source])?;
        self.chezmoi.re_add(target)?;
        let (committed, pushed, note) = self.commit_phase("keep_disk", target);
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
    /// never touches the source repo.
    pub fn keep_source(&self, target: &Path) -> Result<ResolveOutcome, ResolveError> {
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
        let target = inputs.target.as_path();
        let session = self.session_start()?;
        let hashes =
            self.snapshot_blobs(vec![inputs.target.clone(), inputs.source_path.clone()])?;
        let [dest_blob, source_blob] = hashes.as_slice() else {
            return Err(ResolveError::Failed(format!(
                "snapshot returned {} blobs for 2 paths",
                hashes.len()
            )));
        };
        let decision = build_decision("merge", target, dest_blob, Some(source_blob));
        self.session_decision(session, decision)?;
        self.expect_changes(vec![inputs.target.clone(), inputs.source_path.clone()])?;

        if inputs.templated {
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
                WriteBackAttempt::Verified(new_template) => {
                    std::fs::write(&inputs.source_path, new_template)?;
                }
                WriteBackAttempt::Rejected(detail) => {
                    self.session_decision(
                        session,
                        serde_json::json!({
                            "action": "merge_rejected",
                            "target": target.to_string_lossy(),
                            "detail": detail,
                        }),
                    )?;
                    self.session_end(
                        session,
                        &format!("merge rejected (protected span) {}", display_name(target)),
                    )?;
                    return Ok(ResolveOutcome::ProtectedSpan { detail });
                }
            }
        } else {
            std::fs::write(&inputs.source_path, resolved)?;
        }

        self.chezmoi.apply(Some(target))?;
        let (committed, pushed, note) = self.commit_phase("merge", target);
        self.session_end(session, &format!("merge {}", display_name(target)))?;
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
                Err(e) => {
                    append_note(&mut note, format!("push failed: {e}"));
                    false
                }
            };
        (committed, pushed, note)
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
