//! Validate a single CLI transcript without changing its content or tool authority.
use super::super::media::ExpectedUserContent;
use serde_json::Value;
use std::borrow::Cow;

pub(super) fn validate(
    body: &mut Value,
    expected: &ExpectedUserContent,
) -> Result<(), &'static str> {
    validate_transcript(body, expected)?;
    if !has_only_structured_output_carrier(body) {
        return Err("claude_cli_tools_not_disabled");
    }
    Ok(())
}

fn validate_transcript(
    body: &mut Value,
    expected_user_content: &ExpectedUserContent,
) -> Result<(), &'static str> {
    if !normalize_cli_system_messages(body) {
        return Err("claude_cli_system_format_mismatch");
    }
    if !messages_match(body, expected_user_content) {
        return Err("claude_cli_content_mismatch");
    }
    Ok(())
}

fn normalize_cli_system_messages(body: &mut Value) -> bool {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return false;
    };
    let mut user_message = None;
    let mut moved_system_blocks = Vec::new();
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("user") if user_message.is_none() => user_message = Some(message.clone()),
            Some("user") | Some("assistant") => return false,
            Some("system") => {
                let Some(fields) = message.as_object() else {
                    return false;
                };
                if !fields.contains_key("content")
                    || fields
                        .keys()
                        .any(|key| !matches!(key.as_str(), "role" | "content" | "output_config"))
                {
                    return false;
                }
                // Some CLI models repeat the request's effort on their date
                // reminder. Discard only that identical duplicate; the actual
                // request setting remains at the Anthropic top level.
                if let Some(output_config) = fields.get("output_config") {
                    let Some(settings) = output_config.as_object() else {
                        return false;
                    };
                    if settings.len() != 1
                        || !settings.get("effort").is_some_and(Value::is_string)
                        || settings.get("effort")
                            != body.get("output_config").and_then(|v| v.get("effort"))
                    {
                        return false;
                    }
                }
                match message.get("content") {
                    Some(Value::String(text)) => {
                        moved_system_blocks.push(serde_json::json!({"type":"text","text":text}));
                    }
                    // Current CLI versions may send an empty system content
                    // block solely to repeat the already-validated effort.
                    // It carries no transcript content to discard.
                    Some(Value::Array(blocks))
                        if !blocks.is_empty() || fields.contains_key("output_config") =>
                    {
                        if blocks.iter().any(|block| {
                            block.get("type").and_then(Value::as_str) != Some("text")
                                || !block.get("text").is_some_and(Value::is_string)
                        }) {
                            return false;
                        }
                        moved_system_blocks.extend(blocks.iter().cloned());
                    }
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
    let Some(mut user_message) = user_message else {
        return false;
    };
    // The CLI also puts its date reminder before the user's content on some
    // models. Preserve it as system metadata; the remaining user content must
    // still match the entire original transcript, including media and tools.
    if let Some(parts) = user_message
        .get_mut("content")
        .and_then(Value::as_array_mut)
        && parts.len() > 1
        && is_cli_date_reminder(&parts[0])
    {
        moved_system_blocks.push(parts.remove(0));
    }
    let mut system_blocks = match body.get("system") {
        None => Vec::new(),
        Some(Value::String(text)) => vec![serde_json::json!({"type":"text","text":text})],
        Some(Value::Array(blocks)) => {
            if blocks.iter().any(|block| {
                block.get("type").and_then(Value::as_str) != Some("text")
                    || !block.get("text").is_some_and(Value::is_string)
            }) {
                return false;
            }
            blocks.clone()
        }
        Some(_) => return false,
    };
    system_blocks.extend(moved_system_blocks);

    let Some(object) = body.as_object_mut() else {
        return false;
    };
    object.insert("messages".to_owned(), Value::Array(vec![user_message]));
    if !system_blocks.is_empty() {
        object.insert("system".to_owned(), Value::Array(system_blocks));
    }
    true
}

fn is_cli_date_reminder(block: &Value) -> bool {
    let Some(clean) = without_cli_cache_marker(block) else {
        return false;
    };
    if clean.as_object().is_none_or(|fields| fields.len() != 2) || clean["type"] != "text" {
        return false;
    }
    let Some(date) = clean["text"]
        .as_str()
        .and_then(|text| text.strip_prefix("<system-reminder>\nToday's date is "))
        .and_then(|text| text.strip_suffix(".\n</system-reminder>\n"))
    else {
        return false;
    };
    if date.len() != 10
        || !date.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 4 | 7) {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
    {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        date[..4].parse(),
        date[5..7].parse::<u8>(),
        date[8..].parse(),
    ) else {
        return false;
    };
    time::Month::try_from(month)
        .is_ok_and(|month| time::Date::from_calendar_date(year, month, day).is_ok())
}

fn messages_match(body: &Value, expected: &ExpectedUserContent) -> bool {
    if body.get("stream").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return false;
    };
    let [message] = messages.as_slice() else {
        return false;
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match expected {
        ExpectedUserContent::Text(transcript) => {
            let content = &message["content"];
            let text = match content {
                Value::String(text) => Some(text.as_str()),
                Value::Array(parts) if parts.len() == 1 => parts[0]
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|_| parts[0].get("type").and_then(Value::as_str) == Some("text")),
                _ => None,
            };
            text.is_some_and(|text| text.as_bytes() == transcript)
        }
        ExpectedUserContent::Blocks(expected) => {
            let Some(content) = message.get("content").and_then(Value::as_array) else {
                return false;
            };
            matches_expected_blocks(content, expected)
        }
    }
}

fn matches_expected_blocks(actual: &[Value], expected: &[Value]) -> bool {
    // Claude Code adds prompt-cache breakpoints to native content blocks.
    // Compare without this transport metadata, but forward the original body.
    let Some(actual) = actual
        .iter()
        .map(without_cli_cache_marker)
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let mut actual_index = 0;
    let mut deferred_resizes = Vec::new();
    for expected_block in expected {
        let Some(actual_block) = actual.get(actual_index) else {
            return false;
        };
        if block_matches(expected_block, actual_block) {
            actual_index += 1;
            continue;
        }
        if expected_block.get("type").and_then(Value::as_str) != Some("image")
            || actual_block.get("type").and_then(Value::as_str) != Some("image")
        {
            return false;
        }
        if let Some(note) = actual.get(actual_index + 1)
            && super::super::image_geometry::matches_resize(expected_block, actual_block, note)
        {
            actual_index += 2;
        } else {
            deferred_resizes.push((expected_block, actual_block));
            actual_index += 1;
        }
    }
    let Some(trailing) = actual.get(actual_index..) else {
        return false;
    };
    if trailing.len() != deferred_resizes.len() {
        return false;
    }
    deferred_resizes
        .iter()
        .zip(trailing)
        .all(|((expected_image, actual_image), note)| {
            super::super::image_geometry::matches_resize(expected_image, actual_image, note)
        })
}

fn without_cli_cache_marker(block: &Value) -> Option<Cow<'_, Value>> {
    let Some(cache) = block.get("cache_control") else {
        return Some(Cow::Borrowed(block));
    };
    // Only the documented cache breakpoint shape is independent of content:
    // https://platform.claude.com/docs/en/build-with-claude/prompt-caching
    let cache = cache.as_object()?;
    if !matches!(
        block.get("type").and_then(Value::as_str),
        Some("text" | "image" | "document")
    ) || cache.get("type").and_then(Value::as_str) != Some("ephemeral")
        || cache.keys().any(|key| key != "type" && key != "ttl")
        || cache
            .get("ttl")
            .is_some_and(|ttl| !matches!(ttl.as_str(), Some("5m" | "1h")))
    {
        return None;
    }
    let mut comparison = block.clone();
    comparison.as_object_mut()?.remove("cache_control");
    Some(Cow::Owned(comparison))
}

fn block_matches(expected: &Value, actual: &Value) -> bool {
    if expected == actual {
        return true;
    }
    let (Some(expected_fields), Some(actual_fields)) = (expected.as_object(), actual.as_object())
    else {
        return false;
    };
    if expected_fields.len() != actual_fields.len()
        || expected_fields
            .iter()
            .any(|(key, value)| key != "text" && actual_fields.get(key) != Some(value))
        || expected_fields.get("type").and_then(Value::as_str) != Some("text")
    {
        return false;
    }
    let Some(expected_text) = expected_fields.get("text").and_then(Value::as_str) else {
        return false;
    };
    let Some(actual_text) = actual_fields.get("text").and_then(Value::as_str) else {
        return false;
    };
    let (Some(expected_value), Some(actual_value)) = (
        transcript_marker_value(expected_text),
        transcript_marker_value(actual_text),
    ) else {
        return false;
    };
    expected_value == actual_value
}

fn transcript_marker_value(text: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(text).ok()?;
    let object = value.as_object()?;
    let metadata_marker = object.len() == 1 && object.contains_key("codex_responses_metadata");
    let item_marker = object
        .get("codex_item_index")
        .and_then(Value::as_u64)
        .is_some()
        && object.get("item").is_some_and(Value::is_object);
    (metadata_marker || item_marker).then_some(value)
}

fn has_only_structured_output_carrier(body: &Value) -> bool {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return false;
    };
    tools.len() == 1 && tools[0].get("name").and_then(Value::as_str) == Some("StructuredOutput")
}

#[cfg(test)]
#[path = "transcript_tests.rs"]
mod tests;
