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

use czui_core::chezmoi::{ChezmoiClient, ChezmoiError};
use czui_core::git::{GitClient, GitError};
use czui_journal::{Journal, JournalError};
use czui_proto::{Request, Response};

use crate::ipc::{IpcClient, IpcError};

/// TTL for ExpectChanges pre-announcements: generously covers a slow chezmoi
/// invocation without leaving stale suppressions around for long.
const EXPECT_TTL_SECS: u32 = 60;

/// Machine label for the app's read-only journal handle. Only stamped on
/// writes, which a read-only handle rejects — it never reaches the database.
const RO_MACHINE: &str = "czui-app";

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

/// Whether a source path is a chezmoi template (`.tmpl` extension). Templated
/// sources must never be one-click re-added: `chezmoi re-add` silently
/// ignores them.
pub fn is_templated(source_path: &Path) -> bool {
    source_path.extension().is_some_and(|ext| ext == "tmpl")
}

/// Message for the engine's fallback source-repo commit:
/// `chezmoi-ui: <action> <file-name>`.
pub fn commit_message(action: &str, target: &Path) -> String {
    format!("chezmoi-ui: {action} {}", display_name(target))
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
    use super::*;

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
            "chezmoi-ui: keep_disk .zshrc"
        );
        assert_eq!(
            commit_message("keep_disk", Path::new("/Users/x/.config/starship.toml")),
            "chezmoi-ui: keep_disk starship.toml"
        );
    }

    #[test]
    fn commit_message_falls_back_to_full_path_without_file_name() {
        assert_eq!(
            commit_message("keep_source", Path::new("/")),
            "chezmoi-ui: keep_source /"
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
