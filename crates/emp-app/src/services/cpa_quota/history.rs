//! CPA windows share the existing quota store and its retention and point limits.
use super::{Quota, REFRESH_INTERVAL_SECONDS};
use emp_codex::quota_history::{QuotaHistoryError, QuotaHistoryStore};
use serde_json::{Value, json};

pub(super) fn record(
    store: &QuotaHistoryStore,
    identity: &str,
    quota: &Quota,
    observed_at: i64,
) -> Result<usize, QuotaHistoryError> {
    let mut limits = quota
        .rate_limits_by_limit_id
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    for account in quota
        .accounts
        .iter()
        .filter(|account| account.enabled && !account.stale)
    {
        let mut windows = serde_json::Map::new();
        for (name, kind, minutes) in [
            ("five_hour", "primary", 300),
            ("seven_day", "secondary", 10080),
        ] {
            if let Some(window) = account.windows.get(name) {
                windows.insert(
                    kind.into(),
                    json!({
                        "used_percent": window.remaining_percent.map(|value| 100.0 - value),
                        "window_minutes": minutes, "resets_at": window.reset_timestamp,
                    }),
                );
            }
        }
        limits.insert(format!("account:{}", account.id), Value::Object(windows));
    }
    store.append_snapshot(
        &format!("cpa:{identity}"),
        &json!({"rate_limits_by_limit_id":limits}),
        observed_at,
    )
}

pub(super) fn query(
    store: &QuotaHistoryStore,
    identity: &str,
    start: i64,
    end: i64,
) -> Result<Value, &'static str> {
    let mut result = store
        .query_period(&format!("cpa:{identity}"), start, end)
        .map_err(|_| "cpa_quota_history_unavailable")?;
    result["scope"] = json!("shared_pool");
    result["sample_interval_seconds"] = json!(REFRESH_INTERVAL_SECONDS);
    result["interval_seconds"] = json!(
        result["interval_seconds"]
            .as_i64()
            .expect("history interval")
            .max(REFRESH_INTERVAL_SECONDS)
    );
    Ok(result)
}
