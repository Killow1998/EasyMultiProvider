//! Destination-model compaction and portable summaries.

use crate::app::ServerState;
use crate::util::random_hex;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use emp_core::ResolvedRoute;
use emp_router::CompleteResponse;
use emp_router::ExternalRouter;
use emp_router::ProjectionIds;
use emp_router::RouterError;
use emp_router::protocol_candidates;
use emp_transport::protocol_fallback_allowed;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const MAX_EXTERNAL_COMPACTION_SUMMARY_CHARS: usize = 256 * 1024;

pub(crate) enum CompactionError {
    InvalidSummary(&'static str),
    Route(emp_core::RouteResolutionError),
    Execution(SummaryExecutionError),
}

pub(crate) enum SummaryExecutionError {
    Router(RouterError),
    ClaudeCli(crate::services::claude_cli::ClaudeCliError),
    Disconnected,
}

/// Execute one internal summary inference through the selected provider
/// backend while retaining the normal EMP router and cancellation behavior.
pub(crate) fn execute_summary_request(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
    monitor: Option<&mut crate::services::disconnect::DisconnectMonitor>,
) -> Result<(CompleteResponse, ResolvedRoute), SummaryExecutionError> {
    if crate::services::claude_cli::selected(route) {
        return match crate::services::claude_cli::execute_complete(
            state, route, body, incoming, ids, monitor,
        ) {
            Ok(completion) => Ok((completion.response, completion.route)),
            Err(crate::services::claude_cli::ClaudeCliError::Router(error)) => {
                Err(SummaryExecutionError::Router(error))
            }
            Err(crate::services::claude_cli::ClaudeCliError::Disconnected) => {
                Err(SummaryExecutionError::Disconnected)
            }
            Err(error) => Err(SummaryExecutionError::ClaudeCli(error)),
        };
    }

    let router = ExternalRouter::new(&state.backend.transport.client);
    let mut event = Value::Object(crate::services::observation::request::route_fields(
        route,
        &state.backend.diagnostics,
    ));
    event["request_id"] = crate::services::observation::request::request_id(incoming);
    crate::services::observation::record_attempt(state, &event, "summary_dispatch");
    match crate::services::disconnect::raced(
        &state.backend.transport.runtime,
        monitor,
        router.execute_complete(route, body, incoming, ids),
    ) {
        crate::services::disconnect::DisconnectRace::Ready(Ok(response)) => {
            Ok((response, route.clone()))
        }
        crate::services::disconnect::DisconnectRace::Ready(Err(error)) => {
            Err(SummaryExecutionError::Router(error))
        }
        crate::services::disconnect::DisconnectRace::Disconnected => {
            Err(SummaryExecutionError::Disconnected)
        }
    }
}

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

pub(crate) fn external_compaction_from_summary(
    body: &Value,
    summary: &str,
) -> Result<Value, CompactionError> {
    if summary.trim().is_empty() {
        return Err(CompactionError::InvalidSummary("summary_empty"));
    }
    if summary.chars().count() > MAX_EXTERNAL_COMPACTION_SUMMARY_CHARS {
        return Err(CompactionError::InvalidSummary("summary_too_large"));
    }
    let encoded = URL_SAFE.encode(summary.as_bytes());
    let item_id = random_hex(16)
        .map(|value| format!("cmp_{value}"))
        .map_err(|_| CompactionError::InvalidSummary("invalid_response"))?;
    let response_id = random_hex(16)
        .map(|value| format!("resp_{value}"))
        .map_err(|_| CompactionError::InvalidSummary("invalid_response"))?;
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    Ok(serde_json::json!({
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
    }))
}

pub(crate) fn external_compaction_response(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
    mut monitor: Option<&mut crate::services::disconnect::DisconnectMonitor>,
) -> Result<(Value, ResolvedRoute), CompactionError> {
    let summary_body = compaction_summary_body(body);
    let candidates = protocol_candidates(route);
    let mut activity_guard = None;
    for (index, protocol) in candidates.iter().copied().enumerate() {
        let candidate = route
            .with_protocol(protocol)
            .map_err(CompactionError::Route)?;
        if activity_guard.is_none() {
            activity_guard = Some(state.backend.activity.begin(
                crate::services::activity::ActivityIdentity::from_route(route),
            ));
        }
        match execute_summary_request(
            state,
            &candidate,
            &summary_body,
            incoming,
            ids,
            monitor.as_deref_mut(),
        ) {
            Ok((result, executed_route)) => {
                let Some(summary) = response_output_text(&result.body) else {
                    return Err(CompactionError::InvalidSummary("summary_empty"));
                };
                return external_compaction_from_summary(body, &summary)
                    .map(|compacted| (compacted, executed_route));
            }
            Err(SummaryExecutionError::Router(error))
                if index + 1 < candidates.len()
                    && protocol_fallback_allowed(error.status(), false, false) =>
            {
                continue;
            }
            Err(SummaryExecutionError::Router(error)) => {
                return Err(CompactionError::Execution(SummaryExecutionError::Router(
                    error,
                )));
            }
            Err(error) => return Err(CompactionError::Execution(error)),
        }
    }
    Err(CompactionError::InvalidSummary("invalid_response"))
}
