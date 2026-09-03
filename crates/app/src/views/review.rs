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
//! it is not about the selected row. "Open merge editor" (conflicts,
//! templated files) hands the selected target to the Shell's full-window
//! merge editor (plan 7 Task 3).

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    AnyElement, Context, Div, ElementId, Entity, FontWeight, Rgba, SharedString, Stateful,
    WeakEntity, Window, div, prelude::*, px,
};
use tomte_app::action_queue::{Decision, QueuedAction};
use tomte_app::model::{SyncModel, class_label, kind_glyph, kind_label, time_ago};
use tomte_app::resolve::{ResolveEngine, ResolveError, ResolveOutcome};
use tomte_app::theme::Theme;
use tomte_core::chezmoi::ChezmoiError;
use tomte_core::cmd::{CommandRequest, CommandRunner, SystemRunner};
use tomte_core::merge::{MergeDocument, MergeOptions, RegionKind, worddiff::word_diff};
use tomte_journal::{EventRow, Journal};
use tomte_proto::DriftSummary;
use tomte_ui::components as ui;

use super::Shell;
use super::dashboard::system_now;

/// Fixed sidebar width per the approved mockup A (~260px).
const SIDEBAR_WIDTH: f32 = 260.;
/// The queue must stay wide enough to read a filename, and never swallow
/// the diff it exists to introduce.
const MIN_SIDEBAR_W: f32 = 180.;
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

    /// Past-tense verb for success messages.
    pub(super) fn verb(self) -> &'static str {
        match self {
            Self::KeepDisk => "Kept disk version",
            Self::KeepSourceLocal => "Restored chezmoi's version",
            Self::KeepSourceOrigin => "Restored origin's version",
        }
    }

    /// Run the matching engine method. Blocking — background executor only.
    ///
    /// `origin_seen` is origin's text as the caller SHOWED it (`None` on
    /// surfaces that load no content). Only `KeepSourceOrigin` uses it, and it
    /// refuses without it rather than writing something the user never saw —
    /// see [`ResolveEngine::keep_source_origin`].
    pub(super) fn run(
        self,
        engine: &ResolveEngine,
        target: &Path,
        origin_seen: Option<&str>,
    ) -> Result<ResolveOutcome, ResolveError> {
        match self {
            Self::KeepDisk => engine.keep_disk(target),
            Self::KeepSourceLocal => engine.keep_source(target),
            Self::KeepSourceOrigin => match origin_seen {
                Some(seen) => engine.keep_source_origin(target, seen),
                None => Err(ResolveError::Failed(
                    "no origin version was loaded for this file — open it in Review first".into(),
                )),
            },
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

/// Map an action result onto its banner. Tint policy: a `note` means the
/// resolution succeeded locally but commit/push degraded (drift tint, note
/// shown); no note is full success (ok tint); engine errors are conflicts.
/// `split` is whether the source action was split in two when the user
/// pressed it: with two "Keep source …" buttons on screen, a failure that
/// says only "Keep source failed" leaves them guessing which one it was.
fn outcome_banner(
    action: ResolveAction,
    split: bool,
    result: &Result<ResolveOutcome, ResolveError>,
) -> OutcomeBanner {
    match result {
        Ok(ResolveOutcome::Done {
            note: None,
            committed,
            pushed,
            ..
        }) => OutcomeBanner {
            // keep_source runs no commit phase, so only advertise the
            // commit/push when they actually happened.
            text: if *committed && *pushed {
                format!("{} · committed & pushed", action.verb()).into()
            } else {
                action.verb().into()
            },
            tint: BannerTint::Ok,
            undoable: true,
        },
        Ok(ResolveOutcome::Done {
            note: Some(note), ..
        }) => OutcomeBanner {
            text: format!("{} · {note}", action.verb()).into(),
            tint: BannerTint::Drift,
            undoable: true,
        },
        Ok(ResolveOutcome::NeedsMergeEditor) => OutcomeBanner {
            text: "Templated file · use the merge editor".into(),
            tint: BannerTint::Drift,
            undoable: false,
        },
        // Defensive: only `resolve_merged` produces this, and the merge
        // editor (Plan 7 Task 3) reports it through its own flow.
        Ok(ResolveOutcome::ProtectedSpan { .. }) => OutcomeBanner {
            text: "This change touches a templated value · open the merge editor".into(),
            tint: BannerTint::Drift,
            undoable: false,
        },
        Err(e) => OutcomeBanner {
            text: format!("{} failed: {e}", action.button_label(split)).into(),
            tint: BannerTint::Conflict,
            undoable: false,
        },
    }
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
///
/// The queue can only name the CHOICE for a decision it still holds:
/// `ActionQueue::take_batch` moves the running batch's `QueuedAction`s out to
/// whoever runs them and keeps the targets alone, so a running row's decision
/// is genuinely not there to read. Rather than invent a label for it, the row
/// says which of the two states it is in — both true, neither guessed.
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
}

/// The Shell's action queue as one render pass needs to see it.
///
/// Copied out of the Shell ONCE per render rather than read row by row:
/// every row and button below needs `cx` mutably for its click listener, and
/// a live borrow of the Shell entity cannot be held across that.
#[derive(Default)]
pub(super) struct QueueSnapshot {
    /// Everything queued or running, in `ActionQueue::processing` order —
    /// the running batch first, then the rest as the user chose them.
    processing: Vec<(PathBuf, QueuedStatus)>,
    /// Rows whose last decision failed, with the message that says why.
    failures: BTreeMap<PathBuf, SharedString>,
    /// ANY git work is owed — a decision, or a repo op waiting for the latch.
    busy: bool,
}

impl QueueSnapshot {
    fn status(&self, target: &Path) -> Option<QueuedStatus> {
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

    /// Under the latch RIGHT NOW, and so the one state that locks a row.
    ///
    /// A pending decision does NOT lock it: spec §2.4 says a re-queued
    /// target replaces its pending choice, last one wins, and
    /// `ActionQueue::push` implements exactly that. Locking a pending row
    /// would make a mis-click uncorrectable for as long as the queue is
    /// deep, which is precisely when correcting it matters.
    fn is_running(&self, target: &Path) -> bool {
        self.status(target) == Some(QueuedStatus::Running)
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
    /// This outcome cannot be undone (a failure, a templated refusal, an
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
/// Both reasons are about THIS file — every other file's buttons stay live
/// while this one's work drains, which is the whole point of the queue
/// (spec §1.1) — and the queue reason is RUNNING, not merely queued: a
/// pending decision can still be changed, and `ActionQueue::push` replaces
/// it (spec §2.4). Pure so that rule is a value a test can assert, rather
/// than something only a tooltip in a screenshot could show.
pub(super) fn blocked_reason(
    engine_ready: bool,
    queue: &QueueSnapshot,
    target: &Path,
) -> Option<SharedString> {
    if !engine_ready {
        return Some("daemon not connected".into());
    }
    if queue.is_running(target) {
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

/// One entry of the sidebar, in display order.
///
/// The list is built as data before it is built as elements, because the
/// rule that a row appears EXACTLY ONCE — under PROCESSING while a decision
/// is recorded for it, in its severity group otherwise — is invisible in a
/// screenshot the moment it breaks (the duplicate looks like a second file)
/// and unreachable from a render tree.
// No `PartialEq`: `DriftSummary` has none, and the tests below assert on a
// projection of these rows (label / file name / state) rather than on whole
// summaries anyway.
#[derive(Debug, Clone)]
pub(super) enum SidebarRow {
    Group(&'static str),
    Target {
        entry: DriftSummary,
        queued: Option<QueuedStatus>,
    },
}

/// Plan the sidebar: the PROCESSING group first (spec §2.1 — the row MOVES
/// there), then the two severity groups minus whatever moved.
///
/// A queued target the model no longer lists (a rescan dropped it, say)
/// still gets a row from a synthesised summary: it is answered-and-running,
/// and hiding it would lose the only trace of that.
pub(super) fn sidebar_rows(drifted: &[DriftSummary], queue: &QueueSnapshot) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    if !queue.processing.is_empty() {
        rows.push(SidebarRow::Group("PROCESSING"));
        for (target, status) in &queue.processing {
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
                queued: Some(*status),
            });
        }
    }
    let (needs_you, one_click) = severity_groups(drifted);
    for (label, group) in [
        ("NEEDS A DECISION", &needs_you),
        ("SAFE TO RESOLVE", &one_click),
    ] {
        let mut group = group
            .iter()
            .filter(|d| !queue.is_processing(&d.target))
            .peekable();
        if group.peek().is_none() {
            continue;
        }
        rows.push(SidebarRow::Group(label));
        rows.extend(group.map(|entry| SidebarRow::Target {
            entry: entry.clone(),
            queued: None,
        }));
    }
    rows
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
    preview_editor: Option<Entity<ui::TextArea>>,
    /// Rebuild the editor's content + decorations on the next render (the
    /// preview changed, or the theme flipped).
    preview_dirty: bool,
    preview_appearance: Option<gpui::WindowAppearance>,
    /// Draggable sidebar width (2026-08-27); `None` until first render.
    sidebar_width: Option<f32>,
    sidebar_drag: Option<ui::DragState<()>>,
    /// Draggable History height, and its in-flight drag.
    history_height: Option<f32>,
    history_drag: Option<ui::DragState<()>>,
}

impl ReviewView {
    pub fn new(state: Entity<SyncModel>, shell: WeakEntity<Shell>, cx: &mut Context<Self>) -> Self {
        // Re-render whenever the shared model changes so new drift rows land
        // in the sidebar without user interaction.
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
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
                    self.last_outcome = Some(outcome_banner(
                        action,
                        true,
                        &Err(ResolveError::Failed(
                            "no origin version was loaded for this file — open it in \
                             Review first"
                                .into(),
                        )),
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

    /// Hand one decision to the Shell's queue.
    ///
    /// Updating the Shell from inside THIS view's own update is safe because
    /// `Shell::enqueue` → `Shell::pump` only ever SPAWNS: it never reaches
    /// back into the review view synchronously. Nesting an update of a view
    /// that is itself mid-update is what crashed gpui on 2026-08-18.
    fn enqueue(&mut self, target: PathBuf, decision: Decision, cx: &mut Context<Self>) {
        let queued = target.clone();
        if let Err(e) = self.shell.update(cx, |shell, cx| {
            shell.enqueue(QueuedAction { target, decision }, cx)
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
    fn advance_from(&mut self, resolved: &Path, cx: &mut Context<Self>) {
        let ordered: Vec<PathBuf> = {
            let (needs_you, one_click) = severity_groups(&self.state.read(cx).drifted);
            needs_you
                .iter()
                .chain(one_click.iter())
                .map(|d| d.target.clone())
                .collect()
        };
        let queue = self.queue_snapshot(cx);
        if let Some(next) = next_after(resolved, &ordered, &|p| queue.is_processing(p)) {
            self.select(next, cx);
        }
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
    fn run_keep_both(&mut self, cx: &mut Context<Self>) {
        let Some((inputs, assembled)) = self.auto_merge.clone() else {
            return;
        };
        let target = inputs.target.clone();
        self.last_outcome = None;
        self.enqueue(
            target.clone(),
            Decision::Merged {
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
                    let status = if running.contains(&target) {
                        QueuedStatus::Running
                    } else {
                        QueuedStatus::Waiting
                    };
                    (target, status)
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

    /// "Open merge editor" — live when the engine global exists (`detail()`
    /// already implies a selection). Goes through the shared button so it
    /// obeys the surface rule like everything else; it was hand-rolled with
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
        drifted: &[DriftSummary],
        queue: &QueueSnapshot,
        in_sync: u64,
        now: u64,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let mut ix = 0usize;
        let mut rows: Vec<AnyElement> = Vec::new();
        for row in sidebar_rows(drifted, queue) {
            match row {
                SidebarRow::Group(label) => {
                    rows.push(group_header(label, theme).into_any_element())
                }
                SidebarRow::Target { entry, queued } => {
                    rows.push(
                        self.target_row(
                            ix,
                            &entry,
                            now,
                            queued,
                            queue.failure(&entry.target),
                            theme,
                            cx,
                        )
                        .into_any_element(),
                    );
                    ix += 1;
                }
            }
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(theme.text_muted)
                    .child("nothing to review")
                    .into_any_element(),
            );
        }
        div()
            .id("review-sidebar")
            .w(px(self.sidebar_width.unwrap_or(SIDEBAR_WIDTH)))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .py_1()
            .children(rows)
            .child(
                // Collapsed in-sync group: a count line, not a list (§7.2).
                div()
                    .px_3()
                    .py_2()
                    .mt_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child(format!("In sync ({in_sync})")),
            )
    }
}

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
    /// to muted, because a decision is already recorded for it. It stops
    /// being a click target only once that decision is RUNNING — while it
    /// is merely queued, selecting it and choosing again replaces the
    /// pending choice (spec §2.4, `ActionQueue::push`), which is the only
    /// way to correct a mis-click before it lands. `failure` is the message
    /// the last decision came back with — it stays on the row (spec §2.5)
    /// and the row stays live, because choosing again IS the retry.
    #[allow(clippy::too_many_arguments)]
    fn target_row(
        &self,
        ix: usize,
        entry: &DriftSummary,
        now: u64,
        queued: Option<QueuedStatus>,
        failure: Option<&SharedString>,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selected = self.selected.as_deref() == Some(entry.target.as_path());
        // Same rule as `QueueSnapshot::is_running`, which is what
        // `blocked_reason` gates the buttons on: running locks, queued does
        // not (spec §2.4).
        let locked = queued == Some(QueuedStatus::Running);
        // A processing row carries no severity: the question it was asking
        // has been answered. Its class tint would still read as "decide me".
        let color = if queued.is_some() {
            theme.text_muted
        } else {
            theme.class_color(&entry.class)
        };
        let name: SharedString = entry
            .target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.target.display().to_string())
            .into();
        // The trailing column says what the row is waiting on while it is
        // waiting, and how long it has been drifted otherwise. Both are
        // one short line; only one can be true at a time.
        let trailing: SharedString = match queued {
            Some(status) => status.label().into(),
            None => entry
                .since_ts
                .map(|ts| time_ago(now, ts))
                .unwrap_or_default()
                .into(),
        };
        let target = entry.target.clone();
        div()
            .id(ElementId::named_usize("review-target", ix))
            .px_3()
            .py_1()
            .flex()
            .flex_col()
            .gap_0p5()
            .when(!locked, |el| el.cursor_pointer())
            .when(selected, |el| el.bg(theme.surface))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
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
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child(trailing),
                    ),
            )
            // Wraps rather than truncates: this is the only place the
            // failure is ever said, and half of "op could not authenticate"
            // is not a message.
            .children(failure.map(|message| {
                div().pl(px(16.)).child(ui::status_text(
                    theme,
                    message.clone(),
                    ui::StatusTone::Conflict,
                ))
            }))
            .when(!locked, |el| {
                el.on_click(cx.listener(move |view, _ev, _window, cx| {
                    view.select(target.clone(), cx);
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
        let name: SharedString = selected
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| selected.display().to_string())
            .into();
        let path: SharedString =
            super::dashboard::shorten_home(&selected.display().to_string()).into();

        let engine_ready = cx
            .try_global::<crate::EngineSlot>()
            .is_some_and(|slot| slot.0.is_some());
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
        let blocked = blocked_reason(engine_ready, queue, selected);
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
        // External editor: real but rare — an icon, not a fourth competing
        // button.
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
                // header row only when the width genuinely isn't there.
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
                ],
            ))
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
        let drifted = model.drifted.clone();
        let in_sync = model.in_sync;
        // Read once, before anything below needs `cx` mutably for a
        // listener: the Shell cannot be borrowed across those.
        let queue = self.queue_snapshot(cx);

        let dragging = self.sidebar_drag.is_some();
        div()
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
            .child(self.sidebar(&drifted, &queue, in_sync, now, theme, cx))
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
    use std::path::{Path, PathBuf};

    use tomte_app::resolve::{ResolveError, ResolveOutcome};
    use tomte_core::merge::{MergeDocument, MergeOptions, RegionKind};
    use tomte_proto::DriftSummary;

    use super::{
        BannerTint, DiffLine, DiffPair, LineTint, ORIGIN_HINT, OutcomeBanner, QueueSnapshot,
        QueuedStatus, ResolveAction, SidebarRow, UndoAffordance, blocked_reason, flatten_document,
        next_after, origin_diffs, origin_hint, outcome_banner, per_line_ranges, preview_decors,
        severity_groups, sidebar_rows, source_actions, undo_affordance, undo_banner,
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

    #[test]
    fn a_failed_source_action_names_the_button_that_failed() {
        let gone = || Err(ResolveError::Failed("daemon gone".into()));
        // Unsplit: one "Keep source" button, so the plain name is unambiguous.
        assert_eq!(
            outcome_banner(ResolveAction::KeepSourceLocal, false, &gone())
                .text
                .as_ref(),
            "Keep source failed: daemon gone"
        );
        // Split: two "Keep source …" buttons were on screen — the banner has
        // to say which one the user pressed.
        assert_eq!(
            outcome_banner(ResolveAction::KeepSourceLocal, true, &gone())
                .text
                .as_ref(),
            "Keep source · this machine failed: daemon gone"
        );
        assert_eq!(
            outcome_banner(ResolveAction::KeepSourceOrigin, true, &gone())
                .text
                .as_ref(),
            "Keep source · origin failed: daemon gone"
        );
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
    fn posed_queue(running: &[&str], waiting: &[&str], busy: bool) -> QueueSnapshot {
        QueueSnapshot {
            processing: running
                .iter()
                .map(|t| (PathBuf::from(*t), QueuedStatus::Running))
                .chain(
                    waiting
                        .iter()
                        .map(|t| (PathBuf::from(*t), QueuedStatus::Waiting)),
                )
                .collect(),
            failures: Default::default(),
            busy,
        }
    }

    /// The sidebar plan as one line per row: a header's label, or a row's
    /// target with the state it renders in.
    fn plan(rows: &[SidebarRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                SidebarRow::Group(label) => (*label).to_string(),
                SidebarRow::Target { entry, queued } => format!(
                    "{} [{}]",
                    entry.target.display(),
                    match queued {
                        Some(QueuedStatus::Running) => "running",
                        Some(QueuedStatus::Waiting) => "queued",
                        None => "answerable",
                    }
                ),
            })
            .collect()
    }

    /// A file gets EXACTLY ONE row. Spec §2.1 has the row move to
    /// PROCESSING; leaving it in its severity group as well renders the same
    /// file twice — once inert, once live — which reads as two files and
    /// invites answering a decision that is already recorded.
    ///
    /// MUTATION: drop the `!queue.is_processing(…)` filter from
    /// `sidebar_rows` → `/a` appears under PROCESSING and again under NEEDS
    /// A DECISION.
    #[test]
    fn a_processing_row_moves_out_of_its_severity_group_instead_of_doubling() {
        let drifted = vec![s("/a", "conflict"), s("/b", "destination_drift")];

        // Nothing queued: the two groups, no PROCESSING header.
        assert_eq!(
            plan(&sidebar_rows(&drifted, &QueueSnapshot::default())),
            [
                "NEEDS A DECISION",
                "/a [answerable]",
                "SAFE TO RESOLVE",
                "/b [answerable]",
            ]
        );

        // `/a` answered: it moves, and its group goes with it.
        assert_eq!(
            plan(&sidebar_rows(&drifted, &posed_queue(&["/a"], &[], true))),
            [
                "PROCESSING",
                "/a [running]",
                "SAFE TO RESOLVE",
                "/b [answerable]",
            ]
        );

        // Both answered: running first, then waiting, and no severity group
        // is left to head.
        assert_eq!(
            plan(&sidebar_rows(
                &drifted,
                &posed_queue(&["/a"], &["/b"], true)
            )),
            ["PROCESSING", "/a [running]", "/b [queued]"]
        );
    }

    /// A queued target the model has stopped listing still gets its row:
    /// work is owed on it, and the PROCESSING group is the only trace of
    /// that.
    #[test]
    fn a_processing_row_survives_the_model_dropping_it() {
        let rows = sidebar_rows(&[], &posed_queue(&["/gone"], &[], true));
        assert_eq!(plan(&rows), ["PROCESSING", "/gone [running]"]);
    }

    /// Spec §2.4 has to be REACHABLE: a decision that has not started yet
    /// can be changed, so the row's buttons stay live while it is merely
    /// queued and go inert only once it is running. Gating on "processing"
    /// instead made a mis-click uncorrectable for as long as the queue was
    /// deep — exactly when correcting it matters.
    ///
    /// MUTATION: gate `blocked_reason` on `is_processing` instead of
    /// `is_running` → the queued case reports a reason and the buttons go
    /// dead. MUTATION: have `is_running` accept `Waiting` → same.
    #[test]
    fn only_a_running_decision_locks_its_file_out_of_being_re_chosen() {
        let a = Path::new("/a");
        assert_eq!(
            blocked_reason(true, &QueueSnapshot::default(), a),
            None,
            "nothing owed: live"
        );
        assert_eq!(
            blocked_reason(true, &posed_queue(&[], &["/a"], true), a),
            None,
            "queued but not started: still answerable, and choosing again replaces it"
        );
        assert_eq!(
            blocked_reason(true, &posed_queue(&["/a"], &[], true), a)
                .map(|r| r.to_string())
                .unwrap_or_default(),
            "applying this file's decision now"
        );
        // Another file's work never blocks this one — the user's complaint.
        assert_eq!(
            blocked_reason(true, &posed_queue(&["/other"], &["/third"], true), a),
            None
        );
        // No daemon still outranks everything: nothing can run at all.
        assert_eq!(
            blocked_reason(false, &QueueSnapshot::default(), a)
                .map(|r| r.to_string())
                .unwrap_or_default(),
            "daemon not connected"
        );
        // The row's own lock reads the same rule.
        let queue = posed_queue(&["/a"], &["/b"], true);
        assert!(queue.is_running(a));
        assert!(!queue.is_running(Path::new("/b")));
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

    fn done(committed: bool, pushed: bool, note: Option<&str>) -> ResolveOutcome {
        ResolveOutcome::Done {
            session: 7,
            committed,
            pushed,
            note: note.map(str::to_owned),
        }
    }

    #[test]
    fn outcome_banner_full_success_is_ok_tinted_and_undoable() {
        // keep_disk: commit phase ran and fully succeeded
        let b = outcome_banner(ResolveAction::KeepDisk, false, &Ok(done(true, true, None)));
        assert_eq!(b.text.as_ref(), "Kept disk version · committed & pushed");
        assert_eq!(b.tint, BannerTint::Ok);
        assert!(b.undoable);

        // keep_source: no commit phase — the banner must not claim one
        let b = outcome_banner(
            ResolveAction::KeepSourceLocal,
            false,
            &Ok(done(false, false, None)),
        );
        assert_eq!(b.text.as_ref(), "Restored chezmoi's version");
        assert_eq!(b.tint, BannerTint::Ok);
        assert!(b.undoable);
    }

    #[test]
    fn outcome_banner_names_origin_when_origin_won() {
        // keep_source_origin routes through resolve_merged, so it DOES have
        // a commit phase — unlike its local twin.
        let b = outcome_banner(
            ResolveAction::KeepSourceOrigin,
            true,
            &Ok(done(true, true, None)),
        );
        assert_eq!(
            b.text.as_ref(),
            "Restored origin's version · committed & pushed"
        );
        assert_eq!(b.tint, BannerTint::Ok);
        assert!(b.undoable);

        let b = outcome_banner(
            ResolveAction::KeepSourceOrigin,
            true,
            &Err(ResolveError::Failed(
                ".zshrc has no version at origin/main that differs from this machine's source"
                    .into(),
            )),
        );
        assert_eq!(
            b.text.as_ref(),
            "Keep source · origin failed: .zshrc has no version at origin/main that \
             differs from this machine's source"
        );
        assert_eq!(b.tint, BannerTint::Conflict);
        assert!(!b.undoable);
    }

    #[test]
    fn outcome_banner_degraded_commit_shows_note_in_drift_tint() {
        let b = outcome_banner(
            ResolveAction::KeepDisk,
            false,
            &Ok(done(true, false, Some("push failed: locked"))),
        );
        assert_eq!(b.text.as_ref(), "Kept disk version · push failed: locked");
        assert_eq!(b.tint, BannerTint::Drift);
        assert!(b.undoable, "the resolution itself succeeded");
    }

    #[test]
    fn outcome_banner_templated_and_error_are_not_undoable() {
        let b = outcome_banner(
            ResolveAction::KeepDisk,
            false,
            &Ok(ResolveOutcome::NeedsMergeEditor),
        );
        assert_eq!(b.text.as_ref(), "Templated file · use the merge editor");
        assert_eq!(b.tint, BannerTint::Drift);
        assert!(!b.undoable);

        let b = outcome_banner(
            ResolveAction::KeepSourceLocal,
            false,
            &Err(ResolveError::Failed("daemon gone".into())),
        );
        assert_eq!(b.text.as_ref(), "Keep source failed: daemon gone");
        assert_eq!(b.tint, BannerTint::Conflict);
        assert!(!b.undoable);
    }

    #[test]
    fn outcome_banner_protected_span_uses_the_plan_copy() {
        let b = outcome_banner(
            ResolveAction::KeepDisk,
            false,
            &Ok(ResolveOutcome::ProtectedSpan {
                detail: "edit at rendered bytes 8..13 touches a protected template span".into(),
            }),
        );
        assert_eq!(
            b.text.as_ref(),
            "This change touches a templated value · open the merge editor"
        );
        assert_eq!(b.tint, BannerTint::Drift);
        assert!(!b.undoable, "nothing was mutated — nothing to undo");
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
