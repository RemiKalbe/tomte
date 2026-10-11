//! Review shell: severity sidebar + provenance timeline + read-only diff
//! preview (spec §7.2, approved mockup A; plan 5 Task 6).
//!
//! Everything blocking — `chezmoi cat`, `chezmoi source-path`, the read-only
//! journal and destination fs reads — runs on the background executor and
//! lands back in the entity via `WeakEntity::update` (spec §3.2 non-blocking
//! rule).
//!
//! This view NO LONGER RUNS RESOLUTIONS ITSELF (plan 8 Task 4). A one-click
//! decision ("keep disk" / "keep source" / "keep both") becomes a
//! [`Decision`] on the Shell's [`tomte_app::action_queue::ActionQueue`] and
//! the click is done: the row moves to PROCESSING, the selection advances to
//! the next file that still needs one, and the work drains serially on the
//! Shell. A failure comes back onto its row, where choosing again is the
//! retry — no banner, because by then the user is three files further on.
//! Undo is a repo-level op on the Shell for the same reason (it writes the
//! destination files a draining batch reads), and its outcome IS a banner:
//! it is not about the selected row. "Open merge editor" (conflicts, disk
//! edits inside a templated value) hands the selected target to the Shell's
//! full-window merge editor (plan 7 Task 3).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, FontWeight, ListAlignment,
    ListOffset, ListState, Rgba, SharedString, Stateful, WeakEntity, Window, div, list, prelude::*,
    px,
};
use tomte_app::action_queue::{Decision, QueuedAction};
use tomte_app::model::{SyncModel, class_label, kind_glyph, kind_label, time_ago};
use tomte_app::resolve::{ResolveError, ResolveOutcome};
use tomte_app::theme::Theme;
use tomte_core::chezmoi::ChezmoiError;
use tomte_core::cmd::{CommandRequest, CommandRunner, SystemRunner};
use tomte_core::merge::{MergeDocument, MergeOptions, RegionKind, worddiff::word_diff};
use tomte_journal::{EventRow, Journal};
use tomte_proto::DriftSummary;
use tomte_ui::components as ui;
use tomte_ui::components::{CheckState, TREE_INDENT};

use super::Shell;
use super::dashboard::system_now;
use super::review_selection::Selection;
use super::review_tree::{
    FolderNode, TreeRow, count_chip, safe_descendants, tree_order, tree_rows,
};

/// Fixed sidebar width per the approved mockup A (~260px).
const SIDEBAR_WIDTH: f32 = 260.;
/// The queue must stay wide enough to read a filename, and never swallow
/// the diff it exists to introduce.
pub(super) const MIN_SIDEBAR_W: f32 = 180.;
const MAX_SIDEBAR_W: f32 = 520.;
/// History: tall enough for ~3 rows by default, and it must never grow so
/// far that the diff it introduces disappears.
const HISTORY_HEIGHT: f32 = 96.;
const MIN_HISTORY_H: f32 = 40.;
const MAX_HISTORY_H: f32 = 400.;

/// Provenance rows shown for the selected target (newest first).
const PROVENANCE_LIMIT: u32 = 8;

/// Intra-line word-diff is only computed when both sides of a changed region
/// are under this size — "where cheap" (the line tints still show otherwise).
const WORD_DIFF_CHEAP_BYTES: usize = 4096;

/// Drift classes that demand a human decision (mirrors
/// `SyncModel::needs_attention`'s vocabulary, spec §7.2/§7.4).
const NEEDS_YOU_CLASSES: [&str; 3] = ["conflict", "local_source_diverged", "eval_failed"];

/// Split the drifted targets into the two sidebar groups:
/// `(needs_you, one_click)`. "Needs you" holds the human-decision classes with
/// conflicts hoisted to the front; everything else — including unknown future
/// classes, which must not block the attention queue — is "one click". Order
/// is stable within each group (and within the hoisted conflicts).
pub fn severity_groups(drifted: &[DriftSummary]) -> (Vec<DriftSummary>, Vec<DriftSummary>) {
    let mut conflicts = Vec::new();
    let mut needs_rest = Vec::new();
    let mut one_click = Vec::new();
    for d in drifted {
        if d.class == "conflict" {
            conflicts.push(d.clone());
        } else if NEEDS_YOU_CLASSES.contains(&d.class.as_str()) {
            needs_rest.push(d.clone());
        } else {
            one_click.push(d.clone());
        }
    }
    conflicts.append(&mut needs_rest);
    (conflicts, one_click)
}

/// After confirming `resolved`, select the next row that still needs a
/// decision: the one after it in display order, else the one before, else
/// none. Rows already processing are skipped — they are not answerable.
///
/// A pure function over the display order rather than a method, so the rule
/// spec §2.6 asks for ("confirming advances the selection", making a sweep
/// one click per file) is testable without a window.
///
/// `resolved` missing from `ordered` means the list moved underneath the
/// click; the selection then stays where it is rather than jumping to a row
/// chosen from a list the user was not looking at.
pub(super) fn next_after(
    resolved: &Path,
    ordered: &[PathBuf],
    processing: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let ix = ordered.iter().position(|p| p == resolved)?;
    let answerable = |p: &&PathBuf| p.as_path() != resolved && !processing(p);
    ordered[ix + 1..]
        .iter()
        .find(answerable)
        .or_else(|| ordered[..ix].iter().rev().find(answerable))
        .cloned()
}

/// The diff preview lifecycle (spec §10: loading and errors are states).
pub enum PreviewState {
    /// Nothing selected yet.
    Empty,
    /// Background load in flight for the selected target.
    Loading,
    /// 2-way-lite document ready to render (see [`flatten_document`]).
    Ready(MergeDocument),
    /// `chezmoi cat` failed to evaluate the template/secret; payload is the
    /// remediation hint from `tomte_core::chezmoi::classify_eval_stderr`.
    EvalFailed(String),
    /// Any other failure (chezmoi exit, io error reading the destination).
    Error(String),
}

/// One provenance line for the selected target, mapped from a journal
/// [`EventRow`] the same way `SyncModel::hydrate_timeline` maps timeline rows
/// (`meta.class` extraction included).
#[derive(Debug, Clone)]
pub(super) struct ProvRow {
    pub(super) ts: u64,
    pub(super) kind: String,
    pub(super) machine: String,
    pub(super) class: Option<String>,
}

impl From<EventRow> for ProvRow {
    fn from(r: EventRow) -> Self {
        let class = r
            .meta
            .as_ref()
            .and_then(|m| m.get("class"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        Self {
            ts: r.ts,
            kind: r.kind,
            machine: r.machine,
            class,
        }
    }
}

/// Journal location: TOMTE_JOURNAL override, else the app-support default.
/// Mirror of `resolve_paths` in main.rs (which mirrors
/// `tomte_daemon::settings`) — kept in the views tree (shared with the merge
/// editor) so views don't reach into the binary root module.
pub(super) fn journal_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TOMTE_JOURNAL") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Library/Application Support/Tomte/journal.db")
}

/// Where the safe-group tree roots itself ($HOME, which the tree does not
/// show — `review_tree::split`). Same env var the rest of the app resolves
/// paths from; `/` when it is unset rather than a relative `.` (the tree's
/// comparisons need an absolute root).
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Everything the detail pane needs for one target, computed entirely on the
/// background executor.
struct LoadedDetail {
    pub(super) provenance: Vec<ProvRow>,
    pub(super) preview: PreviewState,
    /// The 3-way merge dry-run assembled cleanly (zero conflicts) AND the
    /// result differs from both sides → "Keep both" is a real third option.
    pub(super) auto_merge: Option<(std::sync::Arc<tomte_app::merge_inputs::MergeInputs>, String)>,
    /// `Some` only when origin's content actually loaded for this file.
    pub(super) origin_diffs: Option<OriginDiffs>,
}

/// The merge inputs for one target — the source of BOTH optional affordances
/// below (the "Keep both" dry-run and everything origin). Loaded once per
/// selection; the previous shape ran this same load for the dry-run alone.
///
/// `None` on any failure: these are optional affordances, and the diff itself
/// reports its own failures honestly through [`load_preview_blocking`], so a
/// failed load costs the extra buttons, never the preview. Nothing is
/// swallowed silently either — every subprocess in the chain logs its own
/// failure through the command chokepoint (`SystemRunner::run`), the same
/// reasoning that already justifies `merge_inputs::load_origin`'s misses.
fn merge_inputs_blocking(
    target: &Path,
    journal: &Path,
) -> Option<std::sync::Arc<tomte_app::merge_inputs::MergeInputs>> {
    let chezmoi = crate::app_chezmoi_client();
    let git = crate::app_git_client(&chezmoi).ok()?;
    // Same fallback as the daemon's own derivation (`format!("origin/{branch}")`
    // off `head_branch()`, `daemon/src/main.rs`) and `resolve.rs`'s reconcile
    // helpers: an unreadable branch degrades to "main" rather than failing —
    // the origin pane is optional, never a hard requirement here.
    let branch = git.head_branch().unwrap_or_else(|_| "main".into());
    let remote_ref = format!("origin/{branch}");
    tomte_app::merge_inputs::load(&chezmoi, &git, &remote_ref, journal, target)
        .ok()
        .map(std::sync::Arc::new)
}

/// Dry-run the 3-way merge; `Some` only when it auto-resolves to something
/// neither side already is (otherwise Keep disk / Keep source cover it).
fn auto_merge_from(
    inputs: &std::sync::Arc<tomte_app::merge_inputs::MergeInputs>,
) -> Option<(std::sync::Arc<tomte_app::merge_inputs::MergeInputs>, String)> {
    let state = tomte_app::merge_state::MergeState::new(inputs);
    if !state.conflicts().is_empty() {
        return None;
    }
    let assembled = state.assembled()?;
    if assembled == inputs.ours || assembled == inputs.theirs {
        return None;
    }
    Some((inputs.clone(), assembled))
}

fn load_detail_blocking(target: &Path, journal: &Path) -> LoadedDetail {
    // Provenance is best-effort context, never a blocker for the diff: a
    // missing journal (fresh install, daemon not yet scanned) or a query
    // error both degrade to an empty history strip.
    let provenance = Journal::open_read_only(journal, "app")
        .and_then(|j| j.events_for(target, PROVENANCE_LIMIT))
        .map(|rows| rows.into_iter().map(ProvRow::from).collect())
        .unwrap_or_default();
    let inputs = merge_inputs_blocking(target, journal);
    LoadedDetail {
        provenance,
        preview: load_preview_blocking(target),
        auto_merge: inputs.as_ref().and_then(auto_merge_from),
        // Gated on origin's CONTENT, never on the scan's `origin_differs`
        // hint: a file added locally and never pushed is flagged as
        // differing yet has no origin blob (`blob_at` → `Ok(None)`), and a
        // comparison against nothing would render an empty side as if origin
        // had deleted the file. Same seam the merge editor's fourth pane hit.
        origin_diffs: inputs.as_ref().and_then(|i| {
            i.origin
                .as_deref()
                .map(|origin| origin_diffs(origin, &i.ours, &i.theirs))
        }),
    }
}

fn load_preview_blocking(target: &Path) -> PreviewState {
    let client = crate::app_chezmoi_client();
    let rendered = match client.cat(target) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(ChezmoiError::Eval(failure)) => return PreviewState::EvalFailed(failure.hint),
        Err(e) => return PreviewState::Error(e.to_string()),
    };
    let destination = match std::fs::read(target) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        // Deleted on disk is still a previewable drift: an empty destination
        // makes every rendered line show as source-only.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return PreviewState::Error(format!("read {}: {e}", target.display())),
    };
    // base = rendered, ours = destination, theirs = rendered — see the 2-way
    // mapping note on `flatten_document`.
    PreviewState::Ready(MergeDocument::compute(
        &rendered,
        &destination,
        &rendered,
        MergeOptions::default(),
    ))
}

gpui::actions!(review, [ClearSelection, SelectAllSafe]);

/// Key bindings for Review (active while its root has focus via the
/// "Review" key context). Registered by every app entry point next to the
/// merge editor's (`rg -n 'merge::register_keys' crates/app/src/main.rs`).
pub fn register_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        gpui::KeyBinding::new("escape", ClearSelection, Some("Review")),
        gpui::KeyBinding::new("cmd-a", SelectAllSafe, Some("Review")),
    ]);
}

/// The one-click resolutions (spec §5), shared with the dashboard's
/// inline row actions.
///
/// "Keep source" is TWO actions whenever origin holds its own version of the
/// file: keeping what this machine's source says is not the same decision as
/// keeping what the remote says. They stay one button — reading exactly
/// "Keep source" — whenever origin agrees, which is the common case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The shared "Keep" prefix IS the vocabulary: these are the three things the
// user can keep, and they read at every call site exactly as they read on the
// buttons. Dropping it for `ResolveAction::Disk` would make the code say less
// than the UI does.
#[allow(clippy::enum_variant_names)]
pub(super) enum ResolveAction {
    KeepDisk,
    /// Apply this machine's source over the destination (the source file is
    /// never touched).
    KeepSourceLocal,
    /// Make origin's content the source content, then apply it.
    KeepSourceOrigin,
}

/// The source-side actions to offer. `origin_available` must be answered from
/// origin's LOADED content, never from `DriftSummary::origin_differs`: the
/// scan flag says the repo differs from origin, which is also true for a file
/// added locally and never pushed — that file has no origin blob at all, and
/// an origin action for it could only resolve to nothing.
pub(super) fn source_actions(origin_available: bool) -> Vec<ResolveAction> {
    if origin_available {
        vec![
            ResolveAction::KeepSourceLocal,
            ResolveAction::KeepSourceOrigin,
        ]
    } else {
        vec![ResolveAction::KeepSourceLocal]
    }
}

/// The bulk pair: exactly the one-click decisions (spec §2.1). Keep source ·
/// origin needs each file's origin content shown; Keep both needs a dry-run
/// per file. Neither is a bulk.
pub(super) fn bulk_actions() -> [ResolveAction; 2] {
    [ResolveAction::KeepDisk, ResolveAction::KeepSourceLocal]
}

pub(super) fn bulk_decision(action: ResolveAction) -> Decision {
    match action {
        ResolveAction::KeepDisk => Decision::KeepDisk,
        ResolveAction::KeepSourceLocal => Decision::KeepSourceLocal,
        // Structural: `bulk_actions` never yields it. Falling back to the
        // local source would silently do something the user did not press.
        ResolveAction::KeepSourceOrigin => unreachable!("origin is never a bulk action"),
    }
}

impl ResolveAction {
    /// Short imperative label, used in failure messages and wherever the
    /// split is not offered.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::KeepDisk => "Keep disk",
            Self::KeepSourceLocal => "Keep source",
            Self::KeepSourceOrigin => "Keep source · origin",
        }
    }

    /// Button label. Only the split makes "this machine" worth saying: with
    /// origin agreeing there is nothing to disambiguate, and the common case
    /// must not get wordier.
    pub(super) fn button_label(self, split: bool) -> &'static str {
        match (self, split) {
            (Self::KeepSourceLocal, true) => "Keep source · this machine",
            _ => self.label(),
        }
    }

    /// Stable element id for the button on either surface.
    pub(super) fn button_id(self) -> &'static str {
        match self {
            Self::KeepDisk => "keep-disk",
            Self::KeepSourceLocal => "keep-source",
            Self::KeepSourceOrigin => "keep-source-origin",
        }
    }

    /// Tint: the side the action keeps, same vocabulary as the diff rows.
    fn tint(self, theme: Theme) -> Rgba {
        match self {
            Self::KeepDisk => theme.drift,
            Self::KeepSourceLocal => theme.accent,
            Self::KeepSourceOrigin => theme.remote,
        }
    }
}

/// Which two contents the diff compares. Origin is a content state of its
/// own (plan 7 Task 9), so the pane has to be told which pair to show
/// instead of always assuming disk-vs-this-machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiffPair {
    DiskVsLocal,
    DiskVsOrigin,
    LocalVsOrigin,
}

/// Every pair in selector order; `DiskVsLocal` first — it is the default and
/// the only one that exists for every file.
const DIFF_PAIRS: [DiffPair; 3] = [
    DiffPair::DiskVsLocal,
    DiffPair::DiskVsOrigin,
    DiffPair::LocalVsOrigin,
];

/// One side of a comparison: the tint its lines carry, and the legend copy
/// naming it. The same tint reads differently depending on the pair — the
/// accent side is "what the source would write" against disk, but "this
/// machine's source" against origin — so the copy lives here, not on
/// [`LineTint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiffSide {
    tint: LineTint,
    legend: &'static str,
    /// The side as a noun, for prose about the pair ("X matches Y"). The
    /// legend is a caption under a swatch and does not read as one.
    noun: &'static str,
}

const DISK_SIDE: DiffSide = DiffSide {
    tint: LineTint::Destination,
    legend: "on disk now",
    noun: "the destination",
};
const SOURCE_SIDE: DiffSide = DiffSide {
    tint: LineTint::Rendered,
    legend: "source would write",
    noun: "the rendered source",
};
const LOCAL_SOURCE_SIDE: DiffSide = DiffSide {
    tint: LineTint::Rendered,
    legend: "this machine's source",
    noun: "this machine's source",
};
const ORIGIN_WRITES_SIDE: DiffSide = DiffSide {
    tint: LineTint::Origin,
    legend: "origin would write",
    noun: "origin's version",
};
const ORIGIN_SOURCE_SIDE: DiffSide = DiffSide {
    tint: LineTint::Origin,
    legend: "origin's source",
    noun: "origin's version",
};

impl DiffPair {
    /// Names BOTH sides: a comparison the user picked between three is only
    /// unambiguous if the label says what is being held up against what.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::DiskVsLocal => "on disk ↔ source · this machine",
            Self::DiskVsOrigin => "on disk ↔ source · origin",
            Self::LocalVsOrigin => "source: this machine ↔ origin",
        }
    }

    fn segment_id(self) -> &'static str {
        match self {
            Self::DiskVsLocal => "diff-pair-disk-local",
            Self::DiskVsOrigin => "diff-pair-disk-origin",
            Self::LocalVsOrigin => "diff-pair-local-origin",
        }
    }

    /// Tint of the selected segment: the side that distinguishes this
    /// comparison from the default one.
    fn tint(self, theme: Theme) -> Rgba {
        match self {
            Self::DiskVsLocal => theme.accent,
            Self::DiskVsOrigin | Self::LocalVsOrigin => theme.remote,
        }
    }

    /// What an empty diff means for THIS pair. Composed from the two sides so
    /// the note can never describe a comparison other than the one on screen —
    /// "destination matches the rendered source" was hardcoded, and it is a
    /// false statement on `DiskVsOrigin` (reachable whenever another machine
    /// pushed a change this machine's disk already has).
    fn matches_note(self) -> SharedString {
        let (base, ours) = self.sides();
        format!("{} matches {}", ours.noun, base.noun).into()
    }

    /// `(base-side, ours-side)` exactly as [`MergeDocument::compute`] sees
    /// them in [`origin_diffs`] and [`load_preview_blocking`].
    fn sides(self) -> (DiffSide, DiffSide) {
        match self {
            Self::DiskVsLocal => (SOURCE_SIDE, DISK_SIDE),
            Self::DiskVsOrigin => (ORIGIN_WRITES_SIDE, DISK_SIDE),
            Self::LocalVsOrigin => (LOCAL_SOURCE_SIDE, ORIGIN_SOURCE_SIDE),
        }
    }
}

/// The two comparisons that need origin's content, computed once when a file
/// is selected. Their existence IS the gate for every origin affordance in
/// this view — they are built from loaded content, so they cannot exist for a
/// file that has no origin blob.
pub(super) struct OriginDiffs {
    /// Origin's text these documents were built from — i.e. exactly what the
    /// user is looking at. `keep_source_origin` re-loads origin at click time
    /// and refuses if it no longer matches this, so a fetch between selecting
    /// and clicking can never write unreviewed content into the source.
    seen: String,
    disk_vs_origin: MergeDocument,
    local_vs_origin: MergeDocument,
}

/// Build both origin comparisons. Same 2-way-lite shape as the preview (see
/// [`flatten_document`]): `compute(x, y, x)` degenerates to a 2-way diff with
/// `x` on the base side and `y` on the ours side.
pub(super) fn origin_diffs(origin: &str, disk: &str, local: &str) -> OriginDiffs {
    OriginDiffs {
        seen: origin.to_owned(),
        disk_vs_origin: MergeDocument::compute(origin, disk, origin, MergeOptions::default()),
        local_vs_origin: MergeDocument::compute(local, origin, local, MergeOptions::default()),
    }
}

/// Moved to tomte-ui; re-exported so sibling views keep their import paths.
pub(crate) use tomte_ui::BannerTint;

/// The slim banner above the diff reporting the last action's outcome
/// (spec §10: honest, including degraded commit/push results). Shared with
/// the merge editor, which hands its success banners back to this view.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OutcomeBanner {
    pub(crate) text: SharedString,
    pub(crate) tint: BannerTint,
    /// Successful resolutions offer an Undo button (spec §6.3).
    pub(crate) undoable: bool,
}

/// A managed file as the UI names it: the file name, not the path.
///
/// One function because the name has to MATCH across surfaces — the sidebar
/// row, the detail header, the merge editor's title, the landed banner and
/// the desktop notification all say the same word for the same file, and a
/// banner that named it differently from the row would read as a different
/// file. Falls back to the whole path only for a target with no final
/// component, which no managed destination has.
pub(super) fn display_name(target: &Path) -> SharedString {
    target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.display().to_string())
        .into()
}

/// The banner for a click this view REFUSES before it can become a
/// [`Decision`] at all.
///
/// All that is left of a function that used to map every action RESULT onto a
/// banner. Results come back through the queue now, minutes later, and are
/// keyed by the decision that ran rather than by the button — see
/// [`queued_outcome_banner`]. What only this view can answer is a press it
/// cannot turn into a decision in the first place, and there is exactly one:
/// "keep source · origin" with no origin text loaded, which the engine would
/// refuse anyway (`ResolveEngine::keep_source_origin` will not write content
/// the user was never shown).
///
/// `split` is whether the source action was split in two when the user
/// pressed it: with two "Keep source …" buttons on screen, a refusal that
/// says only "Keep source failed" leaves them guessing which one it was. Same
/// sentence shape as the queue's own failures, so the two read alike wherever
/// they land.
fn refused_banner(action: ResolveAction, split: bool, why: &str) -> OutcomeBanner {
    OutcomeBanner {
        text: format!("{} failed: {why}", action.button_label(split)).into(),
        tint: BannerTint::Conflict,
        undoable: false,
    }
}

/// The banner a decision earns when it LANDS — or `None`, which is most of
/// the possible answers.
///
/// SUCCESSES ONLY, and that is what the banner is FOR: it carries the Undo
/// button, and its sentence has to name what that button would revert. Undo
/// is per file (spec §2.3) and `ResolveEngine::undo_last` reverts the LAST
/// FINISHED journal session, which for a batch is its last item
/// (`resolve_batch` ends one session per file in batch order). So
/// [`super::Shell::batch_finished`] emits one of these per landed target, in
/// result order, and the last wins — naming an earlier one would put the
/// button over the wrong file's name.
///
/// A FAILURE — an `Err` or a protected-span refusal — gets `None`.
/// Its home is the row (spec §2.5: "a failed row shows the message in place,
/// its buttons re-enable, and choosing again retries"), which is where the
/// retry is and where it survives until the file is answered or stops
/// drifting. Putting it in the banner as well would give it a second, shorter
/// life AND take the banner away from the last thing that really landed,
/// leaving Undo pointing at a file the sentence above it does not name.
///
/// A staged repo conflict is `None` too: it has not landed anywhere yet, and
/// its merge's own conclusion ([`super::Shell::conclude_staged_merge`]) is
/// the banner for it.
///
/// Keyed by [`Decision`] rather than by [`ResolveAction`] — half the
/// decisions the queue runs (a merge-editor Save, "Keep both") are not one of
/// Review's three buttons at all. [`Decision::verb`] is the shared
/// vocabulary: what the choice DID, in the user's own words.
pub(super) fn queued_outcome_banner(
    decision: &Decision,
    target: &Path,
    result: &Result<super::BatchResult, ResolveError>,
) -> Option<OutcomeBanner> {
    let Ok(super::BatchResult::Resolved(ResolveOutcome::Done {
        committed,
        pushed,
        note,
        ..
    })) = result
    else {
        return None;
    };
    // NAMES THE FILE, always. The banner renders under the SELECTED file's
    // header, and spec §2.6 makes the two different by design: confirming a
    // file advances the selection to the next one that still needs an answer,
    // so by the time this lands the user is looking at a different file
    // entirely. Without the name, "Kept disk version · committed & pushed"
    // above a live Undo reads as a statement about whatever is on screen —
    // while Undo reverts the file this sentence is actually about.
    let name = display_name(target);
    Some(match note {
        // An apply-only convergence commits nothing, so only advertise the
        // commit and the push when they actually happened.
        None => OutcomeBanner {
            text: if *committed && *pushed {
                format!("{} · {name} · committed & pushed", decision.verb()).into()
            } else {
                format!("{} · {name}", decision.verb()).into()
            },
            tint: BannerTint::Ok,
            undoable: true,
        },
        // The resolution landed; the commit or push degraded. Still undoable
        // — the session exists either way.
        Some(note) => OutcomeBanner {
            text: format!("{} · {name} · {note}", decision.verb()).into(),
            tint: BannerTint::Drift,
            undoable: true,
        },
    })
}

/// Banner for the undo action itself (never undoable again: the undo session
/// journals no destination blobs, so a second undo would restore nothing).
///
/// `pub(super)` because the undo now runs on the Shell, behind its latch:
/// the completion there is what hands this banner back to this view.
pub(super) fn undo_banner(result: &Result<Option<i64>, ResolveError>) -> OutcomeBanner {
    match result {
        Ok(Some(_)) => OutcomeBanner {
            text: "restored files from snapshots".into(),
            tint: BannerTint::Ok,
            undoable: false,
        },
        Ok(None) => OutcomeBanner {
            text: "nothing to undo".into(),
            tint: BannerTint::Drift,
            undoable: false,
        },
        Err(e) => OutcomeBanner {
            text: format!("undo failed: {e}").into(),
            tint: BannerTint::Conflict,
            undoable: false,
        },
    }
}

/// Which side of the comparison a preview line belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineTint {
    /// Both sides agree — muted context.
    Context,
    /// Line this machine's rendered source carries.
    Rendered,
    /// Line as it exists on disk.
    Destination,
    /// Line origin's rendered source carries.
    Origin,
}

impl LineTint {
    fn color(self, theme: Theme) -> Rgba {
        match self {
            Self::Context => theme.text_muted,
            Self::Rendered => theme.accent,
            Self::Destination => theme.drift,
            Self::Origin => theme.remote,
        }
    }
}

/// One uniform-height preview row, precomputed so the `uniform_list` closure
/// owns plain data (same idiom as the dashboard's `RowData`).
#[derive(Debug, Clone, PartialEq)]
struct DiffLine {
    /// Display text: the source line minus its trailing newline.
    text: SharedString,
    tint: LineTint,
    /// Gutter marker: " " context, "●" on the owning side's tint. Never
    /// +/− — those mean add/delete in every diff tool, not sides.
    marker: &'static str,
    /// Word-diff byte ranges into `text` to emphasize within changed pairs.
    highlights: Vec<Range<usize>>,
}

pub(super) fn display_text(line: &str) -> &str {
    line.strip_suffix('\n').unwrap_or(line)
}

/// 2-WAY MAPPING. Every comparison feeds `MergeDocument::compute(base = x,
/// ours = y, theirs = x)`. Because `theirs` is byte-identical to `base`, the
/// 3-way engine degenerates into a 2-way diff of y-vs-x:
///   - `Unchanged` → the two agree → muted context lines;
///   - `OursOnly`  → they differ → `region.base` holds the base side's lines
///     and `region.ours` the ours side's, each tinted by `pair.sides()`, with
///     word-diff emphasis where cheap.
///
/// `TheirsOnly`, `BothSame`, and `Conflict` are unreachable while
/// theirs == base; they are still handled (context / context / changed pair)
/// so a future true-3-way upgrade fails soft rather than mis-rendering.
fn flatten_document(doc: &MergeDocument, pair: DiffPair) -> Vec<DiffLine> {
    let (base_side, ours_side) = pair.sides();
    let context = |line: &str| DiffLine {
        text: display_text(line).to_owned().into(),
        tint: LineTint::Context,
        marker: " ",
        highlights: Vec::new(),
    };
    let mut out = Vec::new();
    for region in &doc.regions {
        match region.kind {
            RegionKind::Unchanged | RegionKind::TheirsOnly => {
                out.extend(
                    doc.base_lines()[region.base.clone()]
                        .iter()
                        .map(|l| context(l)),
                );
            }
            RegionKind::BothSame => {
                out.extend(
                    doc.ours_lines()[region.ours.clone()]
                        .iter()
                        .map(|l| context(l)),
                );
            }
            RegionKind::OursOnly | RegionKind::Conflict => {
                let base = &doc.base_lines()[region.base.clone()];
                let ours = &doc.ours_lines()[region.ours.clone()];
                let (hl_base, hl_ours) = region_highlights(base, ours);
                for (line, highlights) in base.iter().zip(hl_base) {
                    out.push(DiffLine {
                        text: display_text(line).to_owned().into(),
                        tint: base_side.tint,
                        marker: "●",
                        highlights,
                    });
                }
                for (line, highlights) in ours.iter().zip(hl_ours) {
                    out.push(DiffLine {
                        text: display_text(line).to_owned().into(),
                        tint: ours_side.tint,
                        marker: "●",
                        highlights,
                    });
                }
            }
        }
    }
    out
}

/// Per-line word-diff highlight ranges for one side of a changed region.
type LineRanges = Vec<Vec<Range<usize>>>;

/// Word-level changed ranges for one changed region, split per line. Cheap
/// only: both sides non-empty and under [`WORD_DIFF_CHEAP_BYTES`], otherwise
/// no intra-line emphasis (the line tints still carry the information).
fn region_highlights(base: &[String], ours: &[String]) -> (LineRanges, LineRanges) {
    let none = || (vec![Vec::new(); base.len()], vec![Vec::new(); ours.len()]);
    if base.is_empty() || ours.is_empty() {
        return none();
    }
    let a = base.concat();
    let b = ours.concat();
    if a.len() > WORD_DIFF_CHEAP_BYTES || b.len() > WORD_DIFF_CHEAP_BYTES {
        return none();
    }
    let wd = word_diff(&a, &b);
    (
        per_line_ranges(base, &wd.changed_a),
        per_line_ranges(ours, &wd.changed_b),
    )
}

/// Split byte ranges over the concatenation of `lines` into per-line local
/// ranges, clamped to each line's display text (trailing newline excluded).
fn per_line_ranges(lines: &[String], ranges: &[Range<usize>]) -> Vec<Vec<Range<usize>>> {
    let mut out = Vec::with_capacity(lines.len());
    let mut offset = 0usize;
    for line in lines {
        let display_len = display_text(line).len();
        let mut local = Vec::new();
        for r in ranges {
            let start = r.start.max(offset).saturating_sub(offset);
            let end = r.end.min(offset + line.len()).saturating_sub(offset);
            let end = end.min(display_len);
            if start < end {
                local.push(start..end);
            }
        }
        out.push(local);
        offset += line.len();
    }
    out
}

/// What one processing row is waiting on, in the user's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueuedStatus {
    /// Handed to the engine; the git work for it is under way.
    Running,
    /// Recorded, waiting for the repo to be free.
    Waiting,
}

impl QueuedStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Running => "running…",
            Self::Waiting => "queued",
        }
    }

    /// Is this decision past changing?
    ///
    /// THE ONE HOME OF THAT RULE. A row is locked out of being re-selected
    /// and its buttons go inert exactly when its decision is already under
    /// the latch; a decision that has not started can still be replaced
    /// (spec §2.4, `ActionQueue::push`), and re-choosing it is the only way
    /// to correct a mis-click — the selection has already advanced off the
    /// row by then, so the row click is the only route back to its buttons.
    ///
    /// Both readers of that rule call this: [`blocked_reason`] through
    /// [`QueueSnapshot::locks_row`] for the header's buttons, and
    /// `ReviewView::target_row` for the row's own `cursor_pointer` and
    /// `on_click`. Spelling the comparison out a second time is how round
    /// 1's fix silently regressed on one line.
    fn locks_row(self) -> bool {
        matches!(self, Self::Running)
    }
}

/// One processing row's whole state: which choice, and how far along.
///
/// The CHOICE is here because a queue of thirty rows all reading "running…"
/// says nothing about what is happening to any of them — the file name is
/// the only thing distinguishing them, and the decision the user made on it
/// minutes ago is exactly what they are trying to remember. The queue keeps
/// its running actions (not just their targets) so this can be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct QueuedDecision {
    pub(super) status: QueuedStatus,
    /// The user's own words for the choice — `Decision::label`.
    pub(super) label: &'static str,
}

impl QueuedDecision {
    /// The row's trailing column: "Keep disk · running…".
    fn trailing(self) -> SharedString {
        format!("{} · {}", self.label, self.status.label()).into()
    }

    /// Past changing — see [`QueuedStatus::locks_row`], the rule itself.
    fn locks_row(self) -> bool {
        self.status.locks_row()
    }

    /// Still takes a ✕: the decision has not started (spec 2026-10-11 §2.4).
    fn cancellable(self) -> bool {
        matches!(self.status, QueuedStatus::Waiting)
    }
}

/// The Shell's action queue as one render pass needs to see it.
///
/// Copied out of the Shell ONCE per render rather than read row by row:
/// every row and button below needs `cx` mutably for its click listener, and
/// a live borrow of the Shell entity cannot be held across that.
#[derive(Default, Clone, PartialEq)]
pub(super) struct QueueSnapshot {
    /// Everything queued or running, in `ActionQueue::processing` order —
    /// the running batch first, then the rest as the user chose them.
    processing: Vec<(PathBuf, QueuedDecision)>,
    /// Rows whose last decision failed, with the message that says why.
    failures: BTreeMap<PathBuf, SharedString>,
    /// ANY git work is owed — a decision, or a repo op waiting for the latch.
    busy: bool,
}

impl QueueSnapshot {
    fn status(&self, target: &Path) -> Option<QueuedDecision> {
        self.processing
            .iter()
            .find(|(p, _)| p == target)
            .map(|(_, s)| *s)
    }

    /// Queued OR running: a decision is recorded for this target, so it does
    /// not need answering and the auto-advance skips it.
    fn is_processing(&self, target: &Path) -> bool {
        self.status(target).is_some()
    }

    /// Is this target's decision past changing — see
    /// [`QueuedStatus::locks_row`], which is the rule itself.
    fn locks_row(&self, target: &Path) -> bool {
        self.status(target).is_some_and(QueuedDecision::locks_row)
    }

    fn failure(&self, target: &Path) -> Option<&SharedString> {
        self.failures.get(target)
    }
}

/// What the outcome banner's trailing slot offers.
///
/// Split out of `banner_el` so the rule is a value a test can assert on:
/// which of the three the banner shows is the whole of Undo's gating, and
/// a render tree is not something a test can read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UndoAffordance {
    /// This outcome cannot be undone (a failure, a protected-span refusal, an
    /// undo's own banner).
    None,
    Live,
    /// Offered but inert: git work is owed, and "undo the last thing that
    /// landed" is unambiguous only when nothing is landing.
    Blocked,
}

/// Why the selected file's decision buttons (and its "Open merge editor")
/// are inert, or `None` when they are live.
///
/// ONE reason, and it is about THIS file: its decision is already RUNNING.
/// Every other file's buttons stay live while this one's work drains, which
/// is the whole point of the queue (spec §1.1), and RUNNING is not the same
/// as queued — a pending decision can still be changed, and
/// `ActionQueue::push` replaces it (spec §2.4).
///
/// A MISSING DAEMON IS NOT A REASON. It was, and that was the same mistake
/// the `in_flight` booleans were: refusing to remember a decision because it
/// cannot run this second. Parking is the queue's model — `Shell::pump`
/// leaves everything queued when no engine is published and the connect latch
/// in `Shell::render` releases it on the next (re)connect — so the press is
/// recorded, the row reads "queued", and it runs when the daemon is back.
///
/// Pure so that rule is a value a test can assert, rather than something only
/// a tooltip in a screenshot could show.
pub(super) fn blocked_reason(queue: &QueueSnapshot, target: &Path) -> Option<SharedString> {
    if queue.locks_row(target) {
        return Some("applying this file's decision now".into());
    }
    None
}

/// Undo's gate, in one place. Shown-but-inert rather than hidden: a button
/// that vanishes teaches the user nothing about why.
pub(super) fn undo_affordance(banner: &OutcomeBanner, queue: &QueueSnapshot) -> UndoAffordance {
    match (banner.undoable, queue.busy) {
        (false, _) => UndoAffordance::None,
        (true, true) => UndoAffordance::Blocked,
        (true, false) => UndoAffordance::Live,
    }
}

/// Flat: the historical two-group list. Tree: the safe group compacted into
/// folders (spec §2.5); NEEDS A DECISION and PROCESSING stay flat either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReviewMode {
    Flat,
    Tree,
}

/// One entry of the sidebar, in display order.
///
/// The list is built as data before it is built as elements, because the
/// rule that a row appears EXACTLY ONCE — under PROCESSING while a decision
/// is recorded for it, in its severity group otherwise — is invisible in a
/// screenshot the moment it breaks (the duplicate looks like a second file)
/// and unreachable from a render tree.
// No `PartialEq`, `Debug`, or `Clone`: `DriftSummary` has none, `FolderNode`
// (Task 3's pure tree projection) carries none either, and the tests below
// assert on a projection of these rows (label / file name / state) rather
// than on whole rows anyway.
pub(super) enum SidebarRow {
    Group {
        label: &'static str,
        /// The SAFE TO RESOLVE header's select-all; `None` on the others.
        /// Rendered by `group_header_el`.
        check: Option<CheckState>,
    },
    /// Tree mode only: a folder of the safe group. Flat mode never emits one.
    Folder { node: FolderNode, check: CheckState },
    Target {
        entry: DriftSummary,
        queued: Option<QueuedDecision>,
        /// Past changing: not a click target, and its buttons are inert.
        ///
        /// A FIELD, not something the row recomputes. It is derived from
        /// `queued` through [`QueuedDecision::locks_row`] right here, in the
        /// one function a test can read, and `ReviewView::target_row` then
        /// uses it verbatim. Deriving it a second time at the render site is
        /// exactly how round 1's fix regressed on one untested line — so
        /// this apparent redundancy is the point, not something to tidy
        /// away.
        locked: bool,
        /// Under PROCESSING and not yet running: the row carries a ✕ that
        /// removes the decision from the queue. A field like `locked`, for
        /// the same reason — derived once here, read verbatim by the row.
        cancellable: bool,
        /// Tree indent level; 0 in flat mode. Read by `target_row`'s
        /// indent; Task 7 is what makes it ever non-zero.
        depth: usize,
        /// A safe row: carries a checkbox, answers ⌘/⇧-clicks.
        selectable: bool,
        checked: bool,
    },
}

pub(super) struct SidebarPlan {
    pub rows: Vec<SidebarRow>,
    /// Every safe target in display order, collapsed ones included — what
    /// `Selection` is kept in the order of and pruned against.
    pub safe_order: Vec<PathBuf>,
    /// The selectable `Target` rows actually on screen, in row order — what
    /// a ⇧-range and `next_after` walk.
    pub visible_safe: Vec<PathBuf>,
}

/// What identifies a sidebar row to the list element's height cache
/// (spec 2026-10-11 §2.2). A row whose key is unchanged across a plan
/// rebuild keeps its measured height; anything else is re-measured. The
/// queued status and the failure are in the key because they add a
/// sub-line: same path, different height.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RowKey {
    Group(&'static str),
    Folder(PathBuf),
    Target {
        path: PathBuf,
        queued: Option<QueuedStatus>,
        failed: bool,
    },
}

pub(super) fn row_keys(plan: &SidebarPlan, queue: &QueueSnapshot) -> Vec<RowKey> {
    plan.rows
        .iter()
        .map(|row| match row {
            SidebarRow::Group { label, .. } => RowKey::Group(label),
            SidebarRow::Folder { node, .. } => RowKey::Folder(node.path.clone()),
            SidebarRow::Target { entry, queued, .. } => RowKey::Target {
                path: entry.target.clone(),
                queued: queued.map(|q| q.status),
                failed: queue.failure(&entry.target).is_some(),
            },
        })
        .collect()
}

/// The window `ListState::splice` must be told about so that unchanged
/// rows keep their measured heights and the scroll anchor stays on its
/// row: `(old_range, new_count)` for the middle that differs, `None` when
/// the two lists are identical. `splice(0..old, new)` would snap the
/// anchor to item 0 on every rebuild (gpui 0.2.2 `list.rs`, `splice_focusable`).
pub(super) fn splice_window(old: &[RowKey], new: &[RowKey]) -> Option<(Range<usize>, usize)> {
    if old == new {
        return None;
    }
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
        .min(max_suffix);
    Some((prefix..old.len() - suffix, new.len() - prefix - suffix))
}

/// Where the list's scroll anchor belongs after the rows changed from `old`
/// to `new`: the new index of the row the anchor was on (`old[anchor_ix]`),
/// or, when that row left the list (it was resolved, or moved to
/// PROCESSING with a new key), the new index of the nearest LATER old row
/// that survived, else of the nearest earlier one. `None` when the anchor
/// was not on a row (an empty list, or the footer past the last row) or no
/// old row survived.
///
/// Needed because gpui 0.2.2's `ListState::splice` snaps the anchor to the
/// window's START whenever the anchor lies INSIDE the spliced window
/// (`list.rs` `splice_focusable`), and a decision on a row below the anchor
/// splices from the PROCESSING group down past it.
pub(super) fn re_anchor_index(old: &[RowKey], new: &[RowKey], anchor_ix: usize) -> Option<usize> {
    let index_in_new = |key: &RowKey| new.iter().position(|k| k == key);
    let anchor = old.get(anchor_ix)?;
    index_in_new(anchor)
        .or_else(|| old[anchor_ix + 1..].iter().find_map(index_in_new))
        .or_else(|| old[..anchor_ix].iter().rev().find_map(index_in_new))
}

pub(super) struct SidebarInputs<'a> {
    pub drifted: &'a [DriftSummary],
    pub queue: &'a QueueSnapshot,
    pub mode: ReviewMode,
    pub collapsed: &'a BTreeSet<PathBuf>,
    pub selection: &'a Selection,
    pub home: &'a Path,
}

/// Plan the sidebar: the PROCESSING group first (spec §2.1 — the row MOVES
/// there), then NEEDS A DECISION flat, then SAFE TO RESOLVE flat or as a
/// tree (spec §2.5 of the multi-select design). Only the safe group's rows
/// are selectable, and the header's checkbox stands for all of them.
pub(super) fn sidebar_rows(inputs: SidebarInputs<'_>) -> SidebarPlan {
    let SidebarInputs {
        drifted,
        queue,
        mode,
        collapsed,
        selection,
        home,
    } = inputs;
    let mut rows = Vec::new();
    if !queue.processing.is_empty() {
        rows.push(SidebarRow::Group {
            label: "PROCESSING",
            check: None,
        });
        for (target, queued) in &queue.processing {
            let entry = drifted
                .iter()
                .find(|d| d.target == *target)
                .cloned()
                .unwrap_or_else(|| DriftSummary {
                    target: target.clone(),
                    class: String::new(),
                    since_ts: None,
                    origin_differs: false,
                });
            rows.push(SidebarRow::Target {
                entry,
                queued: Some(*queued),
                locked: queued.locks_row(),
                cancellable: queued.cancellable(),
                depth: 0,
                selectable: false,
                checked: false,
            });
        }
    }
    let (needs_you, one_click) = severity_groups(drifted);
    let needs_you: Vec<DriftSummary> = needs_you
        .into_iter()
        .filter(|d| !queue.is_processing(&d.target))
        .collect();
    let safe: Vec<DriftSummary> = one_click
        .into_iter()
        .filter(|d| !queue.is_processing(&d.target))
        .collect();
    if !needs_you.is_empty() {
        rows.push(SidebarRow::Group {
            label: "NEEDS A DECISION",
            check: None,
        });
        rows.extend(needs_you.iter().map(|entry| SidebarRow::Target {
            entry: entry.clone(),
            queued: None,
            locked: false,
            cancellable: false,
            depth: 0,
            selectable: false,
            checked: false,
        }));
    }
    let mut safe_order: Vec<PathBuf> = Vec::new();
    let mut visible_safe: Vec<PathBuf> = Vec::new();
    if !safe.is_empty() {
        let safe_targets: Vec<PathBuf> = safe.iter().map(|d| d.target.clone()).collect();
        safe_order = match mode {
            ReviewMode::Flat => safe_targets.clone(),
            ReviewMode::Tree => tree_order(&safe_targets, home),
        };
        rows.push(SidebarRow::Group {
            label: "SAFE TO RESOLVE",
            check: Some(selection.check_state(&safe_order)),
        });
        let target_row = |entry: &DriftSummary, depth: usize| SidebarRow::Target {
            entry: entry.clone(),
            queued: None,
            locked: false,
            cancellable: false,
            depth,
            selectable: true,
            checked: selection.contains(&entry.target),
        };
        match mode {
            ReviewMode::Flat => {
                for entry in &safe {
                    visible_safe.push(entry.target.clone());
                    rows.push(target_row(entry, 0));
                }
            }
            ReviewMode::Tree => {
                // Excluded = every drifted row that is not safe (needs-you or
                // processing), plus every processing target the drift model
                // has already dropped (a rescan can clear a row while its
                // decision is still running, spec §2.5): both are counted by
                // the folder chips, never listed here. Without the second
                // half a folder that lost its only other file this way would
                // undercount its own total.
                let mut excluded: Vec<PathBuf> = drifted
                    .iter()
                    .filter(|d| !safe_targets.contains(&d.target))
                    .map(|d| d.target.clone())
                    .collect();
                for (target, _) in &queue.processing {
                    if !drifted.iter().any(|d| d.target == *target) {
                        excluded.push(target.clone());
                    }
                }
                for row in tree_rows(&safe_targets, &excluded, collapsed, home) {
                    match row {
                        TreeRow::Folder(node) => {
                            // Equivalent to `safe_descendants(&safe_targets,
                            // &node.path, home)` but reuses the `safe_order`
                            // already computed above instead of rebuilding
                            // the whole tree per folder row (final review,
                            // Important 2).
                            let under: Vec<PathBuf> = safe_order
                                .iter()
                                .filter(|t| t.starts_with(&node.path))
                                .cloned()
                                .collect();
                            let check = selection.check_state(&under);
                            rows.push(SidebarRow::Folder { node, check });
                        }
                        TreeRow::File { target, depth } => {
                            let entry = safe
                                .iter()
                                .find(|d| d.target == target)
                                .expect("tree rows come from `safe`");
                            visible_safe.push(target.clone());
                            rows.push(target_row(entry, depth));
                        }
                    }
                }
            }
        }
    }
    SidebarPlan {
        rows,
        safe_order,
        visible_safe,
    }
}

/// What `sidebar_rows` reads besides the model, compared by value before
/// every rebuild (spec 2026-10-11 §2.3). The model is not in here: its
/// changes arrive through the view's observer as `plan_dirty`. `home()` is
/// left out too: it is constant for the process.
///
/// The observer is deferred: a `plan()` read inside the SAME outer update
/// that changed the model sees the previous plan. No caller does that today;
/// gestures are input-driven.
#[derive(PartialEq)]
struct PlanKey {
    queue: QueueSnapshot,
    mode: ReviewMode,
    collapsed: BTreeSet<PathBuf>,
    selection: Selection,
}

pub struct ReviewView {
    state: Entity<SyncModel>,
    /// Back-reference for cross-view navigation ("Open merge editor" routes
    /// through [`Shell::open_merge`]). Weak: the shell owns this view.
    shell: WeakEntity<Shell>,
    /// `pub(super)` so the render-smoke tests can pose a selected target
    /// without spawning the background detail load.
    pub(super) selected: Option<PathBuf>,
    /// The disk-vs-this-machine comparison — the default pair, and the only
    /// one that carries the load's own failure states.
    pub(super) preview: PreviewState,
    /// Which comparison the diff shows. Reset to the default on every
    /// selection (and on every reload): after a resolution the file's origin
    /// dimension may be gone entirely.
    pub(super) diff_pair: DiffPair,
    /// The two origin comparisons for the selected file, `Some` only when
    /// origin's content loaded. This — not `DriftSummary::origin_differs` —
    /// gates the selector, the origin diff, and the origin action.
    pub(super) origin_diffs: Option<OriginDiffs>,
    /// Provenance for the selected target, newest first (background-loaded
    /// together with the preview).
    pub(super) provenance: Vec<ProvRow>,
    /// Clean 3-way dry-run for the selected target → "Keep both" offered.
    pub(super) auto_merge: Option<(std::sync::Arc<tomte_app::merge_inputs::MergeInputs>, String)>,
    /// Outcome of the last resolve/undo action, rendered as a banner above
    /// the diff. `pub(super)` for the render-smoke tests.
    pub(super) last_outcome: Option<OutcomeBanner>,
    /// The diff is a read-only [`ui::TextArea`] (2026-08-23 feedback: "diffs
    /// should be selectable"): same editor as the merge result, minus the
    /// decision strips — so a diff can be selected and copied like text.
    /// `pub(super)` so a render-smoke test can read its `selected_text()`
    /// after a real click + ⌘A — the only way to prove the editor, not the
    /// sidebar, kept the click's focus.
    pub(super) preview_editor: Option<Entity<ui::TextArea>>,
    /// Rebuild the editor's content + decorations on the next render (the
    /// preview changed, or the theme flipped).
    preview_dirty: bool,
    preview_appearance: Option<gpui::WindowAppearance>,
    /// Draggable sidebar width (2026-08-27); `None` until first render.
    pub(super) sidebar_width: Option<f32>,
    sidebar_drag: Option<ui::DragState<()>>,
    /// Draggable History height, and its in-flight drag.
    history_height: Option<f32>,
    history_drag: Option<ui::DragState<()>>,
    /// Flat list or folder tree (spec §2.5). `pub(super)` for the
    /// render-smoke tests and the gallery poses.
    pub(super) mode: ReviewMode,
    /// The SAFE TO RESOLVE rows ticked right now. `pub(super)` for the same
    /// reason.
    pub(super) selection: Selection,
    /// Tree-mode folders the user closed, by their (compacted) path.
    /// `pub(super)` for the same reason; empty and inert in Flat mode.
    pub(super) collapsed: BTreeSet<PathBuf>,
    /// Backs the "Review" key context (Esc / ⌘A) — see [`register_keys`].
    /// `render_inner`'s `track_focus` alone gives the root this on any click
    /// no focusable descendant claims first (gpui's own protocol, not
    /// something this view writes) — the diff pane's read-only editor is the
    /// one descendant in Review that can claim it instead, and does. A
    /// hand-written `on_mouse_down` here once re-focused the root
    /// unconditionally, after every child's own focus listener in the
    /// bubble, so a click inside the diff editor could never keep its own
    /// focus: do not add one back. The merge editor is a non-issue either
    /// way — the two never render at once (routes are exclusive).
    /// `pub(super)` so the render-smoke tests can focus it directly, the
    /// same reason `selection` is.
    pub(super) focus_handle: gpui::FocusHandle,
    /// The plan the sidebar is showing, with the inputs it was built from.
    /// `plan` hands the `Rc` back while the key still matches and the
    /// model has not changed; a 500-row plan was being rebuilt on every
    /// hover before this (2026-10-11).
    plan_cache: Option<(PlanKey, Rc<SidebarPlan>)>,
    /// Set by the model observer; the next `plan` rebuilds.
    plan_dirty: bool,
    /// The virtualized sidebar's scroll state and height cache (spec
    /// 2026-10-11 §2.2). Kept in step with the plan by `splice_window`
    /// over `row_keys`, never `reset` on an ordinary rebuild (that snaps
    /// the scroll anchor to the top). `pub(super)` for the tests.
    pub(super) list_state: ListState,
    /// The keys of the rows the list currently holds. `pub(super)` for the
    /// tests, which follow a row's key across a rebuild.
    pub(super) row_keys: Vec<RowKey>,
    /// Rows the item closure rendered in the last frame — the proof that
    /// 500 rows do not render 500 elements. A test seam and a perf counter.
    pub(super) rows_rendered: Rc<Cell<usize>>,
    /// The lowest and highest item index the item closure rendered in the
    /// last frame. Rendered items are measured, and the list renders one
    /// contiguous run (the viewport plus its overdraw), so a target inside
    /// this range has true heights all the way from the scroll top; outside
    /// it, rows the list never measured count as height 0 and
    /// `bounds_for_item` / `scroll_to_reveal_item` aim at the wrong place.
    rendered_range: Rc<Cell<Option<(usize, usize)>>>,
    /// A target a frame should scroll into view: set by an auto-advance,
    /// consumed by `render_inner` ONLY on a frame that did not splice or
    /// reset the list. The decision that triggered the advance splices the
    /// rows it moved, and spliced rows are unmeasured (height 0) until a
    /// layout measures them, so on that frame `bounds_for_item` places the
    /// target near the viewport top and it reads "visible" even when it is
    /// hundreds of rows down. The splice frame keeps it and schedules the
    /// next frame, whose previous layout measured the rows on screen.
    reveal: Option<PathBuf>,
}

impl ReviewView {
    pub fn new(state: Entity<SyncModel>, shell: WeakEntity<Shell>, cx: &mut Context<Self>) -> Self {
        // Re-render whenever the shared model changes so new drift rows land
        // in the sidebar without user interaction.
        cx.observe(&state, |view, _, cx| {
            view.plan_dirty = true;
            cx.notify();
        })
        .detach();
        Self {
            state,
            shell,
            selected: None,
            preview: PreviewState::Empty,
            diff_pair: DiffPair::DiskVsLocal,
            origin_diffs: None,
            provenance: Vec::new(),
            auto_merge: None,
            last_outcome: None,
            preview_editor: None,
            preview_dirty: true,
            preview_appearance: None,
            sidebar_width: None,
            sidebar_drag: None,
            history_height: None,
            history_drag: None,
            mode: ReviewMode::Flat,
            selection: Selection::default(),
            collapsed: BTreeSet::new(),
            focus_handle: cx.focus_handle(),
            plan_cache: None,
            plan_dirty: true,
            list_state: ListState::new(0, ListAlignment::Top, px(200.)),
            row_keys: Vec::new(),
            rows_rendered: Rc::new(Cell::new(0)),
            rendered_range: Rc::new(Cell::new(None)),
            reveal: None,
        }
    }

    /// Select a target and kick off the background detail load (journal
    /// provenance + `chezmoi cat` + destination read). Re-selecting the same
    /// target reloads — a free refresh.
    pub fn select(&mut self, target: PathBuf, cx: &mut Context<Self>) {
        // Outcome banners belong to the file they acted on — switching files
        // must not carry a stale "committed & pushed" (or its Undo) along.
        if self.selected.as_deref() != Some(target.as_path()) {
            self.last_outcome = None;
        }
        self.selected = Some(target.clone());
        self.preview = PreviewState::Loading;
        self.diff_pair = DiffPair::DiskVsLocal;
        self.origin_diffs = None;
        self.preview_dirty = true;
        self.provenance.clear();
        self.auto_merge = None;
        cx.notify();

        let journal = journal_path();
        cx.spawn(async move |this, cx| {
            let loaded = {
                let target = target.clone();
                cx.background_executor()
                    .spawn(async move { load_detail_blocking(&target, &journal) })
                    .await
            };
            this.update(cx, |view, cx| {
                // Stale guard: the user may have clicked elsewhere meanwhile.
                if view.selected.as_deref() == Some(target.as_path()) {
                    view.provenance = loaded.provenance;
                    view.auto_merge = loaded.auto_merge;
                    view.origin_diffs = loaded.origin_diffs;
                    view.preview = loaded.preview;
                    view.preview_dirty = true;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Read-only escape hatch (allowed by the plan): resolve the source path
    /// via `chezmoi source-path` and hand it to `open -t`, all on the
    /// background executor. Opening an editor mutates nothing itself.
    fn open_in_editor(&self, cx: &mut Context<Self>) {
        let Some(target) = self.selected.clone() else {
            return;
        };
        cx.background_executor()
            .spawn(async move {
                let client = crate::app_chezmoi_client();
                match client.source_path(&target) {
                    Ok(source) => {
                        let req = CommandRequest::new("open")
                            .arg("-t")
                            .arg(source.to_string_lossy());
                        if let Err(e) = SystemRunner.run(req) {
                            eprintln!("tomte: open -t failed: {e}");
                        }
                    }
                    Err(e) => eprintln!("tomte: source-path failed: {e}"),
                }
            })
            .detach();
    }

    /// Record a one-click resolution for the selected target and move on.
    ///
    /// This is the whole point of the queue (spec §2.1): the click is
    /// remembered, the row moves to "processing", the selection advances,
    /// and the user answers the next file without waiting for this one. The
    /// work runs on the Shell's serial drain; a failure comes back onto this
    /// row (§2.5), where choosing again is the retry.
    ///
    /// `pub(super)` so the render-smoke tests can press a button without a
    /// synthetic mouse event, the same reason `header_source_actions` is.
    pub(super) fn run_action(&mut self, action: ResolveAction, cx: &mut Context<Self>) {
        let Some(target) = self.selected.clone() else {
            return;
        };
        // Origin's text exactly as this view is showing it belongs to the
        // button the user pressed, so it is captured at click time and
        // travels WITH the decision — by the time the queue runs it, the
        // selection may well be three files further on.
        let decision = match action {
            ResolveAction::KeepDisk => Decision::KeepDisk,
            ResolveAction::KeepSourceLocal => Decision::KeepSourceLocal,
            ResolveAction::KeepSourceOrigin => {
                let Some(seen) = self.origin_diffs.as_ref().map(|d| d.seen.clone()) else {
                    // The button only exists when origin's content loaded,
                    // so this is structural — and still reported rather than
                    // dropped, in the same words the engine would use.
                    self.last_outcome = Some(refused_banner(
                        action,
                        true,
                        "no origin version was loaded for this file — open it in Review first",
                    ));
                    cx.notify();
                    return;
                };
                Decision::KeepSourceOrigin { seen }
            }
        };
        self.last_outcome = None;
        self.enqueue(target.clone(), decision, cx);
        self.advance_from(&target, cx);
    }

    /// The ✕ on a waiting PROCESSING row. Safe from inside this view's
    /// update for the same reason `enqueue` is: `Shell::cancel_queued`
    /// only touches the queue and notifies.
    pub(super) fn cancel_queued(&mut self, target: &Path, cx: &mut Context<Self>) {
        let t = target.to_path_buf();
        if let Err(e) = self
            .shell
            .update(cx, |shell, cx| shell.cancel_queued(&t, cx))
        {
            tomte_core::log_error!("resolve", "could not cancel {}: {e}", target.display());
        }
    }

    /// Hand one decision to the Shell's queue.
    ///
    /// Updating the Shell from inside THIS view's own update is safe because
    /// `Shell::enqueue` → `Shell::pump` only ever SPAWNS: it never reaches
    /// back into the review view synchronously. Nesting an update of a view
    /// that is itself mid-update is what crashed gpui on 2026-08-18.
    fn enqueue(&mut self, target: PathBuf, decision: Decision, cx: &mut Context<Self>) {
        let queued = target.clone();
        if let Err(e) = self.shell.update(cx, |shell, cx| {
            shell.enqueue(
                QueuedAction {
                    target,
                    decision,
                    // This view has a banner and a PROCESSING row to say it
                    // with; a desktop notification on top would be a second
                    // report of the same fact.
                    notify_on_done: false,
                },
                cx,
            )
        }) {
            // The window is going away underneath the click. Nothing can be
            // shown to a user who no longer has a view, but a decision that
            // went nowhere is never silent.
            tomte_core::log_error!(
                "resolve",
                "the decision for {} was not queued: {e}",
                queued.display()
            );
        }
        cx.notify();
    }

    /// Spec §2.6: after a decision, select the next row that still needs
    /// one, so a sweep is one click per file with no re-targeting click
    /// between.
    ///
    /// The walk order is `sidebar_rows`'s own — needs-you targets, then the
    /// safe group's `visible_safe` in THIS view's own mode — so this never
    /// drifts from what the sidebar shows, and a collapsed folder's files are
    /// skipped by the sweep exactly as they are invisible in it (spec §2.5).
    ///
    /// `resolved` was just enqueued by the caller and so already reads as
    /// processing in the REAL queue; the plan below is built as if it were
    /// not — `resolved`'s own entry is the only one filtered out of the live
    /// snapshot below — so it stays at its true display position (and, in
    /// Tree mode, its folder does not re-compact around its absence) for
    /// `next_after`'s lookup instead of vanishing the instant it is
    /// answered. Every OTHER processing row is filtered out of the plan
    /// normally: `next_after`'s own `processing` check below (over the REAL,
    /// unfiltered queue) would have skipped those as candidates either way,
    /// so nothing changes for them.
    ///
    /// This needs the REAL queue's shape, not an empty one: in Tree mode,
    /// which files are processing changes `tree_rows`' folder compaction
    /// (one extra processing file under `.config/` can split a compacted
    /// `.config/nvim/` into two levels) and therefore the walk order itself,
    /// so an empty queue is only a safe substitute in Flat, where the queue
    /// cannot influence anything but which rows are filtered out — and
    /// `next_after` filters those out again anyway.
    ///
    /// `pub(super)` so a test can drive an auto-advance without a live engine.
    pub(super) fn advance_from(&mut self, resolved: &Path, cx: &mut Context<Self>) {
        let drifted = self.state.read(cx).drifted.clone();
        let queue = self.queue_snapshot(cx);
        let home = home();
        let (needs_you, _) = severity_groups(&drifted);
        let queue_before_this_click = QueueSnapshot {
            processing: queue
                .processing
                .iter()
                .filter(|(t, _)| t != resolved)
                .cloned()
                .collect(),
            failures: BTreeMap::new(),
            busy: queue.busy,
        };
        let plan = sidebar_rows(SidebarInputs {
            drifted: &drifted,
            queue: &queue_before_this_click,
            mode: self.mode,
            collapsed: &self.collapsed,
            selection: &self.selection,
            home: &home,
        });
        let ordered: Vec<PathBuf> = needs_you
            .iter()
            .map(|d| d.target.clone())
            .chain(plan.visible_safe)
            .collect();
        if let Some(next) = next_after(resolved, &ordered, &|p| queue.is_processing(p)) {
            self.reveal = Some(next.clone());
            self.select(next, cx);
        }
    }

    /// The plan the sidebar is showing right now, cached (spec 2026-10-11
    /// §2.3): rebuilt when the queue, the mode, the collapse set, the
    /// selection or the model changed since the last build, otherwise the
    /// same `Rc`. Every gesture and `render_inner` read through here, so
    /// every rule reads one snapshot of the world.
    ///
    /// `pub(super)` so the render-smoke tests can read `safe_order` the
    /// same way `select_all_safe` does.
    pub(super) fn plan(&mut self, cx: &Context<Self>) -> Rc<SidebarPlan> {
        let queue = self.queue_snapshot(cx);
        self.plan_with(queue, cx)
    }

    /// How many rows the cached plan holds — a test seam for the
    /// virtualized sidebar.
    #[cfg(test)]
    pub(super) fn plan_rows_len_for_test(&self) -> usize {
        self.plan_cache.as_ref().map_or(0, |(_, p)| p.rows.len())
    }

    /// The one plan builder: rebuilds when `plan_dirty` is set or the key
    /// built from `queue` and the view's own state differs from the cached
    /// one, otherwise hands back the cached `Rc`.
    fn plan_with(&mut self, queue: QueueSnapshot, cx: &App) -> Rc<SidebarPlan> {
        let key = PlanKey {
            queue,
            mode: self.mode,
            collapsed: self.collapsed.clone(),
            selection: self.selection.clone(),
        };
        if !self.plan_dirty
            && let Some((cached, plan)) = &self.plan_cache
            && *cached == key
        {
            return plan.clone();
        }
        let drifted = &self.state.read(cx).drifted;
        let plan = Rc::new(sidebar_rows(SidebarInputs {
            drifted,
            queue: &key.queue,
            mode: key.mode,
            collapsed: &key.collapsed,
            selection: &key.selection,
            home: &home(),
        }));
        self.plan_cache = Some((key, plan.clone()));
        self.plan_dirty = false;
        plan
    }

    /// ⌘-click or a checkbox on a safe row.
    fn toggle_member(&mut self, target: &Path, cx: &mut Context<Self>) {
        let plan = self.plan(cx);
        self.selection.toggle(target, &plan.safe_order);
        cx.notify();
    }

    /// ⇧-click on a safe row.
    fn extend_to(&mut self, target: &Path, cx: &mut Context<Self>) {
        let plan = self.plan(cx);
        let fallback = self
            .selected
            .clone()
            .filter(|s| plan.safe_order.contains(s));
        self.selection
            .extend_range(target, &plan.visible_safe, fallback.as_deref());
        cx.notify();
    }

    /// The SAFE TO RESOLVE header's checkbox.
    fn toggle_all_safe(&mut self, cx: &mut Context<Self>) {
        let plan = self.plan(cx);
        let all = plan.safe_order.clone();
        self.selection.toggle_all(&all, &plan.safe_order);
        cx.notify();
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selection.clear();
        cx.notify();
    }

    /// ⌘A: SELECT every safe row, collapsed files included — never toggle.
    /// `toggle_all` is a set-complete-or-clear primitive (the header
    /// checkbox's job), so this only calls it when the set is not already
    /// complete; a second ⌘A is then a no-op rather than clearing everything
    /// it just selected. An empty safe group is a no-op either way —
    /// `toggle_all` already refuses an empty `targets`.
    fn select_all_safe(&mut self, cx: &mut Context<Self>) {
        let plan = self.plan(cx);
        let all = plan.safe_order.clone();
        if self.selection.check_state(&all) != CheckState::On {
            self.selection.toggle_all(&all, &all);
        }
        cx.notify();
    }

    /// The folder-tree toggle on the SAFE TO RESOLVE header.
    ///
    /// `pub(super)` so a `#[gpui::test]` can flip it directly, the same
    /// reason `row_click` is.
    pub(super) fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            ReviewMode::Flat => ReviewMode::Tree,
            ReviewMode::Tree => ReviewMode::Flat,
        };
        // Members keep their identity across modes; only their order changes,
        // and the next render's prune re-sorts them.
        cx.notify();
    }

    /// Set the mode idempotently: re-asserting the active mode is a no-op,
    /// since `toggle_mode` only knows how to flip. Test-only now that the
    /// header's single toggle flips; the T7 tree test still pins the guard.
    #[cfg(test)]
    pub(super) fn set_mode(&mut self, mode: ReviewMode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.toggle_mode(cx);
        }
    }

    /// A folder row's plain click: open/close it. Its members stay selected
    /// while collapsed (spec §2.5) — only `toggle_folder` changes membership.
    ///
    /// `pub(super)` for the same reason as `toggle_mode`.
    pub(super) fn toggle_collapse(&mut self, folder: &Path, cx: &mut Context<Self>) {
        if !self.collapsed.remove(folder) {
            self.collapsed.insert(folder.to_path_buf());
        }
        cx.notify();
    }

    /// The folder's checkbox, or ⌘-click on the folder row: every safe file
    /// under it, collapsed or not.
    ///
    /// `pub(super)` for the same reason as `toggle_mode`.
    pub(super) fn toggle_folder(&mut self, folder: &Path, cx: &mut Context<Self>) {
        let plan = self.plan(cx);
        let under = safe_descendants(&plan.safe_order, folder, &home());
        self.selection.toggle_all(&under, &plan.safe_order);
        cx.notify();
    }

    /// A folder row's click gesture (spec §2.3/§2.5): plain toggles collapse,
    /// ⌘ ticks every safe file under it, collapsed or not. Mirrors
    /// `row_click`'s gesture switch.
    ///
    /// `pub(super)` so the render-smoke tests can drive a gesture with a
    /// synthetic `Modifiers` rather than a real mouse event, the same reason
    /// `row_click` is.
    pub(super) fn folder_click(
        &mut self,
        folder: &Path,
        mods: gpui::Modifiers,
        cx: &mut Context<Self>,
    ) {
        if mods.platform {
            self.toggle_folder(folder, cx);
        } else {
            self.toggle_collapse(folder, cx);
        }
    }

    /// A row's click gesture (spec §2.3): ⌘ toggles the row's membership
    /// (sets the anchor, never moves `selected`), ⇧ extends the range from
    /// the anchor (also never moves `selected`), anything else clears the
    /// selection and plain-selects the row. A NEEDS A DECISION / PROCESSING
    /// row (`selectable = false`) has no membership to toggle or extend, so
    /// a ⌘/⇧-click on one is ignored outright rather than falling through to
    /// the plain-click arm — which would otherwise clear the selection and
    /// move `selected`, silently destroying a large selection with no way
    /// back (final review, Minor 3).
    ///
    /// `pub(super)` so the render-smoke tests can drive a gesture with a
    /// synthetic `Modifiers` rather than a real mouse event, the same
    /// reason `run_action` is.
    pub(super) fn row_click(
        &mut self,
        target: &Path,
        mods: gpui::Modifiers,
        selectable: bool,
        cx: &mut Context<Self>,
    ) {
        if !selectable {
            // ⌘/⇧ on a row with no membership to toggle or extend: ignore
            // the gesture outright. Without this early return the row would
            // fall through to the same plain-click clear+select below,
            // wiping out an unrelated selection just because the click
            // happened to carry a modifier.
            if mods.platform || mods.shift {
                return;
            }
            self.selection.clear();
            self.select(target.to_path_buf(), cx);
            return;
        }
        if mods.platform {
            self.toggle_member(target, cx);
        } else if mods.shift {
            self.extend_to(target, cx);
        } else {
            self.selection.clear();
            self.select(target.to_path_buf(), cx);
        }
    }

    /// A bulk press: one `QueuedAction` per member, ONE batch (spec §2.4).
    /// The selection clears, the rows move to PROCESSING, and the detail
    /// selection advances past the last VISIBLE member (see below).
    ///
    /// Updating the Shell from inside THIS view's own update is safe for the
    /// same reason `enqueue` is (see its own comment): `Shell::enqueue_many`
    /// → `Shell::pump` either returns immediately (already pumping, or no
    /// engine to run with — see `pump`'s own comment on why it parks rather
    /// than drops the batch) or hands the batch to `cx.spawn`, which only
    /// SPAWNS a background task and returns. Neither path reaches back into
    /// this (or any) view synchronously, so nesting this update inside the
    /// Shell's is never the same crash `enqueue`'s comment names.
    ///
    /// `pub(super)` so the render-smoke tests can press the bulk button
    /// without a synthetic mouse event, the same reason `run_action` is.
    pub(super) fn run_bulk(&mut self, action: ResolveAction, cx: &mut Context<Self>) {
        let members = self.selection.members().to_vec();
        let Some(last) = members.last().cloned() else {
            return;
        };
        // Advance from the last member that is actually VISIBLE right now,
        // not simply `members.last()`: `advance_from` walks
        // `plan.visible_safe` (Task 7 hazard) — a member hidden inside a
        // collapsed folder is absent from it, and handing `next_after` a
        // target it cannot find returns `None`, silently stranding the
        // selection. Unreachable in Flat mode today (every member is
        // visible there), falls back to `members.last()` when every member
        // is hidden.
        let plan = self.plan(cx);
        let advance_target = members
            .iter()
            .rev()
            .find(|m| plan.visible_safe.contains(m))
            .cloned()
            .unwrap_or(last);
        let decision = bulk_decision(action);
        let actions: Vec<QueuedAction> = members
            .iter()
            .map(|target| QueuedAction {
                target: target.clone(),
                decision: decision.clone(),
                notify_on_done: false,
            })
            .collect();
        let count = actions.len();
        if let Err(e) = self
            .shell
            .update(cx, |shell, cx| shell.enqueue_many(actions, cx))
        {
            tomte_core::log_error!(
                "resolve",
                "the bulk decision for {count} files was not queued: {e}"
            );
        }
        self.selection.clear();
        self.last_outcome = None;
        self.advance_from(&advance_target, cx);
        cx.notify();
    }

    /// Undo the last finished session (restore destination files from their
    /// journaled snapshots), reported through the same banner.
    ///
    /// Not a per-file decision and so not a queue entry: it is a repo-level
    /// op on the Shell, taking the same latch as everything else that
    /// mutates what the repo tracks — `undo_last` rewrites the very
    /// destination files a draining batch's `chezmoi re-add` is reading.
    fn run_undo(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = self.shell.update(cx, |shell, cx| shell.undo_last(cx)) {
            tomte_core::log_error!("resolve", "undo was not requested: {e}");
        }
        cx.notify();
    }

    /// "Keep both": queue the clean auto-merge as the same merged write the
    /// merge editor's Save makes, and advance like any other decision.
    ///
    /// `Decision::KeepBoth`, not `Decision::Merged`, although the engine call
    /// is identical: the decision carries the words that will be said about
    /// it minutes later, and a row under a button labelled "Keep both" that
    /// comes back saying "Save failed" names a button that is not on screen.
    fn run_keep_both(&mut self, cx: &mut Context<Self>) {
        let Some((inputs, assembled)) = self.auto_merge.clone() else {
            return;
        };
        let target = inputs.target.clone();
        self.last_outcome = None;
        self.enqueue(
            target.clone(),
            Decision::KeepBoth {
                resolved: assembled,
                inputs,
            },
            cx,
        );
        self.advance_from(&target, cx);
    }

    /// The Shell's queue, copied out for this render pass. Empty when the
    /// shell is gone (the window is closing): no rows are processing if
    /// nothing can be running.
    fn queue_snapshot(&self, cx: &Context<Self>) -> QueueSnapshot {
        let Some(shell) = self.shell.upgrade() else {
            return QueueSnapshot::default();
        };
        let shell = shell.read(cx);
        let running = shell.queue.running();
        QueueSnapshot {
            processing: shell
                .queue
                .processing()
                .into_iter()
                .map(|target| {
                    let status = if running.iter().any(|a| a.target == target) {
                        QueuedStatus::Running
                    } else {
                        QueuedStatus::Waiting
                    };
                    // `processing` IS running ∪ pending and `decision_label`
                    // searches both, so the fallback is unreachable. It says
                    // "a decision" rather than naming one, because a row that
                    // claimed a choice the user did not make would be worse
                    // than a row that admits it lost the name.
                    let label = shell.queue.decision_label(&target).unwrap_or("A decision");
                    (target, QueuedDecision { status, label })
                })
                .collect(),
            failures: shell
                .queue
                .failures()
                .map(|(t, m)| (t.clone(), m.clone()))
                .collect(),
            busy: shell.is_busy(),
        }
    }

    /// Hand the selected target to the Shell's full-window merge editor
    /// (plan 7 Task 3). The button gating guarantees a selection and a live
    /// engine, but both are re-checked structurally here.
    fn open_merge_editor(&self, cx: &mut Context<Self>) {
        let Some(target) = self.selected.clone() else {
            return;
        };
        self.shell
            .update(cx, |shell, cx| shell.open_merge(target, cx))
            .ok();
    }

    /// "Open merge editor" — live unless this file's decision is running
    /// (`detail()` already implies a selection).
    ///
    /// No daemon check, and that is a real trade-off rather than an
    /// oversight. `MergeView::load` DOES use the engine when one exists: it
    /// asks whether an open merge with origin left this file conflicted, and
    /// if so reads the panes from the merge's index stages instead of from a
    /// working tree holding git's marker text. With no engine that question
    /// cannot be asked, so on a stranded mid-merge repo the editor opens with
    /// panes derived from the markers — fiction. It is CONTAINED, not
    /// harmless: the Save that fiction would produce is refused by
    /// `ResolveEngine::resolve_merged`, which re-checks `is_mid_merge_conflict`
    /// and fails the decision on its row with "reopen it in the merge editor".
    /// Nothing is written. Gating the button instead would refuse every
    /// ordinary merge for a state that requires both an open merge AND no
    /// daemon.
    ///
    /// Goes through the shared button so it obeys the surface rule like
    /// everything else; it was hand-rolled with
    /// a full-strength `theme.text` outline, which read far louder than the
    /// secondary action it is (2026-08-25).
    fn merge_editor_button(
        &self,
        blocked: Option<SharedString>,
        is_conflict: bool,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        // Inert while this file's decision is RUNNING, for the same reason
        // the quick actions are: the editor's Save is another decision about
        // a file whose last one is mid-flight. A merely QUEUED decision
        // leaves it open — after Task 5 a save there enqueues too, and
        // `ActionQueue::push` replaces the pending choice.
        if let Some(why) = blocked {
            return ui::disabled_button(
                theme,
                "open-merge-editor",
                "Open merge editor".into(),
                ui::ButtonSize::Sm,
                Some(why),
            );
        }
        // Deliberately UNcoloured: "Keep disk" and "Keep source" own the
        // amber/blue side vocabulary, so tinting this one too would imply it
        // acts on a side. It opens a surface instead — neutral says that.
        let _ = is_conflict;
        ui::button(
            theme,
            "open-merge-editor",
            "Open merge editor".into(),
            ui::ButtonVariant::Tonal(theme.text_muted),
            ui::ButtonSize::Sm,
            cx.listener(|view, _ev, _window, cx| view.open_merge_editor(cx)),
        )
    }

    /// The source-side actions for the selected file: one button, or two when
    /// origin's content loaded and can therefore be kept.
    pub(super) fn header_source_actions(&self) -> Vec<ResolveAction> {
        source_actions(self.origin_diffs.is_some())
    }

    /// One header action button. `blocked` is the reason it cannot be
    /// pressed, `None` when it can — `run_action` itself re-checks the
    /// selection.
    ///
    /// The button NEVER swaps to a "working…" label: that swap is the defect
    /// this plan exists to remove (spec §1.1). It is live for every file but
    /// the one whose decision is already recorded, and even that one only
    /// until its work lands.
    fn action_button(
        &self,
        action: ResolveAction,
        split: bool,
        blocked: Option<SharedString>,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let label = action.button_label(split);
        let el = match blocked {
            None => ui::button(
                theme,
                action.button_id(),
                label.into(),
                // The quick actions carry the side they act on, exactly as
                // the diff rows and the merge strips do.
                ui::ButtonVariant::Tonal(action.tint(theme)),
                ui::ButtonSize::Sm,
                cx.listener(move |view, _ev, _window, cx| view.run_action(action, cx)),
            ),
            Some(why) => ui::disabled_button(
                theme,
                action.button_id(),
                label.into(),
                ui::ButtonSize::Sm,
                Some(why),
            ),
        };
        // Wrap onto the next header row rather than squash: a squeezed
        // button overflows its own label (`whitespace_nowrap`).
        el.flex_shrink_0()
    }

    /// The comparison selector above the diff — rendered only when origin's
    /// content loaded (see [`Self::origin_diffs`]). A segmented row: the
    /// chosen comparison carries its own tint, the others recede.
    fn comparison_selector(&self, theme: Theme, cx: &mut Context<Self>) -> Div {
        let segments: Vec<Stateful<Div>> = DIFF_PAIRS
            .iter()
            .map(|&pair| {
                let selected = self.diff_pair == pair;
                ui::button(
                    theme,
                    pair.segment_id(),
                    pair.label().into(),
                    if selected {
                        ui::ButtonVariant::Tonal(pair.tint(theme))
                    } else {
                        ui::ButtonVariant::Ghost
                    },
                    ui::ButtonSize::Micro,
                    cx.listener(move |view, _ev, _window, cx| view.set_diff_pair(pair, cx)),
                )
                .flex_shrink_0()
            })
            .collect();
        div()
            .flex_none()
            .px_3()
            .py_1()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            // No "Comparing:" prefix: each segment already names both of its
            // sides, and the prefix cost the row enough width to wrap at the
            // default window size.
            .children(segments)
    }

    /// Switch comparisons. Everything needed is already in memory (the
    /// documents were computed with the selection), so this is a repaint —
    /// no reload, no subprocess.
    fn set_diff_pair(&mut self, pair: DiffPair, cx: &mut Context<Self>) {
        if self.diff_pair == pair {
            return;
        }
        self.diff_pair = pair;
        self.preview_dirty = true;
        cx.notify();
    }

    /// The document the chosen comparison renders, if it exists.
    fn current_doc(&self) -> Option<&MergeDocument> {
        match self.diff_pair {
            DiffPair::DiskVsLocal => match &self.preview {
                PreviewState::Ready(doc) => Some(doc),
                _ => None,
            },
            DiffPair::DiskVsOrigin => self.origin_diffs.as_ref().map(|d| &d.disk_vs_origin),
            DiffPair::LocalVsOrigin => self.origin_diffs.as_ref().map(|d| &d.local_vs_origin),
        }
    }

    /// The slim outcome banner above the diff, with an Undo button on
    /// successful resolutions.
    ///
    /// Undo is the ONE button in this view that goes inert while work
    /// drains, and it stays on screen while it does: "undo the last thing
    /// that landed" is unambiguous only when nothing is landing, and a
    /// button that vanishes teaches the user nothing about why.
    fn banner_el(
        &self,
        banner: &OutcomeBanner,
        queue: &QueueSnapshot,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let color = banner.tint.color(theme);
        let undo = match undo_affordance(banner, queue) {
            UndoAffordance::None => None,
            UndoAffordance::Blocked => Some(
                ui::disabled_button(
                    theme,
                    "outcome-undo",
                    "Undo".into(),
                    ui::ButtonSize::Micro,
                    Some("finishing queued decisions first".into()),
                )
                .into_any_element(),
            ),
            UndoAffordance::Live => Some(
                ui::button(
                    theme,
                    "outcome-undo",
                    "Undo".into(),
                    ui::ButtonVariant::Solid(color),
                    ui::ButtonSize::Micro,
                    cx.listener(|view, _ev, _window, cx| view.run_undo(cx)),
                )
                .into_any_element(),
            ),
        };
        ui::banner(theme, banner.tint, banner.text.clone(), undo)
    }

    fn sidebar(
        &self,
        plan: &Rc<SidebarPlan>,
        queue: &QueueSnapshot,
        in_sync: u64,
        now: u64,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let plan = plan.clone();
        let queue = queue.clone();
        let weak = cx.weak_entity();
        let counter = self.rows_rendered.clone();
        let range = self.rendered_range.clone();
        // The item closure runs during the list's layout and prepaint,
        // after this view's render returned and its lease was released
        // (gpui 0.2.2 `view.rs`: `Entity::request_layout` produces the
        // element under `update`, then lays it out) — so updating the view
        // here is a FRESH update, not a nested one, and gives each row the
        // `Context` its listeners need. The list holds its own `ListState`
        // borrowed while it calls this closure: never touch `list_state`
        // from a row.
        let rows = list(self.list_state.clone(), move |ix, _window, cx| {
            let Some(view) = weak.upgrade() else {
                return div().into_any_element();
            };
            counter.set(counter.get() + 1);
            range.set(Some(match range.get() {
                Some((lo, hi)) => (lo.min(ix), hi.max(ix)),
                None => (ix, ix),
            }));
            let item = view.update(cx, |view, cx| {
                view.sidebar_item(ix, &plan, &queue, in_sync, now, theme, cx)
            });
            // The list lays each item out as a root at the list's width
            // but leaves its own width to the item, so a bare row would
            // shrink to its content and its selected / ticked wash would
            // stop short. A full-width flex column stretches it the way
            // the old column did.
            div()
                .w_full()
                .flex()
                .flex_col()
                .child(item)
                .into_any_element()
        })
        .size_full();
        div()
            .id("review-sidebar")
            .w(px(self.sidebar_width.unwrap_or(SIDEBAR_WIDTH)))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("review-sidebar-rows")
                    // Test-only (no-op in release builds): lets a
                    // render-smoke test find this column's real bounds and
                    // click into its empty space below the last row.
                    .debug_selector(|| "review-sidebar-rows".into())
                    .flex_1()
                    .min_h_0()
                    .py_1()
                    .child(rows),
            )
    }

    /// One list item: a plan row, the "nothing to review" note (item 0 of
    /// an empty plan), or the In sync footer (the last item). The rows are
    /// `target_row` / `ui::folder_row` / `group_header_el` exactly as
    /// before the sidebar was virtualized; only the container changed.
    #[allow(clippy::too_many_arguments)]
    fn sidebar_item(
        &self,
        ix: usize,
        plan: &SidebarPlan,
        queue: &QueueSnapshot,
        in_sync: u64,
        now: u64,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selection_active = !self.selection.is_empty();
        match plan.rows.get(ix) {
            Some(SidebarRow::Group { label, check }) => {
                group_header_el(label, *check, selection_active, self.mode, theme, cx)
                    .into_any_element()
            }
            Some(SidebarRow::Folder { node, check }) => {
                let row_path = node.path.clone();
                let check_path = node.path.clone();
                ui::folder_row(
                    theme,
                    ElementId::named_usize("review-folder", ix),
                    ui::FolderRow {
                        depth: node.depth,
                        label: node.label.clone(),
                        collapsed: node.collapsed,
                        count: count_chip(node.safe, node.total),
                        check: *check,
                        show_check: selection_active,
                    },
                    cx.listener(move |view, ev: &ClickEvent, _w, cx| {
                        view.folder_click(&row_path, ev.modifiers(), cx);
                    }),
                    cx.listener(move |view, _ev, _w, cx| view.toggle_folder(&check_path, cx)),
                )
                .into_any_element()
            }
            Some(SidebarRow::Target {
                entry,
                queued,
                locked,
                cancellable,
                depth,
                selectable,
                checked,
            }) => self
                .target_row(
                    ix,
                    entry,
                    now,
                    *queued,
                    *locked,
                    *cancellable,
                    *depth,
                    *selectable,
                    *checked,
                    selection_active,
                    queue.failure(&entry.target),
                    theme,
                    cx,
                )
                .into_any_element(),
            None if plan.rows.is_empty() && ix == 0 => div()
                .p_3()
                .text_sm()
                .text_color(theme.text_muted)
                .child("nothing to review")
                .into_any_element(),
            // Collapsed in-sync group: a count line, not a list (§7.2).
            None => div()
                .px_3()
                .py_2()
                .mt_2()
                .border_t_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.text_muted)
                .child(format!("In sync ({in_sync})"))
                .into_any_element(),
        }
    }
}

/// `target_row`'s checkbox slot (`ui::checkbox`'s own `w(px(14.))`) — the
/// first of the three numbers a row's content is indented by before the
/// filename starts.
const ROW_CHECKBOX_W: f32 = 14.;
/// `target_row`'s own `gap_2()` between the checkbox/dot/name flex
/// children — the same gap counted twice below (once after the checkbox,
/// once after the dot).
const ROW_GAP: f32 = 8.;
/// The severity dot's own size (`.w_2()`).
const ROW_DOT_W: f32 = 8.;
/// How far a row's queued/failure sub-line (`.pl(px(..))`) must sit from
/// the row's own left edge to land under the filename: the checkbox, a
/// gap, the dot, a gap — the exact chain `target_row`'s flex layout puts
/// before the name. Deliberately NOT `+ TREE_INDENT * depth`: a row's own
/// `.pl(px(12. + TREE_INDENT * depth as f32))` already applies to every one
/// of its children, sub-lines included, since they are siblings of the flex
/// row inside that same padded container — adding depth again here would
/// double it for sub-lines only, at any depth beyond Flat mode's constant 0
/// (Task 7's indented rows must NOT re-add it).
const ROW_SUBLINE_INDENT: f32 = ROW_CHECKBOX_W + ROW_GAP + ROW_DOT_W + ROW_GAP;

/// The wording the origin hint carries. A HINT, deliberately: it says only
/// what the scan measured — this path is in `git diff HEAD..origin/<branch>`
/// — and promises nothing about what the file will offer once opened. A file
/// added locally and never pushed differs from origin and yet has no origin
/// blob at all, so it is flagged here and then correctly offers no origin
/// action; anything stronger ("origin has its own version", "compare with
/// origin") would be a promise this flag cannot keep.
const ORIGIN_HINT: &str = "differs from origin";

/// The list row's origin affordance: a small [`Theme::remote`] dot, the tint
/// origin owns everywhere else in the app.
///
/// This is the ONLY place `DriftSummary::origin_differs` is allowed to
/// decide anything, and it is allowed here precisely BECAUSE the list holds
/// no content: nothing has been loaded for these rows, so there is nothing
/// more truthful to gate on, and without it the user must select a file and
/// wait for a background load to discover origin is even in play. The moment
/// content exists — the diff, the comparison selector, the action buttons,
/// the merge editor's fourth pane — `inputs.origin.is_some()` is
/// authoritative and this flag has no vote.
fn origin_hint(ix: usize, entry: &DriftSummary, theme: Theme) -> Option<Stateful<Div>> {
    entry.origin_differs.then(|| {
        div()
            .id(ElementId::named_usize("review-origin-hint", ix))
            .w(px(6.))
            .h(px(6.))
            .rounded_full()
            .flex_shrink_0()
            .bg(theme.remote)
            .tooltip(ui::text_tooltip(ORIGIN_HINT))
    })
}

impl ReviewView {
    /// One sidebar row.
    ///
    /// `queued` is `Some` for a row under PROCESSING: the whole row recedes
    /// to muted, because a decision is already recorded for it. `locked`
    /// says whether that decision is past changing, and comes from
    /// [`sidebar_rows`] rather than being re-derived here — while a
    /// decision is merely queued, selecting the row and choosing again
    /// replaces the pending choice (spec §2.4, `ActionQueue::push`), which
    /// is the only way to correct a mis-click once the selection has moved
    /// on. `failure` is the message the last decision came back with — it
    /// stays on the row (spec §2.5) and the row stays live, because
    /// choosing again IS the retry.
    #[allow(clippy::too_many_arguments)]
    fn target_row(
        &self,
        ix: usize,
        entry: &DriftSummary,
        now: u64,
        queued: Option<QueuedDecision>,
        locked: bool,
        cancellable: bool,
        depth: usize,
        selectable: bool,
        checked: bool,
        selection_active: bool,
        failure: Option<&SharedString>,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selected = self.selected.as_deref() == Some(entry.target.as_path());
        // A processing row carries no severity: the question it was asking
        // has been answered. Its class tint would still read as "decide me".
        let color = if queued.is_some() {
            theme.text_muted
        } else {
            theme.class_color(&entry.class)
        };
        let name = display_name(&entry.target);
        // A processing row's trailing column goes BLANK while it RUNS and
        // carries the ✕ while it WAITS: how long the file has been drifted
        // stopped being the interesting fact about it the moment a decision
        // was recorded, and what replaced it — the choice AND its state — is
        // too long for a 200px sidebar's right edge. It goes on its own line
        // under the name instead, where it fits whole: "Keep source · this
        // machine · queued" truncated to "Keep source · thi…" would answer
        // nothing, and answering "which decision is this row waiting on" is
        // the entire point of showing it.
        let trailing: AnyElement = if cancellable {
            let t = entry.target.clone();
            let selector_target = entry.target.clone();
            ui::button(
                theme,
                ElementId::named_usize("review-cancel", ix),
                "✕".into(),
                ui::ButtonVariant::Ghost,
                ui::ButtonSize::Micro,
                cx.listener(move |view, _ev, _w, cx| {
                    // The row must not be selected by its own ✕.
                    cx.stop_propagation();
                    view.cancel_queued(&t, cx);
                }),
            )
            // Test-only hook (a no-op in release): finds the ✕ by file.
            .debug_selector(move || format!("review-cancel:{}", selector_target.display()))
            .tooltip(ui::text_tooltip("Remove from queue"))
            .flex_shrink_0()
            .into_any_element()
        } else {
            let text: SharedString = match queued {
                Some(_) => "".into(),
                None => entry
                    .since_ts
                    .map(|ts| time_ago(now, ts))
                    .unwrap_or_default()
                    .into(),
            };
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(theme.text_muted)
                .child(text)
                .into_any_element()
        };
        let target = entry.target.clone();
        // Ruling 5: the checkbox's gutter is reserved on EVERY row, not only
        // selectable ones — a non-selectable row (PROCESSING, NEEDS A
        // DECISION) gets an inert spacer of the checkbox's exact size in the
        // same slot, so every group's severity dot lands in one column.
        let checkbox_slot: AnyElement = if selectable {
            let t = target.clone();
            ui::checkbox(
                theme,
                ElementId::named_usize("review-check", ix),
                if checked {
                    CheckState::On
                } else {
                    CheckState::Off
                },
                cx.listener(move |view, _ev, _w, cx| view.toggle_member(&t, cx)),
            )
            // Hidden until a selection exists, then always shown; while none
            // exists, a hover over THIS row still reveals it (`group_hover`)
            // so the very first tick has somewhere to start. This is
            // hit-testable even at opacity 0 (gpui 0.2.2's
            // `Div::should_insert_hitbox` never consults `style.opacity`) —
            // on purpose, so a click on the invisible slot still ticks the
            // row: the reveal is `group_hover` over the row's own group, so
            // the pointer cannot be on this slot without the checkbox
            // already being painted (final review, Minor 4).
            .opacity(if selection_active { 1. } else { 0. })
            .group_hover("review-row", |el| el.opacity(1.))
            .into_any_element()
        } else {
            div().w(px(14.)).h(px(14.)).flex_none().into_any_element()
        };
        div()
            .id(ElementId::named_usize("review-target", ix))
            // Test-only (no-op in release builds): a real click in the
            // render-smoke tests finds this row by its plan index.
            .debug_selector(move || format!("review-row-{ix}"))
            .group("review-row")
            .px_3()
            .py_1()
            .pl(px(12. + TREE_INDENT * depth as f32))
            .flex()
            .flex_col()
            .gap_0p5()
            .when(!locked, |el| el.cursor_pointer())
            .when(selected, |el| el.bg(theme.surface))
            // Ticked rows carry a faint accent wash of their own — visible
            // even while `selected` also applies (this one wins, painted
            // after the surface tint above).
            .when(checked, |el| el.bg(Theme::wash(theme.accent, 0.10)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(checkbox_slot)
                    .child(div().w_2().h_2().rounded_full().flex_shrink_0().bg(color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(if queued.is_some() {
                                theme.text_muted
                            } else {
                                theme.text
                            })
                            .truncate()
                            .child(name),
                    )
                    // The ONE surface where `DriftSummary::origin_differs`
                    // is authoritative — see `origin_hint`.
                    .children(origin_hint(ix, entry, theme))
                    .child(trailing),
            )
            // What this row is waiting on, in the words of the button that
            // was pressed. `line_clamp(1)` over a definite width, never
            // `truncate()`: gpui's truncate does not ellipsize (it just
            // clips), so an overlong label would end mid-word with no sign
            // that anything was cut.
            .children(queued.map(|queued| {
                div()
                    .pl(px(ROW_SUBLINE_INDENT))
                    .w_full()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .line_clamp(1)
                    .child(queued.trailing())
            }))
            // Wraps rather than truncates: this is the only place the
            // failure is ever said, and half of "op could not authenticate"
            // is not a message.
            .children(failure.map(|message| {
                div().pl(px(ROW_SUBLINE_INDENT)).child(ui::status_text(
                    theme,
                    message.clone(),
                    ui::StatusTone::Conflict,
                ))
            }))
            .when(!locked, |el| {
                el.on_click(cx.listener(move |view, ev: &ClickEvent, _window, cx| {
                    view.row_click(&target, ev.modifiers(), selectable, cx);
                }))
            })
    }

    fn detail(
        &self,
        queue: &QueueSnapshot,
        now: u64,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(selected) = self.selected.as_deref() else {
            return centered_note(theme, "select a file to review".into(), theme.text_muted);
        };
        let name = display_name(selected);
        let path: SharedString =
            super::dashboard::shorten_home(&selected.display().to_string()).into();

        let is_conflict = self
            .state
            .read(cx)
            .drifted
            .iter()
            .find(|d| d.target == *selected)
            .is_some_and(|d| matches!(d.class.as_str(), "conflict" | "local_source_diverged"));

        // An origin comparison stands on its own: it comes from the merge
        // inputs, not from the preview load, so it renders even in the (never
        // yet observed) case where the two disagreed about the file.
        // Authoritative: origin's LOADED content, not the scan's hint.
        let split = self.origin_diffs.is_some();
        let blocked = blocked_reason(queue, selected);
        // A selection bar REPLACES the per-file actions — it acts on the
        // ticked set, not on the row `selected` happens to still name (the
        // two can disagree: a ⌘/⇧-click never moves `selected`). It is its
        // OWN full-width row below the header, not squeezed into the
        // header's trailing slot alongside the title (MI-1): "Keep source ·
        // this machine" (ruling 6) is wide enough that sharing the row with
        // the title either wraps the bar or, once capped wide enough not
        // to, pushes the title column under ~100px and truncates the
        // filename — a strictly worse trade at ordinary window widths.
        // Giving it the whole row avoids both.
        let selection_bar: Option<AnyElement> = (!self.selection.is_empty()).then(|| {
            let n = self.selection.members().len();
            let mut items: Vec<AnyElement> = vec![
                div()
                    .text_sm()
                    .text_color(theme.text_muted)
                    .flex_shrink_0()
                    .child(format!("{n} selected"))
                    .into_any_element(),
            ];
            items.extend(bulk_actions().into_iter().map(|action| {
                // `split: true` — spec §2.1/§2.4 give "Keep source · this
                // machine" verbatim for the bulk bar's source button; `Keep
                // disk` has no split form, so the flag is a no-op for it
                // (`button_label` only branches on `KeepSourceLocal`).
                ui::button(
                    theme,
                    ElementId::Name(format!("bulk-{}", action.button_id()).into()),
                    action.button_label(true).into(),
                    ui::ButtonVariant::Tonal(action.tint(theme)),
                    ui::ButtonSize::Sm,
                    cx.listener(move |view, _ev, _window, cx| view.run_bulk(action, cx)),
                )
                .flex_shrink_0()
                .into_any_element()
            }));
            items.push(
                ui::button(
                    theme,
                    "bulk-clear",
                    "✕".into(),
                    ui::ButtonVariant::Ghost,
                    ui::ButtonSize::Sm,
                    cx.listener(|view, _ev, _window, cx| view.clear_selection(cx)),
                )
                .tooltip(ui::text_tooltip("Clear selection"))
                .flex_shrink_0()
                .into_any_element(),
            );
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(theme.border)
                .children(items)
                .into_any_element()
        });
        // The selection bar takes the header's trailing slot's PLACE (there
        // is nothing left there to protect a filename from), rather than
        // joining it — an empty `vec![]` whenever the bar is showing, and
        // the per-file buttons below are not even built in that case.
        let title_trailing: Vec<AnyElement> = if self.selection.is_empty() {
            let mut header_actions: Vec<AnyElement> = Vec::new();
            header_actions.push(
                self.action_button(ResolveAction::KeepDisk, split, blocked.clone(), theme, cx)
                    .into_any_element(),
            );
            header_actions.extend(self.header_source_actions().into_iter().map(|action| {
                self.action_button(action, split, blocked.clone(), theme, cx)
                    .into_any_element()
            }));
            if self.auto_merge.is_some() {
                header_actions.push(
                    match &blocked {
                        None => ui::button(
                            theme,
                            "keep-both",
                            "Keep both".into(),
                            ui::ButtonVariant::Tonal(theme.ok),
                            ui::ButtonSize::Sm,
                            cx.listener(|view, _ev, _window, cx| view.run_keep_both(cx)),
                        ),
                        Some(why) => ui::disabled_button(
                            theme,
                            "keep-both",
                            "Keep both".into(),
                            ui::ButtonSize::Sm,
                            Some(why.clone()),
                        ),
                    }
                    .flex_shrink_0()
                    .into_any_element(),
                );
            }
            header_actions.push(
                self.merge_editor_button(blocked.clone(), is_conflict, theme, cx)
                    .flex_shrink_0()
                    .into_any_element(),
            );
            // External editor: real but rare — an icon, not a fourth
            // competing button.
            header_actions.push(
                div()
                    .id("open-in-editor")
                    .flex_shrink_0()
                    .px_1p5()
                    .py_0p5()
                    .rounded_md()
                    .text_sm()
                    .text_color(theme.text_muted)
                    .cursor_pointer()
                    .hover(|el| el.bg(Theme::wash(theme.text, 0.08)))
                    .child("↗")
                    .tooltip(ui::text_tooltip("Open in external editor"))
                    .on_click(cx.listener(|view, _ev, _window, cx| view.open_in_editor(cx)))
                    .into_any_element(),
            );
            vec![
                div()
                    .flex()
                    .flex_wrap()
                    // Capped so the actions cannot eat the whole header:
                    // the title column has flex-basis 0 and would
                    // otherwise shrink to nothing, taking the path line
                    // with it and letting a long filename overlap the
                    // buttons.
                    .max_w_2_3()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    // Flat, not nested: siblings pack onto a wrapped row
                    // together, where a nested cluster would claim a row
                    // of its own and push the rest down.
                    .children(header_actions)
                    .into_any_element(),
            ]
        } else {
            Vec::new()
        };

        let body = if self.diff_pair != DiffPair::DiskVsLocal && self.origin_diffs.is_some() {
            self.diff_preview(theme)
        } else {
            match &self.preview {
                PreviewState::Empty | PreviewState::Loading => {
                    centered_note(theme, "loading preview…".into(), theme.text_muted)
                }
                PreviewState::EvalFailed(hint) => message_box(
                    theme,
                    "template/secret evaluation failed",
                    hint.clone(),
                    Some("Fix this, then re-select the file to reload the preview."),
                ),
                PreviewState::Error(msg) => message_box(theme, "preview failed", msg.clone(), None),
                PreviewState::Ready(_) => self.diff_preview(theme),
            }
        };

        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(ui::detail_header(
                theme,
                name,
                path,
                // ONE trailing element, and it wraps: with the source action
                // split there are four buttons here, and the detail pane is
                // ~370px at the app's minimum window width. Squeezing them
                // (the flex default) overlapped the filename; wrapping costs a
                // header row only when the width genuinely isn't there. Empty
                // whenever the selection bar is showing — see
                // `title_trailing`'s own comment.
                title_trailing,
            ))
            .children(selection_bar)
            .when_some(self.last_outcome.clone(), |el, banner| {
                el.child(self.banner_el(&banner, queue, theme, cx))
            })
            .when(!self.provenance.is_empty(), |el| {
                let dragging = self.history_drag.is_some();
                el.child(provenance_section(
                    &self.provenance,
                    now,
                    self.history_height.unwrap_or(HISTORY_HEIGHT),
                    theme,
                ))
                .child(ui::resize_handle(
                    "review-history-divider",
                    ui::ResizeAxis::Y,
                    dragging,
                    theme,
                    cx.listener(|this, event: &gpui::MouseDownEvent, _w, cx| {
                        let start = this.history_height.unwrap_or(HISTORY_HEIGHT);
                        this.history_drag = Some(ui::DragState::new((), event, start));
                        cx.notify();
                    }),
                ))
            })
            // Directly above the diff it governs — after History, which
            // belongs to the file, not to the comparison.
            .when(self.origin_diffs.is_some(), |el| {
                el.child(self.comparison_selector(theme, cx))
            })
            .child(body)
            .into_any_element()
    }
}

impl Render for ReviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::time("review-render", || self.render_inner(window, cx))
    }
}

impl ReviewView {
    /// (Re)build the preview editor when the diff or the theme changed:
    /// the document text is the flattened diff, the decorations carry the
    /// tints/dots/word-diff spans. Read-only — this is a viewer that happens
    /// to be selectable, not an editing surface.
    fn sync_preview_editor(&mut self, theme: Theme, window: &Window, cx: &mut Context<Self>) {
        let appearance_changed = self.preview_appearance != Some(window.appearance());
        if !self.preview_dirty && !appearance_changed {
            return;
        }
        self.preview_dirty = false;
        self.preview_appearance = Some(window.appearance());
        let flattened = self
            .current_doc()
            .map(|doc| flatten_document(doc, self.diff_pair));
        let Some(lines) = flattened else {
            self.preview_editor = None;
            return;
        };
        if lines.is_empty() {
            // Nothing differs: the pane shows the "matches" note instead.
            self.preview_editor = None;
            return;
        }
        let text: String = lines
            .iter()
            .map(|l| l.text.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        let decors = Rc::new(preview_decors(&lines, theme));
        let decorator = move |_: &str| ui::text_area::Decoration {
            lines: decors.as_ref().clone(),
            blocks: Vec::new(),
        };
        match &self.preview_editor {
            Some(editor) => editor.update(cx, |area, _| {
                area.set_content(text);
                area.set_decorator(Box::new(decorator));
            }),
            None => {
                let editor = cx.new(|cx| {
                    let mut area = ui::TextArea::read_only(text, cx);
                    // Same 28px text column as the merge panes' mono rows
                    // (px_2 + w_4 gutter + gap_1), so the dot gutter lines up.
                    area.set_left_inset(px(28.));
                    area.set_decorator(Box::new(decorator));
                    area
                });
                self.preview_editor = Some(editor);
            }
        }
    }

    /// The diff pane: the read-only editor over a thin legend footer. The
    /// diff IS the pane content — no bordered sub-container (2026-08-08
    /// feedback).
    fn diff_preview(&self, theme: Theme) -> AnyElement {
        let Some(editor) = &self.preview_editor else {
            return centered_note(theme, self.diff_pair.matches_note(), theme.ok);
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("review-diff-scroll")
                    // Test-only (no-op in release builds): lets a
                    // render-smoke test find this pane's real bounds and
                    // click into its actual text, rather than guess a pixel
                    // offset.
                    .debug_selector(|| "review-diff-scroll".into())
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .overflow_x_scroll()
                    .pt_1()
                    .font_family("Menlo")
                    .text_xs()
                    .line_height(px(20.))
                    .child(editor.clone()),
            )
            .child({
                // The legend names THIS comparison's two sides — the accent
                // side is "what the source would write" against disk, but
                // "this machine's source" against origin.
                let (base_side, ours_side) = self.diff_pair.sides();
                div()
                    .flex_none()
                    .h_6()
                    .px_3()
                    .border_t_1()
                    .border_color(Theme::wash(theme.border, 0.7))
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_xs()
                    .child(legend_swatch(base_side.tint.color(theme), base_side.legend))
                    .child(legend_swatch(ours_side.tint.color(theme), ours_side.legend))
            })
            .into_any_element()
    }

    fn render_inner(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        self.sync_preview_editor(theme, window, cx);
        let now = system_now();
        let model = self.state.read(cx);
        let in_sync = model.in_sync;
        // Read once, before anything below needs `cx` mutably for a
        // listener: the Shell cannot be borrowed across those.
        let queue = self.queue_snapshot(cx);

        let plan = self.plan_with(queue.clone(), cx);
        // A member that resolved, got queued or turned into a conflict
        // leaves the selection here, silently (spec §2.2). Pruned AFTER
        // building the plan, not before, and not rebuilt a second time: a
        // `checked`/`check` flag above only ever tests membership within
        // `plan.safe_order`, and `prune` only ever REMOVES members that are
        // NOT in `plan.safe_order` — so it cannot change any flag `plan`
        // already carries. What it changes is `self.selection` itself,
        // which the NEXT gesture reads (`extend_range`'s reordering, the
        // selection bar's "N selected" count) — bookkeeping for later, not
        // something this render needs a second `sidebar_rows` call to
        // reflect.
        self.selection.prune(&plan.safe_order);
        // A prune only removes members outside `safe_order` — nothing the
        // cached rows ever flagged `checked` — so the cache stays valid;
        // record the pruned selection in its key so the next frame does
        // not rebuild for a change that is already reflected.
        if let Some((key, _)) = &mut self.plan_cache {
            key.selection = self.selection.clone();
        }

        // Keep the list's height cache in step with the rows (spec
        // 2026-10-11 §2.2). `reset` only when the plan flips between empty
        // and non-empty (the "nothing to review" item comes and goes);
        // otherwise splice exactly the window that changed, so unchanged
        // rows keep their heights and the scroll anchor stays on its row.
        let keys = row_keys(&plan, &queue);
        let item_count = plan.rows.len().max(1) + 1;
        let top = self.list_state.logical_scroll_top();
        let changed =
            if self.list_state.item_count() == 0 || keys.is_empty() != self.row_keys.is_empty() {
                self.list_state.reset(item_count);
                true
            } else if let Some((range, count)) = splice_window(&self.row_keys, &keys) {
                self.list_state.splice(range, count);
                true
            } else {
                false
            };
        // `splice` snaps the anchor to the window's start whenever the
        // anchor lay inside the window (a decision on a row below it moves
        // that row up into PROCESSING): put it back on the row it was on,
        // found by key. The offset into the row is kept only when the very
        // same row survived; a neighbour standing in for a vanished row is
        // shown from its top.
        if changed && let Some(new_ix) = re_anchor_index(&self.row_keys, &keys, top.item_ix) {
            let same_row = self.row_keys.get(top.item_ix) == keys.get(new_ix);
            self.list_state.scroll_to(ListOffset {
                item_ix: new_ix,
                offset_in_item: if same_row { top.offset_in_item } else { px(0.) },
            });
        }
        self.row_keys = keys;
        // An auto-advance landed on a row the user may not be able to see.
        // Not on a frame that spliced (see `reveal`): defer it to the next
        // frame. `cx.notify()` alone does nothing mid-draw in gpui 0.2.2
        // (`window.rs` `invalidate_view` only marks the view while
        // drawing), so the notify is deferred until the draw is over.
        if changed && self.reveal.is_some() {
            cx.defer_in(window, |_view, _window, cx| cx.notify());
        }
        // The bounds are trusted only for a target the last frame rendered
        // (see `rendered_range`): then `bounds_for_item` is exact, and a
        // target already fully on screen needs nothing. Any other target
        // has no true distance to aim at, so it is put near the top with
        // `scroll_to`.
        if !changed
            && let Some(target) = self.reveal.take()
            && let Some(ix) = plan.rows.iter().position(
                |r| matches!(r, SidebarRow::Target { entry, .. } if entry.target == target),
            )
        {
            let measured = self
                .rendered_range
                .get()
                .is_some_and(|(lo, hi)| lo <= ix && ix <= hi);
            let viewport = self.list_state.viewport_bounds();
            match self.list_state.bounds_for_item(ix) {
                Some(b)
                    if measured && b.top() >= viewport.top() && b.bottom() <= viewport.bottom() => {
                }
                Some(_) if measured => self.list_state.scroll_to_reveal_item(ix),
                _ => self.list_state.scroll_to(ListOffset {
                    item_ix: ix.saturating_sub(2),
                    offset_in_item: px(0.),
                }),
            }
        }
        self.rows_rendered.set(0);
        self.rendered_range.set(None);

        let dragging = self.sidebar_drag.is_some();
        div()
            .track_focus(&self.focus_handle)
            .key_context("Review")
            .on_action(cx.listener(|view, _: &ClearSelection, _w, cx| view.clear_selection(cx)))
            .on_action(cx.listener(|view, _: &SelectAllSafe, _w, cx| view.select_all_safe(cx)))
            .flex_1()
            .min_h_0()
            .flex()
            // A drag that leaves the 7px strip still has to track (see
            // `ui::resize_handle`), so the root owns move/up.
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _w, cx| {
                if let Some(drag) = this.sidebar_drag {
                    let w = drag.start + drag.delta(event, ui::ResizeAxis::X);
                    this.sidebar_width = Some(w.clamp(MIN_SIDEBAR_W, MAX_SIDEBAR_W));
                    cx.notify();
                }
                if let Some(drag) = this.history_drag {
                    let h = drag.start + drag.delta(event, ui::ResizeAxis::Y);
                    this.history_height = Some(h.clamp(MIN_HISTORY_H, MAX_HISTORY_H));
                    cx.notify();
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    if this.sidebar_drag.take().is_some() || this.history_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    if this.sidebar_drag.take().is_some() || this.history_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(self.sidebar(&plan, &queue, in_sync, now, theme, cx))
            .child(ui::resize_handle(
                "review-sidebar-divider",
                ui::ResizeAxis::X,
                dragging,
                theme,
                cx.listener(|this, event: &gpui::MouseDownEvent, _w, cx| {
                    let start = this.sidebar_width.unwrap_or(SIDEBAR_WIDTH);
                    this.sidebar_drag = Some(ui::DragState::new((), event, start));
                    cx.notify();
                }),
            ))
            .child(self.detail(&queue, now, theme, cx))
    }
}

fn group_header(label: &'static str, theme: Theme) -> Div {
    div()
        .px_3()
        .pt_2()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text_muted)
        .child(label)
}

/// A SAFE TO RESOLVE-style group header. When `check.is_some()` (the SAFE
/// header) the right side carries the always-visible flat-list / folder-tree
/// toggle (lit while `mode` is Tree) and the select-all checkbox after it.
/// The toggle is not hover-revealed because it is the mode's only entry
/// point.
///
/// A free function, not a method: it never reads `ReviewView` state (
/// `selection_active` is handed in rather than read via `&self` because the
/// caller already has it computed for the row loop too).
///
/// Hover-revealed exactly like a row's own checkbox (ruling 5): opacity 0 at
/// rest, `group_hover` over the header's own `.group(..)` to 1, and forced
/// to 1 while the selection is non-empty — so the affordance exists at rest
/// (it is reachable by hovering) without being painted when nothing is
/// selected.
fn group_header_el(
    label: &'static str,
    check: Option<CheckState>,
    selection_active: bool,
    mode: ReviewMode,
    theme: Theme,
    cx: &mut Context<ReviewView>,
) -> Div {
    let active = mode == ReviewMode::Tree;
    let wrapper = div()
        .group("review-safe-header")
        .flex()
        .items_center()
        .justify_between()
        .w_full();
    match check {
        None => wrapper.child(group_header(label, theme)),
        Some(state) => wrapper.child(group_header(label, theme)).child(
            div()
                .pr_3()
                .flex()
                .items_center()
                .gap_1p5()
                // The flat-list / folder-tree toggle (spec 2026-10-11 §2.1):
                // on THIS header because Tree mode reshapes only this group.
                // Always visible, unlike the hover-revealed checkbox beside
                // it: it is the mode's only entry point.
                .child(
                    ui::icon_button(
                        theme,
                        "review-mode-toggle",
                        ui::folder_tree(theme),
                        active,
                        "Folder tree".into(),
                        cx.listener(|view, _ev, _w, cx| view.toggle_mode(cx)),
                    )
                    // Test-only (no-op in release builds): a real click in
                    // the render-smoke test finds it by this name, and the
                    // `:on`/`:off` suffix is the lit state, from the same
                    // `active` the button was given.
                    .debug_selector(move || {
                        format!("review-mode-toggle:{}", if active { "on" } else { "off" })
                    }),
                )
                .child(
                    ui::checkbox(
                        theme,
                        "safe-select-all",
                        state,
                        cx.listener(|view, _ev, _w, cx| view.toggle_all_safe(cx)),
                    )
                    .opacity(if selection_active { 1. } else { 0. })
                    .group_hover("review-safe-header", |el| el.opacity(1.)),
                ),
        ),
    }
}

pub(super) fn centered_note(theme: Theme, text: SharedString, color: Rgba) -> AnyElement {
    ui::centered_note(theme, text, color).into_any_element()
}

/// Error/eval-failure box: title + detail + optional muted remediation line
/// (spec §10: errors are states, not gaps).
pub(super) fn message_box(
    theme: Theme,
    title: &'static str,
    detail: String,
    followup: Option<&'static str>,
) -> AnyElement {
    ui::message_box(theme, title, detail.into(), followup).into_any_element()
}

fn provenance_section(rows: &[ProvRow], now: u64, height: f32, theme: Theme) -> Stateful<Div> {
    div()
        .id("review-history")
        .h(px(height))
        .flex_none()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2()
        .overflow_y_scroll()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_muted)
                .child("History"),
        )
        .children(rows.iter().map(|row| provenance_row(row, now, theme)))
}

fn provenance_row(row: &ProvRow, now: u64, theme: Theme) -> Div {
    let glyph_color = match &row.class {
        Some(class) => theme.class_color(class),
        None => theme.text_muted,
    };
    // The chip earns its place only when it says something the event label
    // doesn't (e.g. "applied" + a drift class); otherwise the row would
    // repeat itself ("modified on disk" twice).
    let chip = row
        .class
        .clone()
        .filter(|class| class_label(class) != kind_label(&row.kind))
        .map(|class| {
            let color = theme.class_color(&class);
            (
                SharedString::from(class_label(&class)),
                ui::ChipVariant::Outline(color),
            )
        });
    ui::inert_event_row(
        theme,
        ui::EventRowSpec {
            time: time_ago(now, row.ts).into(),
            glyph: (kind_glyph(&row.kind), glyph_color),
            title: kind_label(&row.kind).into(),
            title_color: theme.text,
            detail: Some(row.machine.clone().into()),
            chip,
        },
    )
}

/// Per-line decorations for the preview diff. Same vocabulary as the merge
/// editor's result rows (2026-08-26 feedback: "the diff coloring should be
/// the same as the one on the editor"): a wash of the owning side's tint
/// with a 2px rail down its edge, context muted and undecorated. No gutter
/// dot: the field and the rail already say which side a row belongs to, so
/// the dot was one signal too many (2026-08-27).
/// One legend entry: a miniature of the row treatment itself (rail + field)
/// rather than an unrelated dot, so the key matches what's above it.
fn legend_swatch(tint: Rgba, label: &'static str) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1p5()
        .child(
            // A miniature of the row treatment, not a dot: rail + field. The
            // field is carried at a higher alpha than the rows use — at
            // 12×12 the row's 7% wash is invisible and the swatch reads as a
            // stray bracket.
            div()
                .w_5()
                .h_3()
                .rounded_r_sm()
                .bg(Theme::wash(tint, 0.22))
                .border_l_2()
                .border_color(tint),
        )
        .child(div().text_color(tint).child(label))
}

fn preview_decors(lines: &[DiffLine], theme: Theme) -> Vec<ui::text_area::LineDecor> {
    lines
        .iter()
        .map(|line| {
            let tint = line.tint.color(theme);
            let changed = line.tint != LineTint::Context;
            ui::text_area::LineDecor {
                // Changed rows sit on their side's field, exactly like the
                // provenance blocks in the merge result.
                bg: changed.then(|| Theme::wash(tint, 0.07)),
                rail: changed.then_some(tint),
                // Text stays readable rather than tinted: the field carries
                // the side, so the glyphs don't have to (matching the editor,
                // where only unchanged context is dimmed).
                text_color: Some(if changed {
                    theme.text
                } else {
                    theme.text_muted
                }),
                // Word-diff emphasis: a translucent wash of the line's own
                // tint, so no colour outside the Theme tokens enters the view.
                spans: line
                    .highlights
                    .iter()
                    .map(|r| (r.clone(), Rgba { a: 0.22, ..tint }))
                    .collect(),
                ..Default::default()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    use tomte_app::action_queue::Decision;
    use tomte_app::resolve::{ResolveError, ResolveOutcome};
    use tomte_core::merge::{MergeDocument, MergeOptions, RegionKind};
    use tomte_proto::DriftSummary;
    use tomte_ui::components::CheckState;

    use super::super::BatchResult;

    use super::{
        BannerTint, DiffLine, DiffPair, LineTint, ORIGIN_HINT, OutcomeBanner, QueueSnapshot,
        QueuedDecision, QueuedStatus, ResolveAction, ReviewMode, RowKey, Selection, SidebarInputs,
        SidebarPlan, SidebarRow, UndoAffordance, blocked_reason, bulk_actions, bulk_decision,
        display_name, flatten_document, next_after, origin_diffs, origin_hint, per_line_ranges,
        preview_decors, queued_outcome_banner, re_anchor_index, refused_banner, row_keys,
        severity_groups, sidebar_rows, source_actions, splice_window, undo_affordance, undo_banner,
    };
    use tomte_app::theme::Theme;

    fn s(target: &str, class: &str) -> DriftSummary {
        DriftSummary {
            target: target.into(),
            class: class.into(),
            since_ts: Some(1),
            origin_differs: false,
        }
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn plan_of(drifted: &[DriftSummary], queue: &QueueSnapshot) -> SidebarPlan {
        sidebar_rows(SidebarInputs {
            drifted,
            queue,
            mode: ReviewMode::Flat,
            collapsed: &BTreeSet::new(),
            selection: &Selection::default(),
            home: Path::new("/Users/x"),
        })
    }

    fn keys(names: &[&str]) -> Vec<RowKey> {
        names
            .iter()
            .map(|n| RowKey::Target {
                path: PathBuf::from(n),
                queued: None,
                failed: false,
            })
            .collect()
    }

    /// `ListState::splice(0..old, new)` snaps the scroll anchor to item 0
    /// whenever the anchor lies in the range — always, for a whole-list
    /// splice. So only the window that changed is spliced (spec 2026-10-11
    /// §2.2): common prefix and suffix are kept.
    ///
    /// MUTATION: ignore the suffix (`suffix = 0`) → "removed from the
    /// middle" reports `(1..3, 1)` instead of `(1..2, 0)`.
    #[test]
    fn splice_window_keeps_the_common_prefix_and_suffix() {
        assert_eq!(
            splice_window(&keys(&["a", "b", "c"]), &keys(&["a", "b", "c"])),
            None
        );
        assert_eq!(
            splice_window(&keys(&["a", "b", "c"]), &keys(&["p", "q", "a", "b", "c"])),
            Some((0..0, 2)),
            "a PROCESSING group appearing at the top"
        );
        assert_eq!(
            splice_window(&keys(&["a", "b", "c"]), &keys(&["a", "c"])),
            Some((1..2, 0))
        );
        assert_eq!(
            splice_window(&keys(&["a", "b"]), &keys(&["c", "d"])),
            Some((0..2, 2))
        );
        assert_eq!(
            splice_window(&keys(&["a", "a"]), &keys(&["a"])),
            Some((1..2, 0)),
            "no overlap"
        );
        assert_eq!(splice_window(&[], &keys(&["a"])), Some((0..0, 1)));
    }

    /// The anchor follows its row by key across a rebuild (gpui's `splice`
    /// snaps it to the window's start when it lies inside the window).
    ///
    /// MUTATION: return `Some(anchor_ix)` unchanged → "a row moved to
    /// PROCESSING above the anchor" reports 2 instead of 3. MUTATION: drop
    /// the later-survivor search → "the anchor row itself left" falls back
    /// to the earlier neighbour (1) instead of the later one (2).
    #[test]
    fn re_anchor_index_follows_the_anchor_row_by_key() {
        let old = keys(&["p", "a", "b", "c", "d"]);
        // `c` (below the anchor `b`) moved up into PROCESSING with a new key.
        let mut new = keys(&["p", "a", "b", "d"]);
        new.insert(
            1,
            RowKey::Target {
                path: PathBuf::from("c"),
                queued: Some(QueuedStatus::Waiting),
                failed: false,
            },
        );
        assert_eq!(
            re_anchor_index(&old, &new, 2),
            Some(3),
            "a row moved to PROCESSING above the anchor: `b` is one lower"
        );
        assert_eq!(
            re_anchor_index(&old, &keys(&["p", "a", "c", "d"]), 2),
            Some(2),
            "the anchor row itself left: the next surviving row (`c`)"
        );
        assert_eq!(
            re_anchor_index(&old, &keys(&["p", "a"]), 3),
            Some(1),
            "no later row survived: the nearest earlier one (`a`)"
        );
        assert_eq!(
            re_anchor_index(&old, &new, 5),
            None,
            "the anchor was the footer, past the last row"
        );
        assert_eq!(re_anchor_index(&[], &new, 0), None, "an empty list");
        assert_eq!(
            re_anchor_index(&old, &keys(&["x"]), 1),
            None,
            "no old row survived"
        );
    }

    /// A row that grew a sub-line (queued, or failed) is taller: its key
    /// changes so the list re-measures it in place.
    ///
    /// MUTATION: drop `queued` (or `failed`) from `RowKey::Target` → the
    /// two keys compare equal and the window is `None`.
    #[test]
    fn a_row_whose_status_changed_is_spliced_in_place() {
        let before = keys(&["a", "b", "c"]);
        let mut after = before.clone();
        after[1] = RowKey::Target {
            path: PathBuf::from("b"),
            queued: Some(QueuedStatus::Waiting),
            failed: false,
        };
        assert_eq!(splice_window(&before, &after), Some((1..2, 1)));
        let mut failed = before.clone();
        failed[2] = RowKey::Target {
            path: PathBuf::from("c"),
            queued: None,
            failed: true,
        };
        assert_eq!(splice_window(&before, &failed), Some((2..3, 1)));
    }

    /// `row_keys` reads the plan AND the failures: a failed row's key says
    /// so even though `SidebarRow::Target` does not carry the message.
    ///
    /// MUTATIONS: `failed: false` always → the `/b` assertion fails;
    /// `queued: None` always → the `/c` assertion fails.
    #[test]
    fn row_keys_follow_the_plan_and_the_failures() {
        let drifted = vec![
            s("/a", "conflict"),
            s("/b", "destination_drift"),
            s("/c", "destination_drift"),
        ];
        let queue = QueueSnapshot {
            processing: vec![(
                PathBuf::from("/c"),
                QueuedDecision {
                    status: QueuedStatus::Waiting,
                    label: "Keep disk",
                },
            )],
            failures: BTreeMap::from([(PathBuf::from("/b"), "op: could not authenticate".into())]),
            busy: false,
        };
        let plan = plan_of(&drifted, &queue);
        let ks = row_keys(&plan, &queue);
        let target = |p: &str, queued, failed| RowKey::Target {
            path: PathBuf::from(p),
            queued,
            failed,
        };
        assert!(matches!(ks[0], RowKey::Group(_)), "PROCESSING leads");
        assert_eq!(ks[1], target("/c", Some(QueuedStatus::Waiting), false));
        assert_eq!(ks[2], RowKey::Group("NEEDS A DECISION"));
        assert_eq!(ks[3], target("/a", None, false));
        assert_eq!(ks[4], RowKey::Group("SAFE TO RESOLVE"));
        assert_eq!(ks[5], target("/b", None, true));
        assert_eq!(ks.len(), 6);
    }

    fn targets(group: &[DriftSummary]) -> Vec<&Path> {
        group.iter().map(|d| d.target.as_path()).collect()
    }

    #[test]
    fn source_action_splits_only_when_origin_differs() {
        assert_eq!(
            source_actions(false),
            vec![ResolveAction::KeepSourceLocal],
            "one action when origin agrees — the common case stays simple"
        );
        assert_eq!(
            source_actions(true),
            vec![
                ResolveAction::KeepSourceLocal,
                ResolveAction::KeepSourceOrigin
            ]
        );
    }

    /// The bulk pair is exactly the one-click pair and nothing else (spec
    /// §2.1). MUTATION: `bulk_actions` returns `[KeepDisk, KeepSourceOrigin]`
    /// (same arity as the real pair, so it still compiles — appending a
    /// THIRD element instead changes the return type, `[ResolveAction; 3]`,
    /// which is a compile error the crate cannot build, not something this
    /// `assert_eq!` catches) → the first assertion fails.
    #[test]
    fn bulk_actions_are_exactly_keep_disk_and_keep_source_this_machine() {
        assert_eq!(
            bulk_actions(),
            [ResolveAction::KeepDisk, ResolveAction::KeepSourceLocal]
        );
        // `Decision` has no `PartialEq` (it holds an `Arc<MergeInputs>` in
        // two of its variants), so the pairing is asserted by shape.
        // MUTATION: `bulk_decision(KeepSourceLocal)` returns
        // `Decision::KeepDisk` → these `matches!` assertions fail.
        assert!(matches!(
            bulk_decision(ResolveAction::KeepDisk),
            Decision::KeepDisk
        ));
        assert!(matches!(
            bulk_decision(ResolveAction::KeepSourceLocal),
            Decision::KeepSourceLocal
        ));
    }

    /// The list row is the one place the scan flag decides anything, and it
    /// must decide exactly that: flag on → hint, flag off → nothing. Nothing
    /// here loads content, so there is no more truthful signal to gate on.
    #[test]
    fn the_row_hint_follows_the_scan_flag_and_nothing_else() {
        let theme = Theme::dark();
        let mut entry = s("/tmp/.zshrc", "destination_drift");
        assert!(
            origin_hint(0, &entry, theme).is_none(),
            "no flag, no hint on the row"
        );
        entry.origin_differs = true;
        assert!(origin_hint(0, &entry, theme).is_some());
    }

    /// …and the hint stays a hint. A file added locally and never pushed is
    /// flagged and has NO origin blob, so the row hints while every
    /// content-bearing surface correctly offers nothing origin-shaped. The
    /// wording has to survive that pairing without reading like a promise.
    #[test]
    fn the_row_hint_only_claims_what_the_scan_measured() {
        assert_eq!(ORIGIN_HINT, "differs from origin");
        for promise in ["compare", "version", "available", "open"] {
            assert!(
                !ORIGIN_HINT.contains(promise),
                "the row hint must not promise {promise:?}: the flagged file may \
                 have no origin blob at all"
            );
        }
        // The surface that DOES hold content is unmoved by the flag: with no
        // origin content loaded there is one source action, flag or no flag.
        assert_eq!(source_actions(false), vec![ResolveAction::KeepSourceLocal]);
    }

    #[test]
    fn diff_pair_labels_name_both_sides() {
        assert_eq!(
            DiffPair::DiskVsLocal.label(),
            "on disk ↔ source · this machine"
        );
        assert_eq!(DiffPair::DiskVsOrigin.label(), "on disk ↔ source · origin");
        assert_eq!(
            DiffPair::LocalVsOrigin.label(),
            "source: this machine ↔ origin"
        );
    }

    #[test]
    fn button_labels_only_say_this_machine_when_the_split_is_offered() {
        // Origin agrees: the common case must not get wordier.
        assert_eq!(
            ResolveAction::KeepSourceLocal.button_label(false),
            "Keep source"
        );
        assert_eq!(
            ResolveAction::KeepSourceLocal.button_label(true),
            "Keep source · this machine"
        );
        assert_eq!(
            ResolveAction::KeepSourceOrigin.button_label(true),
            "Keep source · origin"
        );
        // The neighbouring action never changes shape.
        assert_eq!(ResolveAction::KeepDisk.button_label(true), "Keep disk");
        // Failure messages keep the plain name in every case.
        assert_eq!(ResolveAction::KeepSourceLocal.label(), "Keep source");
    }

    #[test]
    fn action_tints_carry_the_side_each_one_keeps() {
        // The gallery renders these buttons through the DISABLED path (no
        // daemon), so the tints cannot be read off a screenshot — they are
        // pinned here instead.
        let theme = Theme::dark();
        assert_eq!(ResolveAction::KeepDisk.tint(theme), theme.drift);
        assert_eq!(ResolveAction::KeepSourceLocal.tint(theme), theme.accent);
        assert_eq!(ResolveAction::KeepSourceOrigin.tint(theme), theme.remote);
        // …and the selector's chosen segment matches the pair it shows.
        assert_eq!(DiffPair::DiskVsLocal.tint(theme), theme.accent);
        assert_eq!(DiffPair::DiskVsOrigin.tint(theme), theme.remote);
        assert_eq!(DiffPair::LocalVsOrigin.tint(theme), theme.remote);
    }

    /// A press this view refuses outright names the button that was pressed —
    /// and with two "Keep source …" buttons on screen, WHICH one.
    #[test]
    fn a_refused_source_action_names_the_button_that_was_pressed() {
        let why = "daemon gone";
        // Unsplit: one "Keep source" button, so the plain name is unambiguous.
        assert_eq!(
            refused_banner(ResolveAction::KeepSourceLocal, false, why)
                .text
                .as_ref(),
            "Keep source failed: daemon gone"
        );
        // Split: two were on screen — the banner has to disambiguate.
        assert_eq!(
            refused_banner(ResolveAction::KeepSourceLocal, true, why)
                .text
                .as_ref(),
            "Keep source · this machine failed: daemon gone"
        );
        let b = refused_banner(ResolveAction::KeepSourceOrigin, true, why);
        assert_eq!(b.text.as_ref(), "Keep source · origin failed: daemon gone");
        assert_eq!(b.tint, BannerTint::Conflict);
        assert!(!b.undoable, "nothing ran, so there is nothing to undo");
    }

    #[test]
    fn an_empty_diff_names_the_pair_it_is_empty_for() {
        // The note used to be hardcoded to the default pair, which made it a
        // false statement on the other two: disk CAN equal origin (another
        // machine pushed a change this disk already has) while still differing
        // from this machine's source.
        assert_eq!(
            DiffPair::DiskVsLocal.matches_note().as_ref(),
            "the destination matches the rendered source"
        );
        assert_eq!(
            DiffPair::DiskVsOrigin.matches_note().as_ref(),
            "the destination matches origin's version"
        );
        assert_eq!(
            DiffPair::LocalVsOrigin.matches_note().as_ref(),
            "origin's version matches this machine's source"
        );
    }

    #[test]
    fn origin_diffs_remember_the_text_they_were_built_from() {
        // The click-time guard compares the reloaded origin against this, so
        // it has to be origin's text verbatim — not a re-derivation.
        let diffs = origin_diffs("a=origin\n", "a=disk\n", "a=local\n");
        assert_eq!(diffs.seen, "a=origin\n");
    }

    /// A queue pose: `running` are under the latch, `waiting` behind it.
    /// Every row carries the same choice — the tests that care about WHICH
    /// choice pose it explicitly (`posed_queue_of`).
    fn posed_queue(running: &[&str], waiting: &[&str], busy: bool) -> QueueSnapshot {
        let rows: Vec<(&str, QueuedStatus, &'static str)> = running
            .iter()
            .map(|t| (*t, QueuedStatus::Running, "Keep disk"))
            .chain(
                waiting
                    .iter()
                    .map(|t| (*t, QueuedStatus::Waiting, "Keep disk")),
            )
            .collect();
        posed_queue_of(&rows, busy)
    }

    /// The same, with each row's decision label spelled out.
    fn posed_queue_of(rows: &[(&str, QueuedStatus, &'static str)], busy: bool) -> QueueSnapshot {
        QueueSnapshot {
            processing: rows
                .iter()
                .map(|(t, status, label)| {
                    (
                        PathBuf::from(*t),
                        QueuedDecision {
                            status: *status,
                            label,
                        },
                    )
                })
                .collect(),
            failures: Default::default(),
            busy,
        }
    }

    /// The sidebar plan as one line per row: a header's label, or a row's
    /// target, the DECISION it is waiting on, the state it renders in, and
    /// whether it is still a click target.
    ///
    /// The lock is in here because it is what makes §2.4 reachable: the
    /// selection has advanced off the row by the time a decision is queued,
    /// so the row click is the ONLY route back to its live buttons. It is
    /// read off the row's own `locked` flag — the very bool `target_row`
    /// renders from — so this projection moves when that rule moves, and
    /// the render site has no second rule to drift into.
    fn plan(rows: &[SidebarRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                SidebarRow::Group { label, .. } => (*label).to_string(),
                SidebarRow::Folder { .. } => unreachable!("these tests never build a tree"),
                SidebarRow::Target {
                    entry,
                    queued,
                    locked,
                    ..
                } => format!(
                    "{} [{}]",
                    entry.target.display(),
                    match queued {
                        None => "answerable".to_string(),
                        Some(queued) => format!(
                            "{} {}, {}",
                            queued.label,
                            match queued.status {
                                QueuedStatus::Running => "running",
                                QueuedStatus::Waiting => "queued",
                            },
                            // The row's OWN flag, not a re-derivation.
                            if *locked { "locked" } else { "selectable" }
                        ),
                    }
                ),
            })
            .collect()
    }

    /// MUTATION: `cancellable: queued.is_some()` in `sidebar_rows` → the
    /// running row reads cancellable and this fails. (Pins the plan field
    /// only; the render wiring is pinned by the real-click test in mod.rs.)
    #[test]
    fn only_a_waiting_processing_row_is_cancellable() {
        let drifted = vec![
            s("/a", "destination_drift"),
            s("/b", "destination_drift"),
            s("/c", "destination_drift"),
        ];
        let queue = posed_queue(&["/a"], &["/b"], true);
        let flags: Vec<(String, bool)> = plan_of(&drifted, &queue)
            .rows
            .iter()
            .filter_map(|r| match r {
                SidebarRow::Target {
                    entry, cancellable, ..
                } => Some((entry.target.display().to_string(), *cancellable)),
                _ => None,
            })
            .collect();
        assert_eq!(
            flags,
            vec![
                ("/a".to_string(), false),
                ("/b".to_string(), true),
                ("/c".to_string(), false),
            ]
        );
    }

    /// A file gets EXACTLY ONE row. Spec §2.1 has the row move to
    /// PROCESSING; leaving it in its severity group as well renders the same
    /// file twice — once inert, once live — which reads as two files and
    /// invites answering a decision that is already recorded.
    ///
    /// MUTATION: drop the `!queue.is_processing(…)` filter from
    /// `sidebar_rows` → `/a` appears under PROCESSING and again under NEEDS
    /// A DECISION. MUTATION: widen `QueuedStatus::locks_row` to
    /// `matches!(self, Running | Waiting)` (or spell the comparison out as
    /// `queued.is_some()` in `target_row`) → the queued row reads `locked`,
    /// and the only route back to a mis-clicked file's buttons is gone.
    #[test]
    fn a_processing_row_moves_out_of_its_severity_group_instead_of_doubling() {
        let drifted = vec![s("/a", "conflict"), s("/b", "destination_drift")];

        // Nothing queued: the two groups, no PROCESSING header.
        assert_eq!(
            plan(&plan_of(&drifted, &QueueSnapshot::default()).rows),
            [
                "NEEDS A DECISION",
                "/a [answerable]",
                "SAFE TO RESOLVE",
                "/b [answerable]",
            ]
        );

        // `/a` answered: it moves, and its group goes with it.
        assert_eq!(
            plan(&plan_of(&drifted, &posed_queue(&["/a"], &[], true)).rows),
            [
                "PROCESSING",
                "/a [Keep disk running, locked]",
                "SAFE TO RESOLVE",
                "/b [answerable]",
            ]
        );

        // Both answered: running first, then waiting, and no severity group
        // is left to head.
        assert_eq!(
            plan(&plan_of(&drifted, &posed_queue(&["/a"], &["/b"], true)).rows),
            [
                "PROCESSING",
                "/a [Keep disk running, locked]",
                // The whole of §2.4's reachability, in one word: a decision
                // that has not started yet can still be re-chosen.
                "/b [Keep disk queued, selectable]",
            ]
        );
    }

    /// A processing row names the DECISION it is waiting on, not merely that
    /// it is waiting. Thirty rows all reading "running…" are thirty file
    /// names and no answer to the question the user is actually asking:
    /// which of these did I press keep-disk on?
    ///
    /// MUTATION: drop `label` from the row's trailing text (render the bare
    /// status) → the two rows below become indistinguishable. MUTATION: have
    /// `ActionQueue::decision_label` search pending before running → a row
    /// re-chosen mid-flight would name the choice that is NOT running.
    #[test]
    fn a_processing_row_names_the_decision_it_is_waiting_on() {
        let drifted = vec![s("/a", "conflict"), s("/b", "destination_drift")];
        let queue = posed_queue_of(
            &[
                ("/a", QueuedStatus::Running, "Keep both"),
                ("/b", QueuedStatus::Waiting, "Keep source · this machine"),
            ],
            true,
        );
        assert_eq!(
            plan(&plan_of(&drifted, &queue).rows),
            [
                "PROCESSING",
                "/a [Keep both running, locked]",
                "/b [Keep source · this machine queued, selectable]",
            ]
        );
        // And the row's own trailing line, which is what the sidebar renders.
        assert_eq!(
            queue
                .status(Path::new("/a"))
                .expect("posed")
                .trailing()
                .as_ref(),
            "Keep both · running…"
        );
        assert_eq!(
            queue
                .status(Path::new("/b"))
                .expect("posed")
                .trailing()
                .as_ref(),
            "Keep source · this machine · queued"
        );
    }

    /// A queued target the model has stopped listing still gets its row:
    /// work is owed on it, and the PROCESSING group is the only trace of
    /// that.
    #[test]
    fn a_processing_row_survives_the_model_dropping_it() {
        let plan_result = plan_of(&[], &posed_queue(&["/gone"], &[], true));
        assert_eq!(
            plan(&plan_result.rows),
            ["PROCESSING", "/gone [Keep disk running, locked]"]
        );
    }

    /// Flat mode: safe rows are selectable, needs-you and processing rows
    /// are not; `safe_order` and `visible_safe` agree and list the safe rows
    /// in group order; the SAFE header carries the tri-state.
    /// MUTATION: mark every Target selectable → the needs-you assertion
    /// fails; drop the header check → the last fails.
    #[test]
    fn flat_plan_marks_only_safe_rows_selectable_and_orders_them() {
        let drifted = vec![
            s("/Users/x/.config/a", "conflict"),
            s("/Users/x/b", "destination_drift"),
            s("/Users/x/c", "remote_ahead"),
        ];
        let plan = plan_of(&drifted, &QueueSnapshot::default());
        let selectable: Vec<(PathBuf, bool)> = plan
            .rows
            .iter()
            .filter_map(|r| match r {
                SidebarRow::Target {
                    entry, selectable, ..
                } => Some((entry.target.clone(), *selectable)),
                _ => None,
            })
            .collect();
        assert_eq!(
            selectable,
            vec![
                (p("/Users/x/.config/a"), false),
                (p("/Users/x/b"), true),
                (p("/Users/x/c"), true)
            ]
        );
        assert_eq!(plan.safe_order, vec![p("/Users/x/b"), p("/Users/x/c")]);
        assert_eq!(plan.visible_safe, plan.safe_order);
        assert!(plan.rows.iter().any(|r| matches!(
            r,
            SidebarRow::Group {
                label: "SAFE TO RESOLVE",
                check: Some(CheckState::Off)
            }
        )));
        assert!(plan.rows.iter().any(|r| matches!(
            r,
            SidebarRow::Group {
                label: "NEEDS A DECISION",
                check: None
            }
        )));
    }

    /// Tree mode: needs-you and processing stay flat above; the safe group is
    /// folders + files; a folder's check reflects the selection over its
    /// descendants; collapsed files leave `visible_safe` but not `safe_order`.
    /// MUTATION: build the tree over ALL drifted rows (pass `drifted`'s
    /// targets instead of `safe_targets` to `tree_rows`) → this does not
    /// quietly show the conflict as a file row; `sidebar_rows`' tree arm's
    /// `safe.iter().find(...)`'s `.expect("tree rows come from \`safe\`")`
    /// panics first, since the conflict target is not in `safe`. MUTATION:
    /// use `visible_safe` for `safe_order` → the collapsed file is missing
    /// from it.
    #[test]
    fn tree_plan_keeps_the_other_groups_flat_and_tracks_collapse_and_ticks() {
        let drifted = vec![
            s("/Users/x/.config/needs", "conflict"),
            s("/Users/x/.config/nvim/init.lua", "destination_drift"),
            s("/Users/x/.config/nvim/lua/p.lua", "source_ahead"),
            s("/Users/x/top", "remote_ahead"),
        ];
        let mut selection = Selection::default();
        let order = vec![
            p("/Users/x/.config/nvim/init.lua"),
            p("/Users/x/.config/nvim/lua/p.lua"),
            p("/Users/x/top"),
        ];
        selection.toggle(&p("/Users/x/.config/nvim/init.lua"), &order);
        let collapsed = BTreeSet::from([p("/Users/x/.config/nvim/lua")]);
        let plan = sidebar_rows(SidebarInputs {
            drifted: &drifted,
            queue: &QueueSnapshot::default(),
            mode: ReviewMode::Tree,
            collapsed: &collapsed,
            selection: &selection,
            home: Path::new("/Users/x"),
        });
        let labels: Vec<String> = plan
            .rows
            .iter()
            .map(|r| match r {
                SidebarRow::Group { label, .. } => format!("G:{label}"),
                SidebarRow::Folder { node, check } => {
                    format!("F:{}:{}:{:?}", node.label, node.collapsed, check)
                }
                SidebarRow::Target {
                    entry,
                    depth,
                    selectable,
                    checked,
                    ..
                } => {
                    format!(
                        "T:{}:{depth}:{selectable}:{checked}",
                        entry.target.file_name().unwrap().to_string_lossy()
                    )
                }
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                "G:NEEDS A DECISION",
                "T:needs:0:false:false",
                "G:SAFE TO RESOLVE",
                // `.config/` holds the excluded `needs`, so it is NOT
                // compacted into `nvim/` (a folder with a file stops the
                // chain — by structure, not by name).
                "F:.config/:false:Partial",
                "F:nvim/:false:Partial",
                "F:lua/:true:Off",
                "T:init.lua:2:true:true",
                "T:top:0:true:false",
            ]
        );
        assert_eq!(
            plan.safe_order,
            vec![
                p("/Users/x/.config/nvim/lua/p.lua"),
                p("/Users/x/.config/nvim/init.lua"),
                p("/Users/x/top")
            ]
        );
        assert_eq!(
            plan.visible_safe,
            vec![p("/Users/x/.config/nvim/init.lua"), p("/Users/x/top")]
        );
    }

    /// A PROCESSING target the drift model has already dropped (a rescan
    /// cleared its row while its decision was still running, spec §2.5)
    /// still counts toward its folder's denominator — the chip reads "1 of
    /// 2", not a bare "1". MUTATION: build `excluded` from `drifted` alone
    /// (drop the `queue.processing` pass in `sidebar_rows`) → the folder
    /// reads `(safe: 1, total: 1)` instead of `(1, 2)`.
    #[test]
    fn a_dropped_processing_target_still_counts_in_its_folders_denominator() {
        let drifted = vec![s("/Users/x/.config/a", "destination_drift")];
        let queue = posed_queue(&["/Users/x/.config/b"], &[], false);
        let plan = sidebar_rows(SidebarInputs {
            drifted: &drifted,
            queue: &queue,
            mode: ReviewMode::Tree,
            collapsed: &BTreeSet::new(),
            selection: &Selection::default(),
            home: Path::new("/Users/x"),
        });
        let folder = plan.rows.iter().find_map(|r| match r {
            SidebarRow::Folder { node, .. } => Some((node.safe, node.total)),
            _ => None,
        });
        assert_eq!(
            folder,
            Some((1, 2)),
            "the dropped processing target still counts toward the folder's total"
        );
    }

    /// Spec §2.4 has to be REACHABLE: a decision that has not started yet
    /// can be changed, so the row's buttons stay live while it is merely
    /// queued and go inert only once it is running. Gating on "processing"
    /// instead made a mis-click uncorrectable for as long as the queue was
    /// deep — exactly when correcting it matters.
    ///
    /// MUTATION: gate `blocked_reason` on `is_processing` instead of
    /// `QueueSnapshot::locks_row` → the queued case reports a reason and the
    /// buttons go dead. MUTATION: widen `QueuedStatus::locks_row`, the rule
    /// itself, to accept `Waiting` → same.
    #[test]
    fn only_a_running_decision_locks_its_file_out_of_being_re_chosen() {
        let a = Path::new("/a");
        assert_eq!(
            blocked_reason(&QueueSnapshot::default(), a),
            None,
            "nothing owed: live"
        );
        assert_eq!(
            blocked_reason(&posed_queue(&[], &["/a"], true), a),
            None,
            "queued but not started: still answerable, and choosing again replaces it"
        );
        assert_eq!(
            blocked_reason(&posed_queue(&["/a"], &[], true), a)
                .map(|r| r.to_string())
                .unwrap_or_default(),
            "applying this file's decision now"
        );
        // Another file's work never blocks this one — the user's complaint.
        assert_eq!(
            blocked_reason(&posed_queue(&["/other"], &["/third"], true), a),
            None
        );
        // The rule itself, which the row and the header both read.
        assert!(QueuedStatus::Running.locks_row());
        assert!(
            !QueuedStatus::Waiting.locks_row(),
            "a queued decision has not started; re-choosing it replaces it"
        );
        let queue = posed_queue(&["/a"], &["/b"], true);
        assert!(queue.locks_row(a));
        assert!(!queue.locks_row(Path::new("/b")));
        assert!(
            queue.is_processing(Path::new("/b")),
            "a queued row is still listed under PROCESSING and skipped by the advance"
        );
    }

    /// Undo is offered-but-inert while ANY git work is owed, never hidden:
    /// "undo the last thing that landed" is unambiguous only when nothing is
    /// landing, and a button that vanishes says nothing about why.
    ///
    /// MUTATION: drop the busy arm from `undo_affordance` (always `Live`
    /// when undoable) → the second case fails, and the banner would offer an
    /// Undo whose target depends on which queued decision finishes first.
    #[test]
    fn undo_is_shown_inert_while_the_queue_owes_work() {
        let landed = OutcomeBanner {
            text: "Kept disk version · committed & pushed".into(),
            tint: BannerTint::Ok,
            undoable: true,
        };
        let failed = OutcomeBanner {
            text: "Keep disk failed: daemon gone".into(),
            tint: BannerTint::Conflict,
            undoable: false,
        };
        let idle = QueueSnapshot::default();
        let busy = posed_queue(&[], &["/a"], true);

        assert_eq!(undo_affordance(&landed, &idle), UndoAffordance::Live);
        assert_eq!(undo_affordance(&landed, &busy), UndoAffordance::Blocked);
        // An outcome that cannot be undone offers nothing either way — the
        // busy state must not turn a non-offer into an inert offer.
        assert_eq!(undo_affordance(&failed, &idle), UndoAffordance::None);
        assert_eq!(undo_affordance(&failed, &busy), UndoAffordance::None);
    }

    /// Spec §2.6: confirming advances the selection, so a sweep is one click
    /// per file with no re-targeting click between.
    ///
    /// MUTATION: drop the `.or_else(…)` backward search → the "last row"
    /// case returns `None`. MUTATION: drop `!processing(p)` from
    /// `answerable` → the third case lands on `/c`, a row whose buttons are
    /// inert.
    #[test]
    fn next_after_prefers_the_following_row_then_the_preceding_one_and_skips_processing() {
        let order: Vec<PathBuf> = ["/a", "/b", "/c", "/d"].iter().map(PathBuf::from).collect();
        let none = |_: &Path| false;
        assert_eq!(
            next_after(Path::new("/b"), &order, &none),
            Some(PathBuf::from("/c"))
        );
        assert_eq!(
            next_after(Path::new("/d"), &order, &none),
            Some(PathBuf::from("/c")),
            "last row: fall back to the one before"
        );
        let c_busy = |p: &Path| p == Path::new("/c");
        assert_eq!(
            next_after(Path::new("/b"), &order, &c_busy),
            Some(PathBuf::from("/d")),
            "a processing row is not a place to land"
        );
        let only: Vec<PathBuf> = vec![PathBuf::from("/a")];
        assert_eq!(next_after(Path::new("/a"), &only, &none), None);
        // The row just answered is itself processing by the time this runs,
        // but it must not be a candidate even if the caller says otherwise.
        assert_eq!(
            next_after(Path::new("/a"), &only, &|_| false),
            None,
            "the row just answered is never the next one to answer"
        );
        // A list that moved underneath the click leaves the selection alone.
        assert_eq!(next_after(Path::new("/zz"), &order, &none), None);
    }

    #[test]
    fn severity_groups_classifies_needs_you_vs_one_click() {
        let drifted = vec![
            s("/a", "conflict"),
            s("/b", "destination_drift"),
            s("/c", "local_source_diverged"),
            s("/d", "source_ahead"),
            s("/e", "eval_failed"),
            s("/f", "remote_ahead"),
        ];
        let (needs_you, one_click) = severity_groups(&drifted);
        assert_eq!(
            targets(&needs_you),
            [Path::new("/a"), Path::new("/c"), Path::new("/e")]
        );
        assert_eq!(
            targets(&one_click),
            [Path::new("/b"), Path::new("/d"), Path::new("/f")]
        );
    }

    #[test]
    fn severity_groups_orders_conflicts_first_within_needs_you() {
        let drifted = vec![
            s("/lsd", "local_source_diverged"),
            s("/c1", "conflict"),
            s("/ef", "eval_failed"),
            s("/c2", "conflict"),
        ];
        let (needs_you, one_click) = severity_groups(&drifted);
        assert_eq!(
            targets(&needs_you),
            [
                Path::new("/c1"),
                Path::new("/c2"), // conflicts first, input order preserved
                Path::new("/lsd"),
                Path::new("/ef"), // then the rest, input order preserved
            ]
        );
        assert!(one_click.is_empty());
    }

    #[test]
    fn severity_groups_is_stable_within_groups() {
        let drifted = vec![
            s("/1", "destination_drift"),
            s("/2", "remote_ahead"),
            s("/3", "destination_drift"),
            s("/4", "source_ahead"),
        ];
        let (needs_you, one_click) = severity_groups(&drifted);
        assert!(needs_you.is_empty());
        assert_eq!(
            targets(&one_click),
            [
                Path::new("/1"),
                Path::new("/2"),
                Path::new("/3"),
                Path::new("/4")
            ]
        );
    }

    #[test]
    fn severity_groups_puts_unknown_classes_in_one_click() {
        // Unknown future classes must not block the "needs you" queue; they
        // surface in the one-click group where the class chip still shows.
        let drifted = vec![s("/a", "some_future_class"), s("/b", "conflict")];
        let (needs_you, one_click) = severity_groups(&drifted);
        assert_eq!(targets(&needs_you), [Path::new("/b")]);
        assert_eq!(targets(&one_click), [Path::new("/a")]);
    }

    #[test]
    fn severity_groups_empty_input_yields_empty_groups() {
        let (needs_you, one_click) = severity_groups(&[]);
        assert!(needs_you.is_empty());
        assert!(one_click.is_empty());
    }

    /// base = rendered, ours = destination, theirs = rendered (the preview's
    /// exact call shape).
    fn two_way(rendered: &str, destination: &str) -> MergeDocument {
        MergeDocument::compute(rendered, destination, rendered, MergeOptions::default())
    }

    fn shape(lines: &[DiffLine]) -> Vec<(&str, LineTint, &str)> {
        lines
            .iter()
            .map(|l| (l.text.as_ref(), l.tint, l.marker))
            .collect()
    }

    #[test]
    fn two_way_compute_never_yields_conflicts() {
        // With theirs == base the 3-way engine must degenerate to 2-way:
        // only Unchanged and OursOnly regions can appear.
        let cases = [
            ("a\nb\nc\n", "a\nB\nc\n"),
            ("a\n", "a\nextra\n"),
            ("gone\nkeep\n", "keep\n"),
            ("", "new\n"),
            ("old\n", ""),
        ];
        for (rendered, destination) in cases {
            let doc = two_way(rendered, destination);
            for region in &doc.regions {
                assert!(
                    matches!(region.kind, RegionKind::Unchanged | RegionKind::OursOnly),
                    "unexpected {:?} for ({rendered:?}, {destination:?})",
                    region.kind
                );
            }
        }
    }

    #[test]
    fn flatten_maps_two_way_regions_to_tinted_lines() {
        let doc = two_way("a\nv = 1\nz\n", "a\nv = 2\nz\n");
        let lines = flatten_document(&doc, DiffPair::DiskVsLocal);
        assert_eq!(
            shape(&lines),
            [
                ("a", LineTint::Context, " "),
                ("v = 1", LineTint::Rendered, "●"),
                ("v = 2", LineTint::Destination, "●"),
                ("z", LineTint::Context, " "),
            ]
        );
        // word-diff emphasis lands on the changed word only ("1" / "2")
        assert_eq!(lines[1].highlights, vec![4..5]);
        assert_eq!(lines[2].highlights, vec![4..5]);
        assert!(lines[0].highlights.is_empty());
        assert!(lines[3].highlights.is_empty());
    }

    #[test]
    fn flatten_disk_only_addition_has_no_rendered_pair() {
        let doc = two_way("a\nb\n", "a\nx\nb\n");
        let lines = flatten_document(&doc, DiffPair::DiskVsLocal);
        assert_eq!(
            shape(&lines),
            [
                ("a", LineTint::Context, " "),
                ("x", LineTint::Destination, "●"),
                ("b", LineTint::Context, " "),
            ]
        );
        // one-sided region: no word-diff emphasis (nothing to pair against)
        assert!(lines[1].highlights.is_empty());
    }

    #[test]
    fn flatten_deleted_on_disk_shows_rendered_only() {
        let doc = two_way("only\n", "");
        let lines = flatten_document(&doc, DiffPair::DiskVsLocal);
        assert_eq!(shape(&lines), [("only", LineTint::Rendered, "●")]);
    }

    #[test]
    fn origin_pairs_tint_origin_lines_as_the_remote_side() {
        let local = "a\nv = 1\n";
        let disk = "a\nv = 2\n";
        let origin = "a\nv = 3\n";
        let diffs = origin_diffs(origin, disk, local);

        // disk ↔ origin: origin's line on the remote side, disk's on drift.
        let lines = flatten_document(&diffs.disk_vs_origin, DiffPair::DiskVsOrigin);
        assert_eq!(
            shape(&lines),
            [
                ("a", LineTint::Context, " "),
                ("v = 3", LineTint::Origin, "●"),
                ("v = 2", LineTint::Destination, "●"),
            ]
        );

        // this machine ↔ origin: no disk anywhere — the two SOURCES.
        let lines = flatten_document(&diffs.local_vs_origin, DiffPair::LocalVsOrigin);
        assert_eq!(
            shape(&lines),
            [
                ("a", LineTint::Context, " "),
                ("v = 1", LineTint::Rendered, "●"),
                ("v = 3", LineTint::Origin, "●"),
            ]
        );

        // Origin's field and rail are theme.remote — the tint origin owns
        // everywhere else in the app (merge pane, origin picks).
        let theme = Theme::dark();
        let decors = preview_decors(&lines, theme);
        let ix = lines
            .iter()
            .position(|l| l.tint == LineTint::Origin)
            .unwrap();
        assert_eq!(decors[ix].bg, Some(Theme::wash(theme.remote, 0.07)));
        assert_eq!(decors[ix].rail, Some(theme.remote));
    }

    #[test]
    fn origin_pair_legends_name_the_sides_they_show() {
        let legends = |pair: DiffPair| {
            let (base, ours) = pair.sides();
            (base.legend, ours.legend)
        };
        assert_eq!(
            legends(DiffPair::DiskVsLocal),
            ("source would write", "on disk now")
        );
        assert_eq!(
            legends(DiffPair::DiskVsOrigin),
            ("origin would write", "on disk now")
        );
        assert_eq!(
            legends(DiffPair::LocalVsOrigin),
            ("this machine's source", "origin's source")
        );
    }

    /// What the QUEUE says when a decision lands, minutes after the click.
    ///
    /// A SUCCESS reports itself, whichever surface made it, in that
    /// decision's own words — and undoably, because the banner carries the
    /// Undo button and `undo_last` reverts exactly that session.
    ///
    /// EVERYTHING ELSE stays silent here. A failure's home is its row (spec
    /// §2.5), which is where the retry is; banner-ing it as well would take
    /// the banner away from the last thing that really landed and leave Undo
    /// pointing at a file the sentence above it does not name.
    ///
    /// MUTATION: banner the `Err` arm (or the protected-span
    /// arm) → the `None` assertions below fail, and a failure landing after
    /// a success would clear that success's undoable banner.
    /// MUTATION: return `None` for `Decision::KeepDisk` (the narrowing that
    /// was rejected) → the one-click cases fail, and a sweep after a merge
    /// save would leave the save's banner over an Undo that reverts the
    /// sweep's last file.
    #[test]
    fn a_landed_success_reports_itself_and_a_failure_stays_on_its_row() {
        let inputs = || {
            std::sync::Arc::new(tomte_app::merge_inputs::MergeInputs {
                target: PathBuf::from("/a"),
                ours: String::new(),
                theirs: String::new(),
                base: None,
                source_path: PathBuf::from("/src/a"),
                templated: false,
                template: None,
                span_map: None,
                origin: None,
            })
        };
        let merged = || Decision::Merged {
            resolved: "x".into(),
            inputs: inputs(),
        };
        let ok = |o: ResolveOutcome| Ok(BatchResult::Resolved(o));
        // A file whose name is NOT the one the banner renders under: the
        // auto-advance (spec §2.6) has moved the selection on by the time
        // this lands, so the name is the only thing tying the sentence — and
        // the Undo button under it — to a file.
        let t = Path::new("/Users/x/.config/nushell/env.nu");

        // The merge editor's Save.
        let b = queued_outcome_banner(&merged(), t, &ok(done(true, true, None))).expect("landed");
        assert_eq!(b.text.as_ref(), "Merged · env.nu · committed & pushed");
        assert_eq!(b.tint, BannerTint::Ok);
        assert!(b.undoable);

        // A one-click decision reports itself too: its row is gone from the
        // model by the time this is read, so the banner is the only place
        // "committed & pushed" is ever said — and the only honest home for
        // the Undo button that reverts it.
        let b = queued_outcome_banner(&Decision::KeepDisk, t, &ok(done(true, true, None)))
            .expect("landed");
        assert_eq!(
            b.text.as_ref(),
            "Kept disk version · env.nu · committed & pushed"
        );
        assert!(b.undoable, "its session is exactly what Undo reverts");

        // Keep-source applies and commits nothing: no claim of a push, and
        // the name still has to be there.
        let b = queued_outcome_banner(&Decision::KeepSourceLocal, t, &ok(done(false, false, None)))
            .expect("landed");
        assert_eq!(b.text.as_ref(), "Restored chezmoi's version · env.nu");

        // keep_source_origin routes through `resolve_merged`, so it DOES have
        // a commit phase — unlike its local twin.
        let b = queued_outcome_banner(
            &Decision::KeepSourceOrigin {
                seen: "a=origin\n".into(),
            },
            t,
            &ok(done(true, true, None)),
        )
        .expect("landed");
        assert_eq!(
            b.text.as_ref(),
            "Restored origin's version · env.nu · committed & pushed"
        );

        // A degraded commit still landed, so it is still undoable — and the
        // note goes AFTER the name, so the file is never buried.
        let b = queued_outcome_banner(&merged(), t, &ok(done(true, false, Some("push failed"))))
            .expect("landed");
        assert_eq!(b.text.as_ref(), "Merged · env.nu · push failed");
        assert_eq!(b.tint, BannerTint::Drift);
        assert!(b.undoable, "the merge itself succeeded");

        // The name is the row's, never the path.
        assert_eq!(display_name(t).as_ref(), "env.nu");

        // Nothing landed: every one of these lives on its row instead.
        assert!(
            queued_outcome_banner(
                &merged(),
                t,
                &Err(ResolveError::Failed("daemon gone".into()))
            )
            .is_none(),
            "a failure belongs on its row, where the retry is (spec §2.5)"
        );
        assert!(
            queued_outcome_banner(
                &Decision::KeepBoth {
                    resolved: "x".into(),
                    inputs: inputs(),
                },
                t,
                &Err(ResolveError::Failed("daemon gone".into())),
            )
            .is_none()
        );
        assert!(
            queued_outcome_banner(
                &Decision::KeepDisk,
                t,
                &ok(ResolveOutcome::ProtectedSpan {
                    detail: "touches a protected template span".into()
                })
            )
            .is_none(),
            "a protected-span refusal is a row message, not a landing"
        );
        assert!(
            queued_outcome_banner(
                &merged(),
                t,
                &ok(ResolveOutcome::ProtectedSpan {
                    detail: "edit at rendered bytes 8..13 touches a protected span".into(),
                }),
            )
            .is_none(),
            "nothing was mutated, so there is nothing to report as landed"
        );
        assert!(
            queued_outcome_banner(
                &Decision::RepoConflict {
                    source_path: PathBuf::from("/src/a"),
                    resolved: "x".into(),
                },
                t,
                &Ok(BatchResult::RepoConflictStaged),
            )
            .is_none(),
            "staged into the merge: its conclusion is the banner for it"
        );
    }

    fn done(committed: bool, pushed: bool, note: Option<&str>) -> ResolveOutcome {
        ResolveOutcome::Done {
            session: 7,
            committed,
            pushed,
            note: note.map(str::to_owned),
        }
    }

    #[test]
    fn undo_banner_covers_restored_nothing_and_error() {
        let b = undo_banner(&Ok(Some(3)));
        assert_eq!(b.text.as_ref(), "restored files from snapshots");
        assert_eq!(b.tint, BannerTint::Ok);
        assert!(!b.undoable);

        let b = undo_banner(&Ok(None));
        assert_eq!(b.text.as_ref(), "nothing to undo");
        assert_eq!(b.tint, BannerTint::Drift);

        let b = undo_banner(&Err(ResolveError::Failed("blob missing".into())));
        assert_eq!(b.text.as_ref(), "undo failed: blob missing");
        assert_eq!(b.tint, BannerTint::Conflict);
    }

    #[test]
    fn per_line_ranges_splits_and_clamps_to_display_text() {
        let lines = vec!["ab\n".to_string(), "cd\n".to_string()];
        // one range spanning "b\nc": split across both lines, the newline
        // clamped away from line 0
        let out = per_line_ranges(&lines, std::slice::from_ref(&(1..4)));
        assert_eq!(out, vec![vec![1..2], vec![0..1]]);
        // a range entirely inside the trailing newline vanishes
        let out = per_line_ranges(&lines, std::slice::from_ref(&(2..3)));
        assert_eq!(out, vec![Vec::<std::ops::Range<usize>>::new(), Vec::new()]);
    }

    #[test]
    fn preview_decors_match_the_merge_editor_vocabulary() {
        let theme = Theme::dark();
        // rendered "font = 14" vs disk "font = 15": one changed pair with a
        // word-diff span on the differing digits.
        let doc = MergeDocument::compute(
            "a\nfont = 14\n",
            "a\nfont = 15\n",
            "a\nfont = 14\n",
            MergeOptions::default(),
        );
        let lines = flatten_document(&doc, DiffPair::DiskVsLocal);
        let decors = preview_decors(&lines, theme);
        assert_eq!(decors.len(), lines.len());

        // Context: muted text, no field, no rail, no dot.
        let ctx = lines
            .iter()
            .position(|l| l.tint == LineTint::Context)
            .unwrap();
        assert_eq!(decors[ctx].text_color, Some(theme.text_muted));
        assert_eq!(decors[ctx].bg, None);
        assert_eq!(decors[ctx].rail, None);
        assert!(decors[ctx].gutter.is_none());

        // Changed rows sit on their side's field with a rail — the same
        // treatment the merge result gives its provenance blocks.
        for (tint, side) in [
            (LineTint::Rendered, theme.accent),
            (LineTint::Destination, theme.drift),
        ] {
            let ix = lines.iter().position(|l| l.tint == tint).unwrap();
            assert_eq!(
                decors[ix].bg,
                Some(Theme::wash(side, 0.07)),
                "{tint:?} field"
            );
            assert_eq!(decors[ix].rail, Some(side), "{tint:?} rail");
            assert_eq!(
                decors[ix].text_color,
                Some(theme.text),
                "{tint:?} glyphs stay readable; the field carries the side"
            );
            assert!(
                decors[ix].gutter.is_none(),
                "{tint:?} carries no dot: the field and rail already name the side"
            );
        }

        // Word-diff emphasis survives as translucent spans of the line tint.
        let disk = lines
            .iter()
            .position(|l| l.tint == LineTint::Destination)
            .unwrap();
        assert_eq!(decors[disk].spans.len(), lines[disk].highlights.len());
        if let Some((range, color)) = decors[disk].spans.first() {
            assert_eq!(*range, lines[disk].highlights[0]);
            assert!((color.a - 0.22).abs() < f32::EPSILON);
        }
    }
}
