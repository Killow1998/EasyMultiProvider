//! Responses HTTP request orchestration.

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
use crate::http::auth::proxy_allowed;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::request::read_json_body;
use crate::http::response::body_error_response;
use crate::http::response::json_error_response;
use crate::http::response::response;
use crate::http::response::status_text;
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
use crate::util::python_truthy;
use emp_history::HistoryError;
use std::net::TcpStream;

pub(crate) enum ResponsesRequestResult {
    Buffered(Vec<u8>),
    Streamed,
}

pub(crate) fn responses_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
    observation: &mut RequestObservation,
) -> ResponsesRequestResult {
    if !proxy_allowed(request, state, now) {
        let status = if same_origin(request, state.port) {
            401
        } else {
            403
        };
        return ResponsesRequestResult::Buffered(json_error_response(
            status,
            status_text(status),
            "proxy caller authentication is required",
            None,
            &[],
        ));
    }
    if same_origin(request, state.port)
        && state
            .sessions
            .contains(request.session_token().as_deref(), now)
        && let Some(kind) = request.header("x-emp-probe")
        && emp_state::diagnostics::MODEL_TEST_OPERATIONS.contains(&kind)
    {
        observation.probe(kind);
    }
    observation.phase(Phase::ReadBody);
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return ResponsesRequestResult::Buffered(body_error_response(error)),
    };
    observation.body(
        body.get("reasoning"),
        body.get("input"),
        body.get("client_metadata"),
        body.get("stream"),
    );
    let incoming = request.incoming();
    let incoming = observation.headers(incoming);
    observation.phase(Phase::ResolveRoute);
    let PreparedRequest {
        config,
        route,
        mut body,
    } = match crate::services::memory_routing::prepare(state, body, &incoming) {
        Ok(prepared) => prepared,
        Err(RequestPreparationError::ModelRequired) => {
            return ResponsesRequestResult::Buffered(request_router_error_response(
                400,
                "request.model is required",
            ));
        }
        Err(RequestPreparationError::Route(error)) => {
            return ResponsesRequestResult::Buffered(route_resolution_response(error));
        }
        Err(RequestPreparationError::MemoryUnavailable(reason)) => {
            return ResponsesRequestResult::Buffered(request_router_error_response(
                503,
                crate::services::memory_routing::error_message(reason),
            ));
        }
        Err(_) => {
            return ResponsesRequestResult::Buffered(json_error_response(
                500,
                status_text(500),
                "internal server error",
                None,
                &[],
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
        return ResponsesRequestResult::Buffered(
            crate::api::claude_response::http_response_for_route(&error, &route),
        );
    }
    let ids = match projection_ids() {
        Ok(ids) => ids,
        Err(_) => {
            return ResponsesRequestResult::Buffered(json_error_response(
                500,
                status_text(500),
                "internal server error",
                None,
                &[],
            ));
        }
    };
    let stream_requested = python_truthy(body.get("stream"));
    observation.phase(Phase::PrepareHistory);
    body = match prepare_history(state, &route, body, &incoming) {
        Ok(body) => body,
        Err(error) if stream_requested => {
            let failed = history_stream_error(&error, observation);
            let frame = match sse_frame("response.failed", &failed) {
                Ok(frame) => frame,
                Err(_) => {
                    return ResponsesRequestResult::Buffered(json_error_response(
                        500,
                        status_text(500),
                        "internal server error",
                        None,
                        &[],
                    ));
                }
            };
            let mut downstream = ObservedSse::new(stream, observation, &incoming);
            let _ = downstream.head();
            let _ = downstream.frames(&[frame], "response.failed");
            return ResponsesRequestResult::Streamed;
        }
        Err(error) => {
            return ResponsesRequestResult::Buffered(history_http_error(&error, observation));
        }
    };
    observation.phase(Phase::PrepareDestination);
    let destination_context = {
        let mut monitor = crate::services::disconnect::DisconnectMonitor::start(stream).ok();
        prepare_destination_context(state, &route, body, &incoming, monitor.as_mut())
    };
    body = match destination_context {
        Ok(body) => body,
        Err(DestinationPrepareError::History(reason)) if stream_requested => {
            let failed = history_stream_error(&HistoryError::new(reason), observation);
            if let Ok(frame) = sse_frame("response.failed", &failed) {
                let mut downstream = ObservedSse::new(stream, observation, &incoming);
                let _ = downstream.head();
                let _ = downstream.frames(&[frame], "response.failed");
            }
            return ResponsesRequestResult::Streamed;
        }
        Err(DestinationPrepareError::Disconnected) => return ResponsesRequestResult::Streamed,
        Err(error) => {
            return ResponsesRequestResult::Buffered(destination_error_response(
                error,
                observation,
            ));
        }
    };
    observation.phase(Phase::Execute);
    if route.dialect != emp_core::Dialect::CodexNative && has_trailing_compaction_trigger(&body) {
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
            let mut monitor = crate::services::disconnect::DisconnectMonitor::start(stream).ok();
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
                        return ResponsesRequestResult::Buffered(json_error_response(
                            502,
                            status_text(502),
                            "external compaction failed",
                            Some("external_compaction_failed"),
                            &[],
                        ));
                    };
                    usage.http_status(completion.response.status);
                    let compacted =
                        match crate::services::compaction::external_compaction_from_summary(
                            &body, &summary,
                        ) {
                            Ok(compacted) => compacted,
                            Err(error) => {
                                return ResponsesRequestResult::Buffered(
                                    crate::api::history_response::compaction_error_response(error),
                                );
                            }
                        };
                    (compacted, completion.route)
                }
                Err(crate::services::claude_cli::ClaudeCliError::Disconnected) => {
                    usage.disconnected();
                    return ResponsesRequestResult::Streamed;
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
                    return ResponsesRequestResult::Buffered(
                        crate::api::claude_response::http_response_for_route(&error, &route),
                    );
                }
            }
        } else {
            let mut monitor = crate::services::disconnect::DisconnectMonitor::start(stream).ok();
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
                    return ResponsesRequestResult::Buffered(
                        crate::api::history_response::compaction_error_response(error),
                    );
                }
            }
        };
        usage.observe(&compacted);
        usage.finish();
        persist_protocol_observation(state, &candidate);
        if python_truthy(body.get("stream")) {
            let downstream = ObservedSse::new(stream, observation, &incoming);
            return match serve_generated_response(downstream, compacted, &ids) {
                Ok(()) => ResponsesRequestResult::Streamed,
                Err(error) => ResponsesRequestResult::Buffered(error),
            };
        }
        let compacted = match serde_json::to_vec(&compacted) {
            Ok(body) => body,
            Err(_) => {
                return ResponsesRequestResult::Buffered(json_error_response(
                    500,
                    status_text(500),
                    "internal server error",
                    None,
                    &[],
                ));
            }
        };
        return ResponsesRequestResult::Buffered(response(
            "HTTP/1.1 200 OK",
            "application/json",
            &compacted,
            &[],
        ));
    }
    if crate::services::claude_cli::selected(&route) {
        let started = std::time::Instant::now();
        let mut monitor = crate::services::disconnect::DisconnectMonitor::start(stream).ok();
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
            stream_requested.then(|| ObservedSse::new(stream, observation, &incoming));
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
                return ResponsesRequestResult::Streamed;
            }
            Err(error) => {
                if live.started() {
                    let _ = live.event(&crate::api::claude_response::websocket_value_for_route(
                        &error, &route,
                    ));
                    return ResponsesRequestResult::Streamed;
                }
                return ResponsesRequestResult::Buffered(
                    crate::api::claude_response::http_response_for_route(&error, &route),
                );
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
            return ResponsesRequestResult::Streamed;
        }
        let body = serde_json::to_vec(&response_value).expect("validated Claude response");
        return ResponsesRequestResult::Buffered(response(
            &format!(
                "HTTP/1.1 {} {}",
                completion.response.status,
                status_text(completion.response.status)
            ),
            &completion.response.content_type,
            &body,
            &[],
        ));
    }
    if route.dialect == emp_core::Dialect::CodexNative {
        if python_truthy(body.get("stream")) {
            let _activity_guard = state.backend.activity.begin(
                crate::services::activity::ActivityIdentity::from_route(&route),
            );
            return match serve_native_stream(
                ObservedSse::new(stream, observation, &incoming),
                state,
                &route,
                &config,
                &body,
                &incoming,
                &ids,
            ) {
                Ok(()) => ResponsesRequestResult::Streamed,
                Err(response) => ResponsesRequestResult::Buffered(response),
            };
        }
        let _activity_guard =
            state
                .backend
                .activity
                .begin(crate::services::activity::ActivityIdentity::from_route(
                    &route,
                ));
        return ResponsesRequestResult::Buffered(crate::api::native_response::complete_response(
            native::complete(state, &route, &config, &body, &incoming),
        ));
    }
    if python_truthy(body.get("stream")) {
        let _activity_guard =
            state
                .backend
                .activity
                .begin(crate::services::activity::ActivityIdentity::from_route(
                    &route,
                ));
        return match serve_external_stream(
            ObservedSse::new(stream, observation, &incoming),
            state,
            &route,
            &body,
            &incoming,
            &ids,
        ) {
            Ok(()) => ResponsesRequestResult::Streamed,
            Err(response) => ResponsesRequestResult::Buffered(response),
        };
    }
    let result = match crate::services::external::complete(state, &route, &body, &incoming, &ids) {
        Ok(result) => result,
        Err(error) => {
            return ResponsesRequestResult::Buffered(
                crate::api::failure_response::external_complete_error(error),
            );
        }
    };
    let body = match serde_json::to_vec(&result.body) {
        Ok(body) => body,
        Err(_) => {
            return ResponsesRequestResult::Buffered(json_error_response(
                500,
                status_text(500),
                "internal server error",
                None,
                &[],
            ));
        }
    };
    ResponsesRequestResult::Buffered(response(
        &format!("HTTP/1.1 {} {}", result.status, status_text(result.status)),
        &result.content_type,
        &body,
        &[],
    ))
}
