//! Chunk packing, summary requests and Python-compatible tool serialization.
use super::units::{flatten, tool_role};
use crate::context::estimate::{IncrementalEstimate, JsonCost, json_cost};
use serde_json::{Value, json};

pub(super) const MAX_SUMMARY_REQUESTS: usize = 256;
pub(super) const MAP_PROMPT: &str = "Create a structured portable checkpoint from the visible history below.\nInclude only visible facts: objective, user constraints, decisions, completed work,\nfiles and code changed, relevant tool results, current state, failures, and remaining\nsteps. Preserve important identifiers and facts. Do not include hidden reasoning or\ninvented details. Return only the checkpoint text.";
const REDUCE_PROMPT: &str = "Merge the visible portable checkpoints and any visible history below\ninto one structured portable checkpoint. Preserve objective, user constraints,\ndecisions, completed work, files and code changed, relevant tool results, current\nstate, failures, and remaining steps. Do not include hidden reasoning or invented\ndetails. Return only the checkpoint text.";

pub(super) fn map_reduce<F>(
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
    let mut estimate = empty;
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
            estimate = empty;
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

pub(super) fn summary_item(item: Value) -> Value {
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
pub(super) fn python_json_text(value: &Value) -> String {
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

pub(super) fn summary_body(
    model: &str,
    units: &[Vec<Value>],
    prompt: &str,
    output_limit: u64,
) -> Value {
    let mut input = flatten(units)
        .into_iter()
        .map(summary_item)
        .collect::<Vec<_>>();
    input.push(message(prompt));
    json!({"model":model,"input":input,"stream":false,"tools":[],"max_output_tokens":output_limit})
}

pub(super) fn message(text: &str) -> Value {
    json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]})
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
