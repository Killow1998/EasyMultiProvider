//! Optional per-inference effort selection on the existing Native transport.
use crate::app::ServerState;
use crate::services::{native, request_facts};
use emp_core::{Dialect, ResolvedRoute};
use emp_state::config::adaptive_effort::{AdaptiveEffort, Effort};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
mod classifier;
mod history;
pub(crate) use history::State;

pub(crate) const DECISION_HEADER: &str = "X-EMP-Effort-Decision";

#[derive(Clone, Serialize, Deserialize)]
struct Decision {
    effort: Effort,
    classifier_model: String,
    classifier_ms: u64,
    fallback: Option<String>,
    cached: bool,
}

pub(crate) fn prepare(
    state: &ServerState,
    config: &Value,
    route: &ResolvedRoute,
    body: &mut Map<String, Value>,
    incoming: &mut BTreeMap<String, String>,
) {
    if config["adaptive_effort"]["enabled"] != true || route.dialect != Dialect::CodexNative {
        return;
    }
    let model = route.model.value();
    if model.get("supports_reasoning_effort_updates") != Some(&Value::Bool(true))
        || body.get("generate") == Some(&Value::Bool(false))
        || body
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("mode"))
            .and_then(Value::as_str)
            .is_some_and(|mode| mode != "standard")
        || incoming
            .keys()
            .any(|name| name.eq_ignore_ascii_case("x-openai-subagent"))
        || request_facts::memory_kind(&Value::Object(body.clone()), incoming).is_some()
    {
        return;
    }
    let configured = request_facts::reasoning_effort(
        body.get("reasoning"),
        body.get("input"),
        body.get("client_metadata"),
    );
    if matches!(configured, "none" | "minimal" | "ultra" | "invalid_type") {
        return;
    }
    let settings: AdaptiveEffort =
        serde_json::from_value(config["adaptive_effort"].clone()).expect("normalized settings");
    let allowed: Vec<Effort> = Effort::ALL
        .into_iter()
        .filter(|effort| {
            *effort <= settings.max_effort
                && model
                    .get("supported_reasoning_levels")
                    .and_then(Value::as_array)
                    .is_some_and(|levels| {
                        levels
                            .iter()
                            .any(|level| level["effort"] == effort.as_str())
                    })
        })
        .collect();
    if allowed.len() < 2 {
        return;
    }
    let Some(input) = body
        .get("input")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
    else {
        return;
    };
    let Ok(mut headers) = native::resolve_headers(state, route, incoming, false, false) else {
        return;
    };
    if let Some(metadata) = body
        .get("client_metadata")
        .and_then(|value| value["x-codex-turn-metadata"].as_str())
    {
        headers.insert("x-codex-turn-metadata".into(), metadata.into());
    }
    let owner = emp_state::usage::account_owner(&headers);
    let anchor = emp_history::request_history_anchor(body, incoming).ok();
    let scope = json!([
        owner,
        route.endpoint_fingerprint,
        route.requested_model,
        anchor
            .as_ref()
            .and_then(|anchor| anchor.thread_id.as_deref()),
        anchor
            .as_ref()
            .and_then(|anchor| anchor.window_id.as_deref()),
        body.get("prompt_cache_key"),
        body.get("reasoning"),
        settings.classifier_model,
        settings.max_effort
    ]);
    let incremental = body
        .get("previous_response_id")
        .is_some_and(Value::is_string);
    let history = &state.backend.transport.adaptive_effort;
    let step = history.prepare(&scope, input, incremental);
    let baseline = body
        .get("reasoning")
        .and_then(|value| value.get("effort"))
        .or_else(|| model.get("default_reasoning_level"))
        .and_then(|value| serde_json::from_value::<Effort>(value.clone()).ok());
    let fallback = serde_json::from_value(json!(configured))
        .ok()
        .or(baseline)
        .unwrap_or(Effort::High);
    let decision = if let Some(previous) = step.previous.last().and_then(Option::as_ref) {
        Decision {
            cached: true,
            classifier_ms: 0,
            ..previous.clone()
        }
    } else {
        classifier::select(
            state,
            route,
            &settings,
            &allowed,
            &step.context,
            &headers,
            fallback,
        )
    };
    history.remember(&step, &decision);
    let Value::Array(input) = body.remove("input").unwrap() else {
        unreachable!("array checked at entry")
    };
    body.insert(
        "input".into(),
        json!(step.rewrite(
            input,
            decision.effort,
            if incremental { None } else { baseline }
        )),
    );
    let mut facts = serde_json::to_value(&decision).expect("effort decision");
    facts["configured_effort"] = json!(configured);
    incoming.insert(DECISION_HEADER.into(), facts.to_string());
}
