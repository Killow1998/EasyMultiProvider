//! History reconstruction and destination context preparation.

use crate::app::ServerState;
use crate::http::response::json_error_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::services::compaction::compaction_summary_body;
use crate::services::compaction::has_trailing_compaction_trigger;
use crate::services::compaction::response_output_text;
use crate::services::failures::router_error_response;
use crate::util::projection_ids;
use crate::util::random_hex;
use emp_core::ResolvedRoute;
use emp_history::HistoryError;
use emp_router::ExternalRouter;
use emp_router::RouterError;
use emp_router::project_external_payload;
use emp_router::protocol_candidates;
use serde_json::Value;
use std::collections::BTreeMap;

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

pub(crate) fn prepare_history(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
) -> Result<Value, HistoryError> {
    let reader =
        emp_codex::history::CodexHomeHistoryReader::new(&state.backend.accounts.codex_home);
    emp_history::prepare(
        body,
        incoming,
        route.dialect == emp_core::Dialect::CodexNative,
        &reader,
    )
}

pub(crate) enum DestinationPrepareError {
    Router(RouterError),
    History(&'static str),
    Context(emp_history::context::ContextAssessment),
}

pub(crate) fn prepare_destination_context(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
) -> Result<Value, DestinationPrepareError> {
    if route.dialect == emp_core::Dialect::CodexNative {
        return Ok(body.clone());
    }
    let protocol =
        protocol_candidates(route)
            .into_iter()
            .next()
            .ok_or(DestinationPrepareError::History(
                "history_compaction_failed",
            ))?;
    let candidate = route
        .with_protocol(protocol)
        .map_err(|_| DestinationPrepareError::History("history_compaction_failed"))?;
    let guard_body = if has_trailing_compaction_trigger(body) {
        compaction_summary_body(body)
    } else {
        body.clone()
    };
    let payload = project_external_payload(&candidate, &guard_body)
        .map_err(DestinationPrepareError::Router)?;
    let assessment = emp_history::context::assess(
        candidate.provider.value(),
        candidate.model.value(),
        candidate.protocol.as_config_str(),
        &payload,
    );
    if !assessment.blocked() {
        return Ok(body.clone());
    }
    let Some(safe_budget) = assessment.safe_input_limit else {
        return Err(DestinationPrepareError::Context(assessment));
    };
    let router = ExternalRouter::new(&state.backend.transport.client);
    let mut summary_failure = None;
    let compacted = emp_history::context::compact_with(
        body,
        candidate.model.value(),
        safe_budget,
        |summary_body| {
            let ids = match projection_ids() {
                Ok(ids) => ids,
                Err(_) => return Err(()),
            };
            match state
                .backend
                .transport
                .runtime
                .block_on(router.execute_complete(&candidate, summary_body, incoming, &ids))
            {
                Ok(result) => response_output_text(&result.body).ok_or(()),
                Err(error) => {
                    summary_failure = Some(error);
                    Err(())
                }
            }
        },
    )
    .map_err(|reason| {
        summary_failure.take().map_or(
            DestinationPrepareError::History(reason),
            DestinationPrepareError::Router,
        )
    })?;
    let final_guard_body = if has_trailing_compaction_trigger(&compacted) {
        compaction_summary_body(&compacted)
    } else {
        compacted.clone()
    };
    let payload = project_external_payload(&candidate, &final_guard_body)
        .map_err(DestinationPrepareError::Router)?;
    let final_assessment = emp_history::context::assess(
        candidate.provider.value(),
        candidate.model.value(),
        candidate.protocol.as_config_str(),
        &payload,
    );
    if final_assessment.blocked() {
        return Err(DestinationPrepareError::Context(final_assessment));
    }
    Ok(compacted)
}

pub(crate) fn destination_error_response(error: DestinationPrepareError) -> Vec<u8> {
    match error {
        DestinationPrepareError::Router(error) => router_error_response(error),
        DestinationPrepareError::History(reason) => history_http_error(&HistoryError::new(reason)),
        DestinationPrepareError::Context(assessment) => {
            let estimate = assessment
                .input_estimate
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
            let limit = assessment
                .safe_input_limit
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
            json_error_response(
                413,
                status_text(413),
                &format!(
                    "context length exceeded: estimated input {estimate} tokens, safe input limit {limit}; next action: reduce input or use native remote compaction"
                ),
                Some("context_length_exceeded"),
                &[],
            )
        }
    }
}
