//! Lift only Codex instruction messages; user text and tool results stay in history.
use super::projection::{MAX_TRANSCRIPT_BYTES, SYSTEM_PROMPT};
use serde_json::Value;

pub(super) fn take(transcript: &mut Value) -> Result<String, &'static str> {
    let mut prompt = SYSTEM_PROMPT.to_owned();
    if let Some(instructions) = transcript
        .as_object_mut()
        .and_then(|body| body.remove("instructions"))
        && !instructions.is_null()
    {
        append(&mut prompt, "Codex instructions", &instructions)?;
    }
    let Some(input) = transcript.get_mut("input") else {
        return Ok(prompt);
    };
    match input {
        Value::Array(items) => {
            // Validate before removing anything; preserve the remaining history order.
            for item in items.iter().filter(|item| instruction_role(item).is_some()) {
                append(
                    &mut prompt,
                    instruction_role(item).unwrap(),
                    &item["content"],
                )?;
            }
            items.retain(|item| instruction_role(item).is_none());
        }
        item if instruction_role(item).is_some() => {
            append(
                &mut prompt,
                instruction_role(item).unwrap(),
                &item["content"],
            )?;
            *item = Value::Array(Vec::new());
        }
        _ => {}
    }
    Ok(prompt)
}

fn instruction_role(item: &Value) -> Option<&'static str> {
    if !matches!(item["type"].as_str(), None | Some("message")) {
        return None;
    }
    match item["role"].as_str() {
        Some("system") => Some("Codex system instructions"),
        Some("developer") => Some("Codex developer instructions"),
        _ => None,
    }
}

fn append(prompt: &mut String, label: &str, content: &Value) -> Result<(), &'static str> {
    push(prompt, &format!("\n\n## {label}\n"))?;
    match content {
        Value::String(text) => push(prompt, text),
        Value::Array(parts) => {
            for part in parts {
                if !matches!(
                    part["type"].as_str(),
                    Some("input_text" | "output_text" | "text")
                ) {
                    return Err("claude_cli_invalid_transcript");
                }
                let text = part["text"]
                    .as_str()
                    .ok_or("claude_cli_invalid_transcript")?;
                push(prompt, text)?;
                push(prompt, "\n")?;
            }
            Ok(())
        }
        _ => Err("claude_cli_invalid_transcript"),
    }
}

fn push(prompt: &mut String, text: &str) -> Result<(), &'static str> {
    if text.len() > MAX_TRANSCRIPT_BYTES.saturating_sub(prompt.len()) {
        return Err("claude_cli_input_too_large");
    }
    prompt.push_str(text);
    Ok(())
}
