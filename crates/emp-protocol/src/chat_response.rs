//! Complete Chat Completions responses projected into Responses output.

use super::*;

fn tool_item(
    call: &Map<String, Value>,
    raw_id: &str,
    custom_names: &BTreeSet<String>,
) -> Result<Value, ProtocolError> {
    let function = call
        .get("function")
        .and_then(object)
        .ok_or_else(|| protocol_error("Chat Completions upstream returned an invalid tool call"))?;
    let name = string_at(function, "name")
        .filter(|name| !name.is_empty())
        .ok_or_else(|| protocol_error("Chat Completions upstream returned an invalid tool call"))?;
    let arguments = string_at(function, "arguments").ok_or_else(|| {
        protocol_error("Chat Completions upstream returned invalid tool arguments")
    })?;
    let fields = tool_arguments(arguments)?;
    let mut item = if custom_names.contains(name) {
        serde_json::json!({
            "id": custom_tool_id(raw_id),
            "type": "custom_tool_call",
            "status": "completed",
            "call_id": raw_id,
            "name": name,
            "input": custom_tool_input(fields),
        })
    } else {
        serde_json::json!({
            "id": raw_id,
            "type": "function_call",
            "status": "completed",
            "call_id": raw_id,
            "name": name,
            "arguments": arguments,
        })
    };
    if let Some(extra) = call.get("extra_content").filter(|extra| extra.is_object()) {
        item["extra_content"] = extra.clone();
    }
    Ok(item)
}

/// Project one complete Chat Completions response into Responses JSON.
pub fn response_from_chat(
    value: &Value,
    requested_model: &str,
    custom_names: &[&str],
    ids: &ChatIds,
) -> Result<ChatProjection, ProtocolError> {
    let Some(root) = object(value) else {
        return Err(protocol_error(
            "Chat Completions upstream returned an invalid response",
        ));
    };
    if root.get("error").is_some_and(json_truthy) {
        return Err(upstream_error(
            "Chat Completions upstream returned an error",
        ));
    }
    let choice = root
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(object)
        .ok_or_else(|| protocol_error("Chat Completions upstream returned an invalid response"))?;
    let message = choice
        .get("message")
        .and_then(object)
        .ok_or_else(|| protocol_error("Chat Completions upstream returned an invalid response"))?;
    let custom_names = custom_names
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<BTreeSet<_>>();
    let mut output = Vec::new();
    let reasoning = chat_reasoning_text(message);
    if !reasoning.is_empty() {
        output.push(reasoning_item(&ids.reasoning, reasoning));
    }
    let mut text_channel = chat_text::ChatText::default();
    let mut text = text_channel.push(&message_text(message)?)?;
    text.push_str(&text_channel.finish());
    let refusal = refusal_text(message)?;
    let calls = match message.get("tool_calls") {
        None => &[][..],
        Some(Value::Array(calls)) => calls.as_slice(),
        Some(_) => {
            return Err(protocol_error(
                "Chat Completions upstream returned an invalid tool call",
            ));
        }
    };
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(serde_json::json!({
            "type": "output_text", "text": text, "annotations": []
        }));
    }
    if !refusal.is_empty() {
        content.push(serde_json::json!({"type": "refusal", "refusal": refusal}));
    }
    if !content.is_empty() {
        output.push(serde_json::json!({
            "id": ids.message,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "phase": if calls.is_empty() { "final_answer" } else { "commentary" },
            "content": content,
        }));
    }
    let mut seen_ids = BTreeSet::new();
    for call in calls {
        let Some(call) = object(call) else {
            return Err(protocol_error(
                "Chat Completions upstream returned an invalid tool call",
            ));
        };
        let raw_id = string_at(call, "id").unwrap_or_default();
        if raw_id.is_empty() || !seen_ids.insert(raw_id) {
            return Err(protocol_error(
                "Chat Completions upstream returned an invalid tool call",
            ));
        }
        output.push(tool_item(call, raw_id, &custom_names)?);
    }
    let reason = incomplete_reason(choice.get("finish_reason"))?;
    let mut response = serde_json::json!({
        "id": ids.response,
        "object": "response",
        "status": if reason.is_some() { "incomplete" } else { "completed" },
        "model": requested_model,
        "output": output,
        "output_text": text,
    });
    if let Some(reason) = reason {
        response["incomplete_details"] = serde_json::json!({"reason": reason});
    }
    if let Some(usage) = root.get("usage").filter(|usage| !usage.is_null()) {
        response["usage"] = projected_usage(usage)?;
    }
    if let Some(service_tier) = string_at(root, "service_tier") {
        response["service_tier"] = Value::String(service_tier.to_owned());
    }
    let mut unknown_fields = Vec::new();
    collect_unknown_fields(&mut unknown_fields, value);
    Ok(ChatProjection {
        response,
        unknown_fields,
    })
}
