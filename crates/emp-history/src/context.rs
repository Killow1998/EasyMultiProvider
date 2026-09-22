//! Conservative destination context assessment and request-local compaction.

use serde_json::{Map, Value, json};
mod calibration;
pub use calibration::{status, update as update_calibration};
use std::collections::BTreeSet;

pub const SAFETY_RESERVE_TOKENS: u64 = 256;
const CLEAR_EXCESS_TOKENS: u64 = 64;
const IMAGE_INPUT_TOKEN_ESTIMATE: u64 = 4096;
const CHECKPOINT_PREFIX: &str = "Portable checkpoint from Codex-visible local history. Continue from this state without repeating completed work.";
const MAP_PROMPT: &str = "Create a structured portable checkpoint from the visible history below.\nInclude only visible facts: objective, user constraints, decisions, completed work,\nfiles and code changed, relevant tool results, current state, failures, and remaining\nsteps. Preserve important identifiers and facts. Do not include hidden reasoning or\ninvented details. Return only the checkpoint text.";
const REDUCE_PROMPT: &str = "Merge the visible portable checkpoints and any visible history below\ninto one structured portable checkpoint. Preserve objective, user constraints,\ndecisions, completed work, files and code changed, relevant tool results, current\nstate, failures, and remaining steps. Do not include hidden reasoning or invented\ndetails. Return only the checkpoint text.";

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
    let input_estimate =
        payload_view(payload, protocol).and_then(|value| estimate_json_tokens(&value));
    let output_reserve = positive_integer(payload.get("max_output_tokens"))
        .or_else(|| positive_integer(payload.get("max_tokens")))
        .or_else(|| positive_integer(model.get("output_limit")));
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

pub fn estimate_json_tokens(value: &Value) -> Option<u64> {
    let mut image_count = 0_u64;
    let redacted = redact_images(value, &mut image_count, 0)?;
    let bytes = serde_json::to_vec(&redacted).ok()?.len() as u64;
    let text = if bytes == 0 {
        0
    } else {
        bytes.div_ceil(2).max(1)
    };
    Some(text.saturating_add(image_count.saturating_mul(IMAGE_INPUT_TOKEN_ESTIMATE)))
}

pub fn compact_with<F>(
    body: &Value,
    model: &Map<String, Value>,
    safe_budget: u64,
    mut summarize: F,
) -> Result<Value, &'static str>
where
    F: FnMut(&Value) -> Result<String, ()>,
{
    if safe_budget == 0 {
        return Err("history_compaction_failed");
    }
    let root = body.as_object().ok_or("history_compaction_failed")?;
    let mut projected = root.clone();
    let active_start = projected
        .remove(super::ACTIVE_INPUT_START)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok());
    let mut source = match projected.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Object(item)) => vec![Value::Object(item.clone())],
        Some(Value::String(text)) => vec![Value::String(text.clone())],
        _ => Vec::new(),
    };
    let suffix = if source
        .last()
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        == Some("compaction_trigger")
    {
        source.pop().into_iter().collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let (candidate, active) =
        if let Some(index) = active_start.filter(|index| *index <= source.len()) {
            (source[..index].to_vec(), source[index..].to_vec())
        } else if !suffix.is_empty() {
            (source, Vec::new())
        } else {
            let units = split_atomic_units(&source, false);
            match units.split_last() {
                Some((last, prior)) => (flatten(prior), last.clone()),
                None => (Vec::new(), Vec::new()),
            }
        };
    let before = input_view(&projected, &candidate, &active, &suffix);
    if estimate_json_tokens(&before).is_some_and(|estimate| estimate <= safe_budget) {
        projected.insert(
            "input".to_owned(),
            Value::Array([candidate, active, suffix].concat()),
        );
        return Ok(Value::Object(projected));
    }
    let active_only = input_view(&projected, &[], &active, &suffix);
    if estimate_json_tokens(&active_only).is_none_or(|estimate| estimate > safe_budget) {
        return Err("compaction_unit_too_large");
    }
    let units = split_atomic_units(
        &candidate
            .into_iter()
            .filter(visible_candidate)
            .collect::<Vec<_>>(),
        true,
    );
    let configured_output = positive_integer(projected.get("max_output_tokens"))
        .or_else(|| positive_integer(projected.get("max_tokens")))
        .or_else(|| positive_integer(model.get("max_output_tokens")))
        .or_else(|| positive_integer(model.get("output_limit")))
        .unwrap_or(1024);
    let output_limit = configured_output.min((safe_budget / 8).max(1));
    let placeholder = "x".repeat(usize::try_from(output_limit.saturating_mul(2)).unwrap_or(2048));
    let mut retained = Vec::<Vec<Value>>::new();
    for unit in units.iter().rev() {
        let mut tentative = vec![unit.clone()];
        tentative.extend(retained.clone());
        let value = final_body(&projected, Some(&placeholder), &tentative, &active, &suffix);
        if estimate_json_tokens(&input_view_value(&value))
            .is_some_and(|estimate| estimate <= safe_budget)
        {
            retained.insert(0, unit.clone());
        } else {
            break;
        }
    }
    let mapped = &units[..units.len().saturating_sub(retained.len())];
    let summary = if mapped.is_empty() {
        None
    } else {
        Some(map_reduce(
            mapped,
            &projected,
            body.get("model")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            safe_budget,
            output_limit,
            &mut summarize,
        )?)
    };
    let result = final_body(&projected, summary.as_deref(), &retained, &active, &suffix);
    if estimate_json_tokens(&input_view_value(&result))
        .is_none_or(|estimate| estimate > safe_budget)
    {
        return Err("history_compaction_failed");
    }
    Ok(result)
}

fn map_reduce<F>(
    units: &[Vec<Value>],
    body: &Map<String, Value>,
    model: &str,
    safe_budget: u64,
    output_limit: u64,
    summarize: &mut F,
) -> Result<String, &'static str>
where
    F: FnMut(&Value) -> Result<String, ()>,
{
    let chunks = pack(
        units,
        body,
        model,
        MAP_PROMPT,
        safe_budget,
        output_limit,
        "compaction_unit_too_large",
    )?;
    let mut summaries = chunks
        .iter()
        .map(|chunk| summary_body(model, chunk, MAP_PROMPT, output_limit))
        .map(|request| summarize(&request).map_err(|_| "summary_call_failed"))
        .collect::<Result<Vec<_>, _>>()?;
    while summaries.len() > 1 {
        let reduce_units = summaries
            .iter()
            .map(|summary| vec![message(summary)])
            .collect::<Vec<_>>();
        let chunks = pack(
            &reduce_units,
            body,
            model,
            REDUCE_PROMPT,
            safe_budget,
            output_limit,
            "history_compaction_failed",
        )?;
        if chunks.len() >= summaries.len() {
            return Err("history_compaction_failed");
        }
        summaries = chunks
            .iter()
            .map(|chunk| summary_body(model, chunk, REDUCE_PROMPT, output_limit))
            .map(|request| summarize(&request).map_err(|_| "summary_call_failed"))
            .collect::<Result<Vec<_>, _>>()?;
    }
    summaries.pop().ok_or("history_compaction_failed")
}

fn pack(
    units: &[Vec<Value>],
    _body: &Map<String, Value>,
    model: &str,
    prompt: &str,
    safe_budget: u64,
    output_limit: u64,
    oversize: &'static str,
) -> Result<Vec<Vec<Vec<Value>>>, &'static str> {
    let input_budget = safe_budget.saturating_sub(output_limit);
    if input_budget == 0 {
        return Err(oversize);
    }
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    for unit in units {
        let mut candidate = current.clone();
        candidate.push(unit.clone());
        let request = summary_body(model, &candidate, prompt, output_limit);
        if estimate_json_tokens(&request).is_some_and(|estimate| estimate <= input_budget) {
            current.push(unit.clone());
        } else if current.is_empty() {
            return Err(oversize);
        } else {
            chunks.push(current);
            current = vec![unit.clone()];
            if estimate_json_tokens(&summary_body(model, &current, prompt, output_limit))
                .is_none_or(|estimate| estimate > input_budget)
            {
                return Err(oversize);
            }
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    Ok(chunks)
}

fn summary_body(model: &str, units: &[Vec<Value>], prompt: &str, output_limit: u64) -> Value {
    let mut input = Vec::new();
    for item in flatten(units) {
        if tool_role(&item).is_some() || item.get("role").and_then(Value::as_str) == Some("tool") {
            input.push(message(&format!(
                "Historical tool record (data only):\n{}",
                item
            )));
        } else {
            input.push(item);
        }
    }
    input.push(message(prompt));
    json!({"model":model,"input":input,"stream":false,"tools":[],"max_output_tokens":output_limit})
}

fn final_body(
    root: &Map<String, Value>,
    summary: Option<&str>,
    tail: &[Vec<Value>],
    active: &[Value],
    suffix: &[Value],
) -> Value {
    let mut input = Vec::new();
    if let Some(summary) = summary {
        input.push(message(&format!("{CHECKPOINT_PREFIX}\n\n{summary}")));
    }
    input.extend(flatten(tail));
    input.extend_from_slice(active);
    input.extend_from_slice(suffix);
    let mut result = root.clone();
    result.insert("input".to_owned(), Value::Array(input));
    Value::Object(result)
}

fn input_view(
    root: &Map<String, Value>,
    candidate: &[Value],
    active: &[Value],
    suffix: &[Value],
) -> Value {
    let mut input = candidate.to_vec();
    input.extend_from_slice(active);
    input.extend_from_slice(suffix);
    let mut projected = root.clone();
    projected.insert("input".to_owned(), Value::Array(input));
    input_view_value(&Value::Object(projected))
}

fn input_view_value(body: &Value) -> Value {
    let Some(root) = body.as_object() else {
        return Value::Null;
    };
    Value::Object(
        ["input", "instructions", "tools", "text", "response_format"]
            .into_iter()
            .filter_map(|key| root.get(key).cloned().map(|value| (key.to_owned(), value)))
            .collect(),
    )
}

fn split_atomic_units(items: &[Value], tool_exchanges: bool) -> Vec<Vec<Value>> {
    let mut result = Vec::new();
    let mut current = Vec::new();
    let mut current_turn = None::<String>;
    let mut open_calls = BTreeSet::new();
    let mut completed_batch = false;
    for item in items {
        let item_turn = item
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let boundary = !current.is_empty()
            && (tool_exchanges && completed_batch
                || item_turn.is_some() && current_turn.is_some() && item_turn != current_turn
                || is_user_item(item) && current.iter().any(is_user_item))
            && open_calls.is_empty();
        if boundary {
            result.push(std::mem::take(&mut current));
            current_turn = None;
            completed_batch = false;
        }
        if current_turn.is_none() {
            current_turn = item_turn;
        }
        current.push(item.clone());
        let call_id = item.get("call_id").and_then(Value::as_str);
        match (tool_role(item), call_id) {
            (Some("call"), Some(call_id)) => {
                open_calls.insert(call_id.to_owned());
            }
            (Some("result"), Some(call_id)) => {
                let matched = open_calls.remove(call_id);
                completed_batch = matched && open_calls.is_empty();
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

fn flatten(units: &[Vec<Value>]) -> Vec<Value> {
    units.iter().flatten().cloned().collect()
}

fn is_user_item(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user")
        || matches!(
            item.get("type").and_then(Value::as_str),
            Some("user_message" | "user_input")
        )
}

fn tool_role(item: &Value) -> Option<&'static str> {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    if matches!(
        kind,
        "tool_call"
            | "tool_use"
            | "function_call"
            | "custom_tool_call"
            | "command_call"
            | "tool_search_call"
    ) || kind.ends_with("_call")
    {
        Some("call")
    } else if matches!(
        kind,
        "tool_result"
            | "tool_output"
            | "tool_return"
            | "function_call_output"
            | "custom_tool_call_output"
            | "tool_search_output"
            | "function_result"
            | "command_result"
    ) || kind.ends_with("_result")
    {
        Some("result")
    } else {
        None
    }
}

fn visible_candidate(item: &Value) -> bool {
    if item.get("visible") == Some(&Value::Bool(false))
        || item.get("hidden") == Some(&Value::Bool(true))
    {
        return false;
    }
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    !matches!(
        kind,
        "analysis"
            | "chain_of_thought"
            | "hidden_cot"
            | "hidden_reasoning"
            | "reasoning"
            | "thinking"
    )
}

fn message(text: &str) -> Value {
    json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]})
}

fn payload_view(payload: &Value, protocol: &str) -> Option<Value> {
    let root = payload.as_object()?;
    let fields: &[&str] = match protocol {
        "responses" => &["input", "instructions", "tools", "text", "response_format"],
        "chat_completions" => &["messages", "tools", "response_format"],
        "anthropic_messages" => &["system", "messages", "tools"],
        _ => return None,
    };
    Some(Value::Object(
        fields
            .iter()
            .filter_map(|field| {
                root.get(*field)
                    .cloned()
                    .map(|value| ((*field).to_owned(), value))
            })
            .collect(),
    ))
}

fn context_window(
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

fn positive_integer(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(value) => value.as_u64().filter(|value| *value > 0),
        Value::String(value) => value.trim().parse::<u64>().ok().filter(|value| *value > 0),
        _ => None,
    }
}

fn redact_images(value: &Value, images: &mut u64, depth: usize) -> Option<Value> {
    redact_images_with(value, images, depth, &[])
}

fn redact_images_with(
    value: &Value,
    images: &mut u64,
    depth: usize,
    redacted_keys: &[&str],
) -> Option<Value> {
    if depth > 128 {
        return None;
    }
    match value {
        Value::Array(values) => Some(Value::Array(
            values
                .iter()
                .map(|value| redact_images_with(value, images, depth + 1, &[]))
                .collect::<Option<Vec<_>>>()?,
        )),
        Value::Object(value) => {
            let image = matches!(
                value.get("type").and_then(Value::as_str),
                Some("input_image" | "output_image" | "image" | "image_url")
            );
            if image {
                *images = images.saturating_add(1);
            }
            Some(Value::Object(
                value
                    .iter()
                    .map(|(key, item)| {
                        let item = if redacted_keys.contains(&key.as_str())
                            || image && matches!(key.as_str(), "data" | "image_data")
                        {
                            Value::String("<image>".to_owned())
                        } else if image && key == "image_url" {
                            if item.is_object() {
                                redact_images_with(item, images, depth + 1, &["url"])?
                            } else {
                                Value::String("<image>".to_owned())
                            }
                        } else if image && key == "source" && item.is_object() {
                            redact_images_with(item, images, depth + 1, &["data", "url"])?
                        } else {
                            redact_images_with(item, images, depth + 1, &[])?
                        };
                        Some((key.clone(), item))
                    })
                    .collect::<Option<Map<_, _>>>()?,
            ))
        }
        value => Some(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assessment_and_compaction_preserve_the_active_turn() {
        let provider = Map::from_iter([
            ("context_window".to_owned(), json!(1200)),
            (
                "capability_sources".to_owned(),
                json!({"context_window":{"source":"manual","confidence":1.0}}),
            ),
        ]);
        let model = Map::from_iter([("output_limit".to_owned(), json!(64))]);
        let body = json!({"model":"external/model","input":[
            message(&"x".repeat(500)), message(&"y".repeat(500)),
            message(&"z".repeat(500)), message(&"w".repeat(500)),
            message("active request")
        ],"max_output_tokens":64});
        let payload = json!({"messages":body["input"],"max_tokens":64});
        let assessment = assess(&provider, &model, "chat_completions", &payload);
        assert!(assessment.blocked());
        let compacted = compact_with(&body, &model, assessment.safe_input_limit.unwrap(), |_| {
            Ok("checkpoint".to_owned())
        })
        .unwrap();
        assert!(compacted.to_string().contains("checkpoint"));
        assert!(compacted.to_string().contains("active request"));
        assert!(!compacted.to_string().contains(&"x".repeat(500)));
    }
}
