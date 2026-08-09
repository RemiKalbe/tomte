//! Opt-in micro-instrumentation (TOMTE_PERF=1): per-tag timing aggregates
//! printed once a second. Zero overhead when the env var is unset.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

thread_local! {
    static STATS: RefCell<HashMap<&'static str, Agg>> = RefCell::new(HashMap::new());
}

struct Agg {
    n: u64,
    sum: Duration,
    max: Duration,
    since: Instant,
}

pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TOMTE_PERF").is_some())
}

/// Time `f` under `tag`; aggregates print to stderr once per second.
pub fn time<T>(tag: &'static str, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let t0 = Instant::now();
    let out = f();
    let dt = t0.elapsed();
    STATS.with(|stats| {
        let mut stats = stats.borrow_mut();
        let agg = stats.entry(tag).or_insert(Agg {
            n: 0,
            sum: Duration::ZERO,
            max: Duration::ZERO,
            since: Instant::now(),
        });
        agg.n += 1;
        agg.sum += dt;
        agg.max = agg.max.max(dt);
        if agg.since.elapsed() >= Duration::from_secs(1) {
            eprintln!(
                "perf[{tag}] n={}/s avg={}µs max={}µs",
                agg.n,
                (agg.sum.as_micros() as u64) / agg.n.max(1) as u64,
                agg.max.as_micros(),
            );
            agg.n = 0;
            agg.sum = Duration::ZERO;
            agg.max = Duration::ZERO;
            agg.since = Instant::now();
        }
    });
    out
}
