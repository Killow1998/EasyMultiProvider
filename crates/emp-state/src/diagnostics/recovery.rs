//! Correlate observed full-history recovery within the existing bounded ring.
//! These are metadata correlations, not assertions that request contents match.
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub(super) fn previous_rejection<'a>(
    recent: &'a VecDeque<Value>,
    current: &Value,
) -> Option<&'a str> {
    if current["dialect"] != "codex_native"
        || current["route"] != "responses"
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
mod tests {
    use super::*;
    #[test]
    fn recovery_requires_matching_scope_recent_rejection_and_deduplicates_bytes() {
        let prior = json!({"dialect":"codex_native","route":"responses","thread_ref":"t","session_ref":"s","turn_ref":"u","provider_id":"p","model_id":"m","has_previous_response":true,"error_code":"previous_response_not_found","request_id":"aaaaaaaaaaaaaaaa","observed_at":"2026-10-12T00:00:00Z"});
        let recent = VecDeque::from([prior.clone()]);
        let mut current = prior.clone();
        current["has_previous_response"] = json!(false);
        current["request_id"] = json!("bbbbbbbbbbbbbbbb");
        current["observed_at"] = json!("2026-10-12T00:00:05Z");
        current["error_code"] = json!("");
        current["request_bytes"] = json!(10000);
        assert_eq!(
            previous_rejection(&recent, &current),
            Some("aaaaaaaaaaaaaaaa")
        );
        for key in [
            "thread_ref",
            "session_ref",
            "turn_ref",
            "provider_id",
            "model_id",
        ] {
            let mut other = current.clone();
            other[key] = json!("different");
            assert!(previous_rejection(&recent, &other).is_none());
        }
        let mut late = current.clone();
        late["observed_at"] = json!("2026-10-12T00:01:01Z");
        assert!(previous_rejection(&recent, &late).is_none());
        current["continuation_recovery_from"] = prior["request_id"].clone();
        let stats = summary(&[current.clone(), current]);
        assert_eq!(stats["count"], 1);
        assert_eq!(stats["request_json_bytes"], 10000);
    }
}
