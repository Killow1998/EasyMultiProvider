//! Validate the Codex envelope, never treat arbitrary encrypted reasoning as a task.
use super::*;

pub(super) struct Task {
    pub author: String,
    pub recipient: String,
    pub header: String,
    pub ciphertext: String,
}

pub(super) fn ciphertext(text: &str) -> bool {
    (100..=MAX_TEXT * 2).contains(&text.len())
        && text.starts_with("gAAAA")
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_=".contains(&byte))
}

pub(super) fn has_encrypted_task(item: &Value) -> bool {
    item["type"] == "agent_message"
        && (item.get("encrypted_content").is_some()
            || item["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["type"] == "encrypted_content")))
}

pub(super) fn is_child(body: &Value, headers: &BTreeMap<String, String>) -> bool {
    // Review/compaction workers also have parents; their own routing is separate.
    if let Some((_, value)) = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-openai-subagent"))
    {
        return value == "collab_spawn";
    }
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("x-codex-parent-thread-id") && !value.is_empty()
    }) || body["input"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["type"] == "agent_message"
                && item["recipient"]
                    .as_str()
                    .is_some_and(|path| path.starts_with("/root/"))
        })
    })
}

pub(super) fn parse(item: &Value) -> Result<Task, DestinationPrepareError> {
    let invalid = || {
        error(
            400,
            "invalid_agent_task",
            "The encrypted agent task envelope is invalid",
        )
    };
    let path = |name: &str| {
        item[name]
            .as_str()
            .filter(|path| {
                path.len() <= 1024
                    && (path == &"/root" || path.starts_with("/root/"))
                    && !path.chars().any(char::is_whitespace)
            })
            .ok_or_else(invalid)
    };
    let author = path("author")?;
    let recipient = path("recipient")?;
    let parts = item["content"].as_array().ok_or_else(invalid)?;
    if parts.len() != 2
        || item.get("encrypted_content").is_some()
        || parts[0]["type"] != "input_text"
        || parts[1]["type"] != "encrypted_content"
    {
        return Err(invalid());
    }
    let header = parts[0]["text"].as_str().ok_or_else(invalid)?;
    if !["NEW_TASK", "MESSAGE", "FOLLOWUP_TASK", "FINAL_ANSWER"]
        .iter()
        .any(|kind| {
            header
                == format!(
                    "Message Type: {kind}\nTask name: {recipient}\nSender: {author}\nPayload:\n"
                )
        })
    {
        return Err(invalid());
    }
    let ciphertext = parts[1]["encrypted_content"]
        .as_str()
        .filter(|text| ciphertext(text))
        .ok_or_else(invalid)?;
    Ok(Task {
        author: author.into(),
        recipient: recipient.into(),
        header: header.into(),
        ciphertext: ciphertext.into(),
    })
}

pub(super) fn capture(response: &Value) -> Result<String, DestinationPrepareError> {
    let invalid = || {
        error(
            502,
            "agent_task_recovery_invalid",
            "Task recovery did not return one complete plaintext assignment; nothing was forwarded",
        )
    };
    if response["status"] != "completed" || !response["error"].is_null() {
        return Err(invalid());
    }
    let output = response["output"].as_array().ok_or_else(invalid)?;
    let calls: Vec<_> = output
        .iter()
        .filter(|item| item["type"] != "reasoning")
        .collect();
    if calls.len() != 1
        || calls[0]["type"] != "function_call"
        || calls[0]["name"] != "capture_assignment"
    {
        return Err(invalid());
    }
    let args: Value = serde_json::from_str(calls[0]["arguments"].as_str().ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    let text = args["text"]
        .as_str()
        .filter(|text| !text.trim().is_empty() && text.len() <= MAX_TEXT && !ciphertext(text))
        .ok_or_else(invalid)?;
    Ok(text.into())
}
