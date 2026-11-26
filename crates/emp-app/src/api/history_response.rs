use crate::api::failure_response::{
    request_router_error_response, route_resolution_response, router_error_reply,
};
use crate::api::streaming::ObservedSse;
use crate::app::ServerState;
use crate::proxy::workflow::PrepareFailure;
use crate::server::http::error::HttpReply;
use crate::server::http::{inference::InferenceOutcome, stream::BodyOutput};
use crate::services::compaction::{CompactionError, SummaryExecutionError};
use crate::services::events::sse_frame;
use crate::services::history::DestinationPrepareError;
use crate::services::observation::request::RequestObservation;
use crate::services::request_preparation::RequestPreparationError;
use crate::util::random_hex;
use emp_context::HistoryError;
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) enum PreparationOutput<'a, 'connection> {
    Compact,
    Responses,
    Sse(
        &'a mut BodyOutput<'connection>,
        &'a BTreeMap<String, String>,
    ),
}

pub(super) fn prepare_http_error(
    state: &ServerState,
    error: PrepareFailure<'_>,
    observation: &mut RequestObservation,
    output: PreparationOutput<'_, '_>,
) -> InferenceOutcome {
    let (history, at_destination) = match error {
        PrepareFailure::Request(error) => {
            let reply = match error {
                RequestPreparationError::ModelRequired => {
                    request_router_error_response(400, "request.model is required")
                }
                RequestPreparationError::Route(error) => route_resolution_response(error),
                RequestPreparationError::TeamAccess(error) => {
                    super::native_response::error_reply(error.public_error(state))
                }
                RequestPreparationError::MemoryUnavailable(reason)
                    if !matches!(output, PreparationOutput::Compact) =>
                {
                    request_router_error_response(
                        503,
                        crate::services::memory_routing::error_message(reason),
                    )
                }
                RequestPreparationError::ConfigurationUnavailable
                    if matches!(output, PreparationOutput::Compact) =>
                {
                    HttpReply::error(500, "configuration is unavailable", None)
                }
                RequestPreparationError::ConfigurationUnavailable
                | RequestPreparationError::ModelSnapshotTooLarge
                | RequestPreparationError::MemoryUnavailable(_) => {
                    HttpReply::error(500, "internal server error", None)
                }
            };
            return reply.into();
        }
        PrepareFailure::History(error) => (error, false),
        PrepareFailure::Destination(DestinationPrepareError::History(reason), _)
            if matches!(output, PreparationOutput::Sse(..)) =>
        {
            (HistoryError::new(reason), true)
        }
        PrepareFailure::Destination(DestinationPrepareError::Disconnected, _)
            if !matches!(output, PreparationOutput::Compact) =>
        {
            return InferenceOutcome::NoBufferedReply;
        }
        PrepareFailure::Destination(error, _) => {
            return destination_error_response(error, observation).into();
        }
    };
    let PreparationOutput::Sse(downstream, headers) = output else {
        return history_http_error(&history, observation).into();
    };
    let failed = history_stream_error(&history, observation);
    if let Ok(frame) = sse_frame("response.failed", &failed) {
        let mut downstream = ObservedSse::new(downstream, observation, headers);
        let _ = downstream.head();
        let _ = downstream.frames(&[frame], "response.failed");
    } else if !at_destination {
        return HttpReply::error(500, "internal server error", None).into();
    }
    InferenceOutcome::NoBufferedReply
}

fn history_error_detail(error: &HistoryError) -> Value {
    let diagnostic = error.diagnostic();
    serde_json::json!({
        "type":"invalid_request_error",
        "code":"invalid_prompt",
        "origin":"emp", "message":crate::services::error_origin::message("emp", &diagnostic.message()),
        "error_class":"history_reconstruction_failed",
        "reason":diagnostic.reason,
        "category":diagnostic.category
    })
}

pub(crate) fn history_http_error(
    error: &HistoryError,
    observation: &mut RequestObservation,
) -> HttpReply {
    observation.history_failed(error);
    let mut detail = history_error_detail(error);
    detail["code"] = "history_reconstruction_failed".into();
    detail.as_object_mut().unwrap().remove("type");
    let body = serde_json::to_vec(&serde_json::json!({
        "error": detail
    }))
    .expect("history error is serializable");
    HttpReply::new(409, body).with_status_text("Conflict")
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

fn destination_error_response(
    error: DestinationPrepareError,
    observation: &mut RequestObservation,
) -> Option<HttpReply> {
    Some(match error {
        DestinationPrepareError::Collaboration {
            status,
            code,
            message,
        } => HttpReply::error(status, message, Some(code)),
        DestinationPrepareError::Router(error) => router_error_reply(error),
        DestinationPrepareError::ClaudeCli(error) => {
            return crate::api::claude_response::http_response(&error);
        }
        DestinationPrepareError::Disconnected => return None,
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
                "origin":"emp", "message":format!("[EMP] context length exceeded: estimated input {estimate} tokens, safe input limit {limit}; provider {}, model {}; next action: reduce input or use native remote compaction", assessment.provider_id, assessment.model_id)
            }});
            HttpReply::new(413, serde_json::to_vec(&body).expect("context error JSON"))
                .with_status_text("Payload Too Large")
        }
    })
}

fn external_compaction_error(reason: &str) -> HttpReply {
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
    HttpReply::new(502, body)
}

pub(crate) fn compaction_error_response(error: CompactionError) -> Option<HttpReply> {
    Some(match error {
        CompactionError::InvalidSummary(reason) => external_compaction_error(reason),
        CompactionError::Route(error) => route_resolution_response(error),
        CompactionError::Execution(SummaryExecutionError::Router(error)) => {
            router_error_reply(error)
        }
        CompactionError::Execution(SummaryExecutionError::ClaudeCli(error)) => {
            return crate::api::claude_response::http_response(&error);
        }
        CompactionError::Execution(SummaryExecutionError::Disconnected) => return None,
    })
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
            let root = emp_test_support::fixture_dir().unwrap();
            let path = root.path().canonicalize().unwrap();
            if disabled {
                std::fs::write(path.join("logs"), "block journal directory").unwrap();
            }
            let diagnostics = crate::tests::attached_diagnostics(&path);
            let mut receipt = RequestObservation::new(
                Arc::clone(&diagnostics),
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
            let text = String::from_utf8(response.unwrap().encode()).unwrap();
            assert!(text.starts_with("HTTP/1.1 409"));
            let body: Value = serde_json::from_str(text.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(body["error"]["category"], "destination_compaction");
            assert_eq!(body["error"]["reason"], "summary_output_missing");
            if !disabled {
                diagnostics.journal.flush_blocking();
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
