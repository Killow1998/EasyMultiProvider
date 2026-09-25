use super::*;

pub(super) fn visible_payload(record: &Map<String, Value>) -> Option<Map<String, Value>> {
    let kind = token(record.get("type"));
    let payload = record.get("payload").and_then(Value::as_object);
    if matches!(
        kind.as_str(),
        "session_meta" | "sessionmeta" | "turn_context" | "turncontext"
    ) {
        return payload.cloned();
    }
    if kind == "response_item" {
        return payload.cloned();
    }
    if kind == "event_msg" {
        let payload = payload?;
        let event = token(payload.get("type"));
        if matches!(event.as_str(), "user_message" | "usermessage") {
            return Some(Map::from_iter([
                ("type".to_owned(), json!("user_message")),
                (
                    "content".to_owned(),
                    json!({"text":payload.get("message"),"images":payload.get("images")}),
                ),
            ]));
        }
        if matches!(
            event.as_str(),
            "assistant_message" | "assistantmessage" | "agent_message" | "agentmessage"
        ) {
            return Some(Map::from_iter([
                ("type".to_owned(), json!("assistant_message")),
                (
                    "message".to_owned(),
                    payload.get("message").cloned().unwrap_or(Value::Null),
                ),
            ]));
        }
        return (!event.is_empty()).then(|| payload.clone());
    }
    if matches!(
        kind.as_str(),
        "compaction"
            | "compaction_summary"
            | "compacted"
            | "compacted_summary"
            | "compaction_marker"
            | "compaction_boundary"
    ) {
        return Some(
            payload
                .cloned()
                .unwrap_or_else(|| record.clone())
                .into_iter()
                .chain(std::iter::once(("type".to_owned(), Value::String(kind))))
                .collect(),
        );
    }
    record
        .get("item")
        .and_then(Value::as_object)
        .cloned()
        .or_else(|| record.get("type").is_some().then(|| record.clone()))
}

pub(super) fn normalize_visible_item(
    raw: &Map<String, Value>,
    turn: Option<String>,
) -> Option<VisibleItem> {
    if raw.get("visible") == Some(&Value::Bool(false))
        || raw.get("hidden") == Some(&Value::Bool(true))
    {
        return None;
    }
    let raw_type = raw.get("type")?.as_str()?.to_owned();
    let kind = token(raw.get("type"));
    if matches!(
        kind.as_str(),
        "analysis"
            | "chain_of_thought"
            | "hidden_reasoning"
            | "hidden_cot"
            | "reasoning"
            | "thinking"
    ) {
        return None;
    }
    let (visible_kind, content) = match kind.as_str() {
        "message" => match raw.get("role").and_then(Value::as_str) {
            Some("user") => (
                "user_message",
                first_value(raw, &["content", "text", "message"]),
            ),
            Some("assistant") => (
                "assistant_message",
                first_value(raw, &["content", "text", "message"]),
            ),
            _ => return None,
        },
        "user_message" | "usermessage" => (
            "user_message",
            first_value(raw, &["content", "message", "text", "images"]),
        ),
        "assistant_message" | "assistantmessage" | "agent_message" | "agentmessage" => (
            "assistant_message",
            first_value(raw, &["content", "message", "text"]),
        ),
        "function_call" | "custom_tool_call" | "tool_call" | "tool_search_call" => (
            "tool_call",
            selected_map(
                raw,
                &[
                    "name",
                    "namespace",
                    "arguments",
                    "input",
                    "tool",
                    "status",
                    "execution",
                ],
            ),
        ),
        "function_call_output"
        | "custom_tool_call_output"
        | "tool_search_output"
        | "tool_result"
        | "tool_output"
        | "mcp_tool_result" => {
            let standalone = kind == "function_call_output"
                && string_from(raw, &["call_id", "callId"]).is_none()
                && string_from(raw, &["name"]).is_some();
            (
                if standalone {
                    "standalone_tool_output"
                } else {
                    "tool_result"
                },
                selected_map(
                    raw,
                    &[
                        "name",
                        "namespace",
                        "output",
                        "result",
                        "content",
                        "status",
                        "execution",
                        "tools",
                    ],
                ),
            )
        }
        "mcp_tool_call" | "dynamic_tool_call" | "collab_agent_tool_call" => (
            "tool_activity",
            selected_map(
                raw,
                &[
                    "server",
                    "tool",
                    "arguments",
                    "result",
                    "error",
                    "status",
                    "namespace",
                    "prompt",
                    "receiverThreadIds",
                ],
            ),
        ),
        "command_execution" | "command" | "shell_command" => (
            "command_execution",
            selected_map(
                raw,
                &[
                    "command",
                    "cwd",
                    "output",
                    "result",
                    "aggregatedOutput",
                    "exit_code",
                    "exitCode",
                    "status",
                ],
            ),
        ),
        "command_execution_output" | "command_execution_result" | "command_result" => (
            "command_execution_result",
            selected_map(raw, &["command", "output", "result", "exit_code", "status"]),
        ),
        "file_operation" | "file_change" | "file_edit" | "file_write" => (
            "file_operation",
            selected_map(
                raw,
                &[
                    "operation",
                    "path",
                    "diff",
                    "changes",
                    "content",
                    "result",
                    "status",
                ],
            ),
        ),
        "file_operation_result" | "file_change_result" | "file_edit_result" => (
            "file_operation_result",
            selected_map(raw, &["operation", "path", "result", "output", "status"]),
        ),
        "plan" | "plan_update" | "update_plan" => (
            "plan",
            first_value(raw, &["content", "text", "summary", "plan", "items"]),
        ),
        "user_constraint" | "constraint" => (
            "user_constraint",
            first_value(raw, &["content", "text", "constraint"]),
        ),
        "decision" => (
            "decision",
            first_value(raw, &["content", "text", "decision"]),
        ),
        "progress" => (
            "progress",
            first_value(raw, &["content", "text", "summary", "progress"]),
        ),
        "error" => (
            "error",
            first_value(raw, &["content", "text", "message", "error", "details"]),
        ),
        "blocker" => (
            "blocker",
            first_value(raw, &["content", "text", "message", "blocker", "details"]),
        ),
        "compaction" | "compaction_summary" | "compacted" | "compacted_summary" => (
            "compaction_summary",
            first_value(raw, &["message", "summary", "content", "text", "marker"]),
        ),
        "compaction_marker" | "compaction_boundary" | "context_compaction" => (
            "compaction_marker",
            first_value(raw, &["marker", "summary", "content", "text"]),
        ),
        "image" | "image_reference" | "input_image" | "output_image" => (
            "image_reference",
            selected_map(
                raw,
                &[
                    "image_url",
                    "url",
                    "file_id",
                    "detail",
                    "media_type",
                    "alt_text",
                ],
            ),
        ),
        _ => return None,
    };
    Some(VisibleItem {
        kind: visible_kind.to_owned(),
        content,
        item_id: string_from(raw, &["id", "item_id", "itemId"]),
        turn_id: turn.or_else(|| string_from(raw, &["turn_id", "turnId"])),
        call_id: string_from(raw, &["call_id", "callId"]),
        raw_type: Some(raw_type),
    })
}

pub(super) fn message_role(record: &Map<String, Value>) -> Option<String> {
    let payload = record.get("payload")?.as_object()?;
    match token(record.get("type")).as_str() {
        "response_item" if token(payload.get("type")) == "message" => payload
            .get("role")?
            .as_str()
            .filter(|role| matches!(*role, "user" | "assistant"))
            .map(str::to_owned),
        "event_msg" => match token(payload.get("type")).as_str() {
            "user_message" | "usermessage" => Some("user".to_owned()),
            "assistant_message" | "assistantmessage" | "agent_message" | "agentmessage" => {
                Some("assistant".to_owned())
            }
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn record_turn_id(record: &Map<String, Value>) -> Option<String> {
    string_from(record, &["turn_id", "turnId"]).or_else(|| {
        record
            .get("payload")
            .and_then(Value::as_object)
            .and_then(|payload| string_from(payload, &["turn_id", "turnId"]))
    })
}

pub(super) fn ordinal(record: &Map<String, Value>) -> Option<u64> {
    ["ordinal", "rollout_ordinal", "rolloutOrdinal", "sequence"]
        .iter()
        .find_map(|key| {
            record.get(*key).and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str()?.trim().parse().ok())
            })
        })
}

pub(super) fn selected_map(raw: &Map<String, Value>, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|key| {
                raw.get(*key)
                    .map(|value| ((*key).to_owned(), value.clone()))
            })
            .collect(),
    )
}

pub(super) fn first_value(raw: &Map<String, Value>, keys: &[&str]) -> Value {
    keys.iter()
        .find_map(|key| raw.get(*key).cloned())
        .unwrap_or(Value::Null)
}

pub(super) fn content_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(content_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(value) => ["text", "message", "content", "output", "result"]
            .iter()
            .find_map(|key| {
                value
                    .get(*key)
                    .map(content_text)
                    .filter(|text| !text.is_empty())
            })
            .unwrap_or_else(|| Value::Object(value.clone()).to_string()),
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

pub(super) fn string_from(raw: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        raw.get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

pub(super) fn token(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .enumerate()
        .flat_map(|(index, character)| {
            let separator = index > 0 && character.is_ascii_uppercase();
            std::iter::once(separator.then_some('_'))
                .flatten()
                .chain(std::iter::once(character.to_ascii_lowercase()))
        })
        .map(|character| {
            if matches!(character, '-' | ' ') {
                '_'
            } else {
                character
            }
        })
        .collect()
}

pub(super) fn uuid_shape(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(index, character)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                character == '-'
            } else {
                character.is_ascii_hexdigit()
            }
        })
}
