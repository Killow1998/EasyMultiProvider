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
    let mut body = match read_json_body(stream, request, body_prefix, state) {
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
    body = match prepare_history(state, &route, &body, &incoming) {
        Ok(body) => body,
        Err(error) if python_truthy(body.get("stream")) => {
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
    let stream_requested = python_truthy(body.get("stream"));
    body = match prepare_destination_context(state, &route, body, &incoming) {
        Ok(body) => body,
        Err(DestinationPrepareError::History(reason)) if stream_requested => {
            let failed = history_stream_error(&HistoryError::new(reason));
            if let Ok(frame) = sse_frame("response.failed", &failed) {
                let _ = write_stream_head(stream);
                let _ = write_stream_frames(stream, &[frame]);
            }
            return ResponsesRequestResult::Streamed;
        }
        Err(error) => {
            return ResponsesRequestResult::Buffered(destination_error_response(error));
        }
    };
    if route.dialect != emp_core::Dialect::CodexNative && has_trailing_compaction_trigger(&body) {
        let mut usage = crate::services::observation::Observation::new(
            state,
            &route,
            &body,
            &incoming,
            None,
            "responses",
        );
        let (compacted, candidate) =
            match external_compaction_response(state, &route, &body, &incoming, &ids) {
                Ok(result) => result,
                Err(error) => return ResponsesRequestResult::Buffered(error),
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
    if route.dialect == emp_core::Dialect::CodexNative {
        if python_truthy(body.get("stream")) {
            return match serve_native_stream(stream, state, &route, &config, &body, &incoming, &ids)
            {
                Ok(()) => ResponsesRequestResult::Streamed,
                Err(response) => ResponsesRequestResult::Buffered(response),
            };
        }
        return ResponsesRequestResult::Buffered(native::complete(
            state,
            &route,
            &config,
            body.as_object().expect("validated request object"),
            &incoming,
        ));
    }
    if python_truthy(body.get("stream")) {
        return match serve_external_stream(stream, state, &route, &body, &incoming, &ids) {
            Ok(()) => ResponsesRequestResult::Streamed,
            Err(response) => ResponsesRequestResult::Buffered(response),
        };
    }
    let started = std::time::Instant::now();
    let router = ExternalRouter::new(&state.backend.transport.client);
    let candidates = protocol_candidates(&route);
    'candidate: for (index, protocol) in candidates.iter().copied().enumerate() {
        let candidate = match route.with_protocol(protocol) {
            Ok(candidate) => candidate,
            Err(error) => {
                return ResponsesRequestResult::Buffered(route_resolution_response(error));
            }
        };
        for attempt in 0..2 {
            match state
                .backend
                .transport
                .runtime
                .block_on(router.execute_complete(&candidate, &body, &incoming, &ids))
            {
                Ok(result) => {
                    let mut usage = crate::services::observation::Observation::new(
                        state,
                        &candidate,
                        &body,
                        &incoming,
                        None,
                        "responses",
                    )
                    .started_at(started);
                    usage.http_status(result.status);
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
                        thread::sleep(delay);
                        continue;
                    }
                    if index + 1 < candidates.len()
                        && protocol_fallback_allowed(error.status(), false, false)
                    {
                        continue 'candidate;
                    }
                    let mut usage = crate::services::observation::Observation::new(
                        state,
                        &candidate,
                        &body,
                        &incoming,
                        None,
                        "responses",
                    )
                    .started_at(started);
                    usage.router_error(&error);
                    return ResponsesRequestResult::Buffered(router_error_response(error));
                }
            }
        }
    }
    ResponsesRequestResult::Buffered(json_error_response(
        503,
        status_text(503),
        "provider protocol is unsupported",
        Some("router_error"),
        &[],
    ))
}
