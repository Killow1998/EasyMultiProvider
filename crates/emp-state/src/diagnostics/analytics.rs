//! Health, model-speed medians and token-weighted cache charts from observations.
use super::schema::number;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
const DAYS: i64 = 7;
const CALLS: usize = 20;
fn decimal(value: f64) -> f64 {
    format!("{value:.1}").parse().unwrap_or(value)
}
fn rate(count: usize, total: usize) -> f64 {
    if total > 0 {
        decimal(count as f64 * 100.0 / total as f64)
    } else {
        0.0
    }
}
fn observed(value: &Value) -> Option<OffsetDateTime> {
    let value = value.as_str()?;
    OffsetDateTime::parse(value, &Rfc3339)
        .or_else(|_| OffsetDateTime::parse(&format!("{value}Z"), &Rfc3339))
        .ok()
}
fn iso(value: OffsetDateTime) -> String {
    let value = value.to_offset(time::UtcOffset::UTC);
    let fraction = if value.microsecond() > 0 {
        format!(".{:06}", value.microsecond())
    } else {
        String::new()
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{fraction}+00:00",
        value.year(),
        u8::from(value.month()),
        value.day(),
        value.hour(),
        value.minute(),
        value.second()
    )
}
fn text<'a>(event: &'a Value, key: &str) -> &'a str {
    event[key].as_str().unwrap_or("")
}
fn mode(event: &Value) -> &str {
    let value = text(event, "speed_mode");
    if value.is_empty() { "unknown" } else { value }
}
fn success(event: &Value) -> bool {
    event["error_class"] == "none"
        && event["status"]
            .as_u64()
            .is_some_and(|status| (200..300).contains(&status))
}
fn metric(records: &[&Value], key: &str) -> (Option<f64>, usize) {
    let mut values = records
        .iter()
        .filter_map(|event| number(&event[key]).filter(|v| *v > 0.0))
        .collect::<Vec<_>>();
    values.sort_by(f64::total_cmp);
    let count = values.len();
    if count == 0 {
        return (None, 0);
    }
    let median = if count % 2 == 0 {
        (values[count / 2 - 1] + values[count / 2]) / 2.0
    } else {
        values[count / 2]
    };
    (Some(decimal(median)), count)
}
fn change(current: Option<f64>, previous: Option<f64>, lower: bool) -> Option<f64> {
    let (current, previous) = (current?, previous?);
    (previous > 0.0).then(|| {
        decimal(
            (if lower {
                previous - current
            } else {
                current - previous
            }) * 100.0
                / previous,
        )
    })
}
fn cache_totals(records: &[&Value]) -> Value {
    let pairs = records
        .iter()
        .filter_map(|event| {
            let total = crate::usage::token_count(event.get("input_tokens"))?;
            let cached = crate::usage::token_count(event.get("cached_input_tokens"))?;
            (total > 0 && cached <= total).then_some((total, cached))
        })
        .collect::<Vec<_>>();
    let total = pairs.iter().map(|pair| pair.0).sum::<u64>();
    let cached = pairs.iter().map(|pair| pair.1).sum::<u64>();
    json!({"call_count":records.len(),"sample_count":pairs.len(),"hit_count":pairs.iter().filter(|pair|pair.1>0).count(),"input_tokens":total,"cached_input_tokens":cached,"rate":if total>0{json!(rate(cached as usize,total as usize))}else{Value::Null}})
}
fn cache(records: &[&Value], now: OffsetDateTime) -> Value {
    type Group<'a> = (
        (String, String, String, String),
        Vec<(OffsetDateTime, &'a Value)>,
    );
    let mut groups: Vec<Group<'_>> = Vec::new();
    for event in records {
        let model = text(event, "model_id");
        let Some(stamp) = observed(&event["observed_at"]) else {
            continue;
        };
        if event["route"] != "responses"
            || model.is_empty()
            || model.starts_with("codex-auto-")
            || stamp < now - time::Duration::days(DAYS)
            || stamp > now
        {
            continue;
        }
        let speed = mode(event);
        let speed = if ["fast", "standard"].contains(&speed) {
            speed
        } else {
            "unknown"
        };
        let key = (
            model.to_owned(),
            text(event, "provider_id").to_owned(),
            speed.to_owned(),
            text(event, "endpoint_fingerprint").to_owned(),
        );
        if let Some((_, rows)) = groups.iter_mut().find(|(existing, _)| *existing == key) {
            rows.push((stamp, event));
        } else {
            groups.push((key, vec![(stamp, event)]));
        }
    }
    groups.sort_by_key(|(_, rows)| std::cmp::Reverse(rows.iter().map(|row| row.0).max()));
    let mut models = Vec::new();
    for ((model, provider, speed, endpoint), rows) in groups.into_iter().take(5) {
        let mut buckets: BTreeMap<i64, Vec<&Value>> = BTreeMap::new();
        for (stamp, event) in &rows {
            buckets
                .entry(stamp.unix_timestamp().div_euclid(600) * 600)
                .or_default()
                .push(event);
        }
        let periods = buckets
            .into_iter()
            .rev()
            .map(|(start, events)| {
                let mut result = cache_totals(&events);
                result["start"] = json!(start);
                result["end"] = json!(start + 600);
                result["complete"] = json!(start + 600 <= now.unix_timestamp());
                result
            })
            .collect::<Vec<_>>();
        let mut result = cache_totals(&rows.iter().map(|row| row.1).collect::<Vec<_>>());
        result["model_id"] = json!(model);
        result["provider_id"] = json!(provider);
        result["speed_mode"] = json!(speed);
        result["endpoint_fingerprint"] = json!(endpoint);
        result["first_seen"] = json!(iso(rows.iter().map(|row| row.0).min().unwrap()));
        result["last_seen"] = json!(iso(rows.iter().map(|row| row.0).max().unwrap()));
        result["periods"] = json!(periods);
        models.push(result);
    }
    json!({"bucket_minutes":10,"days":DAYS,"models":models})
}
pub fn summarize(records: &[Value], now: OffsetDateTime) -> Value {
    let attempts = records
        .iter()
        .filter(|event| event["route"] == "responses")
        .collect::<Vec<_>>();
    let relevant = attempts
        .iter()
        .copied()
        .filter(|event| event["recovery_mode"] != "native_http_fallback")
        .collect::<Vec<_>>();
    let health = relevant
        .iter()
        .copied()
        .filter(|event| {
            ![
                "client_disconnect",
                "client_cancelled",
                "client_websocket_close",
            ]
            .contains(&text(event, "error_class"))
        })
        .collect::<Vec<_>>();
    let total = health.len();
    let successes = health.iter().filter(|event| success(event)).count();
    let mut failures: Vec<(String, usize)> = Vec::new();
    for event in health.iter().filter(|event| !success(event)) {
        let kind = text(event, "error_class");
        let kind = if kind.is_empty() { "unknown" } else { kind };
        if let Some((_, count)) = failures.iter_mut().find(|(existing, _)| existing == kind) {
            *count += 1;
        } else {
            failures.push((kind.to_owned(), 1));
        }
    }
    failures.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    let mut summary = json!({"sample_count":total,"success_count":successes,"failure_count":total-successes,"success_rate":rate(successes,total),"cancelled_count":relevant.len()-total,"fallback_attempt_count":attempts.len()-relevant.len(),"failure_classes":failures.into_iter().take(6).map(|(kind,count)|json!({"error_class":kind,"count":count,"rate":rate(count,total)})).collect::<Vec<_>>()});
    for status in [429, 502, 503, 504] {
        let count = health
            .iter()
            .filter(|event| event["status"] == status)
            .count();
        summary[format!("status_{status}_count")] = json!(count);
        summary[format!("status_{status}_rate")] = json!(rate(count, total));
    }
    let capacity = health
        .iter()
        .filter(|event| event["error_class"] == "upstream_capacity")
        .count();
    summary["local_capacity_count"] = json!(capacity);
    summary["local_capacity_rate"] = json!(rate(capacity, total));
    let performance = relevant
        .iter()
        .copied()
        .filter(|event| {
            let schema = event["performance_schema"].as_u64();
            matches!(schema, Some(2 | 3))
                && success(event)
                && (number(&event["ttft_ms"]).unwrap_or(0.0) > 0.0
                    || (schema == Some(3)
                        && number(&event["tokens_per_second"]).unwrap_or(0.0) > 0.0))
                && observed(&event["observed_at"]).unwrap_or(now)
                    >= now - time::Duration::days(DAYS)
        })
        .collect::<Vec<_>>();
    let mut keys = Vec::new();
    for event in performance.iter().rev() {
        let model = text(event, "model_id");
        let speed = mode(event);
        if model.is_empty()
            || model.starts_with("codex-auto-")
            || !["standard", "fast", "unknown"].contains(&speed)
        {
            continue;
        }
        let key = (model, speed);
        if !keys.contains(&key) {
            keys.push(key);
        }
        if keys.len() >= 5 {
            break;
        }
    }
    let mut models = Vec::new();
    for (model, speed) in keys {
        let history = performance
            .iter()
            .copied()
            .filter(|event| text(event, "model_id") == model && mode(event) == speed)
            .collect::<Vec<_>>();
        let last = history.len().saturating_sub(CALLS);
        let calls = &history[last..];
        let previous = &history[history.len().saturating_sub(CALLS * 2)..last];
        let (ttft, ttft_n) = metric(calls, "ttft_ms");
        let current_tps = calls
            .iter()
            .copied()
            .filter(|event| event["performance_schema"] == 3)
            .collect::<Vec<_>>();
        let (tps, tps_n) = metric(&current_tps, "tokens_per_second");
        let (old_ttft, old_ttft_n) = metric(previous, "ttft_ms");
        let previous_tps = previous
            .iter()
            .copied()
            .filter(|event| event["performance_schema"] == 3)
            .collect::<Vec<_>>();
        let (old_tps, old_tps_n) = metric(&previous_tps, "tokens_per_second");
        if ttft_n == 0 && tps_n == 0 {
            continue;
        }
        models.push(json!({"model_id":model,"speed_mode":speed,"call_count":calls.len(),"retained_call_count":history.len(),"ttft_ms":ttft,"ttft_samples":ttft_n,"previous_ttft_ms":old_ttft,"previous_ttft_samples":old_ttft_n,"ttft_change_percent":if ttft_n.min(old_ttft_n)>=3{change(ttft,old_ttft,true)}else{None},"tokens_per_second":tps,"tps_samples":tps_n,"previous_tokens_per_second":old_tps,"previous_tps_samples":old_tps_n,"tps_change_percent":if tps_n.min(old_tps_n)>=3{change(tps,old_tps,false)}else{None},"last_seen":calls.last().map(|event|&event["observed_at"])}));
    }
    json!({"health":summary,"performance_window":{"calls":CALLS,"days":DAYS},"models":models,"cache":cache(&relevant,now)})
}

#[cfg(test)]
mod tests {
    use super::summarize;
    use serde_json::{Value, json};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    fn record(schema: u64, observed_at: &str, ttft_ms: Value, tokens_per_second: Value) -> Value {
        json!({
            "route":"responses",
            "model_id":"gpt-6-luna",
            "speed_mode":"standard",
            "status":200,
            "error_class":"none",
            "performance_schema":schema,
            "observed_at":observed_at,
            "ttft_ms":ttft_ms,
            "tokens_per_second":tokens_per_second,
        })
    }

    #[test]
    fn schema_two_keeps_ttft_without_mixing_legacy_tps() {
        let now = OffsetDateTime::parse("2026-09-23T12:00:00Z", &Rfc3339).unwrap();
        let stamp = now.format(&Rfc3339).unwrap();
        let result = summarize(
            &[
                record(2, &stamp, json!(100), json!(999.0)),
                record(2, &stamp, Value::Null, json!(888.0)),
                record(3, &stamp, json!(200), json!(60.0)),
            ],
            now,
        );
        let model = &result["models"][0];
        assert_eq!(model["call_count"], 2);
        assert_eq!(model["ttft_ms"], 150.0);
        assert_eq!(model["ttft_samples"], 2);
        assert_eq!(model["tokens_per_second"], 60.0);
        assert_eq!(model["tps_samples"], 1);
    }
}
