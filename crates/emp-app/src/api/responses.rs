use crate::api::failure_response::request_router_error_response;
use crate::api::failure_response::route_resolution_response;
use crate::api::history_response::destination_error_response;
use crate::api::history_response::history_http_error;
use crate::api::history_response::history_stream_error;
use crate::api::streaming::ObservedSse;
use crate::api::streaming::serve_external_stream;
use crate::api::streaming::serve_generated_response;
use crate::api::streaming::serve_native_stream;
use crate::app::ServerState;
use crate::http::request::Request;
use crate::server::http::stream::BodyOutput;
use crate::server::http::{error::HttpReply, inference::InferenceOutcome};
use crate::services::compaction::external_compaction_response;
use crate::services::compaction::has_trailing_compaction_trigger;
use crate::services::events::sse_frame;
use crate::services::history::DestinationPrepareError;
use crate::services::history::prepare_destination_context;
use crate::services::history::prepare_history;
use crate::services::native;
use crate::services::observation::request::{Phase, RequestObservation};
use crate::services::providers::persist_protocol_observation;
use crate::services::request_preparation::{PreparedRequest, RequestPreparationError};
use crate::util::projection_ids;
use emp_context::HistoryError;
use emp_domain::json_truthy;

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
        mut body,
    } = match crate::services::memory_routing::prepare(state, body, &incoming) {
        Ok(prepared) => prepared,
        Err(RequestPreparationError::ModelRequired) => {
            return InferenceOutcome::Buffered(request_router_error_response(
                400,
                "request.model is required",
            ));
        }
        Err(RequestPreparationError::Route(error)) => {
            return InferenceOutcome::Buffered(route_resolution_response(error));
        }
        Err(RequestPreparationError::TeamAccess(error)) => {
            return InferenceOutcome::Buffered(super::native_response::error_reply(
                error.public_error(state),
            ));
        }
        Err(RequestPreparationError::MemoryUnavailable(reason)) => {
            return InferenceOutcome::Buffered(request_router_error_response(
                503,
                crate::services::memory_routing::error_message(reason),
            ));
        }
        Err(_) => {
            return InferenceOutcome::Buffered(HttpReply::error(
                500,
                "internal server error",
                None,
            ));
        }
    };
    observation.selected(&route);
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
    observation.phase(Phase::PrepareHistory);
    body = match prepare_history(state, &route, body, &incoming) {
        Ok(body) => body,
        Err(error) if stream_requested => {
            let failed = history_stream_error(&error, observation);
            let frame = match sse_frame("response.failed", &failed) {
                Ok(frame) => frame,
                Err(_) => {
                    return InferenceOutcome::Buffered(HttpReply::error(
                        500,
                        "internal server error",
                        None,
                    ));
                }
            };
            let mut downstream = ObservedSse::new(downstream, observation, &incoming);
            let _ = downstream.head();
            let _ = downstream.frames(&[frame], "response.failed");
            return InferenceOutcome::Streamed;
        }
        Err(error) => {
            return InferenceOutcome::Buffered(history_http_error(&error, observation));
        }
    };
    observation.phase(Phase::PrepareDestination);
    let destination_context = {
        let mut monitor = downstream.disconnect().ok();
        prepare_destination_context(state, &route, body, &incoming, monitor.as_mut())
    };
    body = match destination_context {
        Ok(body) => body,
        Err(DestinationPrepareError::History(reason)) if stream_requested => {
            let failed = history_stream_error(&HistoryError::new(reason), observation);
            if let Ok(frame) = sse_frame("response.failed", &failed) {
                let mut downstream = ObservedSse::new(downstream, observation, &incoming);
                let _ = downstream.head();
                let _ = downstream.frames(&[frame], "response.failed");
            }
            return InferenceOutcome::Streamed;
        }
        Err(DestinationPrepareError::Disconnected) => return InferenceOutcome::Streamed,
        Err(error) => {
            return destination_error_response(error, observation).into();
        }
    };
    observation.phase(Phase::Execute);
    if route.dialect != emp_domain::Dialect::CodexNative && has_trailing_compaction_trigger(&body) {
        let started = std::time::Instant::now();
        let mut usage = crate::services::request_outcome::RequestOutcome::new(
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
            let _activity_guard = state.backend.activity.begin(
                crate::services::activity::ActivityIdentity::from_route(&route),
            );
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
                    return InferenceOutcome::Streamed;
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
        usage.observe(&compacted);
        usage.finish();
        persist_protocol_observation(state, &candidate);
        if body.get("stream").is_some_and(json_truthy) {
            let downstream = ObservedSse::new(downstream, observation, &incoming);
            return serve_generated_response(downstream, compacted, &ids)
                .err()
                .map_or(InferenceOutcome::Streamed, Into::into);
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
        let _activity_guard =
            state
                .backend
                .activity
                .begin(crate::services::activity::ActivityIdentity::from_route(
                    &route,
                ));
        let mut usage = crate::services::request_outcome::RequestOutcome::new(
            state,
            &route,
            &body,
            &incoming,
            None,
            "responses",
        )
        .started_at(started);
        let mut downstream =
            stream_requested.then(|| ObservedSse::new(downstream, observation, &incoming));
        let mut live =
            crate::api::claude_stream::LiveText::new(&ids, false, |event: &serde_json::Value| {
                let Some(downstream) = downstream.as_mut() else {
                    return Ok(());
                };
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
            Err(crate::services::claude_cli::ClaudeCliError::Disconnected) => {
                return InferenceOutcome::Streamed;
            }
            Err(error) => {
                if live.started() {
                    let _ = live.event(&crate::api::claude_response::websocket_value_for_route(
                        &error, &route,
                    ));
                    return InferenceOutcome::Streamed;
                }
                return crate::api::claude_response::http_response_for_route(&error, &route).into();
            }
        };
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
        crate::services::providers::persist_protocol_observation(state, route);
        if stream_requested {
            let _ = live.finish(response_value);
            return InferenceOutcome::Streamed;
        }
        let body = serde_json::to_vec(&response_value).expect("validated Claude response");
        return InferenceOutcome::Buffered(
            HttpReply::new(completion.response.status, body)
                .with_content_type(completion.response.content_type),
        );
    }
    let native = route.dialect == emp_domain::Dialect::CodexNative;
    let streaming = body.get("stream").is_some_and(json_truthy);
    let _activity = (native || streaming).then(|| {
        state
            .backend
            .activity
            .begin(crate::services::activity::ActivityIdentity::from_route(
                &route,
            ))
    });
    if streaming {
        let downstream = ObservedSse::new(downstream, observation, &incoming);
        let result = if native {
            serve_native_stream(downstream, state, route, &config, body, &incoming, &ids)
        } else {
            serve_external_stream(downstream, state, &route, body, &incoming, &ids)
        };
        return result.err().map_or(InferenceOutcome::Streamed, Into::into);
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
