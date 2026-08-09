//! Gallery fixtures: named, fully-posed UI states built from synthetic data —
//! no daemon, no IPC, no subprocesses (except the live `settings` state).
//! `tomte --gallery <name>` renders one in a real window so the agent
//! (or a human) can screenshot any state on demand instead of reproducing it
//! by hand. The registry doubles as documentation of every reachable state.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AppContext as _, Context, Entity};
use tomte_app::merge_inputs::MergeInputs;
use tomte_app::model::{SyncModel, TimelineRow};
use tomte_core::merge::{Choice, MergeDocument, MergeOptions};
use tomte_core::template::{anchor::anchor, lexer::lex};
use tomte_proto::DriftSummary;

use super::merge::{LoadedMerge, MergeView};
use super::review::{BannerTint, OutcomeBanner, PreviewState, ProvRow, ReviewView};
use super::settings::SettingsPaths;
use super::{Route, Shell};

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
    ("review", "sidebar groups + selected file with diff preview"),
    ("review-empty", "nothing selected"),
    ("review-banner", "action outcome banner with Undo"),
    ("review-working", "action in flight"),
    (
        "review-keep-both",
        "clean auto-merge: third quick action offered",
    ),
    ("merge", "three-pane editor with unresolved conflicts"),
    (
        "merge-auto",
        "no conflicts: auto-merged regions, overridable strips",
    ),
    (
        "merge-big",
        "500-line templated file, many regions — scroll/perf testbed",
    ),
    ("merge-templated", "templated file with protected 🔒 spans"),
    ("merge-resolved", "all regions decided, Save enabled"),
    ("merge-loading", "inputs loading"),
    ("settings", "live settings view (reads real settings/op)"),
    ("settings-menu", "settings with the account dropdown open"),
    (
        "settings-dirty",
        "unsaved edits: floating Save/Revert toolbar",
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
    if name.starts_with("comp:") {
        size(px(560.), px(640.))
    } else {
        size(px(980.), px(640.))
    }
}

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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(1_784_700_000)
}

/// Small believable config diff for the review preview.
fn preview_document() -> MergeDocument {
    let rendered =
        "theme = \"catppuccin\"\nfont_size = 14\nkeymap = \"vim\"\nformat_on_save = true\n";
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
        dashboard_action_in_flight: false,
        unlock_in_flight: false,
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
        "review" | "review-empty" | "review-banner" | "review-working" | "review-keep-both" => {
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
            let review = posed_review(cx, s.state.clone(), banner, in_flight, selected);
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
                    };
                    view.auto_merge = Some((Arc::new(inputs), "merged\n".into()));
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
        "merge-big" => {
            // Perf/scroll testbed: hundreds of lines, regions of every kind
            // spread through the file, protected lines sprinkled in.
            let mut base = String::new();
            let mut ours = String::new();
            let mut theirs = String::new();
            let mut template = String::new();
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
            };
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
