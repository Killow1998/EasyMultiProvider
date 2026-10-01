//! Destination limits and assessment; independent of compaction and transport.
use super::estimate::estimate_protocol_payload_tokens;
use super::{SAFETY_RESERVE_TOKENS, calibration};
use serde_json::{Map, Value};

const CLEAR_EXCESS_TOKENS: u64 = 64;

#[derive(Clone, Debug, PartialEq)]
pub struct ContextAssessment {
    pub provider_id: String,
    pub model_id: String,
    pub input_estimate: Option<u64>,
    pub output_reserve: Option<u64>,
    pub context_limit: Option<u64>,
    pub safe_input_limit: Option<u64>,
    pub confidence: f64,
    pub source: String,
    pub decision: &'static str,
}

impl ContextAssessment {
    pub fn blocked(&self) -> bool {
        self.decision == "block"
    }
}

pub fn assess(
    provider: &Map<String, Value>,
    model: &Map<String, Value>,
    protocol: &str,
    payload: &Value,
) -> ContextAssessment {
    let input_estimate = estimate_protocol_payload_tokens(payload, protocol);
    let output_reserve = output_reserve(payload, model);
    let limits = calibration::limits(
        provider,
        model,
        protocol,
        output_reserve.map(|output| output.saturating_add(SAFETY_RESERVE_TOKENS)),
    );
    let (context_limit, safe_input_limit, source, confidence) = (
        limits.context,
        limits.input,
        limits.source,
        limits.confidence,
    );
    let decision = match (input_estimate, safe_input_limit) {
        (None, Some(_)) => "block",
        (Some(estimate), Some(limit)) if estimate > limit => {
            let excess = estimate - limit;
            let clear = excess >= CLEAR_EXCESS_TOKENS.max(limit.max(1) / 100);
            if confidence >= 0.75 && clear {
                "block"
            } else {
                "warn"
            }
        }
        (None, None) | (Some(_), None) => "warn",
        _ => "allow",
    };
    ContextAssessment {
        provider_id: emp_core::capability_view::safe_id(provider.get("id"), "unknown"),
        model_id: emp_core::capability_view::safe_id(model.get("id"), "unknown"),
        input_estimate,
        output_reserve,
        context_limit,
        safe_input_limit,
        confidence,
        source,
        decision,
    }
}

fn output_reserve(payload: &Value, model: &Map<String, Value>) -> Option<u64> {
    ["max_output_tokens", "max_completion_tokens", "max_tokens"]
        .into_iter()
        .find_map(|key| positive_integer(payload.get(key)))
        .or_else(|| positive_integer(model.get("output_limit")))
}

pub(super) fn context_window(
    provider: &Map<String, Value>,
    model: &Map<String, Value>,
) -> (Option<u64>, String, f64) {
    for source in [model, provider] {
        let Some(mut limit) = positive_integer(source.get("context_window")) else {
            continue;
        };
        let percentage = source
            .get("effective_context_window_percent")
            .and_then(Value::as_f64)
            .unwrap_or(100.0);
        if percentage.is_finite() && percentage > 0.0 && percentage <= 100.0 {
            limit = ((limit as f64 * percentage / 100.0).round_ties_even() as u64).max(1);
        }
        let provenance = source
            .get("capability_sources")
            .and_then(Value::as_object)
            .and_then(|values| values.get("context_window"))
            .and_then(Value::as_object);
        let name = provenance
            .and_then(|value| value.get("source"))
            .and_then(Value::as_str)
            .unwrap_or("inferred");
        let confidence = provenance
            .and_then(|value| value.get("confidence"))
            .and_then(Value::as_f64)
            .unwrap_or(match name {
                "official" => 0.95,
                "advertised" => 0.75,
                "observed" | "manual" => 1.0,
                "inferred" => 0.35,
                _ => 0.0,
            });
        if matches!(
            name,
            "official" | "advertised" | "observed" | "manual" | "inferred"
        ) && confidence.is_finite()
            && (0.0..=1.0).contains(&confidence)
        {
            return (Some(limit), name.to_owned(), confidence);
        }
    }
    (None, "unknown".to_owned(), 0.0)
}

pub(super) fn positive_integer(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(value) => value.as_u64().filter(|value| *value > 0),
        Value::String(value) => value.trim().parse::<u64>().ok().filter(|value| *value > 0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assessment_separates_requested_output_budget_from_input() {
        let provider = Map::new();
        let model = Map::from_iter([("output_limit".to_owned(), json!(4096))]);
        let explicit = assess(
            &provider,
            &model,
            "chat_completions",
            &json!({"messages":[],"max_completion_tokens":512}),
        );
        assert_eq!(explicit.output_reserve, Some(512));

        let defaulted = assess(
            &provider,
            &model,
            "chat_completions",
            &json!({"messages":[]}),
        );
        assert_eq!(defaulted.output_reserve, Some(4096));
    }
}
