//! The app's stuck-daemon watchdog, pure so it can be tested without a
//! daemon or a clock.

use std::time::{Duration, Instant};

/// The stuck-daemon watchdog (spec 2026-10-11 §2.2): restarts a daemon that
/// reports `scanning` for `limit` with NO sign of life. A `ScanDone` IS a
/// sign of life — scans run back to back while the user clears a queue
/// (every batch ends with a rescan), so `scanning` alone cannot tell
/// "working" from "stuck". Fires once per connection epoch; re-arms when the
/// daemon reports not scanning. A wall gap beyond `sleep_gap` between ticks
/// (the machine napped) resets the clock.
///
/// History, from the 2026-08-02 sleep/wake incident (kill-loop every 180 s,
/// orphaned daemons piling up): the limit sits past the daemon's own 5-min
/// startup deadline so a daemon that can self-recover always gets to; sleep
/// must not count as stuck time; and after firing once it stays quiet until
/// the daemon has reported healthy, so a slow recovery is never kill-looped.
#[derive(Debug)]
pub struct DaemonWatchdog {
    limit: Duration,
    sleep_gap: Duration,
    stuck_since: Option<Instant>,
    armed: bool,
    last_tick: Option<Instant>,
}

/// What one [`DaemonWatchdog::tick`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Quiet,
    /// The daemon has reported `scanning` for longer than the limit with no
    /// `ScanDone`: restart it. Carries how long it has looked stuck.
    Restart {
        stuck_for: Duration,
    },
}

impl DaemonWatchdog {
    pub fn new(limit: Duration, sleep_gap: Duration) -> Self {
        Self {
            limit,
            sleep_gap,
            stuck_since: None,
            armed: true,
            last_tick: None,
        }
    }

    /// A `ScanDone` arrived: the daemon is alive, whatever `scanning` says.
    /// The stuck clock restarts from the next `scanning` tick.
    pub fn scan_done(&mut self, _now: Instant) {
        self.stuck_since = None;
    }

    /// One status poll (every 30 s): `scanning` as the model reports it.
    pub fn tick(&mut self, scanning: bool, now: Instant) -> Verdict {
        // Sleep grace: a wall gap far beyond the poll cadence means the
        // machine napped — nothing the daemon did, so its clock restarts.
        if let Some(last) = self.last_tick
            && now.saturating_duration_since(last) > self.sleep_gap
        {
            self.stuck_since = None;
        }
        self.last_tick = Some(now);
        match (scanning, self.stuck_since) {
            (false, _) => {
                self.stuck_since = None;
                self.armed = true;
                Verdict::Quiet
            }
            (true, None) => {
                self.stuck_since = Some(now);
                Verdict::Quiet
            }
            (true, Some(since)) => {
                let stuck_for = now.saturating_duration_since(since);
                if self.armed && stuck_for > self.limit {
                    self.stuck_since = None;
                    self.armed = false;
                    Verdict::Restart { stuck_for }
                } else {
                    Verdict::Quiet
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = Duration::from_secs(360);
    const GAP: Duration = Duration::from_secs(120);
    const POLL: Duration = Duration::from_secs(30);

    fn dog() -> DaemonWatchdog {
        DaemonWatchdog::new(LIMIT, GAP)
    }

    /// Clearing a queue chains scans back to back, so `scanning` reads true
    /// for many minutes while the daemon is healthy; each `ScanDone` proves
    /// it (final review finding 1).
    ///
    /// MUTATION: make `scan_done` a no-op → `Restart` around minute six.
    #[test]
    fn a_scan_done_between_scanning_ticks_is_progress() {
        let mut w = dog();
        let t0 = Instant::now();
        for i in 0..60u32 {
            let now = t0 + POLL * i;
            assert_eq!(w.tick(true, now), Verdict::Quiet, "tick {i}");
            // One scan finishes every two polls.
            if i % 2 == 1 {
                w.scan_done(now + Duration::from_secs(5));
            }
        }
    }

    /// A daemon that reports scanning with no sign of life is restarted
    /// once, and not again until it has reported healthy.
    ///
    /// MUTATION: drop `self.armed = false` from the restart arm → a second
    /// `Restart` one limit later.
    #[test]
    fn a_silent_scan_restarts_once_per_epoch_until_healthy() {
        let mut w = dog();
        let t0 = Instant::now();
        let mut restarts = 0;
        for i in 0..40u32 {
            if matches!(w.tick(true, t0 + POLL * i), Verdict::Restart { .. }) {
                restarts += 1;
            }
        }
        assert_eq!(restarts, 1, "one shot per epoch");
        // Healthy once, then stuck again: the watchdog fires again.
        let t1 = t0 + POLL * 40;
        assert_eq!(w.tick(false, t1), Verdict::Quiet);
        let mut again = 0;
        for i in 1..20u32 {
            if matches!(w.tick(true, t1 + POLL * i), Verdict::Restart { .. }) {
                again += 1;
            }
        }
        assert_eq!(again, 1, "re-armed by the healthy tick");
    }

    /// A machine nap between polls is not stuck time.
    ///
    /// MUTATION: drop the sleep-gap reset → the tick after the nap fires.
    #[test]
    fn a_nap_between_ticks_does_not_count() {
        let mut w = dog();
        let t0 = Instant::now();
        assert_eq!(w.tick(true, t0), Verdict::Quiet);
        // The lid closes for an hour; the next poll lands after it.
        let woke = t0 + Duration::from_secs(3600);
        assert_eq!(w.tick(true, woke), Verdict::Quiet);
        // And the clock restarted at the wake: still quiet just under the limit.
        assert_eq!(w.tick(true, woke + POLL), Verdict::Quiet);
        assert_eq!(w.tick(true, woke + LIMIT), Verdict::Quiet);
    }
}
