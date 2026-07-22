//! Pure domain state for the app shell (spec §3.2): no gpui imports, fully
//! unit-testable. Hydrated from `Response::Status` + the read-only journal,
//! then kept live by `czui_proto::Event` pushes.

use std::path::PathBuf;

use czui_journal::EventRow;
use czui_proto::{DriftSummary, Event};

/// Rows the dashboard timeline renders; journal-hydrated rows and synthetic
/// rows from live pushes share this shape. `kind` uses the journal's kind
/// vocabulary (`dest_changed`, `fetch`, …); `class` comes from `meta.class`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineRow {
    pub ts: u64,
    pub kind: String,
    pub target: Option<PathBuf>,
    pub machine: String,
    pub class: Option<String>,
}

/// Newest-first timeline length limit (hydrate and live prepends alike).
pub const TIMELINE_CAP: usize = 500;

/// Machine label stamped on synthetic rows built from live daemon pushes
/// (pushes carry no machine name; they are always about this machine).
const LOCAL_MACHINE: &str = "local";

/// Drift classes that demand a human decision (spec §7.4).
const ATTENTION_CLASSES: [&str; 3] = ["conflict", "local_source_diverged", "eval_failed"];

#[derive(Debug, Clone, Default)]
pub struct SyncModel {
    /// Current drifted targets, unique by target path.
    pub drifted: Vec<DriftSummary>,
    pub in_sync: u64,
    pub degraded: Option<String>,
    /// Newest first, capped at [`TIMELINE_CAP`].
    pub timeline: Vec<TimelineRow>,
    pub last_fetch_ts: Option<u64>,
    pub connected: bool,
    /// Daemon is starting or mid-scan: render "scanning", never "in sync".
    pub scanning: bool,
}

impl SyncModel {
    pub fn hydrate_status(
        &mut self,
        drifted: Vec<DriftSummary>,
        in_sync: u64,
        degraded: Option<String>,
        scanning: bool,
    ) {
        self.scanning = scanning;
        // A scan in progress reports placeholder zeros — keep showing the
        // last known numbers instead of wiping the UI (the "everything reset
        // to 0 during rescan" complaint); real data replaces them when the
        // scan lands.
        let placeholder = scanning && drifted.is_empty() && in_sync == 0;
        let have_data = !self.drifted.is_empty() || self.in_sync > 0;
        if placeholder && have_data {
            self.degraded = degraded;
            return;
        }
        self.drifted.clear();
        for d in drifted {
            if !self.drifted.iter().any(|e| e.target == d.target) {
                self.drifted.push(d);
            }
        }
        self.in_sync = in_sync;
        self.degraded = degraded;
    }

    pub fn hydrate_timeline(&mut self, rows: Vec<EventRow>) {
        self.timeline = rows
            .into_iter()
            .take(TIMELINE_CAP)
            .map(|r| TimelineRow {
                ts: r.ts,
                kind: r.kind,
                target: r.target,
                machine: r.machine,
                class: r
                    .meta
                    .as_ref()
                    .and_then(|m| m.get("class"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
            })
            .collect();
    }

    pub fn apply_event(&mut self, ev: Event) {
        match ev {
            Event::Drift { target, class, ts } => {
                if class == "in_sync" {
                    self.drifted.retain(|d| d.target != target);
                    return;
                }
                if self.upsert_drift(&target, &class, ts) {
                    self.push_row(TimelineRow {
                        ts,
                        kind: kind_for_class(&class).into(),
                        target: Some(target),
                        machine: LOCAL_MACHINE.into(),
                        class: Some(class),
                    });
                }
            }
            Event::EvalFailed {
                target,
                hint: _,
                ts,
            } => {
                let is_news = match &target {
                    Some(t) => self.upsert_drift(t, "eval_failed", ts),
                    None => true,
                };
                if is_news {
                    self.push_row(TimelineRow {
                        ts,
                        kind: "eval_failed".into(),
                        target,
                        machine: LOCAL_MACHINE.into(),
                        class: Some("eval_failed".into()),
                    });
                }
            }
            Event::LeftManagement { target, ts } => {
                self.drifted.retain(|d| d.target != target);
                self.push_row(TimelineRow {
                    ts,
                    kind: "left_management".into(),
                    target: Some(target),
                    machine: LOCAL_MACHINE.into(),
                    class: None,
                });
            }
            Event::FetchDone { ts, behind: _ } => {
                self.last_fetch_ts = Some(ts);
                self.push_row(TimelineRow {
                    ts,
                    kind: "fetch".into(),
                    target: None,
                    machine: LOCAL_MACHINE.into(),
                    class: None,
                });
            }
            Event::ScanDone { ts: _, drifted } => {
                // Per-target Drift pushes precede ScanDone, so survivors are
                // already up to date; a clean scan means nothing survived.
                if drifted == 0 {
                    self.drifted.clear();
                }
            }
            Event::RemoteAdvanced { target, ts } => {
                // Informational: any drift consequence arrives as its own push.
                self.push_row(TimelineRow {
                    ts,
                    kind: "remote_advanced".into(),
                    target: Some(target),
                    machine: LOCAL_MACHINE.into(),
                    class: None,
                });
            }
        }
    }

    /// Number of drifted targets whose class demands a human decision.
    pub fn needs_attention(&self) -> usize {
        self.drifted
            .iter()
            .filter(|d| ATTENTION_CLASSES.contains(&d.class.as_str()))
            .count()
    }

    /// NSStatusItem title: "cz", or "cz ●N" when N targets need a human.
    pub fn status_title(&self) -> String {
        if !self.connected || self.scanning {
            return "cz …".to_string();
        }
        match self.needs_attention() {
            0 => "cz".to_string(),
            n => format!("cz ●{n}"),
        }
    }

    /// Data for `platform_mac::MenuSpec` (kept as a tuple so this module
    /// stays free of AppKit types): (header, freshness, review label,
    /// sync-all enabled). Sync-all needs a clean tree AND a live daemon.
    pub fn menu_spec(&self, now_ts: u64) -> (String, String, Option<String>, bool) {
        let header = if !self.connected {
            "chezmoid not connected".to_string()
        } else if self.scanning {
            "scanning…".to_string()
        } else if self.drifted.is_empty() {
            format!("All in sync · {} files", self.in_sync)
        } else {
            match self.needs_attention() {
                0 => format!("{} drifted", self.drifted.len()),
                n => format!("{} drifted · {n} need attention", self.drifted.len()),
            }
        };
        let freshness = match self.last_fetch_ts {
            None => "origin: never fetched".to_string(),
            Some(ts) => format!("origin: fetched {}", time_ago(now_ts, ts)),
        };
        let review_label = (!self.drifted.is_empty()).then(|| {
            let n = self.drifted.len();
            format!("Review {n}\u{2026}")
        });
        // Never enabled on placeholder data: a scan in progress or a
        // degraded evaluation might be hiding drift (spec §7.4).
        let sync_all_enabled =
            self.drifted.is_empty() && self.connected && !self.scanning && self.degraded.is_none();
        (header, freshness, review_label, sync_all_enabled)
    }

    /// Insert or update the drift entry for `target`. Returns whether this
    /// was new information (new target, or the class changed) — repeat
    /// observations of a known state must not spam the timeline.
    fn upsert_drift(&mut self, target: &std::path::Path, class: &str, ts: u64) -> bool {
        match self.drifted.iter_mut().find(|d| d.target == target) {
            Some(existing) if existing.class == class => false,
            Some(existing) => {
                existing.class = class.to_string();
                existing.since_ts = Some(ts);
                true
            }
            None => {
                self.drifted.push(DriftSummary {
                    target: target.to_path_buf(),
                    class: class.to_string(),
                    since_ts: Some(ts),
                });
                true
            }
        }
    }

    fn push_row(&mut self, row: TimelineRow) {
        self.timeline.insert(0, row);
        self.timeline.truncate(TIMELINE_CAP);
    }
}

/// Journal event kind a live Drift push would have been recorded under, so
/// synthetic rows use the same vocabulary as hydrated ones.
fn kind_for_class(class: &str) -> &'static str {
    match class {
        "remote_ahead" => "remote_advanced",
        "source_ahead" | "local_source_diverged" => "source_changed",
        "eval_failed" => "eval_failed",
        _ => "dest_changed", // destination_drift, conflict, unknown future classes
    }
}

/// Coarse relative-time label ("just now", "3m ago", "2h ago", "4d ago") for
/// menu freshness lines and dashboard timeline rows. Clock skew (a timestamp
/// from the future) clamps to "just now" rather than underflowing.
pub fn time_ago(now_ts: u64, ts: u64) -> String {
    let delta = now_ts.saturating_sub(ts);
    match delta {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", delta / 60),
        3_600..=86_399 => format!("{}h ago", delta / 3_600),
        _ => format!("{}d ago", delta / 86_400),
    }
}

/// Timeline glyph for a journal event kind (spec §7.1, approved mockup B+C).
/// Kinds outside the glyph vocabulary (fetch, source_changed, future kinds)
/// fall back to a neutral dot.
/// A dashboard timeline item after collapsing noise: either a real row or a
/// group of consecutive info rows (scans/fetches) folded into one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineItem {
    Row(TimelineRow),
    /// `newest_ts` doubles as the stable expansion key across refreshes.
    ScanGroup {
        count: usize,
        newest_ts: u64,
        rows: Vec<TimelineRow>,
    },
}

/// Collapse runs of ≥2 consecutive target-less info rows (scan/fetch) into a
/// single expandable group; everything else passes through in order.
pub fn group_timeline(rows: &[TimelineRow]) -> Vec<TimelineItem> {
    let mut items = Vec::new();
    let mut run: Vec<TimelineRow> = Vec::new();
    let is_info = |r: &TimelineRow| r.target.is_none() && r.class.is_none();
    let flush = |run: &mut Vec<TimelineRow>, items: &mut Vec<TimelineItem>| match run.len() {
        0 => {}
        1 => items.push(TimelineItem::Row(run.remove(0))),
        _ => {
            let newest_ts = run.first().map(|r| r.ts).unwrap_or(0);
            items.push(TimelineItem::ScanGroup {
                count: run.len(),
                newest_ts,
                rows: std::mem::take(run),
            });
        }
    };
    for row in rows {
        if is_info(row) {
            run.push(row.clone());
        } else {
            flush(&mut run, &mut items);
            items.push(TimelineItem::Row(row.clone()));
        }
    }
    flush(&mut run, &mut items);
    items
}

/// Human label for a drift class — raw enum names ("destination_drift")
/// never reach the UI.
pub fn class_label(class: &str) -> &'static str {
    match class {
        "destination_drift" => "modified on disk",
        "source_ahead" => "source updated",
        "remote_ahead" => "origin ahead",
        "local_source_diverged" => "diverged from origin",
        "conflict" => "conflict",
        "eval_failed" => "can't evaluate",
        "in_sync" => "in sync",
        _ => "changed",
    }
}

/// Human label for a journal/timeline event kind.
pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "dest_changed" => "modified on disk",
        "source_changed" => "source changed",
        "remote_advanced" => "origin advanced",
        "applied" => "applied",
        "readded" => "re-added",
        "resolved" => "resolved",
        "eval_failed" => "can't evaluate",
        "fetch" => "scan",
        "left_management" => "left management",
        "session_start" | "session_end" => "session",
        _ => "event",
    }
}

pub fn kind_glyph(kind: &str) -> &'static str {
    match kind {
        "dest_changed" => "Δ",
        "remote_advanced" => "↓",
        "applied" | "resolved" => "✓",
        "eval_failed" => "⛔",
        "left_management" => "−",
        _ => "·",
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use czui_journal::EventRow;
    use czui_proto::{DriftSummary, Event};

    use super::*;

    fn drift(target: &str, class: &str, ts: u64) -> Event {
        Event::Drift {
            target: PathBuf::from(target),
            class: class.into(),
            ts,
        }
    }

    fn summary(target: &str, class: &str, since_ts: Option<u64>) -> DriftSummary {
        DriftSummary {
            target: PathBuf::from(target),
            class: class.into(),
            since_ts,
        }
    }

    fn row(id: i64, kind: &str, meta: Option<serde_json::Value>) -> EventRow {
        EventRow {
            id,
            target: Some(PathBuf::from("/home/u/.zshrc")),
            ts: id as u64,
            machine: "mac-a".into(),
            kind: kind.into(),
            from_hash: None,
            to_hash: None,
            meta,
        }
    }

    #[test]
    fn hydrate_status_populates_and_dedups_by_target() {
        let mut m = SyncModel::default();
        m.hydrate_status(
            vec![
                summary("/a", "conflict", Some(1)),
                summary("/b", "destination_drift", Some(2)),
                summary("/a", "destination_drift", Some(3)), // dup target: first wins
            ],
            12,
            Some("chezmoi doctor".into()),
            false,
        );
        assert_eq!(m.in_sync, 12);
        assert_eq!(m.degraded.as_deref(), Some("chezmoi doctor"));
        assert_eq!(m.drifted.len(), 2);
        assert_eq!(m.drifted[0].target, Path::new("/a"));
        assert_eq!(m.drifted[0].class, "conflict");
        assert_eq!(m.drifted[1].target, Path::new("/b"));
    }

    #[test]
    fn hydrate_timeline_maps_rows_and_extracts_meta_class() {
        // (meta, expected class) table
        let cases: Vec<(Option<serde_json::Value>, Option<&str>)> = vec![
            (
                Some(serde_json::json!({"class": "conflict"})),
                Some("conflict"),
            ),
            (Some(serde_json::json!({"hint": "x"})), None), // no class key
            (Some(serde_json::json!({"class": 3})), None),  // non-string class
            (None, None),                                   // no meta at all
        ];
        let mut m = SyncModel::default();
        m.hydrate_timeline(
            cases
                .iter()
                .enumerate()
                .map(|(i, (meta, _))| row(i as i64 + 1, "dest_changed", meta.clone()))
                .collect(),
        );
        assert_eq!(m.timeline.len(), cases.len());
        for (i, (_, want)) in cases.iter().enumerate() {
            assert_eq!(m.timeline[i].class.as_deref(), *want, "case {i}");
            assert_eq!(m.timeline[i].ts, i as u64 + 1);
            assert_eq!(m.timeline[i].kind, "dest_changed");
            assert_eq!(m.timeline[i].machine, "mac-a");
            assert_eq!(
                m.timeline[i].target.as_deref(),
                Some(Path::new("/home/u/.zshrc"))
            );
        }

        // a target-less row stays target-less
        let mut m2 = SyncModel::default();
        m2.hydrate_timeline(vec![EventRow {
            target: None,
            ..row(9, "fetch", None)
        }]);
        assert_eq!(m2.timeline[0].target, None);
        assert_eq!(m2.timeline[0].kind, "fetch");
    }

    #[test]
    fn apply_drift_dedups_by_target_and_rows_only_on_new_information() {
        let mut m = SyncModel::default();

        m.apply_event(drift("/a", "destination_drift", 100));
        assert_eq!(m.drifted.len(), 1);
        assert_eq!(m.drifted[0].since_ts, Some(100));
        assert_eq!(m.timeline.len(), 1);
        assert_eq!(m.timeline[0].kind, "dest_changed");
        assert_eq!(m.timeline[0].class.as_deref(), Some("destination_drift"));
        assert_eq!(m.timeline[0].machine, "local");

        // same target, same class re-observed: no dup entry, no dup row,
        // since_ts keeps the original onset
        m.apply_event(drift("/a", "destination_drift", 160));
        assert_eq!(m.drifted.len(), 1);
        assert_eq!(m.drifted[0].since_ts, Some(100));
        assert_eq!(m.timeline.len(), 1);

        // same target, class escalates: entry updated in place, new row
        m.apply_event(drift("/a", "conflict", 200));
        assert_eq!(m.drifted.len(), 1);
        assert_eq!(m.drifted[0].class, "conflict");
        assert_eq!(m.drifted[0].since_ts, Some(200));
        assert_eq!(m.timeline.len(), 2);
        assert_eq!(m.timeline[0].ts, 200, "newest first");

        // second target appends
        m.apply_event(drift("/b", "remote_ahead", 300));
        assert_eq!(m.drifted.len(), 2);
        assert_eq!(m.timeline[0].kind, "remote_advanced");

        // back in sync removes the entry without a timeline row
        m.apply_event(drift("/a", "in_sync", 400));
        assert_eq!(m.drifted.len(), 1);
        assert_eq!(m.drifted[0].target, Path::new("/b"));
        assert_eq!(m.timeline.len(), 3);
    }

    #[test]
    fn apply_drift_maps_class_to_journal_kind() {
        let cases = [
            ("destination_drift", "dest_changed"),
            ("conflict", "dest_changed"),
            ("source_ahead", "source_changed"),
            ("local_source_diverged", "source_changed"),
            ("remote_ahead", "remote_advanced"),
            ("eval_failed", "eval_failed"),
        ];
        for (class, want_kind) in cases {
            let mut m = SyncModel::default();
            m.apply_event(drift("/f", class, 1));
            assert_eq!(m.timeline[0].kind, want_kind, "class {class}");
            assert_eq!(m.timeline[0].class.as_deref(), Some(class));
        }
    }

    #[test]
    fn apply_lifecycle_events() {
        let mut m = SyncModel::default();
        m.apply_event(drift("/a", "conflict", 10));
        m.apply_event(drift("/b", "destination_drift", 11));

        // LeftManagement drops the drift entry and logs a row
        m.apply_event(Event::LeftManagement {
            target: PathBuf::from("/a"),
            ts: 20,
        });
        assert_eq!(m.drifted.len(), 1);
        assert_eq!(m.timeline[0].kind, "left_management");
        assert_eq!(m.timeline[0].class, None);

        // FetchDone tracks freshness and logs a target-less row
        m.apply_event(Event::FetchDone { ts: 30, behind: 2 });
        assert_eq!(m.last_fetch_ts, Some(30));
        assert_eq!(m.timeline[0].kind, "fetch");
        assert_eq!(m.timeline[0].target, None);

        // ScanDone with survivors leaves the drift list alone…
        m.apply_event(Event::ScanDone { ts: 40, drifted: 1 });
        assert_eq!(m.drifted.len(), 1);
        // …a clean scan clears it
        m.apply_event(Event::ScanDone { ts: 50, drifted: 0 });
        assert!(m.drifted.is_empty());

        // EvalFailed upserts a drift entry and logs a row
        m.apply_event(Event::EvalFailed {
            target: Some(PathBuf::from("/tpl")),
            hint: "bad template".into(),
            ts: 60,
        });
        assert_eq!(m.drifted.len(), 1);
        assert_eq!(m.drifted[0].class, "eval_failed");
        assert_eq!(m.timeline[0].kind, "eval_failed");
        // the follow-up Drift push for the same class is not new information
        m.apply_event(drift("/tpl", "eval_failed", 61));
        assert_eq!(m.drifted.len(), 1);
        assert_eq!(m.timeline[0].ts, 60);

        // target-less EvalFailed logs a row but cannot join the drift list
        m.apply_event(Event::EvalFailed {
            target: None,
            hint: "doctor".into(),
            ts: 70,
        });
        assert_eq!(m.drifted.len(), 1);
        assert_eq!(m.timeline[0].target, None);

        // RemoteAdvanced is informational only
        m.apply_event(Event::RemoteAdvanced {
            target: PathBuf::from("/b"),
            ts: 80,
        });
        assert_eq!(m.timeline[0].kind, "remote_advanced");
        assert_eq!(m.drifted.len(), 1);
    }

    #[test]
    fn needs_attention_counts_only_conflictish_classes() {
        let cases: Vec<(Vec<&str>, usize)> = vec![
            (vec![], 0),
            (vec!["destination_drift", "source_ahead", "remote_ahead"], 0),
            (vec!["conflict"], 1),
            (vec!["local_source_diverged"], 1),
            (vec!["eval_failed"], 1),
            (
                vec!["conflict", "eval_failed", "destination_drift", "conflict"],
                3,
            ),
        ];
        for (classes, want) in cases {
            let mut m = SyncModel::default();
            m.hydrate_status(
                classes
                    .iter()
                    .enumerate()
                    .map(|(i, c)| summary(&format!("/f{i}"), c, Some(1)))
                    .collect(),
                0,
                None,
                false,
            );
            assert_eq!(m.needs_attention(), want, "classes {classes:?}");
        }
    }

    #[test]
    fn status_title_shows_attention_count() {
        let cases: Vec<(Vec<&str>, &str)> = vec![
            (vec![], "cz"),
            (vec!["destination_drift"], "cz"), // drifted but nothing urgent
            (vec!["conflict"], "cz ●1"),
            (
                vec!["conflict", "eval_failed", "local_source_diverged"],
                "cz ●3",
            ),
        ];
        for (classes, want) in cases {
            let mut m = SyncModel {
                connected: true, // the title only reports counts once live
                ..Default::default()
            };
            m.hydrate_status(
                classes
                    .iter()
                    .enumerate()
                    .map(|(i, c)| summary(&format!("/f{i}"), c, Some(1)))
                    .collect(),
                0,
                None,
                false,
            );
            assert_eq!(m.status_title(), want, "classes {classes:?}");
        }
    }

    #[test]
    fn menu_spec_reflects_connection_drift_and_freshness() {
        // disconnected: sync-all locked out even with zero drift
        let mut m = SyncModel::default();
        let (header, freshness, review, sync_all) = m.menu_spec(1_000);
        assert_eq!(header, "chezmoid not connected");
        assert_eq!(freshness, "origin: never fetched");
        assert_eq!(review, None);
        assert!(!sync_all);

        // connected + clean: sync-all enabled, no review entry
        m.connected = true;
        m.hydrate_status(vec![], 42, None, false);
        m.last_fetch_ts = Some(1_000 - 30);
        let (header, freshness, review, sync_all) = m.menu_spec(1_000);
        assert_eq!(header, "All in sync · 42 files");
        assert_eq!(freshness, "origin: fetched just now");
        assert_eq!(review, None);
        assert!(sync_all);

        // connected + drift: review entry, sync-all disabled
        m.apply_event(drift("/a", "conflict", 900));
        m.apply_event(drift("/b", "destination_drift", 901));
        let (header, _, review, sync_all) = m.menu_spec(1_000);
        assert_eq!(header, "2 drifted · 1 need attention");
        assert_eq!(review.as_deref(), Some("Review 2…"));
        assert!(!sync_all);

        // drift without anything urgent
        let mut calm = SyncModel {
            connected: true,
            ..SyncModel::default()
        };
        calm.apply_event(drift("/a", "destination_drift", 1));
        let (header, _, _, _) = calm.menu_spec(1_000);
        assert_eq!(header, "1 drifted");

        // freshness granularity table
        let fresh_cases = [
            (59, "origin: fetched just now"),
            (60, "origin: fetched 1m ago"),
            (185, "origin: fetched 3m ago"),
            (3_600, "origin: fetched 1h ago"),
            (7_260, "origin: fetched 2h ago"),
            (86_400, "origin: fetched 1d ago"),
            (259_200, "origin: fetched 3d ago"),
        ];
        for (age, want) in fresh_cases {
            let m = SyncModel {
                connected: true,
                last_fetch_ts: Some(1_000_000 - age),
                ..SyncModel::default()
            };
            let (_, freshness, _, _) = m.menu_spec(1_000_000);
            assert_eq!(freshness, want, "age {age}s");
        }
        // clock skew (fetch ts in the future) must not underflow
        let m = SyncModel {
            last_fetch_ts: Some(2_000),
            ..SyncModel::default()
        };
        let (_, freshness, _, _) = m.menu_spec(1_000);
        assert_eq!(freshness, "origin: fetched just now");
    }

    #[test]
    fn time_ago_buckets() {
        let cases = [
            (0, "just now"),
            (59, "just now"),
            (60, "1m ago"),
            (185, "3m ago"),
            (3_599, "59m ago"),
            (3_600, "1h ago"),
            (86_399, "23h ago"),
            (86_400, "1d ago"),
            (259_200, "3d ago"),
        ];
        for (age, want) in cases {
            assert_eq!(time_ago(1_000_000, 1_000_000 - age), want, "age {age}s");
        }
        // clock skew (ts in the future) must not underflow
        assert_eq!(time_ago(100, 200), "just now");
    }

    #[test]
    fn kind_glyphs_cover_the_journal_vocabulary() {
        let cases = [
            ("dest_changed", "Δ"),
            ("remote_advanced", "↓"),
            ("applied", "✓"),
            ("resolved", "✓"),
            ("eval_failed", "⛔"),
            ("left_management", "−"),
            // informational rows fall back to a neutral dot
            ("fetch", "·"),
            ("source_changed", "·"),
            ("unknown_future_kind", "·"),
        ];
        for (kind, want) in cases {
            assert_eq!(kind_glyph(kind), want, "kind {kind}");
        }
    }

    #[test]
    fn timeline_caps_at_500_rows() {
        // hydrate: oversized journal page is truncated
        let mut m = SyncModel::default();
        m.hydrate_timeline((0..600).map(|i| row(i, "dest_changed", None)).collect());
        assert_eq!(m.timeline.len(), TIMELINE_CAP);
        assert_eq!(
            m.timeline[0].ts, 0,
            "hydrate keeps journal order (newest first)"
        );

        // live events: prepend + trim keeps the newest 500
        let mut m = SyncModel::default();
        for i in 0..510u64 {
            m.apply_event(drift(&format!("/f{i}"), "destination_drift", i));
        }
        assert_eq!(m.timeline.len(), TIMELINE_CAP);
        assert_eq!(m.timeline[0].ts, 509, "newest first");
        assert_eq!(m.timeline[TIMELINE_CAP - 1].ts, 10, "oldest rows trimmed");
        assert_eq!(m.drifted.len(), 510, "drift list is not capped");
    }

    #[test]
    fn rescan_placeholder_does_not_wipe_known_stats() {
        let mut m = SyncModel {
            connected: true,
            ..Default::default()
        };
        m.hydrate_status(
            vec![summary("/a", "destination_drift", Some(1))],
            954,
            None,
            false,
        );
        // rescan begins: daemon reports placeholder zeros + scanning
        m.hydrate_status(vec![], 0, Some("scan in progress…".into()), true);
        assert_eq!(m.in_sync, 954, "stats must survive a rescan");
        assert_eq!(m.drifted.len(), 1);
        assert!(m.scanning);
        // scan lands: real data replaces
        m.hydrate_status(vec![], 955, None, false);
        assert_eq!(m.in_sync, 955);
        assert!(m.drifted.is_empty());
        assert!(!m.scanning);
    }

    #[test]
    fn group_timeline_collapses_consecutive_info_rows() {
        let info = |ts| TimelineRow {
            ts,
            kind: "fetch".into(),
            target: None,
            machine: "m".into(),
            class: None,
        };
        let drift = |ts| TimelineRow {
            ts,
            kind: "dest_changed".into(),
            target: Some(PathBuf::from("/f")),
            machine: "m".into(),
            class: Some("destination_drift".into()),
        };
        let rows = vec![
            info(10),
            info(9),
            info(8),
            drift(7),
            info(6),
            drift(5),
            info(4),
        ];
        let items = group_timeline(&rows);
        assert_eq!(items.len(), 5);
        match &items[0] {
            TimelineItem::ScanGroup {
                count: 3,
                newest_ts: 10,
                rows,
            } => {
                assert_eq!(rows.len(), 3)
            }
            other => panic!("expected group, got {other:?}"),
        }
        assert!(matches!(&items[1], TimelineItem::Row(r) if r.ts == 7));
        // single info row passes through un-grouped
        assert!(matches!(&items[2], TimelineItem::Row(r) if r.ts == 6));
        assert!(matches!(&items[3], TimelineItem::Row(r) if r.ts == 5));
        assert!(matches!(&items[4], TimelineItem::Row(r) if r.ts == 4));
    }
}
