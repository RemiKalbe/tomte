//! Gallery fixtures: named, fully-posed UI states built from synthetic data —
//! no daemon, no IPC, no subprocesses (except the live `settings` state).
//! `chezmoi-ui --gallery <name>` renders one in a real window so the agent
//! (or a human) can screenshot any state on demand instead of reproducing it
//! by hand. The registry doubles as documentation of every reachable state.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use czui_app::merge_inputs::MergeInputs;
use czui_app::model::{SyncModel, TimelineRow};
use czui_core::merge::{Choice, MergeDocument, MergeOptions};
use czui_core::template::{anchor::anchor, lexer::lex};
use czui_proto::DriftSummary;
use gpui::{AppContext as _, Context, Entity};

use super::merge::{LoadedMerge, MergeView};
use super::review::{BannerTint, OutcomeBanner, PreviewState, ProvRow, ReviewView};
use super::settings::SettingsPaths;
use super::{Route, Shell};

/// Every state the gallery can pose: `(name, description)`.
pub const STATES: &[(&str, &str)] = &[
    ("dashboard", "populated: drifted files, mixed timeline, scan group"),
    ("dashboard-empty", "everything in sync"),
    ("dashboard-scanning", "fresh boot, first scan running"),
    ("dashboard-rescanning", "data present, rescan in progress"),
    ("dashboard-degraded", "1Password-style degraded banner"),
    ("dashboard-disconnected", "daemon not connected"),
    ("review", "sidebar groups + selected file with diff preview"),
    ("review-empty", "nothing selected"),
    ("review-banner", "action outcome banner with Undo"),
    ("review-working", "action in flight"),
    ("merge", "three-pane editor with unresolved conflicts"),
    ("merge-templated", "templated file with protected 🔒 spans"),
    ("merge-resolved", "all regions decided, Save enabled"),
    ("merge-loading", "inputs loading"),
    ("settings", "live settings view (reads real settings/op)"),
];

fn home(sub: &str) -> PathBuf {
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
            },
            DriftSummary {
                target: home(".config/zed/settings.json"),
                class: "destination_drift".into(),
                since_ts: Some(now() - 11_000),
            },
            DriftSummary {
                target: home(".gitconfig"),
                class: "remote_ahead".into(),
                since_ts: Some(now() - 90_000),
            },
            DriftSummary {
                target: home(".config/secrets.yaml"),
                class: "eval_failed".into(),
                since_ts: Some(now() - 200),
            },
        ],
        951,
        None,
        false,
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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(1_784_700_000)
}

/// Small believable config diff for the review preview.
fn preview_document() -> MergeDocument {
    let rendered = "theme = \"catppuccin\"\nfont_size = 14\nkeymap = \"vim\"\nformat_on_save = true\n";
    let disk = "theme = \"catppuccin\"\nfont_size = 15\nkeymap = \"vim\"\nformat_on_save = true\ntelemetry = false\n";
    MergeDocument::compute(rendered, disk, rendered, MergeOptions::default())
}

fn posed_review(
    cx: &mut Context<Shell>,
    state: Entity<SyncModel>,
    banner: Option<OutcomeBanner>,
    in_flight: bool,
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
        view.action_in_flight = in_flight;
        view
    })
}

/// Plain-file conflict: both sides touched the editor line differently, plus
/// one side-only change each — exercises every region kind.
fn conflict_inputs() -> MergeInputs {
    let base = "editor = \"vim\"\nfont = \"Menlo\"\ntheme = \"dark\"\nsplits = true\n";
    let ours = "editor = \"helix\"\nfont = \"Menlo\"\ntheme = \"dark\"\nsplits = true\nmouse = false\n";
    let theirs = "editor = \"zed --wait\"\nfont = \"GeistMono\"\ntheme = \"dark\"\nsplits = true\n";
    MergeInputs {
        target: home(".config/editor.toml"),
        ours: ours.into(),
        theirs: theirs.into(),
        base: Some(base.into()),
        source_path: home(".local/share/chezmoi/dot_config/editor.toml"),
        templated: false,
        span_map: None,
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
        span_map,
    }
}

fn posed_merge(cx: &mut Context<Shell>, inputs: MergeInputs, resolve_all: bool) -> Entity<MergeView> {
    let shell = cx.weak_entity();
    cx.new(|_| {
        let mut view = MergeView::new(shell);
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
    cx.new(|_| {
        let mut view = MergeView::new(shell);
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
        dashboard_action_in_flight: false,
    };

    Some(match name {
        "dashboard" => shell(Route::Dashboard, rich_model()),
        "dashboard-empty" => {
            let mut m = SyncModel {
                connected: true,
                ..Default::default()
            };
            m.hydrate_status(vec![], 955, None, false);
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
                "chezmoi cannot evaluate templates: 1Password is locked (set OP_ACCOUNT in Settings)"
                    .into(),
            );
            shell(Route::Dashboard, m)
        }
        "dashboard-disconnected" => shell(Route::Dashboard, SyncModel::default()),
        "review" | "review-empty" | "review-banner" | "review-working" => {
            let model = rich_model();
            let mut s = shell(Route::Review, model);
            let (banner, in_flight, selected) = match name {
                "review-empty" => (None, false, false),
                "review-banner" => (
                    Some(OutcomeBanner {
                        text: "Kept disk version · committed & pushed".into(),
                        tint: BannerTint::Ok,
                        undoable: true,
                    }),
                    false,
                    true,
                ),
                "review-working" => (None, true, true),
                _ => (None, false, true),
            };
            s.review = Some(posed_review(cx, s.state.clone(), banner, in_flight, selected));
            s
        }
        "merge" => {
            let mut s = shell(Route::Merge, rich_model());
            s.merge = Some(posed_merge(cx, conflict_inputs(), false));
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
        "merge-loading" => {
            let mut s = shell(Route::Merge, rich_model());
            s.merge = Some(posed_merge_loading(cx));
            s
        }
        "settings" => shell(Route::Settings, rich_model()),
        _ => return None,
    })
}
