//! Codex owns the context; the CLI transports small MCP slots and runs its tool loop.
use super::super::{ClaudeCliError, media::ExpectedUserContent};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
};

fn slot(path: &Path) -> String {
    format!(
        "EMP_CODEX_CONTEXT_{}",
        path.parent()
            .expect("turn directory")
            .file_name()
            .expect("turn name")
            .to_string_lossy()
    )
}

pub(super) fn prepare(
    input: ExpectedUserContent,
    local: bool,
    path: &Path,
) -> Result<Vec<Value>, ClaudeCliError> {
    let blocks = match input {
        ExpectedUserContent::Text(text) => vec![json!({"type":"text","text":text})],
        ExpectedUserContent::Blocks(blocks) => blocks,
    };
    let content = if local {
        blocks.into_iter().enumerate().map(|(index,block)| match block["type"].as_str() {
            Some("image") if block["source"]["type"] == "url" => json!({"type":"resource_link","uri":block["source"]["url"],"name":format!("attachment-{index}")}),
            Some("image") => json!({"type":"image","mimeType":block["source"]["media_type"],"data":block["source"]["data"]}),
            Some("document") => json!({"type":"resource","resource":{"uri":format!("emp://context/attachment-{index}"),"mimeType":block["source"]["media_type"],"blob":block["source"]["data"]}}),
            _ => block,
        }).collect::<Vec<_>>()
    } else {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
        serde_json::to_writer(&mut file, &blocks)
            .and_then(|()| file.write_all(b"\n").map_err(serde_json::Error::io))
            .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
        vec![json!({"type":"text","text":slot(path)})]
    };
    Ok(content)
}

pub(super) fn restore(body: &mut Value, path: &Path) -> Result<(), ClaudeCliError> {
    let file =
        File::open(path).map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
    let mut originals = serde_json::Deserializer::from_reader(std::io::BufReader::new(file))
        .into_iter::<Vec<Value>>();
    let slot = slot(path);
    for message in body["messages"].as_array_mut().into_iter().flatten() {
        restore_blocks(&mut message["content"], &slot, &mut originals)?;
    }
    if originals.next().is_some() {
        return Err(ClaudeCliError::Failure("claude_cli_content_mismatch"));
    }
    Ok(())
}

fn restore_blocks(
    content: &mut Value,
    slot: &str,
    originals: &mut impl Iterator<Item = serde_json::Result<Vec<Value>>>,
) -> Result<(), ClaudeCliError> {
    let Some(blocks) = content.as_array_mut() else {
        return Ok(());
    };
    let mut index = 0;
    while index < blocks.len() {
        if blocks[index]["type"] == "text" && blocks[index]["text"] == slot {
            let restored = originals
                .next()
                .and_then(Result::ok)
                .ok_or(ClaudeCliError::Failure("claude_cli_content_mismatch"))?;
            let length = restored.len();
            blocks.splice(index..index + 1, restored);
            index += length;
        } else {
            if blocks[index]["type"] == "tool_result" {
                restore_blocks(&mut blocks[index]["content"], slot, originals)?;
            }
            index += 1;
        }
    }
    Ok(())
}
