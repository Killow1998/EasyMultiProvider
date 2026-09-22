//! Services events.

use crate::http::response::json_error_response;
use crate::http::response::status_text;
use crate::services::failures::router_error_response;
use emp_router::ProjectionIds;
use emp_router::response_json_stream_events;
use serde_json::Value;

pub(crate) fn sse_frame(event: &str, body: &Value) -> Result<Vec<u8>, serde_json::Error> {
    let compact = serde_json::to_vec(body)?;
    let mut data = Vec::with_capacity(compact.len() + compact.len() / 8);
    let mut in_string = false;
    let mut escaped = false;
    for byte in compact {
        data.push(byte);
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if matches!(byte, b',' | b':') {
            data.push(b' ');
        }
    }
    let mut frame = Vec::with_capacity(event.len() + data.len() + 16);
    frame.extend_from_slice(b"event: ");
    frame.extend_from_slice(event.as_bytes());
    frame.extend_from_slice(b"\ndata: ");
    frame.extend_from_slice(&data);
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

pub(crate) fn generated_response_stream(
    response_value: Value,
    ids: &ProjectionIds,
) -> Result<Vec<u8>, Vec<u8>> {
    let events =
        response_json_stream_events(response_value, ids, false).map_err(router_error_response)?;
    let mut output = Vec::new();
    for event in events {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        output.extend(sse_frame(event_type, &event).map_err(|_| {
            json_error_response(500, status_text(500), "internal server error", None, &[])
        })?);
    }
    Ok(output)
}
