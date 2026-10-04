//! Services events.

use serde_json::Value;

#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn sse_frame(event: &str, body: &Value) -> Result<Vec<u8>, serde_json::Error> {
    let mut frame = Vec::with_capacity(event.len() + 128);
    frame.extend_from_slice(b"event: ");
    frame.extend_from_slice(event.as_bytes());
    frame.extend_from_slice(b"\ndata: ");
    crate::util::spaced_json::write(&mut frame, body)?;
    frame.extend_from_slice(b"\n\n");
    Ok(frame)
}

pub(crate) fn stream_event_activity(event: &Value) -> (bool, bool) {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let item = event.get("item").and_then(Value::as_object);
    let item_type = item
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_activity = matches!(
        item_type,
        "function_call" | "custom_tool_call" | "tool_call" | "tool_search_call"
    ) || event_type.contains("function_call")
        || event_type.contains("tool_call");
    let mut output_emitted = tool_activity;
    if event_type.ends_with(".delta") || event_type.ends_with(".done") {
        output_emitted |= [
            "output_text",
            "output_image",
            "image_generation",
            "reasoning",
        ]
        .iter()
        .any(|marker| event_type.contains(marker));
    } else {
        output_emitted |=
            event_type.contains("output_image") || event_type.contains("image_generation");
    }
    output_emitted |= event
        .get("part")
        .and_then(Value::as_object)
        .and_then(|part| part.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|part_type| {
            matches!(
                part_type,
                "output_text" | "output_image" | "reasoning_text" | "summary_text"
            )
        });
    if let Some(content) = item
        .and_then(|item| item.get("content"))
        .and_then(Value::as_array)
    {
        output_emitted |= content.iter().any(|part| {
            part.get("type")
                .and_then(Value::as_str)
                .is_some_and(|part_type| {
                    matches!(
                        part_type,
                        "output_text" | "output_image" | "image" | "image_url" | "reasoning_text"
                    )
                })
        });
    }
    (output_emitted, tool_activity)
}

pub(crate) fn terminal_stream_event(event: &Value) -> bool {
    event
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|event_type| {
            matches!(
                event_type,
                "response.completed" | "response.incomplete" | "response.failed" | "error"
            )
        })
}
