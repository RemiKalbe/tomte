mod support;

use support::{Scratch, git};
use tomte_core::drift::DriftClass;
use tomte_core::scanner::{FileDrift, ScanReport};
use tomte_core::type_change::{EntryKind, TypeChange};

#[test]
fn clean_state_is_in_sync() {
    let s = Scratch::new();
    let report = s.scanner().scan().unwrap();
    assert!(report.drifted.is_empty(), "{:?}", report.drifted);
    assert_eq!(report.in_sync_count, 1);
    assert!(report.degraded.is_none());
}

#[test]
fn tool_rewrite_is_destination_drift() {
    let s = Scratch::new();
    std::fs::write(s.home.join(".testrc"), "a=2\n").unwrap();
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    let d = &report.drifted[0];
    assert!(d.target.ends_with(".testrc"));
    assert_eq!(d.class, DriftClass::DestinationDrift);
}

#[test]
fn source_edit_is_source_ahead() {
    let s = Scratch::new();
    std::fs::write(s.source.join("dot_testrc"), "a=3\n").unwrap();
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    assert_eq!(report.drifted[0].class, DriftClass::SourceAhead);
}

#[test]
fn remote_push_is_remote_ahead() {
    let s = Scratch::new();
    let other = s.root.path().join("other");
    support::sh(
        s.root.path(),
        "git",
        &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    std::fs::write(other.join("dot_testrc"), "a=4\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "remote change"]);
    git(&other, &["push"]);
    // scanner does not fetch; the caller owns fetch cadence (spec §3.1)
    support::git(&s.source, &["fetch", "origin"]);
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    assert_eq!(report.drifted[0].class, DriftClass::RemoteAhead);
}

#[test]
fn probe_one_detects_single_file_drift() {
    let s = Scratch::new();
    std::fs::write(s.home.join(".testrc"), "a=changed\n").unwrap();
    let scanner = s.scanner();
    let fd = scanner.probe_one(&s.home.join(".testrc")).unwrap().unwrap();
    assert_eq!(fd.class, DriftClass::DestinationDrift);
    // untouched target probes clean after re-write back
    std::fs::write(s.home.join(".testrc"), "a=1\n").unwrap();
    assert!(
        scanner
            .probe_one(&s.home.join(".testrc"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn disk_and_remote_change_is_conflict() {
    let s = Scratch::new();
    std::fs::write(s.home.join(".testrc"), "a=local\n").unwrap();
    let other = s.root.path().join("other");
    support::sh(
        s.root.path(),
        "git",
        &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    std::fs::write(other.join("dot_testrc"), "a=remote\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "remote change"]);
    git(&other, &["push"]);
    support::git(&s.source, &["fetch", "origin"]);
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    assert_eq!(report.drifted[0].class, DriftClass::Conflict);
}

/// The 2026-08-27 field bug: chezmoi managed a DIRECTORY that had never been
/// applied, so it had no entry state. The scanner probed it as a file,
/// `chezmoi cat` refused ("not a file, script, or symlink"), and the `?` took
/// the entire scan down — leaving the daemon reporting "0 drifted, 0 in sync"
/// indefinitely while the file watcher kept ticking.
#[test]
fn a_managed_directory_never_applied_does_not_abort_the_scan() {
    let s = Scratch::new();
    // A new source directory, never applied: `git status --porcelain` reports
    // the untracked DIRECTORY, which maps to a target directory with no entry
    // state — exactly the shape that broke.
    let dir = s.source.join("dot_newdir");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("inner"), "x\n").unwrap();

    let report = s
        .scanner()
        .scan()
        .expect("a managed directory must not abort the scan");

    // The rest of the scan still did its job: the pre-existing managed file
    // is still accounted for, and the new file is reported as source-ahead.
    assert_eq!(
        report.in_sync_count, 1,
        "the clean managed file still counts"
    );
    assert!(
        report
            .drifted
            .iter()
            .any(|d| d.target.ends_with("inner") && d.class == DriftClass::SourceAhead),
        "the new file inside the directory is still detected: {:?}",
        report.drifted
    );
    // The directory is out of scope, not a failure — it must not show up as
    // something that "could not be checked".
    assert!(
        report.unprobed.is_empty(),
        "a directory is skipped, not recorded as a failure: {:?}",
        report.unprobed
    );
}

/// The general form of the same bug: ANY single unprobeable target used to
/// abort everything. The shape this test used, a managed file replaced by a
/// folder, is now a type change: one row, and still kept out of the in-sync
/// tally. (No unreadable file survives `chezmoi status` itself: a mode-000
/// file fails status for the whole home.)
///
/// MUTATION: drop `settled` from the step (a) skip → the folder is hashed
/// as a file and lands in `unprobed` beside its row.
#[test]
fn an_unreadable_target_is_recorded_not_fatal() {
    let s = Scratch::new();
    let target = s.home.join(".testrc");
    std::fs::remove_file(&target).unwrap();
    std::fs::create_dir(&target).unwrap();

    let report = s.scanner().scan().expect("one bad target is not fatal");
    assert_eq!(
        type_change_of(&report, ".testrc").type_change,
        Some(TypeChange {
            source: EntryKind::File,
            disk: EntryKind::Folder,
            disk_moved: true
        })
    );
    assert!(report.unprobed.is_empty(), "{:?}", report.unprobed);
    assert_eq!(report.in_sync_count, 0, "a changed file is not in sync");
}

#[test]
fn origin_differences_are_detected_per_target() {
    let s = Scratch::new();
    // Push a change to origin that the local repo does not have.
    let other = s.root.path().join("other");
    support::sh(
        s.root.path(),
        "git",
        &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    std::fs::write(other.join("dot_testrc"), "a=origin\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "origin edit"]);
    git(&other, &["push"]);
    support::git(&s.source, &["fetch", "origin"]);

    // A second managed file, edited only in the local WORKING TREE
    // (uncommitted, never pushed). It lands in the scanner's candidate set
    // via `dirty_files` — exactly like `.testrc` lands via `changed_files`
    // — but origin's committed version of it is untouched: HEAD already
    // matches origin for this file, the dirty edit hasn't been committed at
    // all. An implementation that inserted every mapped candidate into
    // `origin_differs` (i.e. deleted the `differs_from_origin.contains(rel)`
    // gate) would wrongly flag this one too; a selective one must not.
    std::fs::write(s.source.join("dot_otherrc"), "b=uncommitted\n").unwrap();

    let report = s.scanner().scan().unwrap();
    assert!(
        !report
            .origin_differs
            .iter()
            .any(|t| t.ends_with(".otherrc")),
        "a dirty but uncommitted local file is not an origin difference: {:?}",
        report.origin_differs
    );
    assert!(
        report.origin_differs.iter().any(|t| t.ends_with(".testrc")),
        "origin's version of .testrc differs: {:?}",
        report.origin_differs
    );
}

/// `source-path` is batched (spec 2026-10-11 §2.3): one call, one line per
/// target in argument order. A target chezmoi does not know makes the whole
/// call fail, and the chunk is then answered one target at a time — never
/// by zipping a short reply against the full chunk.
///
/// MUTATION: on a failed chunk, extend the result with `None` for EVERY
/// target of the chunk instead of retrying per target → the third entry
/// is `None`. (The line-count check is pinned by the scripted-runner test
/// `source_paths_retries_a_short_reply_per_target` in `chezmoi.rs`: real
/// chezmoi never answers a short reply with exit 0.)
#[test]
fn source_paths_answers_in_argument_order_and_never_misaligns() {
    let s = Scratch::new();
    let chezmoi = s.chezmoi();
    let testrc = s.home.join(".testrc");
    let unmanaged = s.home.join(".nope");
    let got = chezmoi
        .source_paths(&[testrc.clone(), unmanaged, testrc.clone()])
        .unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(
        got[0].as_deref(),
        Some(s.source.join("dot_testrc").as_path())
    );
    assert_eq!(got[1], None);
    assert_eq!(got[2], got[0]);
}

/// The scan reads ahead/behind from two `git log --name-only` sets computed
/// once (spec 2026-10-11 §2.3). A committed, unpushed source edit is
/// `local_ahead`; the per-event probe of one file still asks git per file.
///
/// MUTATION: hand `probe_file` an empty `local` set in `scan` (drop the
/// `files_touched_in("origin/main..HEAD")` call) → `local_ahead` reads
/// false here. (`remote_push_is_remote_ahead` above pins the `remote` set
/// the same way: an empty one turns its RemoteAhead into something else.)
#[test]
fn a_committed_unpushed_source_edit_is_local_ahead_in_both_probes() {
    let s = Scratch::new();
    std::fs::write(s.source.join("dot_testrc"), "a=committed\n").unwrap();
    git(&s.source, &["add", "."]);
    git(&s.source, &["commit", "-m", "local edit"]);
    let report = s.scanner().scan().unwrap();
    assert_eq!(report.drifted.len(), 1);
    assert!(
        report.drifted[0].probe.git.local_ahead,
        "{:?}",
        report.drifted[0].probe.git
    );
    assert!(!report.drifted[0].probe.git.remote_ahead);
    let one = s
        .scanner()
        .probe_one(&s.home.join(".testrc"))
        .unwrap()
        .expect("drifted");
    assert_eq!(
        one.probe.git, report.drifted[0].probe.git,
        "both probes agree"
    );
}

/// `git log --name-only` quotes non-ASCII names (`core.quotePath`); the sets
/// are read NUL-separated so they match the unquoted source path.
///
/// MUTATION: drop `-z` and split on newlines → the quoted path never matches
/// and `local_ahead` reads false.
#[test]
fn a_non_ascii_source_name_is_local_ahead() {
    let s = Scratch::new();
    std::fs::write(s.source.join("dot_café"), "x=1\n").unwrap();
    git(&s.source, &["add", "."]);
    git(&s.source, &["commit", "-m", "add cafe"]);
    let report = s.scanner().scan().unwrap();
    let d = report
        .drifted
        .iter()
        .find(|d| d.target.ends_with(".café"))
        .unwrap_or_else(|| panic!("no .café entry: {:?}", report.drifted));
    assert!(d.probe.git.local_ahead, "{:?} {:?}", d.class, d.probe.git);
}

/// A range holding a merge commit is answered per file: the sets would call
/// a file "ahead" that a merge resolved to origin's version.
///
/// MUTATION: drop the `has_merges` check (always use the sets) →
/// `local_ahead` reads true and the two probes disagree.
#[test]
fn a_range_with_a_merge_is_answered_per_file() {
    let s = Scratch::new();
    let other = s.root.path().join("other");
    support::sh(
        s.root.path(),
        "git",
        &["clone", s.bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    std::fs::write(other.join("dot_testrc"), "a=origin\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "origin edit"]);
    git(&other, &["push"]);
    std::fs::write(s.source.join("dot_testrc"), "a=local\n").unwrap();
    git(&s.source, &["add", "."]);
    git(&s.source, &["commit", "-m", "local edit"]);
    git(&s.source, &["fetch", "origin"]);
    // Conflicts; keep origin's version.
    let _ = std::process::Command::new("git")
        .current_dir(&s.source)
        .args(["merge", "origin/main"])
        .output()
        .unwrap();
    std::fs::write(s.source.join("dot_testrc"), "a=origin\n").unwrap();
    git(&s.source, &["add", "."]);
    git(&s.source, &["commit", "--no-edit", "-m", "merge"]);
    let report = s.scanner().scan().unwrap();
    let d = report
        .drifted
        .iter()
        .find(|d| d.target.ends_with(".testrc"))
        .unwrap_or_else(|| panic!("no .testrc entry: {:?}", report.drifted));
    assert!(!d.probe.git.local_ahead, "{:?}", d.probe.git);
    let one = s
        .scanner()
        .probe_one(&s.home.join(".testrc"))
        .unwrap()
        .unwrap();
    assert_eq!(one.probe.git, d.probe.git, "both probes agree");
}

/// Commit every source change and apply it, so the disk and the state match
/// the source before a test changes something.
fn commit_and_apply(s: &Scratch) {
    git(&s.source, &["add", "-A"]);
    git(&s.source, &["commit", "-m", "setup"]);
    git(&s.source, &["push"]);
    s.chezmoi().apply(None).unwrap();
}

/// Move `path` out of the home into the scratch root (never `rm -r`).
fn move_away(s: &Scratch, path: &std::path::Path) {
    let to = s.root.path().join(format!(
        "moved-{}",
        path.file_name().unwrap().to_string_lossy()
    ));
    std::fs::rename(path, to).unwrap();
}

fn type_change_of<'a>(report: &'a ScanReport, suffix: &str) -> &'a FileDrift {
    let rows: Vec<&FileDrift> = report
        .drifted
        .iter()
        .filter(|d| d.target.ends_with(suffix))
        .collect();
    assert_eq!(rows.len(), 1, "one row for {suffix}: {:?}", report.drifted);
    assert_eq!(rows[0].class, DriftClass::TypeChanged, "{:?}", rows[0]);
    rows[0]
}

fn link(text: &str) -> EntryKind {
    EntryKind::Link(text.into())
}

/// `~/.d` (a managed folder) becomes a link to `.real`, whose `one` differs
/// and which lacks `two`. One row stands for the folder; nothing for its
/// children; an unrelated edit still shows.
///
/// MUTATION: skip step 3 (make the "strictly under a root" check always
/// false) → `.d/one` and `.d/two` come back as file rows.
#[test]
fn folder_replaced_by_a_link_is_one_type_change() {
    let s = Scratch::new();
    std::fs::create_dir(s.source.join("dot_d")).unwrap();
    std::fs::write(s.source.join("dot_d/one"), "1\n").unwrap();
    std::fs::write(s.source.join("dot_d/two"), "2\n").unwrap();
    std::fs::write(s.source.join("dot_f"), "f\n").unwrap();
    commit_and_apply(&s);
    std::fs::rename(s.home.join(".d"), s.home.join(".real")).unwrap();
    std::fs::write(s.home.join(".real/one"), "changed\n").unwrap();
    std::fs::remove_file(s.home.join(".real/two")).unwrap();
    std::os::unix::fs::symlink(".real", s.home.join(".d")).unwrap();
    std::fs::write(s.home.join(".f"), "edited\n").unwrap();

    let report = s.scanner().scan().unwrap();
    let d = type_change_of(&report, ".d");
    assert_eq!(
        d.type_change,
        Some(TypeChange {
            source: EntryKind::Folder,
            disk: link(".real"),
            disk_moved: true
        })
    );
    assert!(
        !report
            .drifted
            .iter()
            .any(|d| d.target.starts_with(s.home.join(".d")) && !d.target.ends_with(".d")),
        "no rows under the folder: {:?}",
        report.drifted
    );
    assert!(
        report
            .drifted
            .iter()
            .any(|d| d.target.ends_with(".f") && d.class == DriftClass::DestinationDrift),
        "{:?}",
        report.drifted
    );
    assert!(report.unprobed.is_empty(), "{:?}", report.unprobed);
    assert_eq!(report.in_sync_count, 1, "only .testrc is in sync");
}

/// `chezmoi status` fails for the whole home when a managed folder became a
/// file (`lstat .d/one: not a directory`); the scan asks only about the
/// targets outside it.
///
/// MUTATION: always call `status()` → the scan returns that error.
#[test]
fn folder_replaced_by_a_file_does_not_break_the_scan() {
    let s = Scratch::new();
    std::fs::create_dir(s.source.join("dot_d")).unwrap();
    std::fs::write(s.source.join("dot_d/one"), "1\n").unwrap();
    std::fs::write(s.source.join("dot_f"), "f\n").unwrap();
    commit_and_apply(&s);
    move_away(&s, &s.home.join(".d"));
    std::fs::write(s.home.join(".d"), "now a file\n").unwrap();
    std::fs::write(s.home.join(".f"), "edited\n").unwrap();

    let report = s.scanner().scan().expect("the scan survives");
    let d = type_change_of(&report, ".d");
    assert_eq!(
        d.type_change,
        Some(TypeChange {
            source: EntryKind::Folder,
            disk: EntryKind::File,
            disk_moved: true
        })
    );
    assert!(
        report
            .drifted
            .iter()
            .any(|d| d.target.ends_with(".f") && d.class == DriftClass::DestinationDrift),
        "{:?}",
        report.drifted
    );
    assert!(report.unprobed.is_empty(), "{:?}", report.unprobed);
    assert!(report.degraded.is_none());
}

/// MUTATION: compare kinds only in `type_change` (return `None` when both
/// sides are the same variant) → the retargeted `.l` shows nothing.
#[test]
fn link_retargeted_and_link_removed() {
    let s = Scratch::new();
    std::fs::write(s.source.join("symlink_dot_l"), ".real").unwrap();
    std::fs::write(s.source.join("symlink_dot_m"), ".real").unwrap();
    commit_and_apply(&s);
    std::fs::remove_file(s.home.join(".l")).unwrap();
    std::os::unix::fs::symlink(".other", s.home.join(".l")).unwrap();
    std::fs::remove_file(s.home.join(".m")).unwrap();

    let report = s.scanner().scan().unwrap();
    assert_eq!(
        type_change_of(&report, ".l").type_change,
        Some(TypeChange {
            source: link(".real"),
            disk: link(".other"),
            disk_moved: true
        })
    );
    assert_eq!(
        type_change_of(&report, ".m").type_change,
        Some(TypeChange {
            source: link(".real"),
            disk: EntryKind::Absent,
            disk_moved: true
        })
    );
    assert_eq!(report.drifted.len(), 2, "{:?}", report.drifted);
}

/// A file replaced by a link is read through the link by the hash pass; the
/// type change must be known before that pass so no file row appears.
///
/// MUTATION: run detection after step (a) (step (a) no longer skips the
/// type-changed targets) → a `destination_drift` row for `.f` as well.
#[test]
fn file_replaced_by_a_link() {
    let s = Scratch::new();
    std::fs::write(s.source.join("dot_f"), "f\n").unwrap();
    commit_and_apply(&s);
    std::fs::write(s.home.join(".f-real"), "other\n").unwrap();
    std::fs::remove_file(s.home.join(".f")).unwrap();
    std::os::unix::fs::symlink(".f-real", s.home.join(".f")).unwrap();

    let report = s.scanner().scan().unwrap();
    assert_eq!(
        type_change_of(&report, ".f").type_change,
        Some(TypeChange {
            source: EntryKind::File,
            disk: link(".f-real"),
            disk_moved: true
        })
    );
    assert!(
        !report
            .drifted
            .iter()
            .any(|d| d.target.ends_with(".f") && d.class == DriftClass::DestinationDrift),
        "{:?}",
        report.drifted
    );
}

/// The source turned the folder into a link (committed, not applied): the
/// disk is what chezmoi last wrote.
///
/// MUTATION: hardcode `disk_moved: true` in `type_change` → reads true.
#[test]
fn source_changed_type_is_not_disk_moved() {
    let s = Scratch::new();
    std::fs::create_dir(s.source.join("dot_d")).unwrap();
    std::fs::write(s.source.join("dot_d/one"), "1\n").unwrap();
    commit_and_apply(&s);
    git(&s.source, &["rm", "-r", "-q", "dot_d"]);
    std::fs::write(s.source.join("symlink_dot_d"), ".real").unwrap();
    git(&s.source, &["add", "-A"]);
    git(&s.source, &["commit", "-m", "folder to link"]);

    let report = s.scanner().scan().unwrap();
    assert_eq!(
        type_change_of(&report, ".d").type_change,
        Some(TypeChange {
            source: link(".real"),
            disk: EntryKind::Folder,
            disk_moved: false
        })
    );
    assert!(report.unprobed.is_empty(), "{:?}", report.unprobed);
}

/// MUTATION: keep skipping symlink entries in the tally → 1.
#[test]
fn link_in_sync_counts_toward_in_sync() {
    let s = Scratch::new();
    std::fs::write(s.source.join("symlink_dot_l"), ".real").unwrap();
    commit_and_apply(&s);
    let report = s.scanner().scan().unwrap();
    assert!(report.drifted.is_empty(), "{:?}", report.drifted);
    assert_eq!(report.in_sync_count, 2, ".testrc and .l");
}

/// The field case: `~/.claude/skills/h` became a link into `~/.agents`,
/// which chezmoi also manages. Only the linked folder's own children are
/// covered by its row; the edited file in the link target keeps its row.
///
/// MUTATION: suppress by canonicalized path (canonicalize both the target
/// and the root before `starts_with`) → the `.agents` row disappears.
#[test]
fn linked_target_inside_another_managed_tree_keeps_its_own_rows() {
    let s = Scratch::new();
    for top in ["dot_agents", "dot_claude"] {
        let dir = s.source.join(top).join("skills/h");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "a\n").unwrap();
    }
    commit_and_apply(&s);
    move_away(&s, &s.home.join(".claude/skills/h"));
    std::os::unix::fs::symlink("../../.agents/skills/h", s.home.join(".claude/skills/h")).unwrap();
    std::fs::write(s.home.join(".agents/skills/h/SKILL.md"), "b\n").unwrap();

    let report = s.scanner().scan().unwrap();
    assert_eq!(
        type_change_of(&report, ".claude/skills/h").type_change,
        Some(TypeChange {
            source: EntryKind::Folder,
            disk: link("../../.agents/skills/h"),
            disk_moved: true
        })
    );
    assert!(
        report
            .drifted
            .iter()
            .any(|d| d.target.ends_with(".agents/skills/h/SKILL.md")
                && d.class == DriftClass::DestinationDrift),
        "{:?}",
        report.drifted
    );
    assert!(
        !report
            .drifted
            .iter()
            .any(|d| d.target.ends_with(".claude/skills/h/SKILL.md")),
        "{:?}",
        report.drifted
    );
}

/// A dangling link is still a link: never followed.
///
/// MUTATION: `disk_kind` follows the link (`std::fs::metadata`) → the folder
/// reads as deleted and `.d/one` shows as a deleted file.
#[test]
fn dangling_link_over_a_folder() {
    let s = Scratch::new();
    std::fs::create_dir(s.source.join("dot_d")).unwrap();
    std::fs::write(s.source.join("dot_d/one"), "1\n").unwrap();
    commit_and_apply(&s);
    move_away(&s, &s.home.join(".d"));
    std::os::unix::fs::symlink("gone", s.home.join(".d")).unwrap();

    let report = s.scanner().scan().unwrap();
    assert_eq!(
        type_change_of(&report, ".d").type_change,
        Some(TypeChange {
            source: EntryKind::Folder,
            disk: link("gone"),
            disk_moved: true
        })
    );
    assert_eq!(report.drifted.len(), 1, "{:?}", report.drifted);
    assert!(report.unprobed.is_empty(), "{:?}", report.unprobed);
}

/// A templated link whose text cannot be rendered has no known source kind
/// to compare: it is the existing `eval_failed` row, once.
///
/// MUTATION: map an `Eval` error from `link_text` to `Link(String::new())`
/// → a `type_changed` row instead.
#[test]
fn templated_link_that_fails_to_render_is_eval_failed() {
    let s = Scratch::new();
    std::fs::write(s.source.join("symlink_dot_t.tmpl"), "{{ fail \"x\" }}").unwrap();

    let report = s.scanner().scan().unwrap();
    let rows: Vec<&FileDrift> = report
        .drifted
        .iter()
        .filter(|d| d.target.ends_with(".t"))
        .collect();
    assert_eq!(rows.len(), 1, "{:?}", report.drifted);
    assert_eq!(rows[0].class, DriftClass::EvalFailed);
    assert!(rows[0].probe.rendered.is_err());
    assert_eq!(rows[0].type_change, None);
}

/// A file event under a linked folder must not bring a file row back; the
/// folder itself still reports its type change.
///
/// MUTATION: drop the ancestor check in `probe_one` → `.d/one` is read
/// through the link and reports `destination_drift`.
#[test]
fn probe_one_under_a_linked_folder_returns_nothing() {
    let s = Scratch::new();
    std::fs::create_dir(s.source.join("dot_d")).unwrap();
    std::fs::write(s.source.join("dot_d/one"), "1\n").unwrap();
    commit_and_apply(&s);
    std::fs::rename(s.home.join(".d"), s.home.join(".real")).unwrap();
    std::fs::write(s.home.join(".real/one"), "changed\n").unwrap();
    std::os::unix::fs::symlink(".real", s.home.join(".d")).unwrap();

    let scanner = s.scanner();
    let child = scanner.probe_one(&s.home.join(".d/one")).unwrap();
    assert!(child.is_none(), "{child:?}");
    let root = scanner
        .probe_one(&s.home.join(".d"))
        .unwrap()
        .expect("the folder's row");
    assert_eq!(root.class, DriftClass::TypeChanged);
    assert_eq!(root.type_change.map(|t| t.disk), Some(link(".real")));
}
