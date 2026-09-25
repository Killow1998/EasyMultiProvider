//! Stable quota display projection.

use super::*;

pub(super) fn read_limits(
    payload: &Map<String, Value>,
    rate_limits: &mut Option<Map<String, Value>>,
    rate_limits_result: &mut Map<String, Value>,
    buckets: &mut Map<String, Value>,
) {
    if let Some(by_id) = payload
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
    {
        let current = by_id
            .iter()
            .filter(|(limit_id, value)| !limit_id.is_empty() && value.is_object())
            .map(|(limit_id, value)| (limit_id.clone(), value.clone()))
            .collect::<Map<_, _>>();
        if !current.is_empty() {
            *buckets = current;
            *rate_limits = buckets
                .get("codex")
                .or_else(|| buckets.values().next())
                .and_then(Value::as_object)
                .cloned();
            *rate_limits_result = payload.clone();
            return;
        }
    }
    if let Some(candidate) = payload.get("rateLimits").and_then(Value::as_object) {
        let limit_id = candidate
            .get("limitId")
            .or_else(|| candidate.get("limit_id"))
            .and_then(Value::as_str)
            .unwrap_or("codex")
            .to_owned();
        buckets.insert(limit_id, Value::Object(candidate.clone()));
        *rate_limits = buckets
            .get("codex")
            .and_then(Value::as_object)
            .cloned()
            .or_else(|| Some(candidate.clone()));
        *rate_limits_result = payload.clone();
    }
}

pub(super) fn mask_email(value: Option<&Value>) -> String {
    let Some(value) = value.and_then(Value::as_str) else {
        return String::new();
    };
    let Some((local, domain)) = value.split_once('@') else {
        return String::new();
    };
    let first = local
        .chars()
        .next()
        .map_or("*".to_owned(), |value| value.to_string());
    format!("{first}***@{domain}")
}

pub(super) fn safe_credit_snapshot(
    rate_limits: &Map<String, Value>,
    result: &Map<String, Value>,
) -> Value {
    let mut snapshot = Map::new();
    if let Some(credits) = rate_limits.get("credits").and_then(Value::as_object) {
        copy_renamed_scalar(credits, &mut snapshot, "hasCredits", "has_credits");
        copy_renamed_scalar(credits, &mut snapshot, "unlimited", "unlimited");
        copy_renamed_scalar(credits, &mut snapshot, "balance", "balance");
    }
    if let Some(limit) = rate_limits
        .get("individualLimit")
        .and_then(Value::as_object)
    {
        let mut safe = Map::new();
        for (source, target) in [
            ("limit", "limit"),
            ("used", "used"),
            ("remainingPercent", "remaining_percent"),
            ("resetsAt", "resets_at"),
        ] {
            if let Some(value) = limit.get(source).filter(|value| safe_scalar(value))
                && !value.is_null()
            {
                safe.insert(target.to_owned(), value.clone());
            }
        }
        if !safe.is_empty() {
            snapshot.insert("individual_limit".to_owned(), Value::Object(safe));
        }
    }
    if let Some(value) = rate_limits
        .get("spendControlReached")
        .filter(|value| value.is_boolean())
    {
        snapshot.insert("spend_control_reached".to_owned(), value.clone());
    }
    if let Some(value) = safe_reset_credits(result.get("rateLimitResetCredits")) {
        snapshot.insert("reset_credits".to_owned(), value);
    }
    if snapshot.is_empty() {
        Value::Null
    } else {
        Value::Object(snapshot)
    }
}

pub(super) fn safe_reset_credits(value: Option<&Value>) -> Option<Value> {
    let value = value?.as_object()?;
    let mut snapshot = Map::new();
    if let Some(count) = value
        .get("availableCount")
        .filter(|count| count.as_i64().is_some() || count.as_u64().is_some())
    {
        snapshot.insert("available_count".to_owned(), count.clone());
    }
    if let Some(credits) = value.get("credits") {
        if credits.is_null() {
            snapshot.insert("credits".to_owned(), Value::Null);
        } else if let Some(credits) = credits.as_array() {
            let projected = credits
                .iter()
                .filter_map(Value::as_object)
                .map(|credit| {
                    let mut safe = Map::new();
                    if let Some(Value::String(id)) = credit.get("id")
                        && !id.trim().is_empty()
                        && id.len() <= 256
                    {
                        safe.insert("id".to_owned(), Value::String(id.clone()));
                    }
                    for (source, target) in [
                        ("resetType", "reset_type"),
                        ("status", "status"),
                        ("grantedAt", "granted_at"),
                        ("expiresAt", "expires_at"),
                        ("title", "title"),
                        ("description", "description"),
                    ] {
                        if let Some(value) = credit.get(source).filter(|value| safe_scalar(value)) {
                            safe.insert(target.to_owned(), value.clone());
                        }
                    }
                    Value::Object(safe)
                })
                .collect();
            snapshot.insert("credits".to_owned(), Value::Array(projected));
        }
    }
    (!snapshot.is_empty()).then_some(Value::Object(snapshot))
}

fn copy_renamed_scalar(
    source: &Map<String, Value>,
    target: &mut Map<String, Value>,
    source_name: &str,
    target_name: &str,
) {
    if let Some(value) = source.get(source_name).filter(|value| safe_scalar(value)) {
        target.insert(target_name.to_owned(), value.clone());
    }
}

fn safe_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}
