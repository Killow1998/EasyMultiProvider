//! History reconstruction and destination context preparation.

use crate::app::ServerState;
use crate::http::response::response;
use crate::services::compaction::compaction_summary_body;
use crate::services::compaction::has_trailing_compaction_trigger;
use crate::services::compaction::response_output_text;
use crate::services::failures::router_error_response;
use crate::util::projection_ids;
use crate::util::random_hex;
use emp_core::ResolvedRoute;
use emp_history::HistoryError;
use emp_router::RouterError;
use emp_router::project_external_payload;
use emp_router::protocol_candidates;
use serde_json::Value;
use std::borrow::Cow;
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
    body: Value,
    incoming: &BTreeMap<String, String>,
) -> Result<Value, HistoryError> {
    let reader =
        emp_codex::history::CodexHomeHistoryReader::new(&state.backend.accounts.codex_home);
    emp_history::prepare_owned(
        body,
        incoming,
        route.dialect == emp_core::Dialect::CodexNative,
        &reader,
    )
}

pub(crate) enum DestinationPrepareError {
    Router(RouterError),
    ClaudeCli(crate::services::claude_cli::ClaudeCliError),
    Disconnected,
    History(&'static str),
    Context(Box<emp_history::context::ContextAssessment>),
}

fn context_guard_body(body: &Value) -> Cow<'_, Value> {
    if has_trailing_compaction_trigger(body) {
        Cow::Owned(compaction_summary_body(body))
    } else {
        Cow::Borrowed(body)
    }
}

fn assess_destination_context(
    candidate: &ResolvedRoute,
    body: &Value,
) -> Result<emp_history::context::ContextAssessment, DestinationPrepareError> {
    let payload = if crate::services::claude_cli::selected(candidate) {
        crate::services::claude_cli::context_estimation_payload(body)
            .map_err(DestinationPrepareError::ClaudeCli)?
    } else {
        project_external_payload(candidate, body).map_err(DestinationPrepareError::Router)?
    };
    Ok(emp_history::context::assess(
        candidate.provider.value(),
        candidate.model.value(),
        candidate.protocol.as_config_str(),
        &payload,
    ))
}

pub(crate) fn prepare_destination_context(
    state: &ServerState,
    route: &ResolvedRoute,
    body: Value,
    incoming: &BTreeMap<String, String>,
    mut monitor: Option<&mut crate::services::disconnect::DisconnectMonitor>,
) -> Result<Value, DestinationPrepareError> {
    if route.dialect == emp_core::Dialect::CodexNative {
        // Incremental native input has unknown history completeness. Codex owns
        // its existing chain; never judge the delta as a full conversation.
        if body.get("previous_response_id").is_none_or(Value::is_null)
            && let Some(payload) = crate::services::context::payload(state, route, &body)
        {
            let assessment = emp_history::context::assess(
                route.provider.value(),
                route.model.value(),
                route.protocol.as_config_str(),
                &payload,
            );
            if assessment.blocked() {
                return Err(DestinationPrepareError::Context(Box::new(assessment)));
            }
        }
        return Ok(body);
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
    let assessment = assess_destination_context(&candidate, context_guard_body(&body).as_ref())?;
    if let Some(bytes) = assessment
        .input_estimate
        .and_then(|tokens| tokens.checked_mul(2))
        .and_then(|bytes| usize::try_from(bytes).ok())
    {
        crate::http::request::release_large_temporary_pages(bytes);
    }
    if !assessment.blocked() {
        return Ok(body);
    }
    let Some(safe_budget) = assessment.safe_input_limit else {
        return Err(DestinationPrepareError::Context(assessment.into()));
    };
    let mut summary_failure = None;
    let mut activity_guard = None;
    let compacted = emp_history::context::compact_with(
        &body,
        candidate.model.value(),
        safe_budget,
        |summary_body| {
            let ids = match projection_ids() {
                Ok(ids) => ids,
                Err(_) => return Err(()),
            };
            if activity_guard.is_none() {
                activity_guard = Some(state.backend.activity.begin(
                    crate::services::activity::ActivityIdentity::from_route(&candidate),
                    &state.backend.accounts.quota_revision,
                    &state.backend.accounts.quota_condition,
                ));
            }
            match crate::services::compaction::execute_summary_request(
                state,
                &candidate,
                summary_body,
                incoming,
                &ids,
                monitor.as_deref_mut(),
            ) {
                Ok((result, _)) => response_output_text(&result.body).ok_or(()),
                Err(error) => {
                    summary_failure = Some(match error {
                        crate::services::compaction::SummaryExecutionError::Router(error) => {
                            DestinationPrepareError::Router(error)
                        }
                        crate::services::compaction::SummaryExecutionError::ClaudeCli(error) => {
                            DestinationPrepareError::ClaudeCli(error)
                        }
                        crate::services::compaction::SummaryExecutionError::Disconnected => {
                            DestinationPrepareError::Disconnected
                        }
                    });
                    Err(())
                }
            }
        },
    )
    .map_err(|reason| {
        if reason == "compaction_unit_too_large" {
            return DestinationPrepareError::Context(Box::new(assessment.clone()));
        }
        summary_failure
            .take()
            .unwrap_or(DestinationPrepareError::History(reason))
    })?;
    let final_assessment =
        assess_destination_context(&candidate, context_guard_body(&compacted).as_ref())?;
    if final_assessment.blocked() {
        return Err(DestinationPrepareError::Context(final_assessment.into()));
    }
    Ok(compacted)
}

pub(crate) fn destination_error_response(error: DestinationPrepareError) -> Vec<u8> {
    match error {
        DestinationPrepareError::Router(error) => router_error_response(error),
        DestinationPrepareError::ClaudeCli(error) => error.http_response(),
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

#[cfg(test)]
mod context_guard_body_tests {
    use super::context_guard_body;
    use crate::services::compaction::COMPACTION_PROMPT;
    use serde_json::json;
    use std::borrow::Cow;

    #[test]
    fn ordinary_context_guard_borrows_the_original_body() {
        let body = json!({
            "model":"demo/model",
            "input":[{"type":"message","role":"user","content":"history"}]
        });

        assert!(matches!(
            context_guard_body(&body),
            Cow::Borrowed(guard) if std::ptr::eq(guard, &body)
        ));
    }

    #[test]
    fn compaction_context_guard_owns_the_summary_projection() {
        let body = json!({
            "model":"demo/model",
            "input":[
                {"type":"message","role":"user","content":"history"},
                {"type":"compaction_trigger"}
            ]
        });

        let Cow::Owned(guard) = context_guard_body(&body) else {
            panic!("compaction trigger must own its summary projection");
        };
        assert_eq!(guard["stream"], false);
        assert_eq!(guard["input"].as_array().unwrap().len(), 2);
        assert_eq!(guard["input"][1]["content"][0]["text"], COMPACTION_PROMPT);
        assert!(!guard.to_string().contains("compaction_trigger"));
        assert_eq!(body["input"][1]["type"], "compaction_trigger");
    }
}
