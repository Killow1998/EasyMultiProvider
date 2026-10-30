//! Validate the CLI transport, then restore authoritative source image blocks.
use super::super::media::ExpectedUserContent;
use serde_json::Value;
use std::borrow::Cow;

mod system;
use system::normalize_cli_system_messages;

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
    if let ExpectedUserContent::Blocks(original) = expected_user_content {
        let actual = body["messages"][0]["content"]
            .as_array_mut()
            .ok_or("claude_cli_content_mismatch")?;
        for (source, slot) in original.iter().zip(actual) {
            if source["type"] == "image" {
                let cache = slot.get("cache_control").cloned();
                *slot = source.clone();
                if let Some(cache) = cache {
                    slot["cache_control"] = cache;
                }
            }
        }
    }
    Ok(())
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
            text == Some(transcript.as_str())
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
    // Image slots need exact positions and bytes. Only CLI cache metadata and
    // JSON formatting in EMP's history markers are independent of content.
    let slot = super::super::media::cli_image_slot();
    actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(actual, expected)| {
            let expected = if expected["type"] == "image" {
                &slot
            } else {
                expected
            };
            without_cli_cache_marker(actual).is_some_and(|actual| block_matches(expected, &actual))
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

#[cfg(all(test, target_os = "macos"))]
mod private_roundtrip;
