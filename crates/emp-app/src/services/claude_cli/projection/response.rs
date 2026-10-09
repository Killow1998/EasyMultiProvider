//! CLI result validation, tool identity restoration and Codex response projection.
use crate::services::claude_cli::ClaudeCliError;
use crate::util::random_hex;
use emp_core::ResolvedRoute;
use emp_protocol::tool_bridge::ExternalTools;
use emp_router::CompleteResponse;
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ToolIdentity {
    namespace: String,
    name: String,
    kind: ToolKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ToolKind {
    Function,
    Custom,
    Search,
}

pub(in crate::services::claude_cli) fn parse_cli_result(
    stdout: &[u8],
) -> Result<Value, &'static str> {
    let mut output: Value = match serde_json::from_slice(stdout) {
        Ok(output) => output,
        Err(_) => stdout
            .split(|byte| *byte == b'\n')
            .rev()
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .find(|event| event.get("type").and_then(Value::as_str) == Some("result"))
            .ok_or("claude_cli_invalid_output")?,
    };
    if output.get("is_error").and_then(Value::as_bool) == Some(true) {
        return Err("claude_cli_result_error");
    }
    output
        .get("structured_output")
        .filter(|value| value.is_object())
        .ok_or("claude_cli_missing_structured_output")?;
    let mut observation = emp_router::model_observation::ModelObservation::default();
    for line in stdout.split(|byte| *byte == b'\n') {
        if let Ok(event) = serde_json::from_slice::<Value>(line)
            && matches!(event["type"].as_str(), Some("assistant" | "stream_event"))
        {
            observation.observe(&event, false);
            observation.observe(&event["event"], false);
        }
    }
    output["_emp_model_observation"] = observation.project("");
    Ok(output)
}

pub(in crate::services::claude_cli) fn response_from_cli(
    cli_output: &Value,
    route: &ResolvedRoute,
    request: &Value,
    provider_status: u16,
) -> Result<CompleteResponse, ClaudeCliError> {
    let proposal = parse_structured_proposal(cli_output)?;
    let mut bridge = ExternalTools::default();
    bridge
        .prepare_or_borrow(request)
        .map_err(|_| ClaudeCliError::Failure("claude_cli_invalid_tool_history"))?;
    let available = collect_tools(request)
        .into_iter()
        .filter(|tool| tool_choice_allows(request.get("tool_choice"), tool))
        .collect::<BTreeSet<_>>();
    let raw_proposals = proposal["tool_calls"]
        .as_array()
        .ok_or(ClaudeCliError::Failure(
            "claude_cli_invalid_structured_output",
        ))?;
    let mut proposed_input = Vec::with_capacity(raw_proposals.len());
    for raw in raw_proposals {
        let kind = match raw.get("type").and_then(Value::as_str) {
            Some("function_call") => ToolKind::Function,
            Some("custom_tool_call") => ToolKind::Custom,
            Some("tool_search_call") => ToolKind::Search,
            _ => return Err(ClaudeCliError::Failure("claude_cli_invalid_tool_proposal")),
        };
        let name = raw
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or(ClaudeCliError::Failure("claude_cli_invalid_tool_proposal"))?;
        let namespace = raw
            .get("namespace")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !available.contains(&ToolIdentity {
            namespace: namespace.to_owned(),
            name: name.to_owned(),
            kind,
        }) {
            return Err(ClaudeCliError::Failure("claude_cli_unknown_tool_proposal"));
        }
        let item = match kind {
            ToolKind::Function => {
                let arguments = raw
                    .get("arguments")
                    .filter(|value| value.is_object())
                    .ok_or(ClaudeCliError::Failure("claude_cli_invalid_tool_proposal"))?;
                json!({"type":"function_call","name":name,"namespace":namespace,"call_id":next_id("call")?,"arguments":arguments})
            }
            ToolKind::Custom => {
                let input = raw
                    .get("input")
                    .and_then(Value::as_str)
                    .ok_or(ClaudeCliError::Failure("claude_cli_invalid_tool_proposal"))?;
                json!({"type":"custom_tool_call","name":name,"namespace":namespace,"call_id":next_id("call")?,"input":input,"execution":"client"})
            }
            ToolKind::Search => {
                let arguments = raw
                    .get("arguments")
                    .filter(|value| value.is_object())
                    .ok_or(ClaudeCliError::Failure("claude_cli_invalid_tool_proposal"))?;
                json!({"type":"tool_search_call","call_id":next_id("call")?,"arguments":arguments,"execution":"client"})
            }
        };
        proposed_input.push(item);
    }
    let mut bridge_input = bridge
        .prepare(&json!({"input":proposed_input}))
        .map_err(|_| ClaudeCliError::Failure("claude_cli_invalid_tool_proposal"))?;
    let mapped = bridge_input
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or(ClaudeCliError::Failure("claude_cli_invalid_tool_proposal"))?;
    let mut output = Vec::new();
    let answer = proposal["answer"].as_str().ok_or(ClaudeCliError::Failure(
        "claude_cli_invalid_structured_output",
    ))?;
    if !answer.is_empty() {
        output.push(json!({
            "id":next_id("msg")?,
            "type":"message",
            "status":"completed",
            "role":"assistant",
            "content":[{"type":"output_text","text":answer,"annotations":[]}]
        }));
    }
    for mapped_item in mapped {
        let id = next_id("item")?;
        let item = match mapped_item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let arguments = mapped_item
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let arguments = match arguments {
                    Value::String(arguments) => arguments,
                    arguments => serde_json::to_string(&arguments)
                        .map_err(|_| ClaudeCliError::Failure("claude_cli_invalid_tool_proposal"))?,
                };
                json!({
                    "id":id,"type":"function_call","status":"completed",
                    "call_id":mapped_item.get("call_id").cloned().unwrap_or(Value::Null),
                    "name":mapped_item.get("name").cloned().unwrap_or(Value::Null),
                    "arguments":arguments
                })
            }
            Some("custom_tool_call") => json!({
                "id":id,"type":"custom_tool_call","status":"completed",
                "call_id":mapped_item.get("call_id").cloned().unwrap_or(Value::Null),
                "name":mapped_item.get("name").cloned().unwrap_or(Value::Null),
                "input":mapped_item.get("input").cloned().unwrap_or_else(|| Value::String(String::new()))
            }),
            _ => return Err(ClaudeCliError::Failure("claude_cli_invalid_tool_proposal")),
        };
        output.push(item);
    }
    let usage = cli_usage(cli_output);
    let mut response = json!({
        "id":next_id("resp")?,
        "object":"response",
        "created_at":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),
        "status":"completed",
        "model":route.requested_model,
        "output":output,
        "usage":usage
    });
    response = bridge
        .restore_response(response)
        .map_err(|_| ClaudeCliError::Failure("claude_cli_tool_projection_failed"))?;
    emp_protocol::portable_responses::validate_responses_body(&response, true)
        .map_err(|error| ClaudeCliError::Router(error.into()))?;
    let mut observation = emp_router::model_observation::ModelObservation::default();
    for declaration in cli_output["_emp_model_observation"]["model_declarations"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let (Some(model), Some(source)) = (
            declaration["model"].as_str(),
            declaration["source"].as_str(),
        ) {
            observation.observe_model(model, source, declaration["terminal"] == true);
        }
    }
    Ok(CompleteResponse {
        observation,
        status: provider_status,
        content_type: "application/json".to_owned(),
        body: response,
    })
}

fn parse_structured_proposal(output: &Value) -> Result<Value, ClaudeCliError> {
    let proposal = output
        .get("structured_output")
        .filter(|proposal| proposal.is_object())
        .ok_or(ClaudeCliError::Failure(
            "claude_cli_missing_structured_output",
        ))?;
    let object = proposal.as_object().ok_or(ClaudeCliError::Failure(
        "claude_cli_invalid_structured_output",
    ))?;
    if object.len() != 2 || !object.contains_key("answer") || !object.contains_key("tool_calls") {
        return Err(ClaudeCliError::Failure(
            "claude_cli_invalid_structured_output",
        ));
    }
    if !proposal.get("answer").is_some_and(Value::is_string)
        || !proposal.get("tool_calls").is_some_and(Value::is_array)
    {
        return Err(ClaudeCliError::Failure(
            "claude_cli_invalid_structured_output",
        ));
    }
    for call in proposal["tool_calls"].as_array().expect("checked array") {
        let Some(call) = call.as_object() else {
            return Err(ClaudeCliError::Failure("claude_cli_invalid_tool_proposal"));
        };
        if call.len() != 5
            || !["type", "name", "namespace", "arguments", "input"]
                .iter()
                .all(|field| call.contains_key(*field))
            || !call.get("type").is_some_and(Value::is_string)
            || !call.get("name").is_some_and(Value::is_string)
            || !call.get("namespace").is_some_and(Value::is_string)
            || !call.get("arguments").is_some_and(Value::is_object)
            || !call.get("input").is_some_and(Value::is_string)
        {
            return Err(ClaudeCliError::Failure("claude_cli_invalid_tool_proposal"));
        }
    }
    Ok(proposal.clone())
}

fn collect_tools(body: &Value) -> BTreeSet<ToolIdentity> {
    let mut tools = BTreeSet::new();
    collect_tool_definitions(body.get("tools"), "", &mut tools);
    if let Some(items) = body.get("input").and_then(Value::as_array) {
        for item in items {
            if matches!(
                item.get("type").and_then(Value::as_str),
                Some("additional_tools" | "tool_search_output")
            ) {
                collect_tool_definitions(item.get("tools"), "", &mut tools);
            }
        }
    }
    tools
}

fn tool_choice_allows(choice: Option<&Value>, tool: &ToolIdentity) -> bool {
    match choice {
        None | Some(Value::Null) => true,
        Some(Value::String(value)) => value != "none",
        Some(Value::Object(choice)) => match choice.get("type").and_then(Value::as_str) {
            Some("allowed_tools") => {
                choice
                    .get("tools")
                    .and_then(Value::as_array)
                    .is_some_and(|tools| {
                        tools.iter().any(|allowed| {
                            let name = allowed.get("name").and_then(Value::as_str);
                            let namespace = allowed
                                .get("namespace")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            name == Some(tool.name.as_str())
                                && namespace == tool.namespace
                                && match allowed.get("type").and_then(Value::as_str) {
                                    Some("custom") => tool.kind == ToolKind::Custom,
                                    Some("tool_search") => tool.kind == ToolKind::Search,
                                    _ => tool.kind == ToolKind::Function,
                                }
                        })
                    })
            }
            Some("function" | "custom") => {
                choice.get("name").and_then(Value::as_str) == Some(tool.name.as_str())
                    && choice
                        .get("namespace")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        == tool.namespace
                    && (choice["type"] != "custom" || tool.kind == ToolKind::Custom)
            }
            Some("tool_search") => tool.kind == ToolKind::Search,
            _ => true,
        },
        Some(_) => false,
    }
}

fn collect_tool_definitions(
    value: Option<&Value>,
    namespace: &str,
    tools: &mut BTreeSet<ToolIdentity>,
) {
    let Some(items) = value.and_then(Value::as_array) else {
        return;
    };
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("namespace") => {
                if let Some(group) = item.get("name").and_then(Value::as_str) {
                    collect_tool_definitions(item.get("tools"), group, tools);
                }
            }
            Some("function" | "custom") => {
                let definition = item
                    .get("function")
                    .filter(|value| value.is_object())
                    .unwrap_or(item);
                if let Some(name) = definition.get("name").and_then(Value::as_str) {
                    tools.insert(ToolIdentity {
                        namespace: namespace.to_owned(),
                        name: name.to_owned(),
                        kind: if item["type"] == "custom" {
                            ToolKind::Custom
                        } else {
                            ToolKind::Function
                        },
                    });
                }
            }
            Some("tool_search")
                if item.get("execution").and_then(Value::as_str) == Some("client") =>
            {
                tools.insert(ToolIdentity {
                    namespace: namespace.to_owned(),
                    name: "tool_search".to_owned(),
                    kind: ToolKind::Search,
                });
            }
            _ => {}
        }
    }
}

pub(super) fn cli_usage(output: &Value) -> Value {
    output
        .get("usage")
        .filter(|usage| usage.is_object())
        .map(emp_protocol::anthropic_projection::anthropic_usage)
        .unwrap_or(Value::Null)
}

fn next_id(prefix: &str) -> Result<String, ClaudeCliError> {
    random_hex(16)
        .map(|value| format!("{prefix}_{value}"))
        .map_err(|_| ClaudeCliError::Failure("claude_cli_id_generation_failed"))
}
