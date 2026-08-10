//! File logging: timestamped, size-rotated, teed to stderr — built after a
//! remote machine failed with "there are no logs to give you" (2026-08-10).
//! One file per component under `<support dir>/logs/`; every subprocess,
//! IPC failure, pipeline step, and panic lands here.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const MAX_BYTES: u64 = 5 * 1024 * 1024;

struct Sink {
    path: PathBuf,
    file: File,
    written: u64,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// Install the process-wide log file (`<dir>/<component>.log`) and a panic
/// hook that records panics before the process dies. Idempotent-ish: first
/// call wins. Failures are swallowed — logging must never break the app.
pub fn init(dir: PathBuf, component: &str) {
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{component}.log"));
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = SINK.set(Mutex::new(Sink {
        path,
        file,
        written,
    }));
    log(
        "INFO",
        "boot",
        &format!("--- {component} started (pid {}) ---", std::process::id()),
    );
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log("PANIC", "panic", &info.to_string());
        previous(info);
    }));
}

/// Append one line: `2026-08-10T19:31:02Z LEVEL [target] message`. Also
/// mirrored to stderr so terminal launches and spawn-log capture see it.
pub fn log(level: &str, target: &str, message: &str) {
    let ts = iso8601_utc(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    let line = format!("{ts} {level:<5} [{target}] {message}\n");
    eprint!("{line}");
    let Some(sink) = SINK.get() else { return };
    let Ok(mut sink) = sink.lock() else { return };
    if sink.written > MAX_BYTES {
        // Rotate: current → .1 (previous .1 dies), fresh file continues.
        let rotated = sink.path.with_extension("log.1");
        let _ = std::fs::rename(&sink.path, rotated);
        if let Ok(fresh) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&sink.path)
        {
            sink.file = fresh;
            sink.written = 0;
        }
    }
    if sink.file.write_all(line.as_bytes()).is_ok() {
        sink.written += line.len() as u64;
    }
}

/// The logs directory for a given support dir.
pub fn dir_for(support: &std::path::Path) -> PathBuf {
    support.join("logs")
}

#[macro_export]
macro_rules! log_info {
    ($target:expr, $($arg:tt)*) => {
        $crate::logging::log("INFO", $target, &format!($($arg)*))
    };
}
#[macro_export]
macro_rules! log_warn {
    ($target:expr, $($arg:tt)*) => {
        $crate::logging::log("WARN", $target, &format!($($arg)*))
    };
}
#[macro_export]
macro_rules! log_error {
    ($target:expr, $($arg:tt)*) => {
        $crate::logging::log("ERROR", $target, &format!($($arg)*))
    };
}

/// Epoch seconds → `YYYY-MM-DDTHH:MM:SSZ` without a date dependency.
pub fn iso8601_utc(epoch: u64) -> String {
    let days = epoch / 86_400;
    let (h, m, s) = ((epoch % 86_400) / 3600, (epoch % 3600) / 60, epoch % 60);
    // Civil-from-days (Howard Hinnant's algorithm), valid for our era.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::iso8601_utc;

    #[test]
    fn timestamps_are_correct_utc() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(951_782_400), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(iso8601_utc(1_754_784_000), "2025-08-10T00:00:00Z");
        assert_eq!(iso8601_utc(1_786_320_000), "2026-08-10T00:00:00Z");
    }
}
