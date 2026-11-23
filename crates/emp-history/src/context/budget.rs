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
    assess_with_input_estimate(provider, model, protocol, payload, input_estimate)
}

/// With a known budget, an unavailable estimate still requires compaction
/// and does not authorize oversized input.
pub fn assess_with_input_estimate(
    provider: &Map<String, Value>,
    model: &Map<String, Value>,
    protocol: &str,
    payload: &Value,
    input_estimate: Option<u64>,
) -> ContextAssessment {
    let flexible = model.get("_emp_flexible_output_limit") == Some(&Value::Bool(true));
    let requested = ["max_output_tokens", "max_completion_tokens", "max_tokens"]
        .into_iter()
        .find_map(|key| positive_integer(payload.get(key)));
    let ceiling = positive_integer(model.get("output_limit"));
    let mut output_reserve = if flexible {
        requested.map(|n| ceiling.map_or(n, |limit| n.min(limit)))
    } else {
        requested.or(ceiling)
    };
    if flexible
        && let (Some(requested), Some(window)) = (output_reserve, context_window(provider, model).0)
        && !input_only_window(model)
    {
        // Fit a ceiling to the measured input. With an unknown estimate, the planner
        // first compacts the input and then assesses the actual outgoing request.
        let remaining = input_estimate.map_or(1, |input| {
            window
                .saturating_sub(input)
                .saturating_sub(SAFETY_RESERVE_TOKENS)
                .max(1)
        });
        output_reserve = Some(requested.min(remaining));
    }
    let calibration::Limits {
        context: context_limit,
        input: safe_input_limit,
        source,
        confidence,
        ..
    } = calibration::limits(
        provider,
        model,
        protocol,
        output_reserve
            .or(flexible.then_some(0))
            .map(|output| output.saturating_add(SAFETY_RESERVE_TOKENS)),
    );
    let decision = match (input_estimate, safe_input_limit) {
        (None, Some(_)) => "block",
        (Some(estimate), Some(limit)) if estimate > limit => {
            let excess = estimate - limit;
            let clear = excess >= CLEAR_EXCESS_TOKENS.max(limit.max(1) / 100);
            if (flexible && output_reserve == Some(1)) || (confidence >= 0.75 && clear) {
                "block"
            } else {
                "warn"
            }
        }
        (None, None) | (Some(_), None) => "warn",
        _ => "allow",
    };
    ContextAssessment {
        provider_id: emp_domain::capability_view::safe_id(provider.get("id"), "unknown"),
        model_id: emp_domain::capability_view::safe_id(model.get("id"), "unknown"),
        input_estimate,
        output_reserve,
        context_limit,
        safe_input_limit,
        confidence,
        source,
        decision,
    }
}

pub(super) fn input_only_window(model: &Map<String, Value>) -> bool {
    model.get("capability_sources").is_some_and(|sources| {
        let window = &sources["context_window"];
        window["input_only"].as_bool().unwrap_or_else(|| {
            window["source"] == "catalog"
                && positive_integer(model.get("context_window"))
                    .is_some_and(|n| Some(n) == positive_integer(model.get("max_input_tokens")))
        })
    })
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
                "catalog" => 0.7,
                "observed" | "manual" => 1.0,
                "inferred" => 0.35,
                _ => 0.0,
            });
        if matches!(
            name,
            "official" | "advertised" | "observed" | "manual" | "catalog" | "inferred"
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
    fn unavailable_bounded_estimate_blocks_without_losing_the_requested_output_budget() {
        let provider = Map::new();
        let model = json!({"context_window":1000,"output_limit":64})
            .as_object()
            .unwrap()
            .clone();
        let assessment = assess_with_input_estimate(
            &provider,
            &model,
            "anthropic_messages",
            &json!({"max_tokens":32}),
            None,
        );
        assert!(assessment.blocked());
        assert_eq!(assessment.input_estimate, None);
        assert_eq!(assessment.output_reserve, Some(32));
        assert_eq!(assessment.safe_input_limit, Some(712));
    }

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
