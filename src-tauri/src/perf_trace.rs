//! Opt-in timing diagnostics for release-bundle performance reproduction.
//! Nothing is emitted unless `LOCALREVIEW_PERF_TRACE=1` is present at process
//! startup. Records contain operation names and aggregate sizes only.

use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("LOCALREVIEW_PERF_TRACE").is_some())
}

pub(crate) fn record(
    operation: &'static str,
    started: Instant,
    outcome: &'static str,
    item_count: Option<usize>,
) {
    if !enabled() {
        return;
    }
    record_duration(operation, started.elapsed(), outcome, item_count);
}

pub(crate) fn record_duration(
    operation: &'static str,
    elapsed: Duration,
    outcome: &'static str,
    item_count: Option<usize>,
) {
    if !enabled() {
        return;
    }
    let record = serde_json::json!({
        "schemaVersion": 1,
        "kind": "native_command",
        "operation": operation,
        "outcome": outcome,
        "durationMicros": elapsed.as_micros(),
        "itemCount": item_count,
    });
    eprintln!("{record}");
}
