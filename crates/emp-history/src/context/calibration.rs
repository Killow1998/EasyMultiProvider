//! Reuse persisted, identity-bound observations when reporting and budgeting context.
use super::{SAFETY_RESERVE_TOKENS, context_window, positive_integer};
use emp_core::capability_view::safe_id;
use emp_core::{deployment_identity, endpoint_fingerprint};
use serde_json::{Map, Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub(super) struct Limits {
    pub context: Option<u64>,
    pub input: Option<u64>,
    pub source: String,
    pub confidence: f64,
    pub calibration: Option<Value>,
}
fn identity(provider: &Map<String, Value>, model: &Map<String, Value>, protocol: &str) -> Value {
    let upstream = model
        .get("upstream_id")
        .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        .or_else(|| model.get("id"));
    json!({"endpoint_fingerprint":endpoint_fingerprint(provider.get("base_url").and_then(Value::as_str)),
        "upstream_model":safe_id(upstream,"unknown"),
        "protocol":if matches!(protocol,"responses"|"chat_completions"|"anthropic_messages"){protocol}else{"unknown"},
        "deployment_identity":deployment_identity(provider,model)})
}
fn fresh(value: Option<&Value>) -> bool {
    let Some(value) = value.and_then(Value::as_str) else {
        return false;
    };
    let utc = if value.len() == 10 {
        format!("{value}T00:00:00Z")
    } else {
        format!("{value}Z")
    };
    let Some(stamp) = OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .or_else(|| OffsetDateTime::parse(&utc, &Rfc3339).ok())
    else {
        return false;
    };
    let age = OffsetDateTime::now_utc() - stamp;
    !age.is_negative() && age.whole_seconds() <= 24 * 60 * 60
}
pub(super) fn limits(
    provider: &Map<String, Value>,
    model: &Map<String, Value>,
    protocol: &str,
    reserves: Option<u64>,
) -> Limits {
    let (context, mut source, mut confidence) = context_window(provider, model);
    let identity = identity(provider, model, protocol);
    let calibration = [model, provider]
        .into_iter()
        .flat_map(|source| {
            source
                .get("context_calibrations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .find(|item| {
            matches!(
                item["protocol"].as_str(),
                Some("responses" | "chat_completions" | "anthropic_messages")
            ) && identity
                .as_object()
                .unwrap()
                .iter()
                .all(|(key, value)| item.get(key) == Some(value))
        })
        .cloned();
    let failure = calibration
        .as_ref()
        .filter(|value| fresh(value.get("smallest_failure_observed_at")))
        .and_then(|value| positive_integer(value.get("smallest_failure_estimate")))
        .map(|value| value - 1);
    let base = context
        .zip(reserves)
        .map(|(limit, reserve)| limit.saturating_sub(reserve));
    let input = if failure.is_some_and(|failure| base.is_none_or(|base| failure <= base)) {
        source = "observed".to_owned();
        confidence = 1.0;
        failure
    } else {
        base
    };
    Limits {
        context,
        input,
        source,
        confidence,
        calibration,
    }
}
pub fn status(provider: &Map<String, Value>, model: &Map<String, Value>, protocol: &str) -> Value {
    let mut result = identity(provider, model, protocol);
    let output = positive_integer(model.get("output_limit"));
    let reserves = output.map(|value| value.saturating_add(SAFETY_RESERVE_TOKENS));
    let limits = limits(provider, model, protocol, reserves);
    let fields = json!({"context_limit":limits.context,"safe_input_limit":limits.input,"output_reserve":output,
        "safety_reserve":SAFETY_RESERVE_TOKENS,"reserves":reserves,"confidence":limits.confidence,"source":limits.source,
        "largest_success_estimate":limits.calibration.as_ref().and_then(|value|positive_integer(value.get("largest_success_estimate"))),
        "smallest_failure_estimate":limits.calibration.as_ref().and_then(|value|positive_integer(value.get("smallest_failure_estimate")))});
    result
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    result
}

/// Retain only numeric evidence, bound to the actual upstream deployment.
pub fn update(
    provider: &Map<String, Value>,
    model: &mut Map<String, Value>,
    protocol: &str,
    estimate: u64,
    success: bool,
    observed_at: &str,
) -> bool {
    if estimate == 0
        || !matches!(
            protocol,
            "responses" | "chat_completions" | "anthropic_messages"
        )
    {
        return false;
    }
    let identity = identity(provider, model, protocol);
    let mut entries = model
        .get("context_calibrations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let position = entries.iter().position(|entry| {
        identity
            .as_object()
            .unwrap()
            .iter()
            .all(|(key, value)| entry.get(key) == Some(value))
    });
    let mut current = position
        .map(|index| entries[index].clone())
        .unwrap_or_else(|| {
            let mut value = identity.clone();
            for side in ["largest_success", "smallest_failure"] {
                value[format!("{side}_estimate")] = Value::Null;
                value[format!("{side}_source")] = json!("unknown");
                value[format!("{side}_confidence")] = json!(0.0);
                value[format!("{side}_observed_at")] = Value::Null;
            }
            value
        });
    let mut changed = false;
    let field = if success {
        "largest_success"
    } else {
        "smallest_failure"
    };
    let old = positive_integer(current.get(format!("{field}_estimate")));
    if old.is_none_or(|old| {
        if success {
            estimate > old
        } else {
            !fresh(current.get("smallest_failure_observed_at")) || estimate < old
        }
    }) {
        current[format!("{field}_estimate")] = json!(estimate);
        current[format!("{field}_source")] = json!("observed");
        current[format!("{field}_confidence")] = json!(1.0);
        current[format!("{field}_observed_at")] = json!(observed_at);
        changed = true;
    }
    if success
        && positive_integer(current.get("smallest_failure_estimate"))
            .is_some_and(|failure| estimate >= failure)
    {
        current["smallest_failure_estimate"] = Value::Null;
        current["smallest_failure_source"] = json!("unknown");
        current["smallest_failure_confidence"] = json!(0.0);
        current["smallest_failure_observed_at"] = Value::Null;
        changed = true;
    }
    if !changed {
        return false;
    }
    match position {
        Some(index) => entries[index] = current.clone(),
        None => entries.push(current.clone()),
    }
    if entries.len() > 8 {
        entries.drain(..entries.len() - 8);
        if !entries.contains(&current) {
            let last = entries.len() - 1;
            entries[last] = current;
        }
    }
    model.insert("context_calibrations".into(), json!(entries));
    true
}
