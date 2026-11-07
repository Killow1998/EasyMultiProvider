use serde_json::{Value, json};

pub(super) const MAX_TRANSCRIPT_BYTES: usize = emp_transport::MAX_EXPANDED_REQUEST_BYTES;

pub(super) const SYSTEM_PROMPT: &str = "You are the reasoning model for a Codex conversation. Codex owns the complete persistent history and executes all user tools. The user prompt supplies its ordered history and available tools. Answer its latest user request. To use Codex tools, call mcp__emp_codex__codex with {\"answer\":\"optional user-visible text\",\"tool_calls\":[...]}, preserving names, namespaces, kinds and arguments from that context. Codex returns actual results through MCP. Use only tools allowed by the current context and tool_choice. Never execute local tools or claim unseen results. Continue the same user turn after tool results. Write the final answer as normal text, without JSON wrappers or tool-result narration.";

pub(super) fn normalized_transcript(body: &Value) -> Value {
    let mut transcript = body.clone();
    transcript["stream"] = Value::Bool(false);
    sanitize_private_reasoning_items(&mut transcript);
    transcript
}

fn sanitize_private_reasoning_items(body: &mut Value) {
    let Some(input) = body.get_mut("input") else {
        return;
    };
    match input {
        Value::Array(items) => {
            items.retain(|item| item.get("type").and_then(Value::as_str) != Some("reasoning"));
        }
        Value::Object(item) if item.get("type").and_then(Value::as_str) == Some("reasoning") => {
            *input = Value::Array(Vec::new());
        }
        _ => {}
    }
}

pub(super) fn proposal_schema() -> Value {
    // The MCP context carries the catalog; validate proposed identities against it.
    json!({
        "type":"object",
        "additionalProperties":false,
        "properties":{
            "answer":{"type":"string"},
            "tool_calls":{
                "type":"array",
                "items":{
                    "type":"object",
                    "additionalProperties":false,
                    "properties":{
                        "type":{"type":"string","enum":["function_call","custom_tool_call","tool_search_call"]},
                        "name":{"type":"string"},
                        "namespace":{"type":"string"},
                        "arguments":{"type":"object"},
                        "input":{"type":"string"}
                    },
                    "required":["type","name","namespace","arguments","input"]
                }
            }
        },
        "required":["answer","tool_calls"]
    })
}

pub(super) fn effort(body: &Value) -> Result<Option<&'static str>, &'static str> {
    let Some(value) = body
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
    else {
        return Ok(None);
    };
    // Responses represents an absent optional effort as either omission or
    // JSON null. Neither requests a CLI override; the string "none" does.
    if value.is_null() {
        return Ok(None);
    }
    match value.as_str() {
        Some("low") => Ok(Some("low")),
        Some("medium") => Ok(Some("medium")),
        Some("high") => Ok(Some("high")),
        Some("xhigh") => Ok(Some("xhigh")),
        Some("max") => Ok(Some("max")),
        Some("none") => Err("unsupported_reasoning_effort_none"),
        _ => Err("unsupported_reasoning_effort"),
    }
}

pub(super) fn effort_diagnostic(body: &Value) -> &'static str {
    match body
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
    {
        None => "omitted",
        Some(Value::Null) => "null",
        Some(Value::String(value)) => match value.as_str() {
            "none" => "none",
            "minimal" => "minimal",
            "low" => "low",
            "medium" => "medium",
            "high" => "high",
            "xhigh" => "xhigh",
            "max" => "max",
            _ => "unknown_string",
        },
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

pub(super) fn effort_for_model(
    body: &Value,
    model: &serde_json::Map<String, Value>,
) -> Result<Option<&'static str>, &'static str> {
    let selected = effort(body)?;
    let Some(level) = selected else {
        return Ok(None);
    };
    let levels = emp_state::normalize_reasoning_levels(model.get("reasoning_levels"));
    if model.get("supports_reasoning") == Some(&Value::Bool(false))
        || (!levels.is_empty() && !levels.iter().any(|allowed| allowed == level))
    {
        return Err("unsupported_model_reasoning_effort");
    }
    Ok(selected)
}

mod response;
mod tools;
pub(super) use response::response_from_cli;
#[cfg(test)]
mod tests;
