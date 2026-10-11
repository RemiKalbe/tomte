//! The daemon's scan driver (spec 2026-10-11 §2.2): the one place a full
//! scan is started in the binary, so every caller runs it with the core
//! UNLOCKED. Phase 1 and 3 take the lock for microseconds and about a
//! second; phase 2 — minutes on a large tree — runs on the caller's thread
//! with nothing held, and every IPC request is served meanwhile.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use tomte_core::scanner::ScanError;

use crate::core::{DaemonCore, DaemonError};

/// Begin → scan (lock released) → apply, and again while a rescan was asked
/// for meanwhile. `Ok(None)`: a scan was already running on another thread;
/// it has been told to go again and its `ScanDone` is the one to wait for.
/// `Ok(Some(n))`: the live drifted count after the last apply.
pub fn run_scan(
    core: &Arc<Mutex<DaemonCore>>,
    now_fn: fn() -> u64,
) -> Result<Option<u32>, DaemonError> {
    let mut last = None;
    loop {
        let begun = core.lock().map_err(|_| poisoned())?.begin_scan(now_fn());
        let Some((scanner, ticket)) = begun else {
            return Ok(last);
        };
        // No lock held here — this line is the whole point of the module.
        // A panic inside the scan must still reach `apply_scan`, or the
        // in-progress flag would outlive its thread.
        let report = match catch_unwind(AssertUnwindSafe(|| scanner.scan())) {
            Ok(report) => report,
            Err(_) => Err(ScanError::Io(std::io::Error::other("the scan panicked"))),
        };
        let outcome = core
            .lock()
            .map_err(|_| poisoned())?
            .apply_scan(ticket, report, now_fn())?;
        last = Some(outcome.drifted);
        if !outcome.rescan_wanted {
            return Ok(last);
        }
        tomte_core::log_info!("scan", "a rescan was requested meanwhile — going again");
    }
}

fn poisoned() -> DaemonError {
    DaemonError::Io(std::io::Error::other("daemon state poisoned"))
}
