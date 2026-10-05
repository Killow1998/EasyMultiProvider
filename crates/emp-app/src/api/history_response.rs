//! Render workflow failures at the HTTP/SSE/WebSocket boundary.
use crate::api::failure_response::{route_resolution_response, router_error_response};
use crate::http::response::response;
use crate::services::compaction::{CompactionError, SummaryExecutionError};
use crate::services::history::DestinationPrepareError;
use crate::services::observation::request::RequestObservation;
use crate::util::random_hex;
use emp_history::HistoryError;
use serde_json::Value;

fn history_error_detail(error: &HistoryError) -> Value {
    let diagnostic = error.diagnostic();
    serde_json::json!({
        "type":"invalid_request_error",
        "code":"invalid_prompt",
        "message":diagnostic.message(),
        "error_class":"history_reconstruction_failed",
        "reason":diagnostic.reason,
        "category":diagnostic.category
    })
}

pub(crate) fn history_http_error(
    error: &HistoryError,
    observation: &mut RequestObservation,
) -> Vec<u8> {
    observation.history_failed(error);
    let mut detail = history_error_detail(error);
    detail["code"] = "history_reconstruction_failed".into();
    detail.as_object_mut().unwrap().remove("type");
    let body = serde_json::to_vec(&serde_json::json!({
        "error": detail
    }))
    .expect("history error is serializable");
    response("HTTP/1.1 409 Conflict", "application/json", &body, &[])
}

pub(crate) fn history_stream_error(
    error: &HistoryError,
    observation: &mut RequestObservation,
) -> Value {
    observation.history_failed(error);
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

pub(crate) fn destination_error_response(
    error: DestinationPrepareError,
    observation: &mut RequestObservation,
) -> Vec<u8> {
    match error {
        DestinationPrepareError::Router(error) => router_error_response(error),
        DestinationPrepareError::ClaudeCli(error) => {
            crate::api::claude_response::http_response(&error)
        }
        DestinationPrepareError::Disconnected => Vec::new(),
        DestinationPrepareError::History(reason) => {
            history_http_error(&HistoryError::new(reason), observation)
        }
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

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use std::sync::Arc;
    #[test]
    fn destination_failure_records_its_actual_phase_and_survives_disabled_logging() {
        use super::destination_error_response;
        use crate::services::history::DestinationPrepareError;
        use crate::services::observation::request::{Phase, RequestObservation};
        for disabled in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().canonicalize().unwrap();
            if disabled {
                std::fs::write(path.join("logs"), "block journal directory").unwrap();
            }
            let diagnostics = Arc::new(emp_state::diagnostics::Diagnostics::new(&path));
            let mut receipt = RequestObservation::new(
                diagnostics,
                Some("0123456789abcdef".into()),
                None,
                "http",
                "responses",
            );
            receipt.phase(Phase::PrepareDestination);
            let response = destination_error_response(
                DestinationPrepareError::History("summary_output_missing"),
                &mut receipt,
            );
            drop(receipt);
            let text = String::from_utf8(response).unwrap();
            assert!(text.starts_with("HTTP/1.1 409"));
            let body: Value = serde_json::from_str(text.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(body["error"]["category"], "destination_compaction");
            assert_eq!(body["error"]["reason"], "summary_output_missing");
            if !disabled {
                let records: Vec<Value> = std::fs::read_dir(path.join("logs"))
                    .unwrap()
                    .flat_map(|entry| {
                        std::fs::read_to_string(entry.unwrap().path())
                            .unwrap()
                            .lines()
                            .map(|line| serde_json::from_str(line).unwrap())
                            .collect::<Vec<_>>()
                    })
                    .collect();
                let done = records
                    .iter()
                    .find(|r| r["event"] == "request_finished")
                    .unwrap();
                assert_eq!(
                    done["fields"]["history_failure"]["phase"],
                    "prepare_destination"
                );
            }
        }
    }
}
