//! Responses HTTP request orchestration.

use crate::api::streaming::serve_external_stream;
use crate::api::streaming::serve_native_stream;
use crate::api::streaming::write_stream_frames;
use crate::api::streaming::write_stream_head;
use crate::app::ServerState;
use crate::http::auth::proxy_allowed;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::request::read_json_body;
use crate::http::response::body_error_response;
use crate::http::response::json_error_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::services::accounts::account_catalog_headers;
use crate::services::auto_review::resolve_auto_review_route;
use crate::services::compaction::external_compaction_response;
use crate::services::compaction::has_trailing_compaction_trigger;
use crate::services::events::generated_response_stream;
use crate::services::events::sse_frame;
use crate::services::failures::external_retry_delay;
use crate::services::failures::request_router_error_response;
use crate::services::failures::route_resolution_response;
use crate::services::failures::router_error_response;
use crate::services::history::DestinationPrepareError;
use crate::services::history::destination_error_response;
use crate::services::history::history_http_error;
use crate::services::history::history_stream_error;
use crate::services::history::prepare_destination_context;
use crate::services::history::prepare_history;
use crate::services::native;
use crate::services::providers::hydrate_provider_keys;
use crate::services::providers::persist_protocol_observation;
use crate::services::request_preparation::prepare_external_request;
use crate::util::projection_ids;
use crate::util::python_truthy;
use crate::util::random_hex;
use emp_codex::subscription_route_model;
use emp_core::resolve_route;
use emp_history::HistoryError;
use emp_router::ExternalRouter;
use emp_router::protocol_candidates;
use emp_transport::protocol_fallback_allowed;
use serde_json::Value;
use std::collections::BTreeMap;
use std::net::TcpStream;
use std::thread;

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
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return ResponsesRequestResult::Buffered(body_error_response(error)),
    };
    let Some(model) = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return ResponsesRequestResult::Buffered(request_router_error_response(
            400,
            "request.model is required",
        ));
    };
    let mut config = match state.backend.configuration.config.lock() {
        Ok(config) => config.clone(),
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
    hydrate_provider_keys(&mut config, &state.backend.configuration.vault);
    if let Some(config) = config.as_object_mut() {
        config.insert(
            "_native_auth_path".to_owned(),
            Value::String(
                state
                    .backend
                    .accounts
                    .native_auth_path
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
    }
    let route = match resolve_auto_review_route(state, &mut config, model).unwrap_or_else(|| {
        resolve_route(&config, model, |config, slug, account| {
            subscription_route_model(config, slug, account, |account| {
                account_catalog_headers(account, &state.backend.configuration.vault)
            })
        })
    }) {
        Ok(route) => route,
        Err(error) => {
            return ResponsesRequestResult::Buffered(route_resolution_response(error));
        }
    };
    let (route, mut body) = match prepare_external_request(&config, route, body) {
        Ok(prepared) => prepared,
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
    if crate::services::claude_cli::selected(&route)
        && let Err(error) = crate::services::claude_cli::preflight_input(&body)
    {
        return ResponsesRequestResult::Buffered(error.http_response());
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
    let request_id = match random_hex(8) {
        Ok(value) => value,
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
    let mut incoming: BTreeMap<String, String> = request
        .headers
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
        .collect();
    incoming.insert("X-EMP-Request-ID".to_owned(), request_id);
    let stream_requested = python_truthy(body.get("stream"));
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
            let _ = write_stream_head(stream);
            let _ = write_stream_frames(stream, &[frame]);
            return ResponsesRequestResult::Streamed;
        }
        Err(error) => return ResponsesRequestResult::Buffered(history_http_error(&error)),
    };
    let destination_context = {
        let mut monitor = crate::services::disconnect::DisconnectMonitor::start(stream).ok();
        prepare_destination_context(state, &route, body, &incoming, monitor.as_mut())
    };
    body = match destination_context {
        Ok(body) => body,
        Err(DestinationPrepareError::History(reason)) if stream_requested => {
            let failed = history_stream_error(&HistoryError::new(reason));
            if let Ok(frame) = sse_frame("response.failed", &failed) {
                let _ = write_stream_head(stream);
                let _ = write_stream_frames(stream, &[frame]);
            }
            return ResponsesRequestResult::Streamed;
        }
        Err(DestinationPrepareError::Disconnected) => return ResponsesRequestResult::Streamed,
        Err(error) => {
            return ResponsesRequestResult::Buffered(destination_error_response(error));
        }
    };
    if route.dialect != emp_core::Dialect::CodexNative && has_trailing_compaction_trigger(&body) {
        let started = std::time::Instant::now();
        let mut usage = crate::services::observation::Observation::new(
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
                &state.backend.accounts.quota_revision,
                &state.backend.accounts.quota_condition,
            );
            match crate::services::claude_cli::execute_complete(
                state,
                &route,
                &summary_body,
                &incoming,
                &ids,
                monitor.as_mut(),
            ) {
                Ok(crate::services::claude_cli::ClaudeCliResult::Completed(completion)) => {
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
                            Err(error) => return ResponsesRequestResult::Buffered(error),
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
                    return ResponsesRequestResult::Buffered(error.http_response());
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
                Err(error) => return ResponsesRequestResult::Buffered(error),
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
            if write_stream_head(stream).is_err()
                || write_stream_frames(stream, &[stream_body]).is_err()
            {
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
        let _activity_guard = state.backend.activity.begin(
            crate::services::activity::ActivityIdentity::from_route(&route),
            &state.backend.accounts.quota_revision,
            &state.backend.accounts.quota_condition,
        );
        let completion = match crate::services::claude_cli::execute_complete(
            state,
            &route,
            &body,
            &incoming,
            &ids,
            monitor.as_mut(),
        ) {
            Ok(crate::services::claude_cli::ClaudeCliResult::Completed(completion)) => completion,
            Err(crate::services::claude_cli::ClaudeCliError::Disconnected) => {
                let mut usage = crate::services::observation::Observation::new(
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
                    let mut usage = crate::services::observation::Observation::new(
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
                return ResponsesRequestResult::Buffered(error.http_response());
            }
        };
        let route = &completion.route;
        let response_value = completion.response.body;
        let mut usage = crate::services::observation::Observation::new(
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
            if write_stream_head(stream).is_err()
                || write_stream_frames(stream, &[stream_body]).is_err()
            {
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
                &state.backend.accounts.quota_revision,
                &state.backend.accounts.quota_condition,
            );
            return match serve_native_stream(stream, state, &route, &config, &body, &incoming, &ids)
            {
                Ok(()) => ResponsesRequestResult::Streamed,
                Err(response) => ResponsesRequestResult::Buffered(response),
            };
        }
        let _activity_guard = state.backend.activity.begin(
            crate::services::activity::ActivityIdentity::from_route(&route),
            &state.backend.accounts.quota_revision,
            &state.backend.accounts.quota_condition,
        );
        return ResponsesRequestResult::Buffered(native::complete(
            state,
            &route,
            &config,
            body.as_object().expect("validated request object"),
            &incoming,
        ));
    }
    if python_truthy(body.get("stream")) {
        let _activity_guard = state.backend.activity.begin(
            crate::services::activity::ActivityIdentity::from_route(&route),
            &state.backend.accounts.quota_revision,
            &state.backend.accounts.quota_condition,
        );
        return match serve_external_stream(stream, state, &route, &body, &incoming, &ids) {
            Ok(()) => ResponsesRequestResult::Streamed,
            Err(response) => ResponsesRequestResult::Buffered(response),
        };
    }
    let started = std::time::Instant::now();
    let router = ExternalRouter::new(&state.backend.transport.client);
    let candidates = protocol_candidates(&route);
    let mut usage = crate::services::observation::Observation::new(
        state,
        &route,
        &body,
        &incoming,
        None,
        "responses",
    )
    .started_at(started);
    let mut activity_guard = None;
    'candidate: for (index, protocol) in candidates.iter().copied().enumerate() {
        let candidate = match route.with_protocol(protocol) {
            Ok(candidate) => candidate,
            Err(error) => {
                return ResponsesRequestResult::Buffered(route_resolution_response(error));
            }
        };
        usage.candidate(&candidate);
        for attempt in 0..3 {
            usage.dispatch();
            if activity_guard.is_none() {
                activity_guard = Some(state.backend.activity.begin(
                    crate::services::activity::ActivityIdentity::from_route(&route),
                    &state.backend.accounts.quota_revision,
                    &state.backend.accounts.quota_condition,
                ));
            }
            match state
                .backend
                .transport
                .runtime
                .block_on(router.execute_complete(&candidate, &body, &incoming, &ids))
            {
                Ok(result) => {
                    usage.http_status(result.status);
                    usage.reported_model(result.reported_model.as_deref());
                    usage.observe(&result.body);
                    if result.body["status"] == "completed" {
                        crate::services::context::record(state, &candidate, &body, true);
                    }
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
                    persist_protocol_observation(state, &candidate);
                    return ResponsesRequestResult::Buffered(response(
                        &format!("HTTP/1.1 {} {}", result.status, status_text(result.status)),
                        &result.content_type,
                        &body,
                        &[],
                    ));
                }
                Err(error) => {
                    if error.error_class() == emp_transport::FailureClass::ContextLengthExceeded {
                        crate::services::context::record(state, &candidate, &body, false);
                    }
                    if let Some(delay) = external_retry_delay(&error, attempt, &candidate) {
                        usage.retry(attempt, delay, &error, false);
                        thread::sleep(delay);
                        continue;
                    }
                    if index + 1 < candidates.len()
                        && protocol_fallback_allowed(error.status(), false, false)
                    {
                        usage.retry(attempt, std::time::Duration::ZERO, &error, true);
                        continue 'candidate;
                    }
                    usage.router_error(&error);
                    return ResponsesRequestResult::Buffered(router_error_response(error));
                }
            }
        }
    }
    usage.status(503, "router_error");
    ResponsesRequestResult::Buffered(json_error_response(
        503,
        status_text(503),
        "provider protocol is unsupported",
        Some("router_error"),
        &[],
    ))
}
