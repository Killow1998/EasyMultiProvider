//! Complete Responses projection from external upstream output.

use super::*;

pub(super) fn custom_tool_ids(raw_id: Option<&Value>, call_id: Option<&Value>) -> (String, String) {
    let paired = call_id
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            raw_id
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("call_unknown");
    let item_id = raw_id
        .and_then(Value::as_str)
        .filter(|value| value.starts_with("ctc_"))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let digest = Sha256::digest(paired.as_bytes());
            format!("ctc_{:x}", digest)[..28].to_owned()
        });
    (item_id, paired.to_owned())
}

pub(super) fn custom_tool_input(value: Option<&Value>) -> String {
    let value = match value {
        Some(Value::Object(value)) => value
            .get("input")
            .cloned()
            .unwrap_or_else(|| Value::Object(value.clone())),
        Some(Value::String(value)) => {
            let decoded = serde_json::from_str::<Value>(value)
                .unwrap_or_else(|_| Value::String(value.clone()));
            if let Value::Object(object) = &decoded {
                object.get("input").cloned().unwrap_or(decoded)
            } else {
                decoded
            }
        }
        Some(value) => value.clone(),
        None => return String::new(),
    };
    if let Some(value) = value.as_str() {
        value.to_owned()
    } else {
        serde_json::to_string(&value).unwrap_or_default()
    }
}

pub fn custom_tool_names(body: &Value) -> Result<BTreeSet<String>, PortableProjectionError> {
    let body = object(body).ok_or_else(|| error(0, "input", "invalid_input"))?;
    Ok(raw_tools(body)?
        .into_iter()
        .filter(|tool| tool.value.get("type").and_then(Value::as_str) == Some("custom"))
        .filter_map(|tool| {
            tool.value
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .collect())
}

pub(super) fn project_reasoning_item(
    item: &Map<String, Value>,
    preserve_summary: bool,
    preserve_state: bool,
) -> Option<Value> {
    if !preserve_summary && !preserve_state {
        return None;
    }
    let mut clean = Map::from_iter([("type".to_owned(), Value::String("reasoning".to_owned()))]);
    for field in ["id", "status"] {
        if let Some(value) = item.get(field) {
            clean.insert(field.to_owned(), value.clone());
        }
    }
    if preserve_state
        && item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    {
        clean.insert(
            "encrypted_content".to_owned(),
            item["encrypted_content"].clone(),
        );
    }
    if preserve_summary {
        let summary = item
            .get("summary")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| {
                let part = object(part)?;
                (part.get("type").and_then(Value::as_str) == Some("summary_text")
                    && part.get("text").is_some_and(Value::is_string))
                .then(|| json!({"type": "summary_text", "text": part["text"]}))
            })
            .collect::<Vec<_>>();
        if !summary.is_empty() {
            clean.insert("summary".to_owned(), Value::Array(summary));
        }
    }
    Some(Value::Object(clean))
}

pub fn project_response(
    response: &Value,
    custom_names: &BTreeSet<String>,
    preserve_reasoning_summary: bool,
    preserve_reasoning_state: bool,
) -> Result<Value, PortableProjectionError> {
    let mut projected = object(response)
        .ok_or_else(|| error(0, "output", "invalid_response_output"))?
        .clone();
    for field in PLAINTEXT_REASONING_FIELDS {
        projected.remove(*field);
    }
    let raw_output = projected.get("output").cloned();
    if !matches!(raw_output, None | Some(Value::Null | Value::Array(_))) {
        return Err(error(0, "output", "invalid_response_output"));
    }
    let mut output = Vec::new();
    for (index, raw) in raw_output
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(item) = object(raw) else {
            return Err(error(index, "output", "invalid_response_output"));
        };
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            if let Some(clean) =
                project_reasoning_item(item, preserve_reasoning_summary, preserve_reasoning_state)
            {
                output.push(clean);
            }
            continue;
        }
        if item.get("type").and_then(Value::as_str) == Some("compaction") {
            return Err(error(index, "compaction", "external_compaction"));
        }
        let mut clean = item.clone();
        for field in PLAINTEXT_REASONING_FIELDS {
            clean.remove(*field);
        }
        if let Some(Value::Array(content)) = clean.get_mut("content") {
            content.retain(|part| {
                !part
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| {
                        matches!(
                            kind.to_ascii_lowercase().as_str(),
                            "reasoning" | "reasoning_text" | "thinking" | "thinking_text"
                        )
                    })
            });
        }
        if clean.get("type").and_then(Value::as_str) == Some("function_call")
            && clean
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| custom_names.contains(name))
        {
            let (item_id, call_id) = custom_tool_ids(clean.get("id"), clean.get("call_id"));
            let input = custom_tool_input(clean.remove("arguments").as_ref());
            clean.insert("id".to_owned(), Value::String(item_id));
            clean.insert("call_id".to_owned(), Value::String(call_id));
            clean.insert(
                "type".to_owned(),
                Value::String("custom_tool_call".to_owned()),
            );
            clean.insert("input".to_owned(), Value::String(input));
        }
        output.push(Value::Object(clean));
    }
    if matches!(raw_output, Some(Value::Array(_))) {
        projected.insert("output".to_owned(), Value::Array(output));
    }
    Ok(Value::Object(projected))
}
