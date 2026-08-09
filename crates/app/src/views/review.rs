//! Review shell: severity sidebar + provenance timeline + read-only diff
//! preview (spec §7.2, approved mockup A; plan 5 Task 6).
//!
//! Everything blocking — `chezmoi cat`, `chezmoi source-path`, the read-only
//! journal, destination fs reads, and the resolve-engine actions — runs on
//! the background executor and lands back in the entity via
//! `WeakEntity::update` (spec §3.2 non-blocking rule). One-click resolutions
//! ("keep disk" / "keep source" / undo, plan 6 Task 3) go through the
//! [`ResolveEngine`] published in the [`crate::EngineSlot`] global; their
//! outcomes render as a slim banner above the diff. "Open merge editor"
//! (conflicts, templated files) hands the selected target to the Shell's
//! full-window merge editor (plan 7 Task 3).

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, Context, Div, ElementId, Entity, FontWeight, HighlightStyle, Rgba, SharedString,
    Stateful, StyledText, WeakEntity, Window, div, prelude::*, px, uniform_list,
};
use tomte_app::model::{SyncModel, class_label, kind_glyph, kind_label, time_ago};
use tomte_app::resolve::{ResolveEngine, ResolveError, ResolveOutcome};
use tomte_app::theme::Theme;
use tomte_core::chezmoi::{ChezmoiClient, ChezmoiError, ChezmoiOptions};
use tomte_core::cmd::{CommandRequest, CommandRunner, SystemRunner};
use tomte_core::merge::{MergeDocument, MergeOptions, RegionKind, worddiff::word_diff};
use tomte_journal::{EventRow, Journal};
use tomte_proto::DriftSummary;
use tomte_ui::components as ui;

use super::Shell;
use super::dashboard::{TextTooltip, system_now};

/// Fixed sidebar width per the approved mockup A (~260px).
const SIDEBAR_WIDTH: f32 = 260.;

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
}

/// Dry-run the 3-way merge; `Some` only when it auto-resolves to something
/// neither side already is (otherwise Keep disk / Keep source cover it).
fn auto_merge_blocking(
    target: &Path,
    journal: &Path,
) -> Option<(std::sync::Arc<tomte_app::merge_inputs::MergeInputs>, String)> {
    let chezmoi = ChezmoiClient::new(Arc::new(SystemRunner), ChezmoiOptions::default());
    let inputs = tomte_app::merge_inputs::load(&chezmoi, journal, target).ok()?;
    let state = tomte_app::merge_state::MergeState::new(&inputs);
    if !state.conflicts().is_empty() {
        return None;
    }
    let assembled = state.assembled()?;
    if assembled == inputs.ours || assembled == inputs.theirs {
        return None;
    }
    Some((std::sync::Arc::new(inputs), assembled))
}

fn load_detail_blocking(target: &Path, journal: &Path) -> LoadedDetail {
    // Provenance is best-effort context, never a blocker for the diff: a
    // missing journal (fresh install, daemon not yet scanned) or a query
    // error both degrade to an empty history strip.
    let provenance = Journal::open_read_only(journal, "app")
        .and_then(|j| j.events_for(target, PROVENANCE_LIMIT))
        .map(|rows| rows.into_iter().map(ProvRow::from).collect())
        .unwrap_or_default();
    LoadedDetail {
        provenance,
        preview: load_preview_blocking(target),
        auto_merge: auto_merge_blocking(target, journal),
    }
}

fn load_preview_blocking(target: &Path) -> PreviewState {
    let client = ChezmoiClient::new(Arc::new(SystemRunner), ChezmoiOptions::default());
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

/// The two one-click resolutions (spec §5), shared with the dashboard's
/// inline row actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResolveAction {
    KeepDisk,
    KeepSource,
}

impl ResolveAction {
    /// Short imperative label for buttons and failure messages.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::KeepDisk => "Keep disk",
            Self::KeepSource => "Keep source",
        }
    }

    /// Past-tense verb for success messages.
    pub(super) fn verb(self) -> &'static str {
        match self {
            Self::KeepDisk => "Kept disk version",
            Self::KeepSource => "Restored chezmoi's version",
        }
    }

    /// Run the matching engine method. Blocking — background executor only.
    pub(super) fn run(
        self,
        engine: &ResolveEngine,
        target: &Path,
    ) -> Result<ResolveOutcome, ResolveError> {
        match self {
            Self::KeepDisk => engine.keep_disk(target),
            Self::KeepSource => engine.keep_source(target),
        }
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

/// Banner for the "Keep both" quick action (merged write-back).
fn keep_both_banner(result: &Result<ResolveOutcome, ResolveError>) -> OutcomeBanner {
    match result {
        Ok(ResolveOutcome::Done {
            note: None,
            committed,
            pushed,
            ..
        }) => OutcomeBanner {
            text: if *committed && *pushed {
                "Kept both (merged) · committed & pushed".into()
            } else {
                "Kept both (merged)".into()
            },
            tint: BannerTint::Ok,
            undoable: true,
        },
        Ok(ResolveOutcome::Done {
            note: Some(note), ..
        }) => OutcomeBanner {
            text: format!("Kept both (merged) · {note}").into(),
            tint: BannerTint::Drift,
            undoable: true,
        },
        Ok(ResolveOutcome::NeedsMergeEditor) => OutcomeBanner {
            text: "Templated file · use the merge editor".into(),
            tint: BannerTint::Drift,
            undoable: false,
        },
        Ok(ResolveOutcome::ProtectedSpan { detail }) => OutcomeBanner {
            text: format!("This change touches a templated value · {detail}").into(),
            tint: BannerTint::Drift,
            undoable: false,
        },
        Err(e) => OutcomeBanner {
            text: format!("Keep both failed: {e}").into(),
            tint: BannerTint::Conflict,
            undoable: false,
        },
    }
}

/// Map an action result onto its banner. Tint policy: a `note` means the
/// resolution succeeded locally but commit/push degraded (drift tint, note
/// shown); no note is full success (ok tint); engine errors are conflicts.
fn outcome_banner(
    action: ResolveAction,
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
            text: format!("{} failed: {e}", action.label()).into(),
            tint: BannerTint::Conflict,
            undoable: false,
        },
    }
}

/// Banner for the undo action itself (never undoable again: the undo session
/// journals no destination blobs, so a second undo would restore nothing).
fn undo_banner(result: &Result<Option<i64>, ResolveError>) -> OutcomeBanner {
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

/// Which side of the drift a preview line belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineTint {
    /// Disk and rendered source agree — muted context.
    Context,
    /// Line the rendered source would write (absent or different on disk).
    Rendered,
    /// Line as it exists on disk (absent or different in the rendered source).
    Destination,
}

/// One uniform-height preview row, precomputed so the `uniform_list` closure
/// owns plain data (same idiom as the dashboard's `RowData`).
#[derive(Debug, Clone, PartialEq)]
struct DiffLine {
    /// Display text: the source line minus its trailing newline.
    text: SharedString,
    tint: LineTint,
    /// Gutter marker: " " context, "−" rendered-only, "+" on-disk-only.
    marker: &'static str,
    /// Word-diff byte ranges into `text` to emphasize within changed pairs.
    highlights: Vec<Range<usize>>,
}

pub(super) fn display_text(line: &str) -> &str {
    line.strip_suffix('\n').unwrap_or(line)
}

/// 2-WAY MAPPING. The preview feeds `MergeDocument::compute(base = rendered,
/// ours = destination, theirs = rendered)`. Because `theirs` is byte-identical
/// to `base`, the 3-way engine degenerates into a 2-way diff of
/// destination-vs-rendered:
///   - `Unchanged` → disk == rendered → muted context lines;
///   - `OursOnly`  → disk differs → `region.base` holds the lines chezmoi
///     would render ("−", accent) and `region.ours` the lines on disk
///     ("+", drift), with word-diff emphasis where cheap.
///
/// `TheirsOnly`, `BothSame`, and `Conflict` are unreachable while
/// theirs == base; they are still handled (context / context / changed pair)
/// so a future true-3-way upgrade fails soft rather than mis-rendering.
fn flatten_document(doc: &MergeDocument) -> Vec<DiffLine> {
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
                let rendered = &doc.base_lines()[region.base.clone()];
                let destination = &doc.ours_lines()[region.ours.clone()];
                let (hl_rendered, hl_destination) = region_highlights(rendered, destination);
                for (line, highlights) in rendered.iter().zip(hl_rendered) {
                    out.push(DiffLine {
                        text: display_text(line).to_owned().into(),
                        tint: LineTint::Rendered,
                        marker: "−",
                        highlights,
                    });
                }
                for (line, highlights) in destination.iter().zip(hl_destination) {
                    out.push(DiffLine {
                        text: display_text(line).to_owned().into(),
                        tint: LineTint::Destination,
                        marker: "+",
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
fn region_highlights(rendered: &[String], destination: &[String]) -> (LineRanges, LineRanges) {
    let none = || {
        (
            vec![Vec::new(); rendered.len()],
            vec![Vec::new(); destination.len()],
        )
    };
    if rendered.is_empty() || destination.is_empty() {
        return none();
    }
    let a = rendered.concat();
    let b = destination.concat();
    if a.len() > WORD_DIFF_CHEAP_BYTES || b.len() > WORD_DIFF_CHEAP_BYTES {
        return none();
    }
    let wd = word_diff(&a, &b);
    (
        per_line_ranges(rendered, &wd.changed_a),
        per_line_ranges(destination, &wd.changed_b),
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

pub struct ReviewView {
    state: Entity<SyncModel>,
    /// Back-reference for cross-view navigation ("Open merge editor" routes
    /// through [`Shell::open_merge`]). Weak: the shell owns this view.
    shell: WeakEntity<Shell>,
    /// `pub(super)` so the render-smoke tests can pose a selected target
    /// without spawning the background detail load.
    pub(super) selected: Option<PathBuf>,
    pub(super) preview: PreviewState,
    /// Provenance for the selected target, newest first (background-loaded
    /// together with the preview).
    pub(super) provenance: Vec<ProvRow>,
    /// Clean 3-way dry-run for the selected target → "Keep both" offered.
    pub(super) auto_merge: Option<(std::sync::Arc<tomte_app::merge_inputs::MergeInputs>, String)>,
    /// Outcome of the last resolve/undo action, rendered as a banner above
    /// the diff. `pub(super)` for the render-smoke tests.
    pub(super) last_outcome: Option<OutcomeBanner>,
    /// A resolve/undo action is running on the background executor: buttons
    /// disable and the header shows "working…". `pub(super)` for smoke tests.
    pub(super) action_in_flight: bool,
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
            provenance: Vec::new(),
            auto_merge: None,
            last_outcome: None,
            action_in_flight: false,
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
                    view.preview = loaded.preview;
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
                let client = ChezmoiClient::new(Arc::new(SystemRunner), ChezmoiOptions::default());
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

    /// Run a one-click resolution for the selected target on the background
    /// executor; the outcome lands back as a banner and the preview reloads
    /// through the existing `select()` path.
    fn run_action(&mut self, action: ResolveAction, cx: &mut Context<Self>) {
        if self.action_in_flight {
            return;
        }
        let Some(target) = self.selected.clone() else {
            return;
        };
        let Some(engine) = cx
            .try_global::<crate::EngineSlot>()
            .and_then(|slot| slot.0.clone())
        else {
            return;
        };
        self.action_in_flight = true;
        self.last_outcome = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = {
                let target = target.clone();
                cx.background_executor()
                    .spawn(async move { action.run(&engine, &target) })
                    .await
            };
            this.update(cx, |view, cx| {
                view.action_in_flight = false;
                view.last_outcome = Some(outcome_banner(action, &result));
                if matches!(result, Ok(ResolveOutcome::Done { .. })) {
                    view.state.update(cx, |model, cx| {
                        model.confirm_resolved(&target);
                        cx.notify();
                    });
                }
                // Reload the preview so the diff reflects the new reality
                // (re-selecting is the established refresh path).
                if view.selected.as_deref() == Some(target.as_path()) {
                    view.select(target.clone(), cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Undo the last finished session (restore destination files from their
    /// journaled snapshots), reported through the same banner.
    fn run_undo(&mut self, cx: &mut Context<Self>) {
        if self.action_in_flight {
            return;
        }
        let Some(engine) = cx
            .try_global::<crate::EngineSlot>()
            .and_then(|slot| slot.0.clone())
        else {
            return;
        };
        self.action_in_flight = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { engine.undo_last() })
                .await;
            this.update(cx, |view, cx| {
                view.action_in_flight = false;
                view.last_outcome = Some(undo_banner(&result));
                if let Some(target) = view.selected.clone() {
                    view.select(target, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// "Keep both": write the clean auto-merge through the same snapshot →
    /// resolve_merged pipeline the merge editor's Save uses (undoable).
    fn run_keep_both(&mut self, cx: &mut Context<Self>) {
        let Some((inputs, assembled)) = self.auto_merge.clone() else {
            return;
        };
        let Some(engine) = cx
            .try_global::<crate::EngineSlot>()
            .and_then(|slot| slot.0.clone())
        else {
            return;
        };
        self.action_in_flight = true;
        cx.notify();
        let resolved_target = inputs.target.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { engine.resolve_merged(&inputs, &assembled) })
                .await;
            this.update(cx, |view, cx| {
                view.action_in_flight = false;
                view.last_outcome = Some(keep_both_banner(&result));
                if matches!(result, Ok(ResolveOutcome::Done { .. })) {
                    view.state.update(cx, |model, cx| {
                        model.confirm_resolved(&resolved_target);
                        cx.notify();
                    });
                }
                if let Some(target) = view.selected.clone() {
                    view.select(target, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
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
    /// already implies a selection).
    fn merge_editor_button(
        &self,
        enabled: bool,
        is_conflict: bool,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let base = div()
            .id("open-merge-editor")
            .px_2()
            .py_0p5()
            .rounded_md()
            .border_1()
            .text_xs()
            .child("Open merge editor");
        if enabled {
            // Accent only when merging is this file's real resolution path
            // (a conflict); for one-click drifts it's a secondary option.
            let color = if is_conflict {
                theme.accent
            } else {
                theme.text
            };
            base.border_color(if is_conflict {
                theme.accent
            } else {
                theme.border
            })
            .text_color(color)
            .cursor_pointer()
            .hover(|el| el.bg(Theme::wash(color, 0.12)))
            .on_click(cx.listener(|view, _ev, _window, cx| view.open_merge_editor(cx)))
        } else {
            base.border_color(theme.border)
                .text_color(theme.text_muted)
                .tooltip(|_window, cx| {
                    cx.new(|_| TextTooltip {
                        text: "daemon not connected".into(),
                    })
                    .into()
                })
        }
    }

    /// One header action button. Enabled needs a live engine (daemon
    /// connected) — `run_action` itself re-checks selection and in-flight.
    fn action_button(
        &self,
        id: &'static str,
        action: ResolveAction,
        enabled: bool,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        if enabled {
            ui::button(
                theme,
                id,
                action.label().into(),
                ui::ButtonVariant::Outline(theme.accent),
                ui::ButtonSize::Sm,
                cx.listener(move |view, _ev, _window, cx| view.run_action(action, cx)),
            )
        } else {
            ui::disabled_button(
                theme,
                id,
                action.label().into(),
                ui::ButtonSize::Sm,
                Some("daemon not connected".into()),
            )
        }
    }

    /// The slim outcome banner above the diff, with an Undo button on
    /// successful resolutions.
    fn banner_el(&self, banner: &OutcomeBanner, theme: Theme, cx: &mut Context<Self>) -> Div {
        let color = banner.tint.color(theme);
        let undo = (banner.undoable && !self.action_in_flight).then(|| {
            ui::button(
                theme,
                "outcome-undo",
                "Undo".into(),
                ui::ButtonVariant::Outline(color),
                ui::ButtonSize::Micro,
                cx.listener(|view, _ev, _window, cx| view.run_undo(cx)),
            )
            .into_any_element()
        });
        ui::banner(theme, banner.tint, banner.text.clone(), undo)
    }

    fn sidebar(
        &self,
        needs_you: &[DriftSummary],
        one_click: &[DriftSummary],
        in_sync: u64,
        now: u64,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let mut ix = 0usize;
        let mut rows: Vec<AnyElement> = Vec::new();
        for (label, group) in [
            ("NEEDS A DECISION", needs_you),
            ("SAFE TO RESOLVE", one_click),
        ] {
            if group.is_empty() {
                continue;
            }
            rows.push(group_header(label, theme).into_any_element());
            for entry in group {
                rows.push(
                    self.target_row(ix, entry, now, theme, cx)
                        .into_any_element(),
                );
                ix += 1;
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
            .w(px(SIDEBAR_WIDTH))
            .flex_shrink_0()
            .h_full()
            .border_r_1()
            .border_color(theme.border)
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

    fn target_row(
        &self,
        ix: usize,
        entry: &DriftSummary,
        now: u64,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selected = self.selected.as_deref() == Some(entry.target.as_path());
        let color = theme.class_color(&entry.class);
        let name: SharedString = entry
            .target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.target.display().to_string())
            .into();
        let since: SharedString = entry
            .since_ts
            .map(|ts| time_ago(now, ts))
            .unwrap_or_default()
            .into();
        let target = entry.target.clone();
        div()
            .id(ElementId::named_usize("review-target", ix))
            .px_3()
            .py_1()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .when(selected, |el| el.bg(theme.surface))
            .child(div().w_2().h_2().rounded_full().flex_shrink_0().bg(color))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(theme.text)
                    .truncate()
                    .child(name),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child(since),
            )
            .on_click(cx.listener(move |view, _ev, _window, cx| {
                view.select(target.clone(), cx);
            }))
    }

    fn detail(&self, now: u64, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
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

        let body = match &self.preview {
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
            PreviewState::Ready(doc) => diff_preview(doc, theme),
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
                vec![
                    if self.action_in_flight {
                        ui::status_text(theme, "working…", ui::StatusTone::Muted).into_any_element()
                    } else {
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(self.action_button(
                                "keep-disk",
                                ResolveAction::KeepDisk,
                                engine_ready,
                                theme,
                                cx,
                            ))
                            .child(self.action_button(
                                "keep-source",
                                ResolveAction::KeepSource,
                                engine_ready,
                                theme,
                                cx,
                            ))
                            .when(self.auto_merge.is_some(), |el| {
                                el.child(if engine_ready {
                                    ui::button(
                                        theme,
                                        "keep-both",
                                        "Keep both".into(),
                                        ui::ButtonVariant::Outline(theme.ok),
                                        ui::ButtonSize::Sm,
                                        cx.listener(|view, _ev, _window, cx| {
                                            view.run_keep_both(cx)
                                        }),
                                    )
                                } else {
                                    ui::disabled_button(
                                        theme,
                                        "keep-both",
                                        "Keep both".into(),
                                        ui::ButtonSize::Sm,
                                        Some("daemon not connected".into()),
                                    )
                                })
                            })
                            .into_any_element()
                    },
                    self.merge_editor_button(engine_ready, is_conflict, theme, cx)
                        .into_any_element(),
                    // External editor: real but rare — an icon, not a fourth
                    // competing button.
                    div()
                        .id("open-in-editor")
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
                ],
            ))
            .when_some(self.last_outcome.clone(), |el, banner| {
                el.child(self.banner_el(&banner, theme, cx))
            })
            .when(!self.provenance.is_empty(), |el| {
                el.child(provenance_section(&self.provenance, now, theme))
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
    fn render_inner(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        let now = system_now();
        let model = self.state.read(cx);
        let drifted = model.drifted.clone();
        let in_sync = model.in_sync;
        let (needs_you, one_click) = severity_groups(&drifted);

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(self.sidebar(&needs_you, &one_click, in_sync, now, theme, cx))
            .child(self.detail(now, theme, cx))
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

fn provenance_section(rows: &[ProvRow], now: u64, theme: Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(theme.border)
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

fn diff_preview(doc: &MergeDocument, theme: Theme) -> AnyElement {
    let lines: Rc<Vec<DiffLine>> = Rc::new(flatten_document(doc));
    if lines.is_empty() {
        return centered_note(
            theme,
            "destination matches the rendered source".into(),
            theme.ok,
        );
    }
    // The diff IS the pane content — no bordered sub-container (2026-08-08
    // feedback); the +/− legend sits in a thin footer line under it.
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(
            uniform_list("review-diff", lines.len(), move |range, _window, _cx| {
                range.map(|ix| diff_row(&lines[ix], theme)).collect()
            })
            .flex_1()
            .pt_1(),
        )
        .child(
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
                .child(div().text_color(theme.accent).child("− source would write"))
                .child(div().text_color(theme.drift).child("+ on disk now")),
        )
        .into_any_element()
}

fn diff_row(line: &DiffLine, theme: Theme) -> Div {
    let color = match line.tint {
        LineTint::Context => theme.text_muted,
        LineTint::Rendered => theme.accent,
        LineTint::Destination => theme.drift,
    };
    let text: AnyElement = if line.highlights.is_empty() {
        line.text.clone().into_any_element()
    } else {
        // Word-diff emphasis: translucent wash of the line's own tint, so no
        // color outside the Theme tokens enters the view.
        let style = HighlightStyle {
            background_color: Some(Rgba { a: 0.25, ..color }.into()),
            ..Default::default()
        };
        StyledText::new(line.text.clone())
            .with_highlights(line.highlights.iter().map(|r| (r.clone(), style)))
            .into_any_element()
    };
    ui::mono_line(theme)
        .text_color(color)
        .child(ui::line_gutter(color, line.marker))
        .child(ui::line_text(text))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tomte_app::resolve::{ResolveError, ResolveOutcome};
    use tomte_core::merge::{MergeDocument, MergeOptions, RegionKind};
    use tomte_proto::DriftSummary;

    use super::{
        BannerTint, DiffLine, LineTint, ResolveAction, flatten_document, outcome_banner,
        per_line_ranges, severity_groups, undo_banner,
    };

    fn s(target: &str, class: &str) -> DriftSummary {
        DriftSummary {
            target: target.into(),
            class: class.into(),
            since_ts: Some(1),
        }
    }

    fn targets(group: &[DriftSummary]) -> Vec<&Path> {
        group.iter().map(|d| d.target.as_path()).collect()
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
        let lines = flatten_document(&doc);
        assert_eq!(
            shape(&lines),
            [
                ("a", LineTint::Context, " "),
                ("v = 1", LineTint::Rendered, "−"),
                ("v = 2", LineTint::Destination, "+"),
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
        let lines = flatten_document(&doc);
        assert_eq!(
            shape(&lines),
            [
                ("a", LineTint::Context, " "),
                ("x", LineTint::Destination, "+"),
                ("b", LineTint::Context, " "),
            ]
        );
        // one-sided region: no word-diff emphasis (nothing to pair against)
        assert!(lines[1].highlights.is_empty());
    }

    #[test]
    fn flatten_deleted_on_disk_shows_rendered_only() {
        let doc = two_way("only\n", "");
        let lines = flatten_document(&doc);
        assert_eq!(shape(&lines), [("only", LineTint::Rendered, "−")]);
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
        let b = outcome_banner(ResolveAction::KeepDisk, &Ok(done(true, true, None)));
        assert_eq!(b.text.as_ref(), "Kept disk version · committed & pushed");
        assert_eq!(b.tint, BannerTint::Ok);
        assert!(b.undoable);

        // keep_source: no commit phase — the banner must not claim one
        let b = outcome_banner(ResolveAction::KeepSource, &Ok(done(false, false, None)));
        assert_eq!(b.text.as_ref(), "Restored chezmoi's version");
        assert_eq!(b.tint, BannerTint::Ok);
        assert!(b.undoable);
    }

    #[test]
    fn outcome_banner_degraded_commit_shows_note_in_drift_tint() {
        let b = outcome_banner(
            ResolveAction::KeepDisk,
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
            &Ok(ResolveOutcome::NeedsMergeEditor),
        );
        assert_eq!(b.text.as_ref(), "Templated file · use the merge editor");
        assert_eq!(b.tint, BannerTint::Drift);
        assert!(!b.undoable);

        let b = outcome_banner(
            ResolveAction::KeepSource,
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
}
