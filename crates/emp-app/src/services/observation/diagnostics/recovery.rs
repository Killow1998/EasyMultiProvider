//! Correlate recovery metadata in the bounded ring without asserting request-content equality.
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub(super) fn previous_rejection<'a>(
    recent: &'a VecDeque<Value>,
    current: &Value,
) -> Option<&'a str> {
    if current["route"] != "responses"
        || current["has_previous_response"] != false
        || current["request_id"].as_str().is_none()
        || ["thread_ref", "session_ref", "turn_ref"]
            .iter()
            .any(|key| current[key].as_str().is_none())
    {
        return None;
    }
    let prior = recent.iter().rev().find(|prior| {
        [
            "thread_ref",
            "session_ref",
            "turn_ref",
            "provider_id",
            "model_id",
        ]
        .iter()
        .all(|key| prior[key] == current[key])
    })?;
    if prior["error_code"] != "previous_response_not_found"
        || prior["has_previous_response"] != true
        || prior["request_id"] == current["request_id"]
    {
        return None;
    }
    let stamp =
        |record: &Value| OffsetDateTime::parse(record["observed_at"].as_str()?, &Rfc3339).ok();
    let elapsed = (stamp(current)? - stamp(prior)?).whole_milliseconds();
    (0..=60_000)
        .contains(&elapsed)
        .then(|| prior["request_id"].as_str())
        .flatten()
}

pub(super) fn summary(records: &[Value]) -> Value {
    let mut recoveries = BTreeMap::new();
    for record in records {
        if record["continuation_recovery_from"].as_str().is_some()
            && let Some(request) = record["request_id"].as_str()
        {
            let bytes = record["request_bytes"].as_u64();
            let current = recoveries.entry(request).or_insert(bytes);
            *current = (*current).max(bytes);
        }
    }
    let measured = recoveries
        .values()
        .filter_map(|bytes| *bytes)
        .collect::<Vec<_>>();
    json!({"count":recoveries.len(), "measured_count":measured.len(),
        "request_json_bytes":if measured.is_empty(){None}else{Some(measured.iter().sum::<u64>())},
        "byte_scope":"normalized_request_json",
        "scope":"bounded_route_observations", "correlation_window_ms":60000})
}

#[cfg(test)]
mod tests;
