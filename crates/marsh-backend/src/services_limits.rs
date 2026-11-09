//! Settings of what a checkpoint may spend on services (`config.json`, read at
//! every use; a malformed value is the default): the time budget of the
//! services capture (P2) and the bounds of the folder local data it saves (C5).
//! Documented in `docs/design/services.md`.

use marsh_daemon::ignored::{DEFAULT_MAX_BYTES, DEFAULT_MAX_FILES, Limits};
use serde_json::Value;
use std::{path::Path, time::Duration};

pub(crate) const DEFAULT_CAPTURE_BUDGET_MS: u64 = 2_000;
const BUDGET_RANGE_MS: std::ops::RangeInclusive<u64> = 100..=60_000;

fn document(home: &Path) -> Value {
    std::fs::read(home.join("config.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

fn budget_of(document: &Value) -> Duration {
    let ms = document
        .get("services_capture_budget_ms")
        .and_then(Value::as_u64)
        .filter(|ms| BUDGET_RANGE_MS.contains(ms))
        .unwrap_or(DEFAULT_CAPTURE_BUDGET_MS);
    Duration::from_millis(ms)
}

fn limits_of(document: &Value) -> Limits {
    let max_files = document
        .get("services_ignored_max_files")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_FILES);
    let max_bytes = match document.get("services_ignored_max_bytes") {
        Some(Value::Number(bytes)) => bytes.as_u64(),
        Some(Value::String(size)) => marsh_sbx::services_engine::parse_size(size).ok(),
        _ => None,
    }
    .unwrap_or(DEFAULT_MAX_BYTES);
    Limits {
        max_files,
        max_bytes,
    }
}

/// `services_capture_budget_ms` (100 to 60000; default 2000).
pub(crate) fn capture_budget(home: &Path) -> Duration {
    budget_of(&document(home))
}

/// `services_ignored_max_files` (default 20000) and
/// `services_ignored_max_bytes` (a byte count or a size such as `2g`;
/// default 2 GiB).
pub(crate) fn ignored_limits(home: &Path) -> Limits {
    limits_of(&document(home))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Setting values and what a bad one becomes (table).
    #[test]
    fn settings_read_from_config_json_fall_back_to_the_documented_defaults() {
        for (config, budget_ms, files, bytes) in [
            (json!({}), 2_000, 20_000, 2 << 30),
            (json!(null), 2_000, 20_000, 2 << 30),
            (
                json!({"services_capture_budget_ms": 1500, "services_ignored_max_files": 5, "services_ignored_max_bytes": "1m"}),
                1_500,
                5,
                1 << 20,
            ),
            (
                json!({"services_ignored_max_bytes": 4096}),
                2_000,
                20_000,
                4096,
            ),
            (
                json!({"services_capture_budget_ms": 5, "services_ignored_max_bytes": "lots", "services_ignored_max_files": "x"}),
                2_000,
                20_000,
                2 << 30,
            ),
            (
                json!({"services_capture_budget_ms": 90_000}),
                2_000,
                20_000,
                2 << 30,
            ),
        ] {
            assert_eq!(
                budget_of(&config),
                Duration::from_millis(budget_ms),
                "{config}"
            );
            let limits = limits_of(&config);
            assert_eq!(
                (limits.max_files, limits.max_bytes),
                (files, bytes),
                "{config}"
            );
        }
    }
}
