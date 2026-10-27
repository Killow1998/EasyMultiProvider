//! Native upstream connection reuse, incremental ownership and HTTP fallback.
use super::Turn;
use super::duplex::{Duplex, Message};
use crate::app::ServerState;
use crate::services::events::stream_event_activity;
use crate::services::events::terminal_stream_event;
use crate::services::native_connections::RouteKey;
use crate::services::observation::continuation::ContinuationReason;
use crate::services::{activity::ActivityGuard, native};
use crate::util::random_hex;
use emp_transport::FailureClass;
use serde_json::Value;
use std::collections::BTreeSet;
mod connection;
mod errors;
mod prefill;
use connection::NativeUpstreamConnection;
use errors::native_stream_error_value_for_route;
pub(super) use errors::{native_stream_error_value, previous_response_error};

#[derive(Default)]
pub(super) struct NativeSession {
    upstream: Option<NativeUpstreamConnection>,
    last_response_id: Option<String>,
    last_scope: (Option<String>, Option<String>),
    http_only_routes: BTreeSet<RouteKey>,
    pub(super) pending_request: Option<String>,
    fallback_active: bool,
    connected_before: bool,
}

pub(super) enum NativeTurnResult<'a> {
    Finished,
    Closed,
    HttpFallback(
        Option<ActivityGuard<'a>>,
        Option<Box<crate::services::request_outcome::RequestOutcome<'a>>>,
    ),
}

impl NativeSession {
    pub(super) fn serve<'a>(
        &mut self,
        state: &'a ServerState,
        turn: &Turn,
        websocket: &mut super::ObservedWebSocket<'_, '_>,
        downstream: Option<&std::net::TcpStream>,
    ) -> NativeTurnResult<'a> {
        let native_connections = &state.backend.transport.native_connections;
        let Turn {
            route,
            config,
            body: request_body,
            headers: request_headers,
            scope: request_scope,
            ..
        } = turn;
        let mut native_turn_activity = None;
        let mut fallback_usage = None;
        let mut plan = match native::websocket_plan(
            state,
            route,
            config,
            request_body,
            request_headers,
        ) {
            Ok(plan) => plan,
            Err(error) => {
                websocket.reject(state, route, request_body, request_headers,
                    &serde_json::json!({"type":"error","status":error.status,"error":error.body["error"]}));
                return NativeTurnResult::Finished;
            }
        };
        let previous = request_body
            .get("previous_response_id")
            .and_then(Value::as_str);
        let (route_key, connected_now) = match self.prepare_connection(
            state,
            &plan,
            previous,
            websocket.observation.native_continuation(),
        ) {
            Ok(connection) => connection,
            Err(reason) => {
                websocket.reject(
                    state,
                    route,
                    request_body,
                    request_headers,
                    &previous_response_error(reason),
                );
                return NativeTurnResult::Finished;
            }
        };
        let Self {
            upstream: native_upstream,
            last_response_id: last_native_response_id,
            last_scope: last_native_scope,
            pending_request,
            fallback_active,
            ..
        } = self;
        if let Some(upstream) = native_upstream.as_mut() {
            if upstream.replay_inline_ids {
                emp_protocol::native_responses::remove_inline_input_ids(&mut plan.payload);
            }
            websocket
                .observation
                .native_continuation()
                .connection_reused = Some(!connected_now);
            if upstream.metadata(&plan, connected_now, websocket).is_err() {
                return NativeTurnResult::Closed;
            }
            let client = &mut upstream.client;
            let traffic_before = client.traffic();
            let compression = client.compression_enabled();
            let Some(mut duplex) = downstream.and_then(|socket| Duplex::new(client, socket).ok())
            else {
                websocket
                    .inner
                    .close(1011, "websocket readiness setup failed");
                return NativeTurnResult::Closed;
            };
            let owner = emp_state::usage::account_owner(&plan.headers);
            let mut usage = crate::services::request_outcome::RequestOutcome::new(
                state,
                route,
                &Value::Object(request_body.clone()),
                request_headers,
                Some(&owner),
                "responses",
            )
            .started_at(websocket.observation.execution_started())
            .transport("websocket");
            native_turn_activity = Some(state.backend.activity.begin(
                crate::services::activity::ActivityIdentity::from_route(route),
            ));
            let payload = if let Some(prefill) = prefill::Prefill::plan(&plan.payload) {
                let result = prefill.run(client, &mut duplex, websocket, pending_request);
                websocket.observation.native_continuation().traffic(
                    traffic_before,
                    client.traffic(),
                    compression,
                );
                websocket.observation.native_continuation().peer_close_code =
                    client.peer_close_code();
                match result {
                    Ok(payload) => std::borrow::Cow::Owned(payload),
                    Err(prefill::PrefillError::Disconnected) => {
                        usage.disconnected();
                        return NativeTurnResult::Closed;
                    }
                    Err(error) => {
                        if matches!(&error, prefill::PrefillError::Transport(_)) {
                            native_connections.defer(route_key.clone());
                        }
                        error.report(route, websocket, &mut usage);
                        *native_upstream = None;
                        *last_native_response_id = None;
                        return NativeTurnResult::Finished;
                    }
                }
            } else {
                std::borrow::Cow::Borrowed(&plan.payload)
            };
            usage.dispatch();
            if let Err(error) = client.send_json(&payload) {
                websocket.observation.native_continuation().transport_error =
                    Some(error.to_string());
                native_connections.defer(route_key.clone());
                websocket.observation.native_continuation().traffic(
                    traffic_before,
                    client.traffic(),
                    compression,
                );
                websocket.observation.native_continuation().reason =
                    Some(ContinuationReason::UpstreamDisconnected);
                *native_upstream = None;
                *last_native_response_id = None;
                let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                let error = native_stream_error_value_for_route(
                    502,
                    FailureClass::Network,
                    Some("network"),
                    &id,
                    route,
                    false,
                );
                usage.observe(&error);
                let _ = websocket.send_json(&error);
                return NativeTurnResult::Finished;
            }
            let mut terminal = false;
            let mut resumable = false;
            let mut completed_id = None;
            let mut projected_error = false;
            let mut received_upstream_event = false;
            let mut delivered_output = false;
            let mut replay_allowed = true;
            let mut connection_expired = false;
            loop {
                let message = match duplex.next(client, websocket.inner, pending_request.is_none())
                {
                    Ok(Some(message)) => message,
                    Ok(None) => break,
                    Err(error) => {
                        websocket.observation.native_continuation().transport_error =
                            Some(error.to_string());
                        break;
                    }
                };
                websocket.observation.native_continuation().traffic(
                    traffic_before,
                    client.traffic(),
                    compression,
                );
                let event = match message {
                    Message::Upstream(event) => event,
                    Message::DownstreamClosed => {
                        usage.disconnected();
                        return NativeTurnResult::Closed;
                    }
                    Message::Downstream(text) => {
                        if serde_json::from_str::<Value>(&text)
                            .ok()
                            .is_some_and(|value| value["type"] == "response.interrupt")
                        {
                            // The active upstream owns response_id validation and
                            // interrupt acknowledgement. Preserve all control fields.
                            if let Err(error) = client.send_text(&text) {
                                websocket.observation.native_continuation().transport_error =
                                    Some(error.to_string());
                                break;
                            }
                            state.backend.diagnostics.journal.event("info", "websocket_control", &serde_json::json!({
                                "type":"response.interrupt", "forwarded":true,
                                "request_id":crate::services::observation::request::request_id(request_headers)
                            }));
                        } else {
                            // Preserve the next request for the ordinary route and
                            // scope checks; never dispatch two turns concurrently.
                            *pending_request = Some(text);
                        }
                        continue;
                    }
                };
                if replay_allowed
                    && emp_protocol::native_responses::persisted_input_lookup_rejected(&event)
                {
                    let mut retry_payload = payload.as_ref().clone();
                    if emp_protocol::native_responses::remove_inline_input_ids(&mut retry_payload)
                        > 0
                    {
                        // A 400 before response creation has not executed the task.
                        // Keep this adaptation local to the selected upstream socket.
                        replay_allowed = false;
                        upstream.replay_inline_ids = true;
                        usage.native_retry(emp_router::native_http::NativeRetryDecision {
                            reason: emp_router::native_http::NativeRetryReason::InlineInputReplay,
                            status: 400,
                            retry: true,
                        });
                        usage.dispatch();
                        if let Err(error) = client.send_json(&retry_payload) {
                            websocket.observation.native_continuation().transport_error =
                                Some(error.to_string());
                            break;
                        }
                        continue;
                    }
                }
                replay_allowed &= matches!(
                    event["type"].as_str(),
                    Some("response.metadata" | "codex.response.metadata" | "codex.rate_limits")
                );
                received_upstream_event = true;
                connection_expired = event["error"]["code"] == "websocket_connection_limit_reached"
                    || event["response"]["error"]["code"] == "websocket_connection_limit_reached";
                if event["error"]["code"] == "previous_response_not_found"
                    || event["response"]["error"]["code"] == "previous_response_not_found"
                {
                    websocket.observation.native_continuation().reason =
                        Some(ContinuationReason::UpstreamRecoveryRequired);
                }
                usage.raw_event(&event);
                let event = match plan.project_event(&event) {
                    Ok(event) => event,
                    Err(error) => {
                        let event = serde_json::json!({
                            "type":"error",
                            "status":error.status,
                            "error":error.body["error"]
                        });
                        usage.observe(&event);
                        let _ = websocket.send_json(&event);
                        projected_error = true;
                        break;
                    }
                };
                resumable = event["type"] == "response.completed"
                    || (event["type"] == "response.incomplete"
                        && event["response"]["incomplete_details"]["reason"] == "interrupted");
                if resumable {
                    completed_id = event
                        .get("response")
                        .and_then(|response| response.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                usage.observe_upstream(&event);
                // Native WS terminal policy preserves Python's generic
                // stream failures; only successful turns calibrate here.
                if request_body.get("generate") != Some(&Value::Bool(false))
                    && crate::services::context::outcome(&event) == Some(true)
                {
                    crate::services::context::record_payload(state, route, &plan.payload, true);
                }
                terminal = terminal_stream_event(&event);
                if websocket.send_upstream_json(&event).is_err() {
                    return NativeTurnResult::Closed;
                }
                delivered_output |= stream_event_activity(&event).0;
                if terminal {
                    break;
                }
            }
            websocket.observation.native_continuation().traffic(
                traffic_before,
                client.traffic(),
                compression,
            );
            websocket.observation.native_continuation().peer_close_code = client.peer_close_code();
            if terminal {
                if connection_expired {
                    *native_upstream = None;
                    *last_native_response_id = None;
                } else if resumable {
                    *last_native_response_id = completed_id;
                    *last_native_scope = request_scope.clone();
                }
                return NativeTurnResult::Finished;
            }
            if !projected_error
                && !received_upstream_event
                && client.peer_close_code() == Some(1009)
                && previous.is_none()
            {
                // A pre-event size rejection permits full HTTP/zstd replay.
                // Incremental turns still require previous-response recovery.
                native_connections.defer(route_key.clone());
                *native_upstream = None;
                *last_native_response_id = None;
                fallback_usage = Some(Box::new(usage));
            } else {
                let peer_close_code = client.peer_close_code();
                *native_upstream = None;
                *last_native_response_id = None;
                if projected_error {
                    return NativeTurnResult::Finished;
                }
                websocket.observation.native_continuation().reason =
                    Some(ContinuationReason::UpstreamDisconnected);
                if previous.is_none() || !matches!(peer_close_code, Some(1000 | 1001)) {
                    native_connections.defer(route_key.clone());
                }
                if previous.is_some() && !received_upstream_event {
                    let reason = ContinuationReason::UpstreamDisconnected;
                    websocket.observation.native_continuation().reason = Some(reason);
                    let event = previous_response_error(reason);
                    usage.observe(&event);
                    let _ = websocket.send_json(&event);
                } else {
                    let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                    let error = native_stream_error_value_for_route(
                        502,
                        FailureClass::StreamIncomplete,
                        Some("stream_incomplete"),
                        &id,
                        route,
                        delivered_output,
                    );
                    usage.observe(&error);
                    let _ = websocket.send_json(&error);
                }
                return NativeTurnResult::Finished;
            }
        }
        if previous.is_some() {
            let reason = ContinuationReason::IncrementalFallbackUnavailable;
            websocket.observation.native_continuation().reason = Some(reason);
            websocket.reject(
                state,
                route,
                request_body,
                request_headers,
                &previous_response_error(reason),
            );
            return NativeTurnResult::Finished;
        }
        *fallback_active = true;
        websocket.observation.native_continuation().http_fallback = true;
        NativeTurnResult::HttpFallback(native_turn_activity, fallback_usage)
    }
}
