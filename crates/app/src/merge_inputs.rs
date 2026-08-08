//! Merge-editor inputs (plan 7 Task 1): everything the three-pane editor
//! needs for one target — ours (disk), theirs (chezmoi's rendered source
//! state), base (journal snapshot of the last-written content, when the
//! daemon snapshotted it), and the protected-span map for templated sources.
//!
//! All blocking — callers use the background executor.

use std::path::{Path, PathBuf};

use czui_core::chezmoi::{ChezmoiClient, ChezmoiError};
use czui_core::template::anchor::{SpanMap, anchor};
use czui_core::template::lexer::{LexError, lex};
use czui_journal::{Journal, JournalError};

use crate::resolve::{RO_MACHINE, is_templated};

/// The three merge panes plus the write-back context for one target.
#[derive(Debug)]
pub struct MergeInputs {
    pub target: PathBuf,
    /// Destination file as it exists on disk. Missing on disk reads as empty:
    /// a deleted destination is still a mergeable drift (same rule as the
    /// review preview).
    pub ours: String,
    /// `chezmoi cat` — the rendered source state.
    pub theirs: String,
    /// Last-written content: the journal blob stored under the state-dump
    /// `contentsSHA256`, if snapshotted. `None` degrades the merge to 2-way
    /// (base := theirs).
    pub base: Option<String>,
    pub source_path: PathBuf,
    /// Source ends in `.tmpl`.
    pub templated: bool,
    /// Raw template source (the `.tmpl` file) when templated — the UI shows
    /// protected lines' template-side text on hover.
    pub template: Option<String>,
    /// Present when templated: protected spans over `theirs` (rendered).
    pub span_map: Option<SpanMap>,
}

#[derive(Debug, thiserror::Error)]
pub enum MergeInputsError {
    /// Ours or theirs is not valid UTF-8 — the choice-based merge editor
    /// handles text only.
    #[error("binary content — the merge editor handles UTF-8 text only")]
    Binary,
    #[error(transparent)]
    Chezmoi(#[from] ChezmoiError),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Lex(#[from] LexError),
}

/// Load the merge inputs for `target`: `chezmoi source-path` + `cat`, the
/// destination read, the best-effort base snapshot, and (templated only)
/// the lex/anchor span map over the rendered text.
pub fn load(
    chezmoi: &ChezmoiClient,
    journal_path: &Path,
    target: &Path,
) -> Result<MergeInputs, MergeInputsError> {
    let source_path = chezmoi.source_path(target)?;
    let theirs = String::from_utf8(chezmoi.cat(target)?).map_err(|_| MergeInputsError::Binary)?;
    let ours_bytes = match std::fs::read(target) {
        Ok(bytes) => bytes,
        // Deleted on disk is still a mergeable drift: an empty ours pane.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let ours = String::from_utf8(ours_bytes).map_err(|_| MergeInputsError::Binary)?;
    let base = load_base(chezmoi, journal_path, target)?;
    let templated = is_templated(&source_path);
    let (template, span_map) = if templated {
        let template = std::fs::read_to_string(&source_path)?;
        let segments = lex(&template)?;
        let map = anchor(&template, &segments, &theirs);
        (Some(template), Some(map))
    } else {
        (None, None)
    };
    Ok(MergeInputs {
        target: target.to_path_buf(),
        ours,
        theirs,
        base,
        source_path,
        templated,
        template,
        span_map,
    })
}

/// Last-written content via the journal: `chezmoi state dump` names the
/// `contentsSHA256` chezmoi last wrote, and the daemon snapshots blobs under
/// exactly that hash (both are SHA-256 of the content). Any miss along the
/// chain — no entry, no hash, no journal (fresh install), no blob, non-UTF-8
/// blob — degrades to `None`, never an error: a missing snapshot must not
/// block the merge editor, it only drops it to 2-way.
fn load_base(
    chezmoi: &ChezmoiClient,
    journal_path: &Path,
    target: &Path,
) -> Result<Option<String>, MergeInputsError> {
    let dump = chezmoi.state_dump()?;
    let Some(hash) = dump
        .entry_state
        .get(target)
        .and_then(|e| e.contents_sha256.clone())
    else {
        return Ok(None);
    };
    let Ok(journal) = Journal::open_read_only(journal_path, RO_MACHINE) else {
        return Ok(None);
    };
    let Ok(Some(bytes)) = journal.get_blob(&hash) else {
        return Ok(None);
    };
    Ok(String::from_utf8(bytes).ok())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use czui_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
    use czui_core::cmd::fake::FakeRunner;
    use czui_journal::Journal;

    use super::{MergeInputsError, load};

    const RENDERED: &str = "a=1\n";
    const DRIFTED: &str = "a=local\n";

    fn client(fake: Arc<FakeRunner>) -> ChezmoiClient {
        ChezmoiClient::new(fake, ChezmoiOptions::default())
    }

    /// Queue the three chezmoi replies `load` makes, in call order:
    /// source-path → cat → state dump.
    fn queue_chezmoi(fake: &FakeRunner, source_path: &Path, rendered: &str, dump_json: &str) {
        fake.push_ok(0, &format!("{}\n", source_path.display()), "");
        fake.push_ok(0, rendered, "");
        fake.push_ok(0, dump_json, "");
    }

    /// A state dump naming `hash` as the last-written content of `target`.
    fn dump_with_hash(target: &Path, hash: &str) -> String {
        let mut entries = serde_json::Map::new();
        entries.insert(
            target.to_string_lossy().into_owned(),
            serde_json::json!({"contentsSHA256": hash, "mode": 420, "type": "file"}),
        );
        serde_json::json!({ "entryState": entries }).to_string()
    }

    struct Lab {
        dir: tempfile::TempDir,
        target: PathBuf,
        journal_path: PathBuf,
    }

    impl Lab {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join(".testrc");
            let journal_path = dir.path().join("journal.db");
            Self {
                dir,
                target,
                journal_path,
            }
        }

        /// Create the journal file and snapshot `content`, returning its hash.
        fn snapshot(&self, content: &str) -> String {
            Journal::open(&self.journal_path, "test")
                .unwrap()
                .put_blob(content.as_bytes(), 1)
                .unwrap()
        }
    }

    #[test]
    fn load_plain_file_with_snapshotted_base() {
        let lab = Lab::new();
        std::fs::write(&lab.target, DRIFTED).unwrap();
        let hash = lab.snapshot(RENDERED);
        let source = lab.dir.path().join("src/dot_testrc");
        let fake = Arc::new(FakeRunner::new());
        queue_chezmoi(
            &fake,
            &source,
            RENDERED,
            &dump_with_hash(&lab.target, &hash),
        );

        let inputs = load(&client(fake), &lab.journal_path, &lab.target).unwrap();
        assert_eq!(inputs.target, lab.target);
        assert_eq!(inputs.ours, DRIFTED);
        assert_eq!(inputs.theirs, RENDERED);
        assert_eq!(inputs.base.as_deref(), Some(RENDERED));
        assert_eq!(inputs.source_path, source);
        assert!(!inputs.templated);
        assert!(inputs.span_map.is_none());
    }

    #[test]
    fn load_detects_templated_source_and_builds_span_map() {
        let lab = Lab::new();
        std::fs::write(&lab.target, DRIFTED).unwrap();
        // The template must exist on disk: load reads it to lex + anchor.
        let source = lab.dir.path().join("dot_testrc.tmpl");
        std::fs::write(&source, "a={{ .v }}\n").unwrap();
        let fake = Arc::new(FakeRunner::new());
        // No entryState entry → base misses to None.
        queue_chezmoi(&fake, &source, RENDERED, "{}");

        let inputs = load(&client(fake), &lab.journal_path, &lab.target).unwrap();
        assert!(inputs.templated);
        assert_eq!(inputs.base, None);
        let map = inputs.span_map.expect("templated load must anchor spans");
        assert!(
            !map.spans.is_empty(),
            "the span map must cover the rendered text"
        );
    }

    #[test]
    fn load_missing_destination_reads_as_empty_ours() {
        let lab = Lab::new();
        // No destination file written: deleted on disk.
        let source = lab.dir.path().join("src/dot_testrc");
        let fake = Arc::new(FakeRunner::new());
        queue_chezmoi(&fake, &source, RENDERED, "{}");

        let inputs = load(&client(fake), &lab.journal_path, &lab.target).unwrap();
        assert_eq!(inputs.ours, "");
        assert_eq!(inputs.theirs, RENDERED);
    }

    #[test]
    fn load_rejects_binary_destination() {
        let lab = Lab::new();
        std::fs::write(&lab.target, [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "/src/dot_testrc\n", "");
        fake.push_ok(0, RENDERED, "");

        let err = load(&client(fake), &lab.journal_path, &lab.target).unwrap_err();
        assert!(matches!(err, MergeInputsError::Binary), "got {err:?}");
    }

    #[test]
    fn load_rejects_binary_rendered_output() {
        let lab = Lab::new();
        std::fs::write(&lab.target, DRIFTED).unwrap();
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "/src/dot_testrc\n", "");
        fake.push_ok_bytes(0, &[0xff, 0xfe, 0x00, 0x01], b"");

        let err = load(&client(fake), &lab.journal_path, &lab.target).unwrap_err();
        assert!(matches!(err, MergeInputsError::Binary), "got {err:?}");
    }

    #[test]
    fn load_base_misses_degrade_to_none_not_errors() {
        // Miss 1: the state dump names a hash but the journal file does not
        // exist (fresh install, daemon never ran).
        let lab = Lab::new();
        std::fs::write(&lab.target, DRIFTED).unwrap();
        let source = lab.dir.path().join("src/dot_testrc");
        let fake = Arc::new(FakeRunner::new());
        queue_chezmoi(
            &fake,
            &source,
            RENDERED,
            &dump_with_hash(&lab.target, &"ab".repeat(32)),
        );
        let inputs = load(&client(fake), &lab.journal_path, &lab.target).unwrap();
        assert_eq!(inputs.base, None, "missing journal must miss to None");

        // Miss 2: the journal exists but holds no blob for the hash (the
        // daemon never snapshotted this content).
        lab.snapshot("unrelated\n");
        let fake = Arc::new(FakeRunner::new());
        queue_chezmoi(
            &fake,
            &source,
            RENDERED,
            &dump_with_hash(&lab.target, &"cd".repeat(32)),
        );
        let inputs = load(&client(fake), &lab.journal_path, &lab.target).unwrap();
        assert_eq!(inputs.base, None, "missing blob must miss to None");
    }
}
