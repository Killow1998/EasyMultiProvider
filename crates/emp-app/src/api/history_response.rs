//! Render workflow failures at the HTTP/SSE/WebSocket boundary.
use crate::api::failure_response::{route_resolution_response, router_error_response};
use crate::http::response::response;
use crate::services::compaction::{CompactionError, SummaryExecutionError};
use crate::services::history::DestinationPrepareError;
use crate::util::random_hex;
use emp_history::HistoryError;
use serde_json::Value;

fn history_error_message(error: &HistoryError) -> &'static str {
    match error.reason() {
        "thread_missing" | "thread_identity_missing" => {
            "This task's local history is unavailable. For a Side chat, continue in the original task or start a new task."
        }
        "state_database_missing" | "database_missing" => {
            "Local Codex history was not found. Use the same Codex data directory as your client."
        }
        _ => "History reconstruction failed. Continue in the original task or start a new task.",
    }
}

fn history_error_detail(error: &HistoryError) -> Value {
    serde_json::json!({
        "type":"invalid_request_error",
        "code":"invalid_prompt",
        "message":history_error_message(error),
        "error_class":"history_reconstruction_failed",
        "reason":error.reason()
    })
}

pub(crate) fn history_http_error(error: &HistoryError) -> Vec<u8> {
    let body = serde_json::to_vec(&serde_json::json!({
        "error": {
            "code":"history_reconstruction_failed",
            "message":history_error_message(error),
            "error_class":"history_reconstruction_failed",
            "reason":error.reason()
        }
    }))
    .expect("history error is serializable");
    response("HTTP/1.1 409 Conflict", "application/json", &body, &[])
}

pub(crate) fn history_stream_error(error: &HistoryError) -> Value {
    let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
    serde_json::json!({
        "type":"response.failed",
        "response":{
            "id":id,
            "object":"response",
            "status":"failed",
            "error":history_error_detail(error)
        }
    })
}

pub(crate) fn destination_error_response(error: DestinationPrepareError) -> Vec<u8> {
    match error {
        DestinationPrepareError::Router(error) => router_error_response(error),
        DestinationPrepareError::ClaudeCli(error) => {
            crate::api::claude_response::http_response(&error)
        }
        DestinationPrepareError::Disconnected => Vec::new(),
        DestinationPrepareError::History(reason) => history_http_error(&HistoryError::new(reason)),
        DestinationPrepareError::Context(assessment) => {
            let estimate = assessment
                .input_estimate
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
            let limit = assessment
                .safe_input_limit
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
            let body = serde_json::json!({"error":{
                "code":"context_length_exceeded", "type":"context_length_exceeded",
                "message":format!("context length exceeded: estimated input {estimate} tokens, safe input limit {limit}; provider {}, model {}; next action: reduce input or use native remote compaction", assessment.provider_id, assessment.model_id)
            }});
            response(
                "HTTP/1.1 413 Payload Too Large",
                "application/json",
                &serde_json::to_vec(&body).expect("context error JSON"),
                &[],
            )
        }
    }
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

pub(crate) fn compaction_error_response(error: CompactionError) -> Vec<u8> {
    match error {
        CompactionError::InvalidSummary(reason) => external_compaction_error(reason),
        CompactionError::Route(error) => route_resolution_response(error),
        CompactionError::Execution(SummaryExecutionError::Router(error)) => {
            router_error_response(error)
        }
        CompactionError::Execution(SummaryExecutionError::ClaudeCli(error)) => {
            crate::api::claude_response::http_response(&error)
        }
        CompactionError::Execution(SummaryExecutionError::Disconnected) => Vec::new(),
    }
}
