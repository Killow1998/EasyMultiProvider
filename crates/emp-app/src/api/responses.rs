use crate::api::history_response::{PreparationOutput, prepare_http_error};
use crate::api::streaming::ObservedSse;
use crate::api::streaming::serve_external_stream;
use crate::api::streaming::serve_generated_response;
use crate::api::streaming::serve_native_stream;
use crate::app::ServerState;
use crate::http::request::Request;
use crate::proxy::workflow::{self, ContextPreparation};
use crate::server::http::stream::BodyOutput;
use crate::server::http::{error::HttpReply, inference::InferenceOutcome};
use crate::services::compaction::external_compaction_response;
use crate::services::compaction::has_trailing_compaction_trigger;
use crate::services::native;
use crate::services::observation::request::{Phase, RequestObservation};
use crate::services::providers::save_learned_protocol;
use crate::services::request_preparation::{PreparedRequest, RequestOperation};
use crate::util::projection_ids;
use emp_domain::json_truthy;
use emp_state::{config::LearnedProtocol, observed_at_now};

pub(crate) fn execute<'a>(
    downstream: &mut BodyOutput<'a>,
    request: Request<'_>,
    body: serde_json::Value,
    state: &'a ServerState,
    observation: &mut RequestObservation,
) -> InferenceOutcome {
    observation.body(
        body.get("reasoning"),
        body.get("input"),
        body.get("client_metadata"),
        body.get("stream"),
    );
    let incoming = request.incoming();
    let incoming = observation.headers(incoming);
    observation.phase(Phase::ResolveRoute);
    if let Some(config) = crate::api::aero::selected_config(state, &body) {
        return crate::api::aero::relay(
            downstream,
            state,
            &config,
            &body,
            (request, &incoming),
            observation,
            false,
        );
    }
    let PreparedRequest {
        config,
        route,
        body,
    } = match workflow::resolve(
        state,
        body,
        RequestOperation::Responses,
        &incoming,
        observation,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            return prepare_http_error(state, error, observation, PreparationOutput::Responses);
        }
    };
    state
        .backend
        .claude_turns
        .selected(&route, &body, &incoming);
    observation.phase(Phase::ValidateInput);
    if crate::services::claude_cli::selected(&route)
        && let Err(error) = crate::services::claude_cli::preflight_input(&body)
    {
        return crate::api::claude_response::http_response_for_route(&error, &route).into();
    }
    let ids = match projection_ids() {
        Ok(ids) => ids,
        Err(_) => {
            return InferenceOutcome::Buffered(HttpReply::error(
                500,
                "internal server error",
                None,
            ));
        }
    };
    let stream_requested = body.get("stream").is_some_and(json_truthy);
    let body = match ContextPreparation::begin(state, &route, &incoming, observation).prepare(
        body,
        observation,
        || downstream.disconnect().ok(),
    ) {
        Ok(body) => body,
        Err(error) => {
            let output = if stream_requested {
                PreparationOutput::Sse(downstream, &incoming)
            } else {
                PreparationOutput::Responses
            };
            return prepare_http_error(state, error, observation, output);
        }
    };
    observation.phase(Phase::Execute);
    if route.dialect != emp_domain::Dialect::CodexNative && has_trailing_compaction_trigger(&body) {
        let started = std::time::Instant::now();
        let mut usage = crate::services::request_outcome::RequestGuard::deferred(
            state,
            &route,
            &body,
            &incoming,
            None,
            "responses",
        )
        .started_at(started);
        let (compacted, candidate) = if crate::services::claude_cli::selected(&route) {
            let summary_body = crate::services::compaction::compaction_summary_body(&body);
            let mut monitor = downstream.disconnect().ok();
            usage.track_route();
            match crate::services::claude_cli::execute_complete(
                state,
                &route,
                &summary_body,
                &incoming,
                monitor.as_mut(),
                &mut usage,
                &mut |_, _| Ok(()),
            ) {
                Ok(completion) => {
                    let Some(summary) = crate::services::compaction::response_output_text(
                        &completion.response.body,
                    ) else {
                        return InferenceOutcome::Buffered(HttpReply::error(
                            502,
                            "external compaction failed",
                            Some("external_compaction_failed"),
                        ));
                    };
                    usage.http_status(completion.response.status);
                    let compacted =
                        match crate::services::compaction::external_compaction_from_summary(
                            &body, &summary,
                        ) {
                            Ok(compacted) => compacted,
                            Err(error) => {
                                return crate::api::history_response::compaction_error_response(
                                    error,
                                )
                                .into();
                            }
                        };
                    (compacted, completion.route)
                }
                Err(crate::services::claude_cli::ClaudeCliError::Disconnected) => {
                    usage.disconnected();
                    return InferenceOutcome::NoBufferedReply;
                }
                Err(error) => {
                    if let crate::services::claude_cli::ClaudeCliError::Router(router_error) =
                        &error
                    {
                        usage.router_error(router_error);
                    } else if matches!(
                        &error,
                        crate::services::claude_cli::ClaudeCliError::ShuttingDown
                    ) {
                        usage.http_status(503);
                    }
                    return crate::api::claude_response::http_response_for_route(&error, &route)
                        .into();
                }
            }
        } else {
            let mut monitor = downstream.disconnect().ok();
            match external_compaction_response(
                state,
                &route,
                &body,
                &incoming,
                &ids,
                monitor.as_mut(),
                &mut usage,
            ) {
                Ok(result) => result,
                Err(error) => {
                    return crate::api::history_response::compaction_error_response(error).into();
                }
            }
        };
        let learned = LearnedProtocol::from_route(&route, candidate.protocol, observed_at_now());
        usage.observe(&compacted);
        usage.finish();
        save_learned_protocol(state, learned);
        if body.get("stream").is_some_and(json_truthy) {
            let downstream = ObservedSse::new(downstream, observation, &incoming);
            return serve_generated_response(downstream, usage, compacted, &ids);
        }
        let compacted = match serde_json::to_vec(&compacted) {
            Ok(body) => body,
            Err(_) => {
                return InferenceOutcome::Buffered(HttpReply::error(
                    500,
                    "internal server error",
                    None,
                ));
            }
        };
        return InferenceOutcome::Buffered(HttpReply::new(200, compacted));
    }
    if crate::services::claude_cli::selected(&route) {
        let started = std::time::Instant::now();
        let mut monitor = downstream.disconnect().ok();
        let mut usage = crate::services::request_outcome::RequestGuard::new(
            state,
            &route,
            &body,
            &incoming,
            None,
            "responses",
        )
        .started_at(started);
        let mut downstream = ObservedSse::new(downstream, observation, &incoming);
        let mut live =
            crate::api::claude_stream::LiveText::new(&ids, false, |event: &serde_json::Value| {
                if event["type"] == "response.created" {
                    downstream.head()?;
                }
                let frame = crate::services::events::sse_frame(
                    event["type"].as_str().expect("event type"),
                    event,
                )
                .map_err(std::io::Error::other)?;
                downstream.event(event, &[frame])
            });
        let completion = match crate::services::claude_cli::execute_complete(
            state,
            &route,
            &body,
            &incoming,
            monitor.as_mut(),
            &mut usage,
            &mut |id, text| {
                if stream_requested {
                    live.push(id, text)
                } else {
                    Ok(())
                }
            },
        ) {
            Ok(completion) => completion,
            Err(error) => {
                let disconnected = matches!(
                    error,
                    crate::services::claude_cli::ClaudeCliError::Disconnected
                );
                if live.started() {
                    if !disconnected {
                        let _ = live.event(
                            &crate::api::claude_response::websocket_value_for_route(&error, &route),
                        );
                    }
                    return downstream.finish(usage, Ok(()));
                }
                if disconnected {
                    return InferenceOutcome::NoBufferedReply;
                }
                return crate::api::claude_response::http_response_for_route(&error, &route).into();
            }
        };
        let learned =
            LearnedProtocol::from_route(&route, completion.route.protocol, observed_at_now());
        let route = &completion.route;
        let mut response_value = completion.response.body;
        if stream_requested {
            response_value["id"] = serde_json::json!(ids.response_id());
        }
        usage.candidate(route);
        usage.http_status(completion.response.status);
        usage.upstream_observation(&completion.response.observation);
        usage.observe(&response_value);
        usage.finish();
        if response_value["status"] == "completed" {
            crate::services::context::record(state, route, &body, true);
        }
        save_learned_protocol(state, learned);
        if stream_requested {
            let _ = live.finish(response_value);
            return downstream.finish(usage, Ok(()));
        }
        let body = serde_json::to_vec(&response_value).expect("validated Claude response");
        return InferenceOutcome::Buffered(
            HttpReply::new(completion.response.status, body)
                .with_content_type(completion.response.content_type),
        );
    }
    let native = route.dialect == emp_domain::Dialect::CodexNative;
    let streaming = body.get("stream").is_some_and(json_truthy);
    if streaming {
        let downstream = ObservedSse::new(downstream, observation, &incoming);
        return if native {
            serve_native_stream(downstream, state, &route, &config, &body, &incoming, &ids)
        } else {
            serve_external_stream(downstream, state, &route, &body, &incoming, &ids)
        };
    }
    if native {
        return crate::api::native_response::complete_response(native::complete(
            state, &route, &config, &body, &incoming,
        ))
        .into();
    }
    let result = match crate::services::external::complete(state, &route, &body, &incoming, &ids) {
        Ok(result) => result,
        Err(error) => {
            return crate::api::failure_response::external_complete_error(error).into();
        }
    };
    let body = match serde_json::to_vec(&result.body) {
        Ok(body) => body,
        Err(_) => {
            return InferenceOutcome::Buffered(HttpReply::error(
                500,
                "internal server error",
                None,
            ));
        }
    };
    HttpReply::new(result.status, body)
        .with_content_type(result.content_type)
        .into()
}
