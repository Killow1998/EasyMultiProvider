use crate::services::claude_cli::ClaudeCliError;
use crate::util::random_hex;
use emp_core::ResolvedRoute;
use emp_protocol::tool_bridge::ExternalTools;
use emp_router::{CompleteResponse, ProjectionIds, response_json_stream_events};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) const MAX_TRANSCRIPT_BYTES: usize = 10 * 1024 * 1024;
const MAX_TOOL_SCHEMA_BYTES: usize = 64 * 1024;

pub(super) const SYSTEM_PROMPT: &str = "You are a one-request compatibility bridge for Codex. The user message contains the ordered Codex Responses conversation. In multimodal input, each text record identifies its input item and content part; the native image or document block immediately following a media record belongs to that exact part. Read the text and associated native media together, then infer the next assistant answer and any Codex tool proposals. Return only the required structured object. Preserve tool names, namespaces, and arguments from the conversation. Do not claim a tool result unless it appears in the conversation. Do not execute tools, access files, or contact services.";

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

pub(super) fn transcript(body: &Value) -> Result<Vec<u8>, &'static str> {
    let transcript = normalized_transcript(body);
    if has_unsupported_input_modality(&transcript) {
        return Err("unsupported_input_modality");
    }
    let encoded = serde_json::to_vec(&transcript).map_err(|_| "claude_cli_invalid_transcript")?;
    if encoded.len() > MAX_TRANSCRIPT_BYTES {
        return Err("claude_cli_input_too_large");
    }
    Ok(encoded)
}

pub(super) fn normalized_transcript(body: &Value) -> Value {
    let mut transcript = body.clone();
    transcript["stream"] = Value::Bool(false);
    sanitize_private_reasoning_items(&mut transcript);
    transcript
}

fn has_unsupported_input_modality(body: &Value) -> bool {
    fn unsupported_part(value: &Value) -> bool {
        matches!(
            value.get("type").and_then(Value::as_str),
            Some("input_image" | "input_file" | "input_audio" | "input_video")
        )
    }
    fn unsupported_content_array(value: Option<&Value>) -> bool {
        value
            .and_then(Value::as_array)
            .is_some_and(|parts| parts.iter().any(unsupported_part))
    }
    let Some(input) = body.get("input") else {
        return false;
    };
    let items: Vec<&Value> = match input {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![input],
        _ => return false,
    };
    items.into_iter().any(|item| {
        if unsupported_part(item) {
            return true;
        }
        let item_type = item.get("type").and_then(Value::as_str);
        let is_message = item_type == Some("message")
            || (item_type.is_none() && item.get("role").and_then(Value::as_str).is_some());
        (is_message && unsupported_content_array(item.get("content")))
            || (matches!(
                item_type,
                Some("function_call_output" | "custom_tool_call_output")
            ) && unsupported_content_array(item.get("output")))
    })
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

pub(super) fn proposal_schema() -> Result<String, &'static str> {
    let schema = json!({
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
    });
    let encoded = serde_json::to_string(&schema).map_err(|_| "claude_cli_invalid_schema")?;
    if encoded.len() > MAX_TOOL_SCHEMA_BYTES {
        return Err("claude_cli_schema_too_large");
    }
    Ok(encoded)
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

pub(super) fn parse_cli_result(stdout: &[u8]) -> Result<Value, &'static str> {
    let output: Value = match serde_json::from_slice(stdout) {
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
    Ok(output)
}

pub(super) fn response_from_cli(
    cli_output: &Value,
    route: &ResolvedRoute,
    request: &Value,
    ids: &ProjectionIds,
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
    response_json_stream_events(response.clone(), ids, true).map_err(ClaudeCliError::Router)?;
    Ok(CompleteResponse {
        reported_model: None,
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

fn cli_usage(output: &Value) -> Value {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_responses_reasoning_items_and_preserves_user_fields() {
        let source = json!({
            "reasoning":{"effort":"high","encrypted_content":"ordinary setting"},
            "input":[
                {"type":"reasoning","encrypted_content":"private state"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"keep"}]},
                {"type":"function_call","call_id":"call-1","name":"read","arguments":"{\"reasoning\":\"argument\",\"encrypted_content\":\"argument value\",\"type\":\"thinking\"}"},
                {"type":"function_call_output","call_id":"call-1","output":{"reasoning":"result field","encrypted_content":"result value","content":[{"type":"thinking","text":"ordinary result"}]}}
            ],
            "tools":[{"type":"function","name":"schema","parameters":{"properties":{"reasoning":{"type":"string"},"encrypted_content":{"type":"string"}}}}]
        });
        let encoded = transcript(&source).expect("transcript");
        let value: Value = serde_json::from_slice(&encoded).expect("JSON");
        assert_eq!(value["reasoning"]["effort"], "high");
        assert_eq!(value["reasoning"]["encrypted_content"], "ordinary setting");
        assert_eq!(value["input"].as_array().unwrap().len(), 3);
        assert_eq!(value["input"][2]["output"]["reasoning"], "result field");
        assert_eq!(
            value["input"][2]["output"]["encrypted_content"],
            "result value"
        );
        assert_eq!(
            value["input"][2]["output"]["content"][0]["type"],
            "thinking"
        );
        assert_eq!(
            value["input"][1]["arguments"],
            source["input"][2]["arguments"]
        );
        assert_eq!(
            value["tools"][0]["parameters"]["properties"]["reasoning"]["type"],
            "string"
        );
        assert!(!value.to_string().contains("private state"));
    }

    #[test]
    fn rejects_typed_multimodal_input_but_keeps_schema_and_tool_result_json() {
        let unsupported = json!({
            "input":[{"type":"message","role":"user","content":[
                {"type":"input_text","text":"hello"},
                {"type":"input_image","image_url":"data:image/png;base64,not-a-real-image"}
            ]}],
            "tools":[{"type":"function","name":"file","parameters":{"properties":{"image":{"type":"string"},"audio":{"type":"string"}}}}]
        });
        assert_eq!(transcript(&unsupported), Err("unsupported_input_modality"));

        let text_only = json!({
            "input":[
                {"type":"function_call_output","call_id":"call-1","output":{"type":"input_image","ordinary_result":true}},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"still text"}]}
            ],
            "tools":[{"type":"function","name":"schema","parameters":{"properties":{"input_file":{"type":"string"}}}}]
        });
        let encoded = transcript(&text_only).expect("ordinary JSON is preserved");
        let projected: Value = serde_json::from_slice(&encoded).expect("JSON transcript");
        assert_eq!(projected["input"][0]["output"]["type"], "input_image");
        assert_eq!(
            projected["tools"][0]["parameters"]["properties"]["input_file"]["type"],
            "string"
        );

        let role_only_message = json!({
            "input":[{"role":"user","content":[
                {"type":"input_image","image_url":"data:image/png;base64,not-a-real-image"}
            ]}]
        });
        assert_eq!(
            transcript(&role_only_message),
            Err("unsupported_input_modality")
        );

        let tool_output_content = json!({
            "input":[{"type":"function_call_output","call_id":"call-1","output":[
                {"type":"input_file","file_id":"file_fake"}
            ]}]
        });
        assert_eq!(
            transcript(&tool_output_content),
            Err("unsupported_input_modality")
        );
    }

    #[test]
    fn cli_usage_reuses_anthropic_cache_projection_and_keeps_unknowns_unknown() {
        let route = test_route();
        let request = json!({"model":"m","input":"hello","tools":[]});
        let ids = ProjectionIds::new("resp_test", "msg_test", "rs_test", "late_test");
        let cli_output = json!({
            "structured_output":{"answer":"answer","tool_calls":[]},
            "usage":{
                "input_tokens":12,
                "cache_read_input_tokens":7000,
                "cache_creation_input_tokens":3000,
                "cache_creation":{"ephemeral_1h_input_tokens":2500},
                "output_tokens":3
            }
        });
        let response = response_from_cli(&cli_output, &route, &request, &ids, 200)
            .expect("project cached usage");
        assert_eq!(
            response.body["usage"],
            json!({
                "input_tokens":10012,
                "input_tokens_details":{
                    "cached_tokens":7000,
                    "cache_creation_tokens":3000,
                    "cache_creation_1h_tokens":2500
                },
                "output_tokens":3,
                "total_tokens":10015
            })
        );

        let missing_usage = json!({
            "structured_output":{"answer":"answer","tool_calls":[]}
        });
        let response = response_from_cli(&missing_usage, &route, &request, &ids, 200)
            .expect("project response without usage");
        assert!(response.body["usage"].is_null());

        assert_eq!(cli_usage(&json!({"usage":{}})), json!({}));
    }

    #[test]
    fn proposal_is_projected_through_external_tool_identity_bridge() {
        let route = test_route();
        let request = json!({"model":"m","input":[{"type":"message","role":"user","content":"go"}],"tools":[{"type":"namespace","name":"fs","tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]}]});
        let cli_output = json!({
            "structured_output":{
                "answer":"",
                "tool_calls":[{"type":"function_call","name":"read","namespace":"fs","arguments":{"path":"/tmp/a"},"input":""}]
            },
            "usage":{"input_tokens":4,"output_tokens":3}
        });
        let ids = ProjectionIds::new("resp_test", "msg_test", "rs_test", "late_test");
        let response =
            response_from_cli(&cli_output, &route, &request, &ids, 200).expect("projection");
        assert_eq!(response.body["output"][0]["type"], "function_call");
        assert_eq!(response.body["output"][0]["namespace"], "fs");
        assert_eq!(response.body["output"][0]["name"], "read");
        assert_eq!(
            response.body["output"][0]["arguments"],
            "{\"path\":\"/tmp/a\"}"
        );
    }

    #[test]
    fn structured_output_rejects_extra_fields_and_tools_disallowed_by_choice() {
        let route = test_route();
        let ids = ProjectionIds::new("resp_test", "msg_test", "rs_test", "late_test");
        let request = json!({
            "model":"m",
            "input":"go",
            "tools":[{"type":"function","name":"read","parameters":{"type":"object"}}],
            "tool_choice":"none"
        });
        let cli_output = json!({
            "structured_output":{
                "answer":"",
                "tool_calls":[{"type":"function_call","name":"read","namespace":"","arguments":{"path":"/tmp/a"},"input":""}]
            }
        });
        assert!(matches!(
            response_from_cli(&cli_output, &route, &request, &ids, 200),
            Err(ClaudeCliError::Failure("claude_cli_unknown_tool_proposal"))
        ));

        let invalid = json!({
            "structured_output":{"answer":"ok","tool_calls":[],"unexpected":true}
        });
        assert!(matches!(
            response_from_cli(&invalid, &route, &request, &ids, 200),
            Err(ClaudeCliError::Failure(
                "claude_cli_invalid_structured_output"
            ))
        ));
    }

    #[test]
    fn nullable_effort_means_no_override_and_explicit_values_keep_their_meaning() {
        for body in [
            json!({}),
            json!({"reasoning":null}),
            json!({"reasoning":{}}),
            json!({"reasoning":{"effort":null}}),
        ] {
            assert_eq!(effort(&body), Ok(None), "optional effort: {body}");
        }
        for value in ["low", "medium", "high", "xhigh", "max"] {
            assert_eq!(
                effort(&json!({"reasoning":{"effort":value}})),
                Ok(Some(value))
            );
        }
        assert_eq!(
            effort(&json!({"reasoning":{"effort":"none"}})),
            Err("unsupported_reasoning_effort_none")
        );
        for value in [
            json!("minimal"),
            json!("disabled"),
            json!("unknown"),
            json!(""),
            json!(0),
            json!(false),
            json!([]),
            json!({}),
        ] {
            assert_eq!(
                effort(&json!({"reasoning":{"effort":value}})),
                Err("unsupported_reasoning_effort")
            );
        }
    }

    #[test]
    fn effort_diagnostics_report_only_known_enums_or_types() {
        assert_eq!(effort_diagnostic(&json!({})), "omitted");
        assert_eq!(
            effort_diagnostic(&json!({"reasoning":{"effort":null}})),
            "null"
        );
        for value in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            assert_eq!(
                effort_diagnostic(&json!({"reasoning":{"effort":value}})),
                value
            );
        }
        for (value, label) in [
            (json!("private arbitrary input"), "unknown_string"),
            (json!(false), "boolean"),
            (json!(0), "number"),
            (json!(["private input"]), "array"),
            (json!({"private":"input"}), "object"),
        ] {
            assert_eq!(
                effort_diagnostic(&json!({"reasoning":{"effort":value}})),
                label
            );
        }
    }

    #[test]
    fn cli_result_and_effort_mapping_reject_unusable_values() {
        let stdout = serde_json::to_vec(&json!({
            "type":"result",
            "structured_output":{"answer":"ok","tool_calls":[]}
        }))
        .expect("CLI result");
        let parsed = parse_cli_result(&stdout).expect("structured CLI result");
        assert_eq!(parsed["structured_output"]["answer"], "ok");
        assert_eq!(
            parse_cli_result(br#"{"type":"result","is_error":true,"structured_output":{}}"#),
            Err("claude_cli_result_error")
        );
        let jsonl = br#"{"type":"system","subtype":"init"}
{"type":"result","subtype":"success","structured_output":{"answer":"ok","tool_calls":[]}}
"#;
        assert_eq!(
            parse_cli_result(jsonl).unwrap()["structured_output"]["answer"],
            "ok"
        );
        assert_eq!(
            effort(&json!({"reasoning":{"effort":"low"}})),
            Ok(Some("low"))
        );
        assert_eq!(
            effort(&json!({"reasoning":{"effort":"xhigh"}})),
            Ok(Some("xhigh"))
        );
        assert_eq!(
            effort(&json!({"reasoning":{"effort":"disabled"}})),
            Err("unsupported_reasoning_effort")
        );
    }

    fn test_route() -> ResolvedRoute {
        let mut provider = serde_json::Map::new();
        provider.insert("id".to_owned(), json!("p"));
        provider.insert("base_url".to_owned(), json!("http://127.0.0.1/v1"));
        provider.insert("auth_mode".to_owned(), json!("api_key"));
        provider.insert("api_key".to_owned(), json!("key"));
        provider.insert("protocol".to_owned(), json!("anthropic_messages"));
        let mut model = serde_json::Map::new();
        model.insert("id".to_owned(), json!("m"));
        model.insert("provider".to_owned(), json!("p"));
        ResolvedRoute::new(
            "m",
            "upstream-m",
            emp_core::RouteSource::ExplicitModel,
            provider,
            model,
            emp_core::Protocol::AnthropicMessages,
            emp_core::Dialect::PortableResponses,
            "p",
            format!("sha256:{}", "a".repeat(64)),
            "deployment",
        )
        .expect("route")
    }
}
