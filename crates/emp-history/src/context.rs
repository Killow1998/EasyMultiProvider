//! Conservative destination context assessment and request-local compaction.

use serde_json::{Map, Value, json};
mod calibration;
pub use calibration::{status, update as update_calibration};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use std::collections::BTreeSet;
use std::io::{self, Write};

pub const SAFETY_RESERVE_TOKENS: u64 = 256;
const CLEAR_EXCESS_TOKENS: u64 = 64;
const IMAGE_INPUT_TOKEN_ESTIMATE: u64 = 4096;
const IMAGE_TYPES: &[&str] = &["input_image", "output_image", "image", "image_url"];
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

pub fn estimate_json_tokens(value: &Value) -> Option<u64> {
    match contains_image(value, 0) {
        Some(false) => {
            let bytes = serialized_json_bytes(value)?;
            Some(tokens_from_bytes(bytes, 0))
        }
        Some(true) | None => materialized_estimate_json_tokens(value),
    }
}

fn estimate_protocol_payload_tokens(payload: &Value, protocol: &str) -> Option<u64> {
    let root = payload.as_object()?;
    let fields = protocol_fields(protocol)?;
    let mut images = false;
    for field in fields {
        if let Some(value) = root.get(*field) {
            match contains_image(value, 1) {
                Some(false) => {}
                Some(true) | None => {
                    images = true;
                    break;
                }
            }
        }
    }
    if images {
        return payload_view(payload, protocol)
            .and_then(|view| materialized_estimate_json_tokens(&view));
    }
    let bytes = serialized_json_bytes(&SelectedProtocolFields { root, fields })?;
    Some(tokens_from_bytes(bytes, 0))
}

fn tokens_from_bytes(bytes: u64, image_count: u64) -> u64 {
    let text = if bytes == 0 {
        0
    } else {
        bytes.div_ceil(2).max(1)
    };
    text.saturating_add(image_count.saturating_mul(IMAGE_INPUT_TOKEN_ESTIMATE))
}

fn materialized_estimate_json_tokens(value: &Value) -> Option<u64> {
    let mut image_count = 0_u64;
    let redacted = redact_images_with(value, &mut image_count, 0, &[])?;
    let bytes = serde_json::to_vec(&redacted).ok()?.len() as u64;
    Some(tokens_from_bytes(bytes, image_count))
}

fn serialized_json_bytes(value: &impl Serialize) -> Option<u64> {
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, value).ok()?;
    u64::try_from(writer.bytes).ok()
}

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("serialized JSON length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct SelectedProtocolFields<'a> {
    root: &'a Map<String, Value>,
    fields: &'static [&'static str],
}

impl Serialize for SelectedProtocolFields<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let count = self
            .root
            .keys()
            .filter(|key| self.fields.contains(&key.as_str()))
            .count();
        let mut object = serializer.serialize_map(Some(count))?;
        for (key, value) in self.root {
            if self.fields.contains(&key.as_str()) {
                object.serialize_entry(key, value)?;
            }
        }
        object.end()
    }
}

fn contains_image(value: &Value, depth: usize) -> Option<bool> {
    if depth > 128 {
        return None;
    }
    match value {
        Value::Array(values) => {
            for value in values {
                if contains_image(value, depth + 1)? {
                    return Some(true);
                }
            }
            Some(false)
        }
        Value::Object(values) => {
            if matches!(
                values.get("type").and_then(Value::as_str),
                Some(kind) if IMAGE_TYPES.contains(&kind)
            ) {
                return Some(true);
            }
            for value in values.values() {
                if contains_image(value, depth + 1)? {
                    return Some(true);
                }
            }
            Some(false)
        }
        _ => Some(false),
    }
}

/// Upper bound on atomic history units considered for one destination compaction.
const MAX_COMPACTION_UNITS: usize = 50_000;
/// Upper bound on map and reduce summary requests issued for one compaction.
const MAX_SUMMARY_REQUESTS: usize = 256;

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
    // Every body built below replaces `input`; keep the key (and its position)
    // but drop the original array so it is never cloned or re-serialised again.
    let mut source = match projected.get_mut("input").map(Value::take) {
        Some(Value::Array(items)) => items,
        Some(Value::Object(item)) => vec![Value::Object(item)],
        Some(Value::String(text)) => vec![Value::String(text)],
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
            let active = source.split_off(index);
            (source, active)
        } else if !suffix.is_empty() {
            (source, Vec::new())
        } else {
            let mut units = split_atomic_units(&source, false);
            match units.pop() {
                Some(last) => (units.into_iter().flatten().collect(), last),
                None => (Vec::new(), Vec::new()),
            }
        };
    let before = input_view_value(&final_body(
        &projected,
        None,
        std::slice::from_ref(&candidate),
        &active,
        &suffix,
    ));
    if estimate_json_tokens(&before).is_some_and(|estimate| estimate <= safe_budget) {
        projected.insert(
            "input".to_owned(),
            Value::Array([candidate, active, suffix].concat()),
        );
        return Ok(Value::Object(projected));
    }
    drop(before);
    let active_only = input_view_value(&final_body(&projected, None, &[], &active, &suffix));
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
    if units.len() > MAX_COMPACTION_UNITS {
        return Err("compaction_unit_too_large");
    }
    let configured_output = positive_integer(projected.get("max_output_tokens"))
        .or_else(|| positive_integer(projected.get("max_tokens")))
        .or_else(|| positive_integer(model.get("max_output_tokens")))
        .or_else(|| positive_integer(model.get("output_limit")))
        .unwrap_or(1024);
    let output_limit = configured_output.min((safe_budget / 8).max(1));
    let placeholder = "x".repeat(usize::try_from(output_limit.saturating_mul(2)).unwrap_or(2048));
    // The tentative body is the placeholder checkpoint, the retained units,
    // then the active turn and suffix; units only add their own items.
    let mut tail = IncrementalEstimate::new(
        &input_view_value(&final_body(
            &projected,
            Some(&placeholder),
            &[],
            &active,
            &suffix,
        )),
        1 + active.len() + suffix.len(),
    );
    let mut kept = 0;
    for unit in units.iter().rev() {
        let costs = unit
            .iter()
            .map(|item| json_cost(item, 2))
            .collect::<Option<Vec<_>>>();
        let fits = costs.as_ref().is_some_and(|costs| {
            tail.tokens_with(costs)
                .is_some_and(|estimate| estimate <= safe_budget)
        });
        match costs {
            Some(costs) if fits => {
                tail.extend(&costs);
                kept += 1;
            }
            _ => break,
        }
    }
    let (mapped, retained) = units.split_at(units.len() - kept);
    let summary = if mapped.is_empty() {
        None
    } else {
        Some(map_reduce(
            mapped,
            body.get("model")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            safe_budget,
            output_limit,
            &mut summarize,
        )?)
    };
    let result = final_body(&projected, summary.as_deref(), retained, &active, &suffix);
    if estimate_json_tokens(&input_view_value(&result))
        .is_none_or(|estimate| estimate > safe_budget)
    {
        return Err("history_compaction_failed");
    }
    Ok(result)
}

/// Serialized size of one JSON value in the token estimator's terms: bytes
/// after image redaction plus the number of redacted images.
#[derive(Clone, Copy, Default)]
struct JsonCost {
    bytes: u64,
    images: u64,
}

/// Mirrors `estimate_json_tokens` for a value nested `depth` levels deep, so
/// per-item costs sum to the estimate of the enclosing array.
fn json_cost(value: &Value, depth: usize) -> Option<JsonCost> {
    match contains_image(value, depth)? {
        false => Some(JsonCost {
            bytes: serialized_json_bytes(value)?,
            images: 0,
        }),
        true => {
            let mut images = 0_u64;
            let redacted = redact_images_with(value, &mut images, depth, &[])?;
            Some(JsonCost {
                bytes: serialized_json_bytes(&redacted)?,
                images,
            })
        }
    }
}

/// Token estimate of a request whose `input` array grows one item at a time,
/// computed without re-serialising the request for every candidate.
struct IncrementalEstimate {
    total: Option<JsonCost>,
    items: u64,
}

impl IncrementalEstimate {
    /// `base` is the request with only its `fixed` always-present input items.
    fn new(base: &Value, fixed: usize) -> Self {
        Self {
            total: json_cost(base, 0),
            items: fixed as u64,
        }
    }

    fn added(&self, costs: &[JsonCost]) -> Option<JsonCost> {
        let mut total = self.total?;
        let mut items = self.items;
        for cost in costs {
            // One separating comma per item beyond the first.
            let comma = u64::from(items > 0);
            total.bytes = total.bytes.checked_add(cost.bytes)?.checked_add(comma)?;
            total.images = total.images.saturating_add(cost.images);
            items += 1;
        }
        Some(total)
    }

    fn tokens_with(&self, costs: &[JsonCost]) -> Option<u64> {
        self.added(costs)
            .map(|total| tokens_from_bytes(total.bytes, total.images))
    }

    fn extend(&mut self, costs: &[JsonCost]) {
        self.total = self.added(costs);
        self.items = self.items.saturating_add(costs.len() as u64);
    }
}

fn map_reduce<F>(
    units: &[Vec<Value>],
    model: &str,
    safe_budget: u64,
    output_limit: u64,
    summarize: &mut F,
) -> Result<String, &'static str>
where
    F: FnMut(&Value) -> Result<String, ()>,
{
    let mut requests = 0_usize;
    let mut run = |chunks: Vec<Vec<Vec<Value>>>, prompt: &str| {
        requests = requests.saturating_add(chunks.len());
        if requests > MAX_SUMMARY_REQUESTS {
            return Err("compaction_unit_too_large");
        }
        chunks
            .iter()
            .map(|chunk| summary_body(model, chunk, prompt, output_limit))
            .map(|request| summarize(&request).map_err(|_| "summary_call_failed"))
            .collect::<Result<Vec<_>, _>>()
    };
    let chunks = pack(
        units,
        model,
        MAP_PROMPT,
        safe_budget,
        output_limit,
        "compaction_unit_too_large",
    )?;
    let mut summaries = run(chunks, MAP_PROMPT)?;
    while summaries.len() > 1 {
        let reduce_units = summaries
            .iter()
            .map(|summary| vec![message(summary)])
            .collect::<Vec<_>>();
        let chunks = pack(
            &reduce_units,
            model,
            REDUCE_PROMPT,
            safe_budget,
            output_limit,
            "history_compaction_failed",
        )?;
        if chunks.len() >= summaries.len() {
            return Err("history_compaction_failed");
        }
        summaries = run(chunks, REDUCE_PROMPT)?;
    }
    summaries.pop().ok_or("history_compaction_failed")
}

fn pack(
    units: &[Vec<Value>],
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
    // The empty request already holds the prompt; units are inserted before it.
    let empty = IncrementalEstimate::new(&summary_body(model, &[], prompt, output_limit), 1);
    let fits = |estimate: &IncrementalEstimate, costs: &Option<Vec<JsonCost>>| {
        costs.as_ref().is_some_and(|costs| {
            estimate
                .tokens_with(costs)
                .is_some_and(|estimate| estimate <= input_budget)
        })
    };
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut estimate = IncrementalEstimate {
        total: empty.total,
        items: empty.items,
    };
    for unit in units {
        let costs = unit
            .iter()
            .map(|item| json_cost(&summary_item(item.clone()), 2))
            .collect::<Option<Vec<_>>>();
        if fits(&estimate, &costs) {
            current.push(unit.clone());
        } else if current.is_empty() {
            return Err(oversize);
        } else {
            chunks.push(std::mem::take(&mut current));
            current.push(unit.clone());
            estimate = IncrementalEstimate {
                total: empty.total,
                items: empty.items,
            };
            if !fits(&estimate, &costs) {
                return Err(oversize);
            }
        }
        if let Some(costs) = &costs {
            estimate.extend(costs);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    Ok(chunks)
}

fn summary_item(item: Value) -> Value {
    if tool_role(&item).is_some() || item.get("role").and_then(Value::as_str) == Some("tool") {
        message(&format!(
            "Historical tool record (data only):\n{}",
            python_json_text(&item)
        ))
    } else {
        item
    }
}

/// Python's summary oracle embeds tool records with `json.dumps` defaults,
/// including one space after commas and colons. Keep that text shape so
/// request packing uses the same serialized size and submits equivalent data.
fn python_json_text(value: &Value) -> String {
    fn append(value: &Value, output: &mut String) {
        match value {
            Value::Null => output.push_str("null"),
            Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Value::Number(value) => output.push_str(&value.to_string()),
            Value::String(value) => {
                output.push_str(&serde_json::to_string(value).expect("serialize JSON string"));
            }
            Value::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push_str(", ");
                    }
                    append(value, output);
                }
                output.push(']');
            }
            Value::Object(values) => {
                output.push('{');
                for (index, (key, value)) in values.iter().enumerate() {
                    if index > 0 {
                        output.push_str(", ");
                    }
                    output.push_str(&serde_json::to_string(key).expect("serialize JSON key"));
                    output.push_str(": ");
                    append(value, output);
                }
                output.push('}');
            }
        }
    }

    let mut output = String::new();
    append(value, &mut output);
    output
}

fn summary_body(model: &str, units: &[Vec<Value>], prompt: &str, output_limit: u64) -> Value {
    let mut input = flatten(units)
        .into_iter()
        .map(summary_item)
        .collect::<Vec<_>>();
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
        input.push(message(&format!(
            "{}\n\n{summary}",
            super::CHECKPOINT_PREFIX
        )));
    }
    input.extend(flatten(tail));
    input.extend_from_slice(active);
    input.extend_from_slice(suffix);
    let mut result = root.clone();
    result.insert("input".to_owned(), Value::Array(input));
    Value::Object(result)
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
        let boundary = if current.is_empty() {
            false
        } else if tool_exchanges && completed_batch {
            true
        } else if let (Some(item_turn), Some(current_turn)) = (&item_turn, &current_turn) {
            item_turn != current_turn
        } else {
            is_user_item(item) && current.iter().any(is_user_item)
        } && open_calls.is_empty();
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
    match kind {
        _ if kind.ends_with("_call") => Some("call"),
        _ if kind.ends_with("_result")
            || matches!(
                kind,
                "tool_output"
                    | "tool_return"
                    | "function_call_output"
                    | "custom_tool_call_output"
                    | "tool_search_output"
            ) =>
        {
            Some("result")
        }
        _ => None,
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
    let fields = protocol_fields(protocol)?;
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

fn protocol_fields(protocol: &str) -> Option<&'static [&'static str]> {
    Some(match protocol {
        "responses" => &["input", "instructions", "tools", "text", "response_format"],
        "chat_completions" => &["messages", "tools", "response_format"],
        "anthropic_messages" => &["system", "messages", "tools"],
        _ => return None,
    })
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
                Some(kind) if IMAGE_TYPES.contains(&kind)
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
    fn streaming_token_estimate_matches_materialized_text_and_nested_values() {
        let value = json!({
            "text":"ordinary ".repeat(64 * 1024),
            "unicode":"snowman ☃ and globe 🌎",
            "escaped":"quote \" slash \\ newline\n tab\t",
            "nested":[{"items":[true,false,null,-5,2.5]}]
        });
        assert_eq!(
            estimate_json_tokens(&value),
            materialized_estimate_json_tokens(&value)
        );
    }

    #[test]
    fn image_estimate_fallback_matches_materialized_redaction_shapes() {
        let value = json!([
            {"type":"input_image","data":"private","image_url":{"url":"https://example.test/a.png","detail":"high"}},
            {"type":"image","source":{"type":"base64","data":"private","url":"private","media_type":"image/png"}},
            {"type":"image_url","image_url":"https://example.test/b.png"},
            {"type":"message","content":[{"type":"output_image","image_data":"private"}]}
        ]);
        assert_eq!(
            estimate_json_tokens(&value),
            materialized_estimate_json_tokens(&value)
        );
    }

    #[test]
    fn protocol_field_streaming_matches_materialized_view_for_all_protocols() {
        let payload = json!({
            "input":[{"type":"message","role":"user","content":"responses"}],
            "instructions":"instruction",
            "tools":[{"type":"function","name":"search","parameters":{"type":"object"}}],
            "text":{"format":{"type":"json_schema"}},
            "response_format":{"type":"json_object"},
            "messages":[{"role":"user","content":[{"type":"image","data":"private"}]}],
            "system":[{"type":"text","text":"anthropic"}],
            "ignored":{"type":"image","data":"must not be counted"}
        });
        let plain_payload = json!({
            "input":"responses selected",
            "instructions":"instruction",
            "tools":[{"type":"function","name":"search","parameters":{"type":"object"}}],
            "text":{"format":{"type":"json_schema"}},
            "response_format":{"type":"json_object"},
            "messages":[{"role":"user","content":"chat selected"}],
            "system":[{"type":"text","text":"anthropic selected"}],
            "ignored":{"type":"image","data":"must not be counted"}
        });
        for protocol in ["responses", "chat_completions", "anthropic_messages"] {
            let expected = payload_view(&payload, protocol)
                .and_then(|view| materialized_estimate_json_tokens(&view));
            assert_eq!(
                estimate_protocol_payload_tokens(&payload, protocol),
                expected
            );
            let plain_expected = payload_view(&plain_payload, protocol)
                .and_then(|view| materialized_estimate_json_tokens(&view));
            assert_eq!(
                estimate_protocol_payload_tokens(&plain_payload, protocol),
                plain_expected
            );
        }
        assert_eq!(estimate_protocol_payload_tokens(&payload, "unknown"), None);
    }

    #[test]
    fn streaming_token_estimate_preserves_the_materialized_depth_limit() {
        let mut within_limit = json!("leaf");
        for _ in 0..128 {
            within_limit = json!([within_limit]);
        }
        assert_eq!(
            estimate_json_tokens(&within_limit),
            materialized_estimate_json_tokens(&within_limit)
        );
        assert!(estimate_json_tokens(&within_limit).is_some());

        let too_deep = json!([within_limit]);
        assert_eq!(
            estimate_json_tokens(&too_deep),
            materialized_estimate_json_tokens(&too_deep)
        );
        assert!(estimate_json_tokens(&too_deep).is_none());
    }

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

    #[test]
    fn incremental_estimate_matches_materialized_request_estimates() {
        let root = Map::from_iter([
            ("instructions".to_owned(), json!("be brief ☃")),
            ("tools".to_owned(), json!([{"type":"function","name":"f"}])),
        ]);
        let items = [
            message("plain \"quoted\" text\n"),
            json!({"type":"function_call","call_id":"c","arguments":"{}"}),
            json!({"type":"input_image","image_url":{"url":"data:private","detail":"high"}}),
            json!({"type":"message","content":[{"type":"output_image","image_data":"private"}]}),
            message(&"🌎".repeat(33)),
        ];
        let active = [message("active")];
        let mut estimate = IncrementalEstimate::new(
            &input_view_value(&final_body(&root, Some("xx"), &[], &active, &[])),
            2,
        );
        let mut summary = IncrementalEstimate::new(&summary_body("m", &[], MAP_PROMPT, 7), 1);
        let mut retained = Vec::new();
        for item in items {
            let cost = [json_cost(&item, 2).unwrap()];
            let summary_cost = [json_cost(&summary_item(item.clone()), 2).unwrap()];
            retained.insert(0, vec![item]);
            let direct = estimate_json_tokens(&input_view_value(&final_body(
                &root,
                Some("xx"),
                &retained,
                &active,
                &[],
            )));
            assert_eq!(estimate.tokens_with(&cost), direct);
            assert_eq!(
                summary.tokens_with(&summary_cost),
                estimate_json_tokens(&summary_body("m", &retained, MAP_PROMPT, 7))
            );
            estimate.extend(&cost);
            summary.extend(&summary_cost);
        }
    }

    #[test]
    fn tool_summary_serialization_matches_python_json_spacing() {
        let call = json!({
            "type":"function_call","call_id":"c1",
            "arguments":{"query":"weather ☃"}
        });
        assert_eq!(
            python_json_text(&call),
            "{\"arguments\": {\"query\": \"weather ☃\"}, \"call_id\": \"c1\", \"type\": \"function_call\"}"
        );
    }

    #[test]
    fn compaction_of_many_small_messages_is_linear() {
        let mut input = (0..20_000)
            .map(|index| message(&format!("small message {index}")))
            .collect::<Vec<_>>();
        input.push(message("active request"));
        let body = json!({"model":"external/model","max_output_tokens":64,"input":input});
        let started = std::time::Instant::now();
        let mut calls = 0;
        let compacted = compact_with(&body, &Map::new(), 400_000, |_| {
            calls += 1;
            Ok("checkpoint".to_owned())
        })
        .unwrap();
        let elapsed = started.elapsed();
        // The quadratic implementation took about a minute here even in release.
        assert!(elapsed.as_secs() < 10, "compaction took {elapsed:?}");
        assert!((1..=4).contains(&calls));
        assert!(compacted.to_string().contains("active request"));
    }

    #[test]
    fn compaction_keeps_recent_tool_pairs_and_preserves_summary_shape() {
        let body = json!({
            "model":"m","max_output_tokens":64,"instructions":"keep these instructions",
            "tools":[{"type":"function","name":"lookup"}],
            "input":[
                {"type":"message","role":"user","turn_id":"old-1",
                    "content":[{"type":"input_text","text":"old ".repeat(330)}]},
                {"type":"message","role":"user","turn_id":"old-2",
                    "content":[{"type":"input_text","text":"old ".repeat(330)}]},
                {"type":"message","role":"user","turn_id":"old-3",
                    "content":[{"type":"input_text","text":"old ".repeat(330)}]},
                {"type":"message","role":"user","turn_id":"old-4",
                    "content":[{"type":"input_text","text":"old ".repeat(330)}]},
                {"type":"function_call","call_id":"c1","turn_id":"tool",
                    "name":"lookup","arguments":"{\"query\":\"weather\"}"},
                {"type":"function_call_output","call_id":"c1","turn_id":"tool",
                    "output":"r".repeat(180)},
                {"type":"message","role":"user","turn_id":"active",
                    "content":[{"type":"input_text","text":"active request"}]},
                {"type":"compaction_trigger"}
            ],
            "_emp_active_input_start":6
        });
        let mut requests = Vec::new();
        let result = compact_with(&body, &Map::new(), 1_200, |request| {
            requests.push(request.clone());
            Ok(format!("summary {}", requests.len()))
        })
        .expect("compaction succeeds");

        let input = result["input"].as_array().expect("compacted input");
        assert!(!requests.is_empty());
        assert!(requests.iter().all(|request| {
            request["stream"] == false
                && request["tools"] == json!([])
                && request["max_output_tokens"].as_u64().unwrap() > 0
                && !request.to_string().contains("c1")
        }));
        assert!(
            requests
                .iter()
                .any(|request| request.to_string().contains("old "))
        );
        assert!(result.to_string().contains("keep these instructions"));
        assert!(result.to_string().contains("summary "));
        assert!(result.to_string().contains("active request"));
        assert_eq!(
            input.iter().filter(|item| item["call_id"] == "c1").count(),
            2
        );
        assert_eq!(
            input
                .iter()
                .filter(|item| item["turn_id"]
                    .as_str()
                    .is_some_and(|turn| turn.starts_with("old-")))
                .count(),
            1
        );
        assert_eq!(input.last().unwrap()["type"], "compaction_trigger");
    }

    #[test]
    fn compaction_keeps_summary_and_unit_failure_reasons() {
        let history = (0..20)
            .map(|index| message(&format!("history {index} {}", "x".repeat(300))))
            .collect::<Vec<_>>();
        let mut input = history;
        input.push(message("active request"));
        let body = json!({
            "model":"m","max_output_tokens":64,
            "input":input,
            "_emp_active_input_start":20
        });
        assert_eq!(
            compact_with(&body, &Map::new(), 1_000, |_| Err(())),
            Err("summary_call_failed")
        );

        let oversized = json!({
            "model":"m","max_output_tokens":64,
            "input":[message(&"large ".repeat(2_000)), message("active request")],
            "_emp_active_input_start":1
        });
        let mut calls = 0;
        assert_eq!(
            compact_with(&oversized, &Map::new(), 1_000, |_| {
                calls += 1;
                Ok("checkpoint".to_owned())
            }),
            Err("compaction_unit_too_large")
        );
        assert_eq!(calls, 0);
    }

    #[test]
    fn compaction_rejects_unbounded_work() {
        let mut input = (0..MAX_COMPACTION_UNITS + 1)
            .map(|_| message("u"))
            .collect::<Vec<_>>();
        input.push(message("active request"));
        let body = json!({"model":"external/model","input":input});
        assert_eq!(
            compact_with(&body, &Map::new(), 4096, |_| Ok("checkpoint".to_owned())),
            Err("compaction_unit_too_large")
        );
        let mut input = (0..2_000)
            .map(|index| message(&format!("{index} {}", "m".repeat(200))))
            .collect::<Vec<_>>();
        input.push(message("active request"));
        let body = json!({"model":"external/model","max_output_tokens":64,"input":input});
        let mut calls = 0;
        assert_eq!(
            compact_with(&body, &Map::new(), 600, |_| {
                calls += 1;
                Ok("checkpoint".to_owned())
            }),
            Err("compaction_unit_too_large")
        );
        assert!(calls <= MAX_SUMMARY_REQUESTS);
    }
}
