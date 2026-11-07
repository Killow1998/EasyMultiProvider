//! CLI result validation, tool identity restoration and Codex response projection.
use super::tools::{ToolKind, available_tools, resolve_tool};
use crate::services::claude_cli::ClaudeCliError;
use crate::util::random_hex;
use emp_core::ResolvedRoute;
use emp_protocol::tool_bridge::ExternalTools;
use emp_router::CompleteResponse;
use serde_json::{Value, json};

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
    let available = available_tools(request);
    let mut proposed_input = Vec::with_capacity(proposal.calls.len());
    for call in proposal.calls {
        let tool = resolve_tool(&available, call.namespace, call.name, call.kind)
            .ok_or(ClaudeCliError::Failure("claude_cli_unknown_tool_proposal"))?;
        let name = tool.name.as_str();
        let namespace = tool.namespace.as_str();
        let call_id = next_id("call")?;
        proposed_input.push(match call.kind {
            ToolKind::Function => json!({"type":"function_call","name":name,"namespace":namespace,"call_id":call_id,"arguments":call.arguments}),
            ToolKind::Custom => json!({"type":"custom_tool_call","name":name,"namespace":namespace,"call_id":call_id,"input":call.input,"execution":"client"}),
            ToolKind::Search => json!({"type":"tool_search_call","call_id":call_id,"arguments":call.arguments,"execution":"client"}),
        });
    }
    let mut bridge_input = bridge
        .prepare(&json!({"input":proposed_input}))
        .map_err(|_| ClaudeCliError::Failure("claude_cli_invalid_tool_proposal"))?;
    let mapped = bridge_input
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or(ClaudeCliError::Failure("claude_cli_invalid_tool_proposal"))?;
    let mut output = Vec::new();
    let answer = proposal.answer;
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

struct Proposal<'a> {
    answer: &'a str,
    calls: Vec<ToolProposal<'a>>,
}

struct ToolProposal<'a> {
    kind: ToolKind,
    name: &'a str,
    namespace: &'a str,
    arguments: &'a serde_json::Map<String, Value>,
    input: &'a str,
}

fn parse_structured_proposal(output: &Value) -> Result<Proposal<'_>, ClaudeCliError> {
    let object = output
        .get("structured_output")
        .and_then(Value::as_object)
        .ok_or(ClaudeCliError::Failure(
            "claude_cli_missing_structured_output",
        ))?;
    let invalid = || ClaudeCliError::Failure("claude_cli_invalid_structured_output");
    if object.len() != 2 {
        return Err(invalid());
    }
    let answer = object
        .get("answer")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let calls = object
        .get("tool_calls")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let invalid = || ClaudeCliError::Failure("claude_cli_invalid_tool_proposal");
    let mut proposals = Vec::with_capacity(calls.len());
    for call in calls {
        let call = call
            .as_object()
            .filter(|call| call.len() == 5)
            .ok_or_else(invalid)?;
        let kind = match call.get("type").and_then(Value::as_str) {
            Some("function_call") => ToolKind::Function,
            Some("custom_tool_call") => ToolKind::Custom,
            Some("tool_search_call") => ToolKind::Search,
            _ => return Err(invalid()),
        };
        proposals.push(ToolProposal {
            kind,
            name: call
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(invalid)?,
            namespace: call
                .get("namespace")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?,
            arguments: call
                .get("arguments")
                .and_then(Value::as_object)
                .ok_or_else(invalid)?,
            input: call
                .get("input")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?,
        });
    }
    Ok(Proposal {
        answer,
        calls: proposals,
    })
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
