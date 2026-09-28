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
/// Confidence of a failure seen once, far below the known window; it is kept
/// as evidence but only lowers limits once a second failure corroborates it.
const UNCONFIRMED_FAILURE_CONFIDENCE: f64 = 0.5;
const FAILURE_CORROBORATION_PERCENT: u64 = 5;
const MIN_FAILURE_CORROBORATION_TOKENS: u64 = 256;

fn confirmed(calibration: &Value) -> bool {
    calibration
        .get("smallest_failure_confidence")
        .and_then(Value::as_f64)
        .is_none_or(|confidence| confidence >= 1.0)
}

fn corroborates_failure(previous: u64, current: u64) -> bool {
    previous.abs_diff(current)
        <= (previous.max(current) / FAILURE_CORROBORATION_PERCENT)
            .max(MIN_FAILURE_CORROBORATION_TOKENS)
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
    // A failure estimate is an input estimate (Python contract), so
    // `failure - 1` is already an input ceiling; reserves are not subtracted
    // again. Calibration data is shared with Python through the config file.
    let failure = calibration
        .as_ref()
        .filter(|value| fresh(value.get("smallest_failure_observed_at")) && confirmed(value))
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
///
/// `estimate` is the input estimate and is what gets stored, matching the
/// Python calibration contract. `output_reserve` is the request's output
/// budget: a failure whose input plus output exceeds the known window says
/// nothing about the input ceiling and is ignored, and one far below the
/// window is only applied after a nearby explicit failure corroborates it.
pub fn update(
    provider: &Map<String, Value>,
    model: &mut Map<String, Value>,
    protocol: &str,
    estimate: u64,
    output_reserve: Option<u64>,
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
    let total_estimate = estimate.saturating_add(output_reserve.unwrap_or(0));
    let mut changed = false;
    let recorded = if success {
        let old = positive_integer(current.get("largest_success_estimate"));
        old.is_none_or(|old| estimate > old)
            .then_some(("largest_success", estimate, 1.0))
    } else {
        let window = context_window(provider, model).0;
        if window.is_some_and(|window| total_estimate > window) {
            return false;
        }
        let plausible = window.is_none_or(|window| total_estimate.saturating_mul(2) >= window);
        let old = positive_integer(current.get("smallest_failure_estimate"))
            .filter(|_| fresh(current.get("smallest_failure_observed_at")));
        match old {
            None if plausible => Some((estimate, 1.0)),
            None => Some((estimate, UNCONFIRMED_FAILURE_CONFIDENCE)),
            Some(_) if !confirmed(&current) && plausible => Some((estimate, 1.0)),
            Some(old) if !confirmed(&current) && corroborates_failure(old, estimate) => {
                Some((old.min(estimate), 1.0))
            }
            // Replace unrelated unconfirmed evidence without applying it.
            Some(_) if !confirmed(&current) => Some((estimate, UNCONFIRMED_FAILURE_CONFIDENCE)),
            Some(old) if plausible && estimate < old => Some((estimate, 1.0)),
            Some(_) => None,
        }
        .map(|(input, confidence)| ("smallest_failure", input, confidence))
    };
    if let Some((field, estimate, confidence)) = recorded {
        current[format!("{field}_estimate")] = json!(estimate);
        current[format!("{field}_source")] = json!("observed");
        current[format!("{field}_confidence")] = json!(confidence);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> Map<String, Value> {
        Map::from_iter([
            ("id".to_owned(), json!("provider")),
            ("base_url".to_owned(), json!("https://api.example.test/v1")),
            ("context_window".to_owned(), json!(100_000)),
            (
                "capability_sources".to_owned(),
                json!({"context_window":{"source":"manual","confidence":1.0}}),
            ),
        ])
    }

    fn model(deployment: &str) -> Map<String, Value> {
        Map::from_iter([
            ("id".to_owned(), json!("alias/model")),
            ("upstream_id".to_owned(), json!("actual/model")),
            ("deployment_identity".to_owned(), json!(deployment)),
            ("output_limit".to_owned(), json!(4_000)),
        ])
    }

    fn observed_at() -> String {
        let now = OffsetDateTime::now_utc();
        let (year, month, day) = now.to_calendar_date();
        format!(
            "{year:04}-{:02}-{day:02}T{:02}:{:02}:{:02}Z",
            month as u8,
            now.hour(),
            now.minute(),
            now.second(),
        )
    }

    #[test]
    fn failure_estimates_are_input_ceilings_like_python() {
        let provider = provider();
        let mut model = model("deployment-a");
        assert!(update(
            &provider,
            &mut model,
            "responses",
            60_000,
            Some(20_000),
            false,
            &observed_at(),
        ));
        assert_eq!(
            model["context_calibrations"][0]["smallest_failure_estimate"],
            60_000
        );
        // Python stores the input estimate; `failure - 1` is the input
        // ceiling whatever the next request's output budget is.
        for reserves in [Some(20_256), Some(5_256), None] {
            assert_eq!(
                limits(&provider, &model, "responses", reserves).input,
                Some(59_999)
            );
        }

        // Input plus output beyond the known window says nothing about the
        // input ceiling.
        let mut other = super::tests::model("deployment-b");
        assert!(!update(
            &provider,
            &mut other,
            "responses",
            90_000,
            Some(20_000),
            false,
            &observed_at(),
        ));
    }

    #[test]
    fn distant_low_failures_do_not_confirm_a_global_window_reduction() {
        let provider = provider();
        let mut model = model("deployment-a");
        let first = observed_at();
        assert!(update(
            &provider,
            &mut model,
            "responses",
            9_000,
            Some(1_000),
            false,
            &first,
        ));
        let unconfirmed = limits(&provider, &model, "responses", Some(1_256));
        assert_eq!(unconfirmed.input, Some(98_744));
        assert_eq!(unconfirmed.source, "manual");

        assert!(update(
            &provider,
            &mut model,
            "responses",
            15_000,
            Some(1_000),
            false,
            &observed_at(),
        ));
        assert_eq!(
            model["context_calibrations"][0]["smallest_failure_estimate"],
            15_000
        );
        assert_eq!(
            model["context_calibrations"][0]["smallest_failure_confidence"],
            UNCONFIRMED_FAILURE_CONFIDENCE
        );
        assert_eq!(
            limits(&provider, &model, "responses", Some(1_256)).source,
            "manual"
        );

        assert!(update(
            &provider,
            &mut model,
            "responses",
            15_100,
            Some(1_000),
            false,
            &observed_at(),
        ));
        assert_eq!(
            model["context_calibrations"][0]["smallest_failure_estimate"],
            15_000
        );
        assert_eq!(
            model["context_calibrations"][0]["smallest_failure_confidence"],
            1.0
        );
        assert_eq!(
            limits(&provider, &model, "responses", Some(1_256)).input,
            Some(14_999)
        );
    }

    #[test]
    fn calibration_is_isolated_by_actual_deployment_identity() {
        let provider = provider();
        let mut deployment_a = model("deployment-a");
        assert!(update(
            &provider,
            &mut deployment_a,
            "responses",
            70_000,
            Some(2_000),
            false,
            &observed_at(),
        ));
        assert_eq!(
            limits(&provider, &deployment_a, "responses", Some(2_256)).source,
            "observed"
        );

        let deployment_b = model("deployment-b");
        let limits_b = limits(&provider, &deployment_b, "responses", Some(2_256));
        assert_eq!(limits_b.input, Some(97_744));
        assert_eq!(limits_b.source, "manual");
    }
}
