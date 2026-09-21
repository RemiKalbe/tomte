//! Merge-editor inputs (plan 7 Task 1): everything the three-pane editor
//! needs for one target — ours (disk), theirs (chezmoi's rendered source
//! state), base (journal snapshot of the last-written content, when the
//! daemon snapshotted it), origin (origin's rendered content when it
//! differs from theirs — plan 7 Task 6; not yet surfaced as a pane), and
//! the protected-span map for templated sources.
//!
//! All blocking — callers use the background executor.

use std::path::{Path, PathBuf};

use tomte_core::chezmoi::{ChezmoiClient, ChezmoiError};
use tomte_core::git::GitClient;
use tomte_core::template::anchor::{SpanMap, anchor};
use tomte_core::template::lexer::{LexError, lex};
use tomte_journal::{Journal, JournalError};

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
    /// Origin's version of this file, rendered on this machine — `Some`
    /// only when it differs from `theirs`. `None` covers both "origin
    /// matches local" (the common case) and "origin could not be
    /// established" (see [`load_origin`]); a missing/failed origin pane
    /// must never block the merge editor, exactly like [`Self::base`].
    pub origin: Option<String>,
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
/// destination read, the best-effort base snapshot, (templated only) the
/// lex/anchor span map over the rendered text, and the best-effort origin
/// pane. `remote_ref` is the daemon's convention (`format!("origin/{branch}")`
/// off `git.head_branch()`) — callers derive it, `load` does not hardcode it.
pub fn load(
    chezmoi: &ChezmoiClient,
    git: &GitClient,
    remote_ref: &str,
    journal_path: &Path,
    target: &Path,
) -> Result<MergeInputs, MergeInputsError> {
    load_inner(
        chezmoi,
        journal_path,
        target,
        |source_path, templated, theirs| {
            load_origin(chezmoi, git, remote_ref, source_path, templated, theirs)
        },
    )
}

/// [`load`] without origin's pane (`origin` is `None`): for the paths that
/// never read it — a one-click keep-disk on a templated file hands the disk
/// text to the write-back and looks at nothing else. Saves the `git cat-file`
/// and, for a template, the render that the origin pane costs per file,
/// which is what makes it affordable inside a batch.
pub fn load_local(
    chezmoi: &ChezmoiClient,
    journal_path: &Path,
    target: &Path,
) -> Result<MergeInputs, MergeInputsError> {
    load_inner(chezmoi, journal_path, target, |_, _, _| None)
}

fn load_inner(
    chezmoi: &ChezmoiClient,
    journal_path: &Path,
    target: &Path,
    origin: impl FnOnce(&Path, bool, &str) -> Option<String>,
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
    let origin = origin(&source_path, templated, &theirs);
    Ok(MergeInputs {
        target: target.to_path_buf(),
        ours,
        theirs,
        base,
        source_path,
        templated,
        template,
        span_map,
        origin,
    })
}

/// Origin's version of this file, rendered on THIS machine so it is
/// comparable with `theirs` (raw template text never is). `None` on ANY
/// failure along the way — relativizing the source path, reading the blob
/// at `remote_ref`, rendering it, or non-UTF-8 output — exactly mirroring
/// [`load_base`]'s contract: a missing/failed optional pane must never
/// block the merge editor. This is not the same silent swallow a bare
/// `unwrap_or_default` would be: every subprocess call here (`chezmoi
/// source-path`/`source-dir`, `git cat-file`, `chezmoi execute-template`)
/// already logs its own failure through the command chokepoint
/// (`SystemRunner::run` in `tomte_core::cmd`), and the one step that has no
/// subprocess behind it — the UTF-8 decode — logs for itself below. So a
/// genuine problem still lands in the logs even though the return type here
/// degrades to `None` — the same reasoning that already justifies
/// `load_base`'s misses.
fn load_origin(
    chezmoi: &ChezmoiClient,
    git: &GitClient,
    remote_ref: &str,
    source_path: &Path,
    templated: bool,
    theirs: &str,
) -> Option<String> {
    let source_dir = chezmoi.source_dir().ok()?;
    let source_rel = source_path.strip_prefix(&source_dir).ok()?;
    // `None` here means "not present at that rev" (e.g. added locally,
    // never pushed) — a normal outcome, not a failure.
    let raw = git.blob_at(remote_ref, source_rel).ok()??;
    let rendered = if templated {
        chezmoi.execute_template(&raw).ok()?
    } else {
        raw
    };
    // The one step here with no subprocess behind it, so the chokepoint
    // cannot speak for it: a non-UTF-8 origin blob would otherwise vanish
    // into `None` with nothing anywhere saying why the origin pane is gone.
    let text = match String::from_utf8(rendered) {
        Ok(text) => text,
        Err(e) => {
            tomte_core::log_info!(
                "merge",
                "origin pane skipped: {} at {remote_ref} is not UTF-8 ({e})",
                source_rel.display()
            );
            return None;
        }
    };
    (text != theirs).then_some(text)
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

    use tomte_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
    use tomte_core::cmd::fake::FakeRunner;
    use tomte_core::git::GitClient;
    use tomte_journal::Journal;

    use super::{MergeInputsError, load};

    const RENDERED: &str = "a=1\n";
    const DRIFTED: &str = "a=local\n";
    const REMOTE_REF: &str = "origin/main";

    fn client(fake: Arc<FakeRunner>) -> ChezmoiClient {
        ChezmoiClient::new(fake, ChezmoiOptions::default())
    }

    /// A `GitClient` backed by its own `FakeRunner` — kept separate from the
    /// chezmoi fake deliberately: `FakeRunner` replays one ordered queue
    /// blindly (it does not match on the command), so sharing a single
    /// queue between two clients would require interleaving every push to
    /// match the exact cross-client call order. Two independent fakes sidestep
    /// that entirely — each only has to match its OWN call order — and it
    /// mirrors production, where the chezmoi and git clients are never built
    /// from the same runner instance either (see `app_git_client` in
    /// `main.rs`).
    fn git_client(fake: Arc<FakeRunner>, repo: &Path) -> GitClient {
        GitClient::new(fake, repo.to_path_buf())
    }

    /// Queue the three chezmoi replies `load` makes before `load_origin`, in
    /// call order: source-path → cat → state dump.
    fn queue_chezmoi(fake: &FakeRunner, source_path: &Path, rendered: &str, dump_json: &str) {
        fake.push_ok(0, &format!("{}\n", source_path.display()), "");
        fake.push_ok(0, rendered, "");
        fake.push_ok(0, dump_json, "");
    }

    /// Queue `load_origin`'s calls for tests that don't care about origin
    /// content: chezmoi's `source-dir` (to relativize `source_path`) reports
    /// `source_path`'s parent, then git's `blob_at` reports the path absent
    /// at `REMOTE_REF` (exit 1) — a real, common outcome (e.g. added
    /// locally, never pushed), so `load_origin` short-circuits there and
    /// never reaches `execute-template`. Must be called AFTER `queue_chezmoi`
    /// (same chezmoi fake, next in its queue) with a freshly built git fake.
    fn queue_no_origin(chezmoi_fake: &FakeRunner, git_fake: &FakeRunner, source_path: &Path) {
        let source_dir = source_path.parent().unwrap();
        chezmoi_fake.push_ok(0, &format!("{}\n", source_dir.display()), "");
        git_fake.push_ok(1, "", "");
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
        let git_fake = Arc::new(FakeRunner::new());
        queue_no_origin(&fake, &git_fake, &source);
        let git = git_client(git_fake, lab.dir.path());

        let inputs = load(
            &client(fake),
            &git,
            REMOTE_REF,
            &lab.journal_path,
            &lab.target,
        )
        .unwrap();
        assert_eq!(inputs.target, lab.target);
        assert_eq!(inputs.ours, DRIFTED);
        assert_eq!(inputs.theirs, RENDERED);
        assert_eq!(inputs.base.as_deref(), Some(RENDERED));
        assert_eq!(inputs.source_path, source);
        assert!(!inputs.templated);
        assert!(inputs.span_map.is_none());
        assert_eq!(inputs.origin, None);
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
        let git_fake = Arc::new(FakeRunner::new());
        queue_no_origin(&fake, &git_fake, &source);
        let git = git_client(git_fake, lab.dir.path());

        let inputs = load(
            &client(fake),
            &git,
            REMOTE_REF,
            &lab.journal_path,
            &lab.target,
        )
        .unwrap();
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
        let git_fake = Arc::new(FakeRunner::new());
        queue_no_origin(&fake, &git_fake, &source);
        let git = git_client(git_fake, lab.dir.path());

        let inputs = load(
            &client(fake),
            &git,
            REMOTE_REF,
            &lab.journal_path,
            &lab.target,
        )
        .unwrap();
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
        // Never reached: the destination decode fails before `load_base` or
        // `load_origin` run, so no source-dir/blob_at replies are queued —
        // an empty git fake would panic if load_origin were ever reached.
        let git = git_client(Arc::new(FakeRunner::new()), lab.dir.path());

        let err = load(
            &client(fake),
            &git,
            REMOTE_REF,
            &lab.journal_path,
            &lab.target,
        )
        .unwrap_err();
        assert!(matches!(err, MergeInputsError::Binary), "got {err:?}");
    }

    #[test]
    fn load_rejects_binary_rendered_output() {
        let lab = Lab::new();
        std::fs::write(&lab.target, DRIFTED).unwrap();
        let fake = Arc::new(FakeRunner::new());
        fake.push_ok(0, "/src/dot_testrc\n", "");
        fake.push_ok_bytes(0, &[0xff, 0xfe, 0x00, 0x01], b"");
        // Never reached: `theirs` fails to decode right after `cat`, before
        // `load_origin` runs — same reasoning as above.
        let git = git_client(Arc::new(FakeRunner::new()), lab.dir.path());

        let err = load(
            &client(fake),
            &git,
            REMOTE_REF,
            &lab.journal_path,
            &lab.target,
        )
        .unwrap_err();
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
        let git_fake = Arc::new(FakeRunner::new());
        queue_no_origin(&fake, &git_fake, &source);
        let git = git_client(git_fake, lab.dir.path());
        let inputs = load(
            &client(fake),
            &git,
            REMOTE_REF,
            &lab.journal_path,
            &lab.target,
        )
        .unwrap();
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
        let git_fake = Arc::new(FakeRunner::new());
        queue_no_origin(&fake, &git_fake, &source);
        let git = git_client(git_fake, lab.dir.path());
        let inputs = load(
            &client(fake),
            &git,
            REMOTE_REF,
            &lab.journal_path,
            &lab.target,
        )
        .unwrap();
        assert_eq!(inputs.base, None, "missing blob must miss to None");
    }

    /// A `Lab` wired all the way through `load_origin`: chezmoi's fake
    /// queues source-path → cat → state-dump → source-dir; git's fake
    /// queues one `blob_at` reply holding `origin_blob`. Plain (non-.tmpl)
    /// source, so `load_origin` never calls `execute-template`.
    fn origin_lab(origin_blob: &str) -> (Lab, ChezmoiClient, GitClient) {
        let lab = Lab::new();
        std::fs::write(&lab.target, DRIFTED).unwrap();
        let source = lab.dir.path().join("src/dot_testrc");
        let fake = Arc::new(FakeRunner::new());
        queue_chezmoi(&fake, &source, RENDERED, "{}");
        fake.push_ok(0, &format!("{}\n", source.parent().unwrap().display()), "");
        let git_fake = Arc::new(FakeRunner::new());
        git_fake.push_ok(0, origin_blob, "");
        let git = git_client(git_fake, lab.dir.path());
        let chezmoi = client(fake);
        (lab, chezmoi, git)
    }

    #[test]
    fn origin_is_loaded_and_rendered_only_when_it_differs() {
        // Identical origin → None, so the fourth pane never appears for the
        // overwhelmingly common case.
        let (lab, chezmoi, git) = origin_lab(RENDERED);
        let inputs = load(&chezmoi, &git, REMOTE_REF, &lab.journal_path, &lab.target).unwrap();
        assert_eq!(inputs.origin, None);

        // Different origin → Some, the raw blob text (plain file: no
        // execute-template involved).
        let (lab, chezmoi, git) = origin_lab("a=origin\n");
        let inputs = load(&chezmoi, &git, REMOTE_REF, &lab.journal_path, &lab.target).unwrap();
        assert_eq!(inputs.origin.as_deref(), Some("a=origin\n"));
    }

    #[test]
    fn origin_is_rendered_through_execute_template_when_templated() {
        let lab = Lab::new();
        std::fs::write(&lab.target, DRIFTED).unwrap();
        // The template must exist on disk: load reads it to lex + anchor,
        // exactly like `load_detects_templated_source_and_builds_span_map`.
        let source = lab.dir.path().join("dot_testrc.tmpl");
        std::fs::write(&source, "a={{ .v }}\n").unwrap();
        let fake = Arc::new(FakeRunner::new());
        queue_chezmoi(&fake, &source, RENDERED, "{}");
        fake.push_ok(0, &format!("{}\n", lab.dir.path().display()), "");
        // load_origin's execute-template call, queued last: it only runs
        // AFTER the git blob below is read.
        fake.push_ok(0, "a=origin\n", "");
        let git_fake = Arc::new(FakeRunner::new());
        // Origin's raw (unrendered) template text — deliberately different
        // template source, to prove `origin` carries the RENDERED text
        // (`execute-template`'s output), never the raw template.
        git_fake.push_ok(0, "a={{ .other }}\n", "");
        let git = git_client(git_fake, lab.dir.path());

        let inputs = load(
            &client(fake),
            &git,
            REMOTE_REF,
            &lab.journal_path,
            &lab.target,
        )
        .unwrap();
        assert_eq!(inputs.origin.as_deref(), Some("a=origin\n"));
    }
}
