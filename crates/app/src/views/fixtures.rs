//! Gallery fixtures: named, fully-posed UI states built from synthetic data —
//! no daemon, no IPC, no subprocesses (except the live `settings` state).
//! `tomte --gallery <name>` renders one in a real window so the agent
//! (or a human) can screenshot any state on demand instead of reproducing it
//! by hand. The registry doubles as documentation of every reachable state.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AppContext as _, Context, Entity};
use tomte_app::merge_inputs::MergeInputs;
use tomte_app::model::{SyncModel, TimelineRow};
use tomte_core::merge::{Choice, MergeDocument, MergeOptions};
use tomte_core::template::{anchor::anchor, lexer::lex};
use tomte_proto::DriftSummary;

use super::PendingRepoOp;
use super::merge::{LoadedMerge, MergeView, Rollback};
use super::review::{
    BannerTint, DiffPair, OutcomeBanner, PreviewState, ProvRow, QueueSnapshot, ReviewMode,
    ReviewView, SidebarInputs, origin_diffs, sidebar_rows,
};
use super::review_selection::Selection;
use super::review_tree::safe_descendants;
use super::settings::SettingsPaths;
use super::{Route, Shell};
use tomte_app::action_queue::{Decision, QueuedAction};

/// Every state the gallery can pose: `(name, description)`.
pub const STATES: &[(&str, &str)] = &[
    (
        "dashboard",
        "populated: drifted files, mixed timeline, scan group",
    ),
    ("dashboard-empty", "everything in sync"),
    ("dashboard-scanning", "fresh boot, first scan running"),
    ("dashboard-rescanning", "data present, rescan in progress"),
    ("dashboard-degraded", "1Password-style degraded banner"),
    ("dashboard-disconnected", "daemon not connected"),
    (
        "dashboard-rolling-back",
        "pending-merge banner while a confirmed roll-back waits: Finish is inert",
    ),
    ("review", "sidebar groups + selected file with diff preview"),
    ("review-empty", "nothing selected"),
    ("review-banner", "action outcome banner with Undo"),
    (
        "review-processing",
        "two decisions recorded: the Processing group, the selected file's buttons inert",
    ),
    (
        "review-failed",
        "a decision came back failed: the message sticks to its row, buttons live",
    ),
    (
        "review-keep-both",
        "clean auto-merge: third quick action offered",
    ),
    (
        "review-origin",
        "origin has its own version: split source actions + comparison selector",
    ),
    (
        "review-origin-compared",
        "the origin comparison picked: disk ↔ origin, origin's lines in the remote tint",
    ),
    (
        "review-multiselect",
        "three safe rows ticked: the selection bar replaces the file's actions, checkboxes visible",
    ),
    (
        "review-tree",
        "folder tree: .config/nvim/ collapsed, .config/ partially ticked, an \"n of m\" chip",
    ),
    (
        "review-tree-selected",
        "a whole folder ticked: its checkbox On, selection bar for its files",
    ),
    (
        "review-many",
        "500 safe files in Tree mode: queued, running, failed and ticked rows — the perf pose",
    ),
    ("merge", "three-pane editor with unresolved conflicts"),
    (
        "merge-auto",
        "no conflicts: auto-resolved regions, overridable strips",
    ),
    (
        "merge-origin",
        "origin has its own version: fourth pane + origin pick",
    ),
    (
        "merge-big",
        "500-line templated file, many regions — scroll/perf testbed",
    ),
    ("merge-templated", "templated file with protected 🔒 spans"),
    ("merge-resolved", "all regions decided, Save enabled"),
    (
        "merge-editing",
        "region editor open: free-text edit of a conflict",
    ),
    ("merge-loading", "inputs loading"),
    ("changelog", "the changelog window body, markdown-rendered"),
    ("settings", "live settings view (reads real settings/op)"),
    ("settings-menu", "settings with the account dropdown open"),
    (
        "settings-dirty",
        "unsaved edits: floating Save/Revert toolbar",
    ),
    (
        "settings-update-ready",
        "staged update: restart button, release notes, sidebar hint",
    ),
    (
        "settings-update-checking",
        "update check in flight: spinner + downloading note",
    ),
];

/// All gallery states: the screen poses above plus one `comp:<name>` state
/// per tomte-ui preview registry entry.
pub fn states() -> Vec<(String, &'static str)> {
    let mut all: Vec<(String, &'static str)> =
        STATES.iter().map(|(n, d)| (n.to_string(), *d)).collect();
    all.extend(
        tomte_ui::preview::COMPONENTS
            .iter()
            .map(|(n, d)| (format!("comp:{n}"), *d)),
    );
    all
}

/// Window size for a gallery state: component previews get a compact window
/// so the screenshot is mostly component.
pub fn window_size(name: &str) -> gpui::Size<gpui::Pixels> {
    use gpui::{px, size};
    if name == "comp:buttons" {
        // The lab is a tall page of scenes — give it a real window.
        size(px(1100.), px(900.))
    } else if name.starts_with("comp:") {
        size(px(560.), px(640.))
    } else {
        size(px(980.), px(640.))
    }
}

pub(super) fn home(sub: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Users/gallery"));
    home.join(sub)
}

/// A believable populated model: what the dashboard looks like on a real
/// machine with a few days of history.
fn rich_model() -> SyncModel {
    let mut m = SyncModel {
        connected: true,
        ..Default::default()
    };
    m.hydrate_status(
        vec![
            DriftSummary {
                target: home(".config/nushell/env.nu"),
                class: "conflict".into(),
                since_ts: Some(now() - 9_000),
                origin_differs: false,
            },
            DriftSummary {
                target: home(".config/zed/settings.json"),
                class: "destination_drift".into(),
                since_ts: Some(now() - 11_000),
                origin_differs: false,
            },
            DriftSummary {
                target: home(".gitconfig"),
                class: "remote_ahead".into(),
                since_ts: Some(now() - 90_000),
                origin_differs: false,
            },
            DriftSummary {
                target: home(".config/secrets.yaml"),
                class: "eval_failed".into(),
                since_ts: Some(now() - 200),
                origin_differs: false,
            },
        ],
        951,
        None,
        false,
        None,
    );
    m.last_fetch_ts = Some(now() - 240);
    let row = |ts: u64, kind: &str, target: Option<PathBuf>, class: Option<&str>| TimelineRow {
        ts,
        kind: kind.into(),
        target,
        machine: "this mac".into(),
        class: class.map(str::to_owned),
    };
    m.timeline = vec![
        row(
            now() - 200,
            "eval_failed",
            Some(home(".config/secrets.yaml")),
            Some("eval_failed"),
        ),
        row(now() - 240, "fetch", None, None),
        row(
            now() - 9_000,
            "dest_changed",
            Some(home(".config/nushell/env.nu")),
            Some("conflict"),
        ),
        row(
            now() - 11_000,
            "dest_changed",
            Some(home(".config/zed/settings.json")),
            Some("destination_drift"),
        ),
        row(now() - 12_000, "fetch", None, None),
        row(now() - 15_500, "fetch", None, None),
        row(now() - 19_000, "fetch", None, None),
        row(
            now() - 90_000,
            "remote_advanced",
            Some(home(".gitconfig")),
            Some("remote_ahead"),
        ),
        row(
            now() - 95_000,
            "applied",
            Some(home(".config/starship.toml")),
            None,
        ),
        row(
            now() - 170_000,
            "left_management",
            Some(home(".config/old-tool.conf")),
            None,
        ),
    ];
    m
}

/// `rich_model()` plus three more safe (one-click) rows: the multi-select
/// pose needs enough safe rows for the tri-state header and a bulk press to
/// say anything.
fn multiselect_model() -> SyncModel {
    let mut m = rich_model();
    let mut drifted = m.drifted.clone();
    drifted.extend([
        DriftSummary {
            target: home(".config/starship.toml"),
            class: "destination_drift".into(),
            since_ts: Some(now() - 40_000),
            origin_differs: false,
        },
        DriftSummary {
            target: home(".zshrc"),
            class: "source_ahead".into(),
            since_ts: Some(now() - 60_000),
            origin_differs: false,
        },
        DriftSummary {
            target: home(".config/nvim/init.lua"),
            class: "destination_drift".into(),
            since_ts: Some(now() - 80_000),
            origin_differs: false,
        },
    ]);
    m.hydrate_status(drifted, m.in_sync, None, false, None);
    m
}

/// Five hundred drifted files under forty skill folders and twenty tool
/// folders — the user's real shape on 2026-10-11 (499 drifted under
/// `~/.agents/skills`), which the sidebar must render without lag.
fn many_model() -> SyncModel {
    let mut m = multiselect_model();
    let mut drifted = m.drifted.clone();
    for skill in 0..40u64 {
        for file in 0..12u64 {
            drifted.push(DriftSummary {
                target: home(&format!(
                    ".agents/skills/skill-{skill:02}/reference/doc-{file:02}.md"
                )),
                class: if file % 7 == 0 {
                    "source_ahead"
                } else {
                    "destination_drift"
                }
                .into(),
                since_ts: Some(now() - 1_000 * (skill * 12 + file)),
                origin_differs: file % 11 == 0,
            });
        }
    }
    for tool in 0..20u64 {
        drifted.push(DriftSummary {
            target: home(&format!(".config/tool-{tool:02}/config.toml")),
            class: "destination_drift".into(),
            since_ts: Some(now() - 5_000 * tool),
            origin_differs: false,
        });
    }
    m.hydrate_status(drifted, 1_200, None, false, None);
    m
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(1_784_700_000)
}

/// This machine's rendered source for the review preview.
const PREVIEW_LOCAL: &str =
    "theme = \"catppuccin\"\nfont_size = 14\nkeymap = \"vim\"\nformat_on_save = true\n";
/// The same file as it sits on disk: one value edited, one line added.
const PREVIEW_DISK: &str = "theme = \"catppuccin\"\nfont_size = 15\nkeymap = \"vim\"\nformat_on_save = true\ntelemetry = false\n";
/// Origin's rendered version: another machine pushed a different font size
/// and a key this machine's source has never had.
const PREVIEW_ORIGIN: &str = "theme = \"catppuccin\"\nfont_size = 13\nkeymap = \"vim\"\nsoft_wrap = \"editor_width\"\nformat_on_save = true\n";

/// Small believable config diff for the review preview.
fn preview_document() -> MergeDocument {
    MergeDocument::compute(
        PREVIEW_LOCAL,
        PREVIEW_DISK,
        PREVIEW_LOCAL,
        MergeOptions::default(),
    )
}

fn posed_review(
    cx: &mut Context<Shell>,
    state: Entity<SyncModel>,
    banner: Option<OutcomeBanner>,
    selected: bool,
) -> Entity<ReviewView> {
    let shell = cx.weak_entity();
    cx.new(|cx| {
        let mut view = ReviewView::new(state, shell, cx);
        if selected {
            view.selected = Some(home(".config/zed/settings.json"));
            view.preview = PreviewState::Ready(preview_document());
            view.provenance = vec![
                ProvRow {
                    ts: now() - 11_000,
                    kind: "dest_changed".into(),
                    machine: "this mac".into(),
                    class: Some("destination_drift".into()),
                },
                ProvRow {
                    ts: now() - 95_000,
                    kind: "applied".into(),
                    machine: "this mac".into(),
                    class: None,
                },
                ProvRow {
                    ts: now() - 260_000,
                    kind: "remote_advanced".into(),
                    machine: "macbook-b".into(),
                    class: None,
                },
            ];
        }
        view.last_outcome = banner;
        view
    })
}

/// Plain-file conflict: both sides touched the editor line differently, plus
/// one side-only change each — exercises every region kind.
fn conflict_inputs() -> MergeInputs {
    let base = "editor = \"vim\"\nfont = \"Menlo\"\ntheme = \"dark\"\nsplits = true\n";
    let ours =
        "editor = \"helix\"\nfont = \"Menlo\"\ntheme = \"dark\"\nsplits = true\nmouse = false\n";
    let theirs = "editor = \"zed --wait\"\nfont = \"GeistMono\"\ntheme = \"dark\"\nsplits = true\n";
    MergeInputs {
        target: home(".config/editor.toml"),
        ours: ours.into(),
        theirs: theirs.into(),
        base: Some(base.into()),
        source_path: home(".local/share/chezmoi/dot_config/editor.toml"),
        templated: false,
        template: None,
        span_map: None,
        origin: None,
    }
}

/// Templated inputs with a real span map (lex + anchor over the rendered
/// text), so the 🔒 protected rows render exactly as they would live.
fn templated_inputs() -> MergeInputs {
    let template = "[user]\n    name = Remi\n    email = {{ .email }}\n[core]\n    editor = vim\n";
    let theirs = "[user]\n    name = Remi\n    email = me@example.com\n[core]\n    editor = vim\n";
    let ours = "[user]\n    name = Remi\n    email = me@example.com\n[core]\n    editor = helix\n";
    let span_map = lex(template)
        .ok()
        .map(|segments| anchor(template, &segments, theirs));
    MergeInputs {
        target: home(".gitconfig"),
        ours: ours.into(),
        theirs: theirs.into(),
        base: None,
        source_path: home(".local/share/chezmoi/dot_gitconfig.tmpl"),
        templated: true,
        template: Some(template.into()),
        span_map,
        origin: None,
    }
}

fn posed_merge(
    cx: &mut Context<Shell>,
    inputs: MergeInputs,
    resolve_all: bool,
) -> Entity<MergeView> {
    let shell = cx.weak_entity();
    cx.new(|cx| {
        let mut view = MergeView::new(shell, cx);
        view.target = Some(inputs.target.clone());
        let mut loaded = LoadedMerge::new(Arc::new(inputs));
        if resolve_all {
            for region in loaded.state.conflicts() {
                loaded.state.pick(region, Choice::Ours);
            }
        }
        view.loaded = Some(loaded);
        view.loading = false;
        view
    })
}

fn posed_merge_loading(cx: &mut Context<Shell>) -> Entity<MergeView> {
    let shell = cx.weak_entity();
    cx.new(|cx| {
        let mut view = MergeView::new(shell, cx);
        view.target = Some(home(".config/editor.toml"));
        view.loading = true;
        view
    })
}

/// Build the posed Shell for a gallery state. `None` = unknown name.
pub fn build(name: &str, paths: SettingsPaths, cx: &mut Context<Shell>) -> Option<Shell> {
    let mut shell = |route: Route, model: SyncModel| Shell {
        route,
        state: cx.new(|_| model),
        review: None,
        settings: None,
        merge: None,
        paths: paths.clone(),
        expanded_scans: HashSet::new(),
        unlock_in_flight: false,
        pending_merge: false,
        pending_merge_probed: false,
        changelog: None,
        queue: Default::default(),
        queue_pumping: false,
        deferred_repo_op: None,
        running_repo_op: None,
    };

    // Component previews: bare Shell routed at the preview (no posed data).
    if let Some(comp) = name.strip_prefix("comp:") {
        let comp = tomte_ui::preview::COMPONENTS
            .iter()
            .find(|(n, _)| *n == comp)?
            .0;
        return Some(shell(Route::Component(comp), SyncModel::default()));
    }

    Some(match name {
        "dashboard" => shell(Route::Dashboard, rich_model()),
        "dashboard-empty" => {
            let mut m = SyncModel {
                connected: true,
                ..Default::default()
            };
            m.hydrate_status(vec![], 955, None, false, None);
            m.last_fetch_ts = Some(now() - 120);
            shell(Route::Dashboard, m)
        }
        "dashboard-scanning" => shell(
            Route::Dashboard,
            SyncModel {
                connected: true,
                scanning: true,
                ..Default::default()
            },
        ),
        "dashboard-rescanning" => {
            let mut m = rich_model();
            m.scanning = true;
            shell(Route::Dashboard, m)
        }
        "dashboard-degraded" => {
            let mut m = rich_model();
            m.degraded = Some(
                "chezmoi cannot evaluate templates: 1Password CLI could not authenticate. Unlock 1Password and retry."
                    .into(),
            );
            shell(Route::Dashboard, m)
        }
        "dashboard-disconnected" => shell(Route::Dashboard, SyncModel::default()),
        // The one state where the pending-merge banner must NOT offer to
        // commit: the user confirmed discarding this merge in the editor and
        // the roll-back is waiting for the latch. A live "Finish merge" here
        // would be a button offering the opposite of the answer they gave.
        "dashboard-rolling-back" => {
            let mut s = shell(Route::Dashboard, rich_model());
            s.pending_merge = true;
            s.queue_pumping = true; // a batch holds the latch, so the abort waits
            s.deferred_repo_op = Some(PendingRepoOp::AbortMerge {
                rolled_back: Rollback::Files(2),
                dropped: Vec::new(),
            });
            s
        }
        "review"
        | "review-empty"
        | "review-banner"
        | "review-processing"
        | "review-failed"
        | "review-keep-both"
        | "review-origin"
        | "review-origin-compared"
        | "review-multiselect"
        | "review-tree"
        | "review-tree-selected"
        | "review-many" => {
            let mut model = match name {
                "review-multiselect" | "review-tree" | "review-tree-selected" => {
                    multiselect_model()
                }
                "review-many" => many_model(),
                _ => rich_model(),
            };
            let origin = name.starts_with("review-origin");
            if origin {
                // The scan's hint, kept honest alongside the loaded content
                // the view actually gates on. TWO rows carry it: the selected
                // file, whose detail pane also has origin content loaded, and
                // an UNSELECTED one that has none — the list hint is the only
                // thing origin-shaped that row will ever show, which is the
                // whole reason it exists.
                for entry in &mut model.drifted {
                    if entry.target == home(".config/zed/settings.json")
                        || entry.target == home(".gitconfig")
                    {
                        entry.origin_differs = true;
                    }
                }
            }
            let mut s = shell(Route::Review, model);
            let (banner, selected) = match name {
                "review-empty" => (None, false),
                "review-banner" => (
                    Some(OutcomeBanner {
                        // NOT the selected file: confirming `secrets.yaml`
                        // advanced the selection to `settings.json`, which is
                        // what the header shows. The name is what ties the
                        // sentence — and its Undo button — to a file.
                        text: "Kept disk version · secrets.yaml · committed & pushed".into(),
                        tint: BannerTint::Ok,
                        undoable: true,
                    }),
                    true,
                ),
                _ => (None, true),
            };
            // The queue is the Shell's, so these poses push straight into it
            // — exactly what `Shell::enqueue` does with the click.
            if name == "review-processing" {
                s.queue.push(QueuedAction {
                    target: home(".config/zed/settings.json"),
                    decision: Decision::KeepDisk,
                    notify_on_done: false,
                });
                // Hand it to the (absent) engine: this one is RUNNING, and
                // the one pushed after it is waiting for the repo.
                let _running = s.queue.take_batch();
                s.queue.push(QueuedAction {
                    target: home(".gitconfig"),
                    decision: Decision::KeepSourceLocal,
                    notify_on_done: false,
                });
                s.queue_pumping = true;
            }
            if name == "review-failed" {
                let target = home(".config/zed/settings.json");
                s.queue.push(QueuedAction {
                    target: target.clone(),
                    decision: Decision::KeepDisk,
                    notify_on_done: false,
                });
                s.queue.take_batch();
                s.queue.finish_all(&[(
                    target,
                    Some("Keep disk failed: chezmoi exited with code 1".into()),
                )]);
            }
            let review = posed_review(cx, s.state.clone(), banner, selected);
            if origin {
                review.update(cx, |view, _| {
                    view.origin_diffs =
                        Some(origin_diffs(PREVIEW_ORIGIN, PREVIEW_DISK, PREVIEW_LOCAL));
                    // `review-origin` poses the state a user actually lands on
                    // — the default comparison, with the selector offering the
                    // others. `review-origin-compared` is one click later: the
                    // pair where origin's own tint reaches the diff.
                    if name == "review-origin-compared" {
                        view.diff_pair = DiffPair::DiskVsOrigin;
                    }
                });
            }
            if name == "review-keep-both" {
                review.update(cx, |view, _| {
                    let inputs = tomte_app::merge_inputs::MergeInputs {
                        target: home(".config/zed/settings.json"),
                        ours: "a\n".into(),
                        theirs: "b\n".into(),
                        base: Some("x\n".into()),
                        source_path: home(".local/share/chezmoi/dot_config/zed/settings.json"),
                        templated: false,
                        template: None,
                        span_map: None,
                        origin: None,
                    };
                    view.auto_merge = Some((Arc::new(inputs), "merged\n".into()));
                });
            }
            if name == "review-multiselect" {
                // Ticked against the plan's own `safe_order`, exactly as a
                // real ⌘-click does (`ReviewView::toggle_member`) — not the
                // detail pane's `selected` file, which stays
                // `.config/zed/settings.json` (from `posed_review`) so the
                // pose also shows the selection bar replacing a file's
                // per-row actions while a DIFFERENT file's diff still shows
                // underneath it.
                let home_dir = home("");
                let plan = sidebar_rows(SidebarInputs {
                    drifted: &s.state.read(cx).drifted,
                    queue: &QueueSnapshot::default(),
                    mode: ReviewMode::Flat,
                    collapsed: &BTreeSet::new(),
                    selection: &Selection::default(),
                    home: &home_dir,
                });
                review.update(cx, |view, _| {
                    for target in [
                        home(".gitconfig"),
                        home(".config/starship.toml"),
                        home(".zshrc"),
                    ] {
                        view.selection.toggle(&target, &plan.safe_order);
                    }
                });
            }
            if name == "review-tree" || name == "review-tree-selected" {
                let home_dir = home("");
                // `review-tree`: `.config/nvim/` closed, so its chevron and
                // "no children" are on screen; `review-tree-selected`: fully
                // open, so a folder ticked On shows its files right under it.
                let collapsed: BTreeSet<PathBuf> = if name == "review-tree" {
                    BTreeSet::from([home(".config/nvim")])
                } else {
                    BTreeSet::new()
                };
                let plan = sidebar_rows(SidebarInputs {
                    drifted: &s.state.read(cx).drifted,
                    queue: &QueueSnapshot::default(),
                    mode: ReviewMode::Tree,
                    collapsed: &collapsed,
                    selection: &Selection::default(),
                    home: &home_dir,
                });
                review.update(cx, |view, _| {
                    view.mode = ReviewMode::Tree;
                    view.collapsed = collapsed.clone();
                    if name == "review-tree" {
                        // One tick under `.config/`, none under `nvim/` or
                        // `zed/`: `.config/` reads Partial, "3 of 5".
                        view.selection
                            .toggle(&home(".config/starship.toml"), &plan.safe_order);
                    } else {
                        // Every safe file under `nvim/` (its checkbox On)
                        // plus one more under `.config/` (`.config/` stays
                        // Partial) — "2 selected".
                        let under =
                            safe_descendants(&plan.safe_order, &home(".config/nvim"), &home_dir);
                        view.selection.toggle_all(&under, &plan.safe_order);
                        view.selection
                            .toggle(&home(".config/zed/settings.json"), &plan.safe_order);
                    }
                });
            }
            if name == "review-many" {
                // One failed, one running, three waiting: the sub-line rows
                // the list must size one by one (spec 2026-10-11 §2.4).
                let failed = home(".agents/skills/skill-02/reference/doc-03.md");
                s.queue.push(QueuedAction {
                    target: failed.clone(),
                    decision: Decision::KeepDisk,
                    notify_on_done: false,
                });
                let _ = s.queue.take_batch();
                s.queue
                    .finish_all(&[(failed, Some("op: could not authenticate".into()))]);
                s.queue.push(QueuedAction {
                    target: home(".agents/skills/skill-03/reference/doc-01.md"),
                    decision: Decision::KeepDisk,
                    notify_on_done: false,
                });
                let _running = s.queue.take_batch();
                for file in ["doc-02", "doc-04", "doc-05"] {
                    s.queue.push(QueuedAction {
                        target: home(&format!(".agents/skills/skill-03/reference/{file}.md")),
                        decision: Decision::KeepSourceLocal,
                        notify_on_done: false,
                    });
                }
                s.queue_pumping = true;
                let home_dir = home("");
                let collapsed: BTreeSet<PathBuf> = BTreeSet::from([
                    home(".agents/skills/skill-05/reference"),
                    home(".agents/skills/skill-06/reference"),
                ]);
                let plan = sidebar_rows(SidebarInputs {
                    drifted: &s.state.read(cx).drifted,
                    queue: &QueueSnapshot::default(),
                    mode: ReviewMode::Tree,
                    collapsed: &collapsed,
                    selection: &Selection::default(),
                    home: &home_dir,
                });
                review.update(cx, |view, _| {
                    view.mode = ReviewMode::Tree;
                    view.collapsed = collapsed.clone();
                    for file in 0..6u64 {
                        view.selection.toggle(
                            &home(&format!(
                                ".agents/skills/skill-10/reference/doc-{file:02}.md"
                            )),
                            &plan.safe_order,
                        );
                    }
                });
            }
            s.review = Some(review);
            s
        }
        "merge" => {
            let mut s = shell(Route::Merge, rich_model());
            s.merge = Some(posed_merge(cx, conflict_inputs(), false));
            s
        }
        "merge-editing" => {
            let mut s = shell(Route::Merge, rich_model());
            let shell_weak = cx.weak_entity();
            let merge = cx.new(|cx| {
                let inputs = conflict_inputs();
                let mut view = MergeView::new(shell_weak, cx);
                view.target = Some(inputs.target.clone());
                let loaded = LoadedMerge::new(Arc::new(inputs));
                let region = loaded.state.conflicts()[0];
                view.loaded = Some(loaded);
                view.start_edit(region, cx);
                view
            });
            s.merge = Some(merge);
            s
        }
        "merge-big" => {
            // Perf/scroll testbed: hundreds of lines, regions of every kind
            // spread through the file, protected lines sprinkled in.
            let mut base = String::new();
            let mut ours = String::new();
            let mut theirs = String::new();
            let mut template = String::new();
            // One deliberately over-long line so horizontal scrolling has
            // something to scroll (2026-08-26).
            {
                let long = format!("long_line = \"{}\"\n", "x".repeat(400));
                base.push_str(&long);
                ours.push_str(&long);
                theirs.push_str(&long);
                template.push_str(&long);
            }
            for i in 0..100 {
                // 4 context lines per stanza
                for j in 0..4 {
                    let line = format!("key_{i}_{j} = \"value\"\n");
                    base.push_str(&line);
                    ours.push_str(&line);
                    theirs.push_str(&line);
                    template.push_str(&line);
                }
                match i % 10 {
                    // disk-only edit
                    2 => {
                        base.push_str(&format!("edited_{i} = 0\n"));
                        theirs.push_str(&format!("edited_{i} = 0\n"));
                        template.push_str(&format!("edited_{i} = 0\n"));
                        ours.push_str(&format!("edited_{i} = 99\n"));
                    }
                    // source-only insertion (templated value)
                    5 => {
                        theirs.push_str(&format!("secret_{i} = hunter2\n"));
                        template.push_str(&format!(
                            "secret_{i} = {{{{ onepasswordRead \"op://v/it{i}\" }}}}\n"
                        ));
                    }
                    // true conflict
                    8 => {
                        base.push_str(&format!("mode_{i} = a\n"));
                        ours.push_str(&format!("mode_{i} = disk\n"));
                        theirs.push_str(&format!("mode_{i} = src\n"));
                        template.push_str(&format!("mode_{i} = src\n"));
                    }
                    _ => {}
                }
            }
            let span_map = lex(&template)
                .ok()
                .map(|segments| anchor(&template, &segments, &theirs));
            let inputs = MergeInputs {
                target: home(".config/big.conf"),
                ours,
                theirs,
                base: Some(base),
                source_path: home(".local/share/chezmoi/dot_config/big.conf.tmpl"),
                templated: true,
                template: Some(template),
                span_map,
                origin: None,
            };
            let mut s = shell(Route::Merge, rich_model());
            s.merge = Some(posed_merge(cx, inputs, false));
            s
        }
        "merge-auto" => {
            // The 2026-07-30 user report: source added lines, disk changed a
            // different line — diff3 auto-merges, every region overridable.
            let base =
                "{\n  \"plugin\": [\n    \"cmux\",\n    \"pty\"\n  ],\n  \"theme\": \"dark\"\n}\n";
            let ours =
                "{\n  \"plugin\": [\n    \"cmux\",\n    \"pty\"\n  ],\n  \"theme\": \"light\"\n}\n";
            let theirs = "{\n  \"plugin\": [\n    \"cmux\",\n    \"pty\",\n    \"hindsight\",\n    \"xberg\"\n  ],\n  \"theme\": \"dark\"\n}\n";
            let inputs = MergeInputs {
                target: home(".config/opencode/opencode.json"),
                ours: ours.into(),
                theirs: theirs.into(),
                base: Some(base.into()),
                source_path: home(".local/share/chezmoi/dot_config/opencode/opencode.json"),
                templated: false,
                template: None,
                span_map: None,
                origin: None,
            };
            let mut s = shell(Route::Merge, rich_model());
            s.merge = Some(posed_merge(cx, inputs, false));
            s
        }
        "merge-origin" => {
            // The remote's copy of the source spells the editor flag as a
            // key of its own instead of inlining it: a fourth pane, and an
            // origin pick on the one region where origin has something of
            // its own to offer. Deliberately a DIFFERENT line count from the
            // other three columns — `merge_scroll_sync_spans_four_panes_and_
            // the_result` needs genuinely distinct per-column geometry, since
            // columns that all start a region on the same line cannot catch a
            // column-index mix-up.
            let mut inputs = conflict_inputs();
            inputs.origin = Some(
                concat!(
                    "editor = \"zed\"\n",
                    "editor_args = \"--wait\"\n",
                    "font = \"GeistMono\"\n",
                    "theme = \"dark\"\n",
                    "splits = true\n",
                )
                .into(),
            );
            let mut s = shell(Route::Merge, rich_model());
            s.merge = Some(posed_merge(cx, inputs, false));
            s
        }
        "merge-templated" => {
            let mut s = shell(Route::Merge, rich_model());
            s.merge = Some(posed_merge(cx, templated_inputs(), false));
            s
        }
        "merge-resolved" => {
            let mut s = shell(Route::Merge, rich_model());
            s.merge = Some(posed_merge(cx, conflict_inputs(), true));
            s
        }
        "changelog" => shell(Route::Changelog, rich_model()),
        "merge-loading" => {
            let mut s = shell(Route::Merge, rich_model());
            s.merge = Some(posed_merge_loading(cx));
            s
        }
        "settings" => shell(Route::Settings, rich_model()),
        "settings-menu" => {
            let mut s = shell(Route::Settings, rich_model());
            let state = s.state.clone();
            let posed = cx.new(|_| {
                let mut view =
                    super::settings::SettingsView::posed_for_gallery(paths.clone(), state, false);
                view.menu_open = true;
                view
            });
            s.settings = Some(posed);
            s
        }
        "settings-update-ready" => {
            let mut m = rich_model();
            m.update_ready = Some(tomte_app::update::StagedUpdate {
                version: "0.2.0".into(),
                staged: std::path::PathBuf::from("/tmp/updates/unpacked/Tomte.app"),
                notes: Some(
                    "## What's new in 0.2.0\n\n\
                     - Merge editor: butter-smooth synced scrolling across all panes\n\
                     - Fetch now button with honest failure states\n\
                     - Saves wait their turn instead of failing mid-scan\n\
                     - Resolved items leave the review list instantly"
                        .into(),
                ),
            });
            let mut s = shell(Route::Settings, m);
            let state = s.state.clone();
            s.settings = Some(cx.new(|_| {
                super::settings::SettingsView::posed_for_gallery(paths.clone(), state, false)
            }));
            s
        }
        "settings-update-checking" => {
            let mut s = shell(Route::Settings, rich_model());
            let state = s.state.clone();
            s.settings = Some(cx.new(|_| {
                let mut view =
                    super::settings::SettingsView::posed_for_gallery(paths.clone(), state, false);
                view.update_checking = true;
                view.update_note = Some("downloading 0.2.0…".into());
                view
            }));
            s
        }
        "settings-dirty" => {
            let mut s = shell(Route::Settings, rich_model());
            let state = s.state.clone();
            s.settings = Some(cx.new(|_| {
                super::settings::SettingsView::posed_for_gallery(paths.clone(), state, true)
            }));
            s
        }
        _ => return None,
    })
}
