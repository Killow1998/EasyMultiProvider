use super::Turn;
use super::socket::{Downstream, DownstreamEvent};
use crate::app::ServerState;
use crate::services::events::stream_event_activity;
use crate::services::events::terminal_stream_event;
use crate::services::native_connections::RouteKey;
use crate::services::observation::continuation::ContinuationReason;
use crate::services::{activity::ActivityGuard, native};
use crate::util::random_hex;
use emp_transport::{AsyncClientWebSocket, FailureClass, WebSocketEvent};
use serde_json::Value;
use std::{collections::BTreeSet, io, time::Duration};
use tokio::time::Instant;
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
    pub(super) async fn close_upstream(&mut self) {
        NativeUpstreamConnection::close(&mut self.upstream).await;
    }

    pub(super) async fn serve<'a>(
        &mut self,
        state: &'a ServerState,
        turn: &Turn,
        websocket: &mut super::ObservedWebSocket<'_>,
    ) -> NativeTurnResult<'a> {
        let native_connections = &state.backend.native_websocket_connections;
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
        let (mut plan, native_account) = match tokio::task::block_in_place(|| {
            native::websocket_plan(state, route, config, request_body, request_headers)
        }) {
            Ok(plan) => plan,
            Err(error) => {
                websocket.reject(state, route, request_body, request_headers,
                    &serde_json::json!({"type":"error","status":error.status,"error":error.body["error"]})).await;
                return NativeTurnResult::Finished;
            }
        };
        let previous = request_body
            .get("previous_response_id")
            .and_then(Value::as_str);
        let (route_key, connected_now) = match self
            .prepare_connection(
                state,
                &plan,
                previous,
                websocket.observation.native_continuation(),
            )
            .await
        {
            Ok(connection) => connection,
            Err(reason) => {
                websocket
                    .reject(
                        state,
                        route,
                        request_body,
                        request_headers,
                        &previous_response_error(reason),
                    )
                    .await;
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
            if upstream
                .metadata(&plan, connected_now, websocket)
                .await
                .is_err()
            {
                return NativeTurnResult::Closed;
            }
            let client = &mut upstream.client;
            let traffic_before = client.traffic();
            let compression = client.compression_enabled();
            let mut idle_deadline = Instant::now() + UPSTREAM_IDLE;
            let mut usage = crate::services::request_outcome::RequestOutcome::new(
                state,
                route,
                request_body,
                request_headers,
                None,
                "responses",
            )
            .started_at(websocket.observation.execution_started())
            .transport("websocket");
            usage.native_credentials(route, &plan.headers, native_account.as_deref());
            native_turn_activity = Some(state.backend.activity.begin(
                crate::services::activity::ActivityIdentity::from_route(route),
            ));
            let payload = if let Some(prefill) = prefill::Prefill::plan(&plan.payload, &plan.url) {
                let result = prefill
                    .run(
                        state,
                        client,
                        &mut idle_deadline,
                        websocket,
                        pending_request,
                    )
                    .await;
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
                        error.report(route, websocket, &mut usage).await;
                        NativeUpstreamConnection::close(native_upstream).await;
                        *last_native_response_id = None;
                        return NativeTurnResult::Finished;
                    }
                }
            } else {
                std::borrow::Cow::Borrowed(&plan.payload)
            };
            usage.dispatch();
            if let Err(error) = client.send_json(&payload).await {
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
                NativeUpstreamConnection::close(native_upstream).await;
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
                let _ = websocket.send_json(&error).await;
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
                let message = match next_message(
                    state,
                    client,
                    websocket.inner,
                    pending_request.is_none(),
                    &mut idle_deadline,
                )
                .await
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
                let mut event = match message {
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
                            if let Err(error) = client.send_text(&text).await {
                                websocket.observation.native_continuation().transport_error =
                                    Some(error.to_string());
                                break;
                            }
                            state.backend.diagnostics.journal.event("info", "websocket_control", &serde_json::json!({
                                "type":"response.interrupt", "forwarded":true,
                                "request_id":crate::services::observation::request::request_id(request_headers)
                            }));
                        } else {
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
                        replay_allowed = false;
                        upstream.replay_inline_ids = true;
                        usage.native_retry(emp_router::native_http::NativeRetryDecision {
                            reason: emp_router::native_http::NativeRetryReason::InlineInputReplay,
                            status: 400,
                            retry: true,
                        });
                        usage.dispatch();
                        if let Err(error) = client.send_json(&retry_payload).await {
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
                emp_router::normalize_upstream_error_event(&mut event);
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
                        let _ = websocket.send_json(&event).await;
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
                if request_body.get("generate") != Some(&Value::Bool(false))
                    && crate::services::context::outcome(&event) == Some(true)
                {
                    tokio::task::block_in_place(|| {
                        crate::services::context::record_payload(state, route, &plan.payload, true)
                    });
                }
                terminal = terminal_stream_event(&event);
                if websocket.send_upstream_json(&event).await.is_err() {
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
                    NativeUpstreamConnection::close(native_upstream).await;
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
                native_connections.defer(route_key.clone());
                NativeUpstreamConnection::close(native_upstream).await;
                *last_native_response_id = None;
                fallback_usage = Some(Box::new(usage));
            } else {
                let peer_close_code = client.peer_close_code();
                NativeUpstreamConnection::close(native_upstream).await;
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
                    let _ = websocket.send_json(&event).await;
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
                    let _ = websocket.send_json(&error).await;
                }
                return NativeTurnResult::Finished;
            }
        }
        if previous.is_some() {
            let reason = ContinuationReason::IncrementalFallbackUnavailable;
            websocket.observation.native_continuation().reason = Some(reason);
            websocket
                .reject(
                    state,
                    route,
                    request_body,
                    request_headers,
                    &previous_response_error(reason),
                )
                .await;
            return NativeTurnResult::Finished;
        }
        *fallback_active = true;
        websocket.observation.native_continuation().http_fallback = true;
        NativeTurnResult::HttpFallback(native_turn_activity, fallback_usage)
    }
}

const UPSTREAM_IDLE: Duration = Duration::from_secs(300);

enum Message {
    Upstream(Value),
    Downstream(String),
    DownstreamClosed,
}

async fn next_message(
    state: &ServerState,
    client: &mut AsyncClientWebSocket,
    downstream: &Downstream,
    read_downstream: bool,
    idle_deadline: &mut Instant,
) -> io::Result<Option<Message>> {
    loop {
        tokio::select! {
            biased;
            _ = super::wait_for_drain(state) => {
                downstream.close(1001, "EMP connection is draining").await;
                return Ok(Some(Message::DownstreamClosed));
            }
            _ = tokio::time::sleep_until(*idle_deadline) => return Err(io::ErrorKind::TimedOut.into()),
            event = downstream.read(), if read_downstream => {
                match downstream.finish_read(event).await {
                    DownstreamEvent::Text(text) => return Ok(Some(Message::Downstream(text))),
                    DownstreamEvent::Control => {}
                    DownstreamEvent::Closed => return Ok(Some(Message::DownstreamClosed)),
                }
            }
            event = client.read() => match event {
                Ok(WebSocketEvent::Text(text)) => {
                    *idle_deadline = Instant::now() + UPSTREAM_IDLE;
                    if let Ok(value @ Value::Object(_)) = serde_json::from_str(&text) {
                        return Ok(Some(Message::Upstream(value)));
                    }
                }
                Ok(WebSocketEvent::Ping(payload)) => {
                    *idle_deadline = Instant::now() + UPSTREAM_IDLE;
                    client.send_pong(&payload).await.map_err(io::Error::other)?;
                }
                Ok(WebSocketEvent::Closed { .. }) => {
                    client.close(1000, "").await;
                    return Ok(None);
                }
                Err(error) => {
                    client.close(1000, "").await;
                    return Err(io::Error::other(error));
                }
            }
        }
    }
}
