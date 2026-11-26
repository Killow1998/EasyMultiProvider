use crate::app::ServerState;
use crate::services::compaction::compaction_summary_body;
use crate::services::compaction::has_trailing_compaction_trigger;
use crate::services::compaction::response_output_text;
use crate::util::projection_ids;
use emp_context::HistoryError;
use emp_domain::ResolvedRoute;
use emp_router::RouterError;
use emp_router::project_external_payload;
use emp_router::protocol_candidates;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::BTreeMap;

pub(crate) fn prepare_history(
    state: &ServerState,
    route: &ResolvedRoute,
    body: Value,
    incoming: &BTreeMap<String, String>,
) -> Result<Value, HistoryError> {
    let reader = emp_codex::history::CodexHomeHistoryReader::new(&state.backend.codex.home);
    emp_context::prepare_owned(
        body,
        incoming,
        route.dialect == emp_domain::Dialect::CodexNative,
        &reader,
    )
}

pub(crate) enum DestinationPrepareError {
    Collaboration {
        status: u16,
        code: &'static str,
        message: &'static str,
    },
    Router(RouterError),
    ClaudeCli(crate::services::claude_cli::ClaudeCliError),
    Disconnected,
    History(&'static str),
    Context(Box<emp_context::context::ContextAssessment>),
}

#[cfg(test)]
mod claude_context_tests;

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
) -> Result<emp_context::context::ContextAssessment, DestinationPrepareError> {
    if crate::services::claude_cli::selected(candidate) {
        return crate::services::claude_cli::assess_context(candidate, body)
            .map_err(DestinationPrepareError::ClaudeCli);
    }
    let payload =
        project_external_payload(candidate, body).map_err(DestinationPrepareError::Router)?;
    Ok(emp_context::context::assess(
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
    if route.dialect == emp_domain::Dialect::CodexNative {
        if body.get("previous_response_id").is_none_or(Value::is_null)
            && let Some(payload) = crate::services::context::payload(state, route, &body)
        {
            let assessment = emp_context::context::assess(
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
    crate::services::collaboration::validate(state, &body, incoming)?;
    let protocol =
        protocol_candidates(route)
            .into_iter()
            .next()
            .ok_or(DestinationPrepareError::History(
                "destination_protocol_missing",
            ))?;
    let candidate = route
        .with_protocol(protocol)
        .map_err(|_| DestinationPrepareError::History("destination_protocol_invalid"))?;
    let assessment = assess_destination_context(&candidate, context_guard_body(&body).as_ref())?;
    if let Some(bytes) = assessment
        .input_estimate
        .and_then(|tokens| tokens.checked_mul(2))
        .and_then(|bytes| usize::try_from(bytes).ok())
    {
        crate::util::release_large_temporary_pages(bytes);
    }
    let fit_output = |mut request: Value, assessment: &emp_context::context::ContextAssessment| {
        if candidate.model.value().get("_emp_flexible_output_limit") == Some(&Value::Bool(true))
            && let Some(cap) = assessment.output_reserve
            && (request["max_output_tokens"]
                .as_u64()
                .is_some_and(|n| n > cap)
                || (request["max_output_tokens"].is_null()
                    && candidate.protocol == emp_domain::Protocol::AnthropicMessages))
        {
            request["max_output_tokens"] = cap.into();
        }
        request
    };
    if !assessment.blocked() {
        return Ok(fit_output(body, &assessment));
    }
    let Some(safe_budget) = assessment.safe_input_limit else {
        return Err(DestinationPrepareError::Context(assessment.into()));
    };
    let mut summary_failure = None;
    let mut summary_activity = None;
    let lineage = body
        .as_object()
        .and_then(|root| emp_context::request_history_anchor(root, incoming).ok())
        .and_then(|anchor| anchor.forked_from_thread_id.or(anchor.thread_id));
    let scope = lineage.as_deref().map(|thread| {
        [
            thread,
            candidate.provider_id.as_str(),
            candidate.endpoint_fingerprint.as_str(),
            candidate.deployment_identity.as_str(),
            candidate.upstream_model.as_str(),
            candidate.protocol.as_config_str(),
        ]
    });
    let compacted = emp_context::context::compact_with_cache(
        &body,
        candidate.model.value(),
        safe_budget,
        scope
            .as_ref()
            .map(|scope| (&state.backend.history_summaries, scope.as_slice())),
        |summary_body| {
            let (ids, request_id) = match (projection_ids(), crate::util::random_hex(8)) {
                (Ok(ids), Ok(request_id)) => (ids, request_id),
                _ => {
                    summary_failure = Some(DestinationPrepareError::History(
                        "summary_request_id_failed",
                    ));
                    return Err(());
                }
            };
            let mut summary_headers = incoming.clone();
            summary_headers.retain(|name, _| !name.eq_ignore_ascii_case("x-emp-request-id"));
            summary_headers.insert("x-emp-request-id".into(), request_id);
            let mut usage = crate::services::request_outcome::RequestGuard::deferred(
                state,
                &candidate,
                summary_body,
                &summary_headers,
                None,
                "history_summary",
            );
            usage.track_history_route(&mut summary_activity);
            match crate::services::compaction::execute_summary_request(
                state,
                &candidate,
                summary_body,
                &summary_headers,
                &ids,
                monitor.as_deref_mut(),
                &mut usage,
            ) {
                Ok((result, _)) => {
                    usage.http_status(result.status);
                    usage.upstream_observation(&result.observation);
                    usage.observe(&result.body);
                    response_output_text(&result.body).ok_or_else(|| {
                        summary_failure =
                            Some(DestinationPrepareError::History("summary_output_missing"));
                    })
                }
                Err(error) => {
                    use crate::services::compaction::SummaryExecutionError;
                    summary_failure = Some(match error {
                        SummaryExecutionError::Router(error) => {
                            usage.router_error(&error);
                            DestinationPrepareError::Router(error)
                        }
                        SummaryExecutionError::ClaudeCli(error) => {
                            DestinationPrepareError::ClaudeCli(error)
                        }
                        SummaryExecutionError::Disconnected => {
                            usage.disconnected();
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
    Ok(fit_output(compacted, &final_assessment))
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
