//! Responses HTTP request orchestration.

use crate::api::failure_response::request_router_error_response;
use crate::api::failure_response::route_resolution_response;
use crate::api::history_response::destination_error_response;
use crate::api::history_response::history_http_error;
use crate::api::history_response::history_stream_error;
use crate::api::streaming::ObservedSse;
use crate::api::streaming::generated_response_stream;
use crate::api::streaming::serve_external_stream;
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
use crate::services::request_preparation::{
    PreparedRequest, RequestOperation, RequestPreparationError, prepare_request,
};
use crate::util::projection_ids;
use crate::util::python_truthy;
use emp_history::HistoryError;
use std::collections::BTreeMap;
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
    observation.phase(Phase::ReadBody);
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return ResponsesRequestResult::Buffered(body_error_response(error)),
    };
    observation.body(body.get("reasoning"), body.get("stream"));
    observation.phase(Phase::ResolveRoute);
    let PreparedRequest {
        config,
        route,
        mut body,
    } = match prepare_request(state, body, RequestOperation::Responses) {
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
    let incoming: BTreeMap<String, String> = request
        .headers
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
        .collect();
    let incoming = observation.headers(incoming);
    let stream_requested = python_truthy(body.get("stream"));
    observation.phase(Phase::PrepareHistory);
    body = match prepare_history(state, &route, body, &incoming) {
        Ok(body) => body,
        Err(error) if stream_requested => {
            let failed = history_stream_error(&error);
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
            let mut downstream = ObservedSse::new(stream, observation);
            let _ = downstream.head();
            let _ = downstream.frames(&[frame], true);
            return ResponsesRequestResult::Streamed;
        }
        Err(error) => return ResponsesRequestResult::Buffered(history_http_error(&error)),
    };
    observation.phase(Phase::PrepareDestination);
    let destination_context = {
        let mut monitor = crate::services::disconnect::DisconnectMonitor::start(stream).ok();
        prepare_destination_context(state, &route, body, &incoming, monitor.as_mut())
    };
    body = match destination_context {
        Ok(body) => body,
        Err(DestinationPrepareError::History(reason)) if stream_requested => {
            let failed = history_stream_error(&HistoryError::new(reason));
            if let Ok(frame) = sse_frame("response.failed", &failed) {
                let mut downstream = ObservedSse::new(stream, observation);
                let _ = downstream.head();
                let _ = downstream.frames(&[frame], true);
            }
            return ResponsesRequestResult::Streamed;
        }
        Err(DestinationPrepareError::Disconnected) => return ResponsesRequestResult::Streamed,
        Err(error) => {
            return ResponsesRequestResult::Buffered(destination_error_response(error));
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
                &ids,
                monitor.as_mut(),
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
            let stream_body = match generated_response_stream(compacted, &ids) {
                Ok(body) => body,
                Err(error) => return ResponsesRequestResult::Buffered(error),
            };
            let mut downstream = ObservedSse::new(stream, observation);
            if downstream.head().is_err() || downstream.frames(&[stream_body], true).is_err() {
                return ResponsesRequestResult::Streamed;
            }
            return ResponsesRequestResult::Streamed;
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
        let completion = match crate::services::claude_cli::execute_complete(
            state,
            &route,
            &body,
            &incoming,
            &ids,
            monitor.as_mut(),
        ) {
            Ok(completion) => completion,
            Err(crate::services::claude_cli::ClaudeCliError::Disconnected) => {
                let mut usage = crate::services::request_outcome::RequestOutcome::new(
                    state,
                    &route,
                    &body,
                    &incoming,
                    None,
                    "responses",
                )
                .started_at(started);
                usage.disconnected();
                return ResponsesRequestResult::Streamed;
            }
            Err(error) => {
                if matches!(
                    &error,
                    crate::services::claude_cli::ClaudeCliError::Router(_)
                        | crate::services::claude_cli::ClaudeCliError::ShuttingDown
                ) {
                    let mut usage = crate::services::request_outcome::RequestOutcome::new(
                        state,
                        &route,
                        &body,
                        &incoming,
                        None,
                        "responses",
                    )
                    .started_at(started);
                    match &error {
                        crate::services::claude_cli::ClaudeCliError::Router(router_error) => {
                            usage.router_error(router_error);
                        }
                        crate::services::claude_cli::ClaudeCliError::ShuttingDown => {
                            usage.http_status(503);
                        }
                        _ => unreachable!("guarded Claude CLI error variant"),
                    }
                }
                return ResponsesRequestResult::Buffered(
                    crate::api::claude_response::http_response_for_route(&error, &route),
                );
            }
        };
        let route = &completion.route;
        let response_value = completion.response.body;
        let mut usage = crate::services::request_outcome::RequestOutcome::new(
            state,
            route,
            &body,
            &incoming,
            None,
            "responses",
        )
        .started_at(completion.request_started);
        usage.http_status(completion.response.status);
        usage.observe(&response_value);
        usage.finish();
        if response_value["status"] == "completed" {
            crate::services::context::record(state, route, &body, true);
        }
        crate::services::providers::persist_protocol_observation(state, route);
        if stream_requested {
            let stream_body = match generated_response_stream(response_value, &ids) {
                Ok(body) => body,
                Err(error) => return ResponsesRequestResult::Buffered(error),
            };
            let mut downstream = ObservedSse::new(stream, observation);
            if downstream.head().is_err() || downstream.frames(&[stream_body], true).is_err() {
                return ResponsesRequestResult::Streamed;
            }
            return ResponsesRequestResult::Streamed;
        }
        let body = match serde_json::to_vec(&response_value) {
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
                ObservedSse::new(stream, observation),
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
            native::complete(
                state,
                &route,
                &config,
                body.as_object().expect("validated request object"),
                &incoming,
            ),
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
            ObservedSse::new(stream, observation),
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
