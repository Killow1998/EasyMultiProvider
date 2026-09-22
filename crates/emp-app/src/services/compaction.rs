//! Destination-model compaction and portable summaries.

use crate::app::ServerState;
use crate::http::response::response;
use crate::services::failures::route_resolution_response;
use crate::services::failures::router_error_response;
use crate::util::random_hex;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use emp_core::ResolvedRoute;
use emp_router::ExternalRouter;
use emp_router::ProjectionIds;
use emp_router::protocol_candidates;
use emp_transport::protocol_fallback_allowed;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const MAX_EXTERNAL_COMPACTION_SUMMARY_CHARS: usize = 256 * 1024;

pub(crate) const COMPACTION_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another language model that will resume the task.\n\nInclude current progress, key decisions, constraints, user preferences, remaining steps, and critical data or references. Be concise, structured, and focused on seamless continuation.";

pub(crate) fn has_trailing_compaction_trigger(body: &Value) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .and_then(|items| items.last())
        .and_then(Value::as_object)
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        == Some("compaction_trigger")
}

pub(crate) fn compaction_summary_body(body: &Value) -> Value {
    let mut input = match body.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Object(item)) => vec![Value::Object(item.clone())],
        Some(Value::String(text)) => vec![serde_json::json!({
            "type":"message",
            "role":"user",
            "content":[{"type":"input_text","text":text}]
        })],
        _ => Vec::new(),
    };
    input.retain(|item| item.get("type").and_then(Value::as_str) != Some("compaction_trigger"));
    input.push(serde_json::json!({
        "type":"message",
        "role":"user",
        "content":[{"type":"input_text","text":COMPACTION_PROMPT}]
    }));
    serde_json::json!({
        "model":body.get("model").cloned().unwrap_or(Value::Null),
        "input":input,
        "stream":false,
        "tools":[]
    })
}

pub(crate) fn response_output_text(value: &Value) -> Option<String> {
    if let Some(text) = value
        .get("output_text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return Some(text.to_owned());
    }
    let mut parts = Vec::new();
    for item in value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for part in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if matches!(
                part.get("type").and_then(Value::as_str),
                Some("output_text" | "text")
            ) && let Some(text) = part.get("text").and_then(Value::as_str)
            {
                parts.push(text);
            }
        }
    }
    let text = parts.join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn external_compaction_error(reason: &str) -> Vec<u8> {
    let message = format!("external_compaction_failed: reason={reason}");
    let body = serde_json::to_vec(&serde_json::json!({
        "error": {
            "code":"external_compaction_failed",
            "type":"external_compaction_failed",
            "message":message,
            "failure_reason":reason
        }
    }))
    .expect("external compaction error is serializable");
    response("HTTP/1.1 502 Bad Gateway", "application/json", &body, &[])
}

pub(crate) fn external_compaction_response(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<(Value, ResolvedRoute), Vec<u8>> {
    let summary_body = compaction_summary_body(body);
    let router = ExternalRouter::new(&state.backend.transport.client);
    let candidates = protocol_candidates(route);
    for (index, protocol) in candidates.iter().copied().enumerate() {
        let candidate = route
            .with_protocol(protocol)
            .map_err(route_resolution_response)?;
        match state
            .backend
            .transport
            .runtime
            .block_on(router.execute_complete(&candidate, &summary_body, incoming, ids))
        {
            Ok(result) => {
                let Some(summary) = response_output_text(&result.body) else {
                    return Err(external_compaction_error("summary_empty"));
                };
                if summary.chars().count() > MAX_EXTERNAL_COMPACTION_SUMMARY_CHARS {
                    return Err(external_compaction_error("summary_too_large"));
                }
                let encoded = URL_SAFE.encode(summary.as_bytes());
                let item_id = random_hex(16)
                    .map(|value| format!("cmp_{value}"))
                    .map_err(|_| external_compaction_error("invalid_response"))?;
                let response_id = random_hex(16)
                    .map(|value| format!("resp_{value}"))
                    .map_err(|_| external_compaction_error("invalid_response"))?;
                let created_at = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or_default();
                return Ok((
                    serde_json::json!({
                        "id":response_id,
                        "object":"response",
                        "created_at":created_at,
                        "status":"completed",
                        "model":body.get("model").cloned().unwrap_or(Value::Null),
                        "output":[{
                            "id":item_id,
                            "type":"compaction",
                            "encrypted_content":format!("emp1:{encoded}")
                        }],
                        "usage":Value::Null
                    }),
                    candidate,
                ));
            }
            Err(error)
                if index + 1 < candidates.len()
                    && protocol_fallback_allowed(error.status(), false, false) =>
            {
                continue;
            }
            Err(error) => return Err(router_error_response(error)),
        }
    }
    Err(external_compaction_error("invalid_response"))
}
