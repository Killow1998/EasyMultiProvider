//! Normalize Claude CLI system metadata without changing conversation content.
use super::without_cli_cache_marker;
use serde_json::Value;

pub(super) fn normalize_cli_system_messages(body: &mut Value) -> bool {
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
