//! HTTP/SSE fallback and external streams delivered through the downstream WebSocket.
use super::{Turn, TurnResult};
use crate::api::failure_response::{stream_failure_value, websocket_router_error};
use crate::app::ServerState;
use crate::services::disconnect::DisconnectRace;
use crate::services::events::{stream_event_activity, terminal_stream_event};
use crate::services::{activity::ActivityGuard, native};
use crate::util::random_hex;
use emp_transport::WebSocketConnection;
use serde_json::Value;
use std::net::TcpStream;

pub(super) fn serve_native(
    state: &ServerState,
    turn: &Turn,
    websocket: &mut WebSocketConnection<'_, TcpStream>,
    monitor_stream: Option<&TcpStream>,
    mut native_turn_activity: Option<ActivityGuard<'_>>,
) -> TurnResult {
    let Turn {
        route,
        config,
        body: request_body,
        headers: request_headers,
        ids,
        ..
    } = turn;
    let mut sent_output = false;
    let _activity_guard = native_turn_activity.take().unwrap_or_else(|| {
        state
            .backend
            .activity
            .begin(crate::services::activity::ActivityIdentity::from_route(
                route,
            ))
    });
    let mut upstream = match native::open_stream_result(
        state,
        route,
        config,
        request_body,
        request_headers,
        ids,
    ) {
        Ok(stream) => stream,
        Err(error) => {
            let _=websocket.send_json(&serde_json::json!({"type":"error","status":error.status,"error":error.body["error"]}));
            return TurnResult::Finished;
        }
    };
    let state_headers = upstream
        .headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("x-models-etag"))
        .map(|(name, value)| (name.clone(), Value::String(value.clone())))
        .collect::<serde_json::Map<_, _>>();
    if !state_headers.is_empty()
        && websocket
            .send_json(&serde_json::json!({"type":"response.metadata","headers":state_headers}))
            .is_err()
    {
        return TurnResult::Closed;
    }
    let mut usage = crate::services::request_outcome::RequestOutcome::new(
        state,
        route,
        &Value::Object(request_body.clone()),
        request_headers,
        upstream.usage_owner.as_deref(),
        "responses",
    )
    .started_at(upstream.request_started)
    .transport("websocket");
    let mut monitor = monitor_stream
        .and_then(|probe| crate::services::disconnect::DisconnectMonitor::start(probe).ok());
    loop {
        let polled = crate::services::disconnect::raced(
            &state.backend.transport.runtime,
            monitor.as_mut(),
            upstream.next_event(),
        );
        match polled {
            DisconnectRace::Disconnected => {
                usage.disconnected();
                return TurnResult::Closed;
            }
            DisconnectRace::Ready(Ok(Some(event))) => {
                usage.observe(&event.body);
                crate::services::context::record_event(
                    state,
                    route,
                    &Value::Object(request_body.clone()),
                    &event.body,
                );
                sent_output |= stream_event_activity(&event.body).0;
                if websocket.send_json(&event.body).is_err() {
                    return TurnResult::Closed;
                }
                if terminal_stream_event(&event.body) {
                    break;
                }
            }
            DisconnectRace::Ready(Ok(None)) => break,
            DisconnectRace::Ready(Err(error)) => {
                if sent_output {
                    let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                    let failure = stream_failure_value(&error, &id);
                    let _ = websocket.send_json(&failure);
                } else {
                    let _ = websocket.send_json(&websocket_router_error(&error));
                }
                break;
            }
        }
    }
    TurnResult::Finished
}

pub(super) fn serve_external(
    state: &ServerState,
    turn: &Turn,
    websocket: &mut WebSocketConnection<'_, TcpStream>,
    monitor_stream: Option<&TcpStream>,
) -> TurnResult {
    let Turn {
        route,
        body: request_body,
        headers: request_headers,
        ids,
        ..
    } = turn;
    let mut sent_output = false;
    let _activity_guard =
        state
            .backend
            .activity
            .begin(crate::services::activity::ActivityIdentity::from_route(
                route,
            ));
    let (mut upstream, candidate) = match crate::services::external::open_stream(
        state,
        route,
        &Value::Object(request_body.clone()),
        request_headers,
        ids,
        None,
    ) {
        Ok(result) => result,
        Err(error) => {
            let event = crate::api::failure_response::external_websocket_error(error);
            let _ = websocket.send_json(&event);
            return TurnResult::Finished;
        }
    };
    let mut usage = crate::services::request_outcome::RequestOutcome::new(
        state,
        &candidate,
        &Value::Object(request_body.clone()),
        request_headers,
        None,
        "responses",
    )
    .started_at(upstream.request_started)
    .transport("websocket");
    let mut monitor = monitor_stream
        .and_then(|probe| crate::services::disconnect::DisconnectMonitor::start(probe).ok());
    loop {
        let polled = crate::services::disconnect::raced(
            &state.backend.transport.runtime,
            monitor.as_mut(),
            upstream.next_event(),
        );
        usage.reported_model(upstream.reported_model.as_deref());
        match polled {
            DisconnectRace::Disconnected => {
                usage.disconnected();
                return TurnResult::Closed;
            }
            DisconnectRace::Ready(Ok(Some(event))) => {
                usage.observe(&event.body);
                crate::services::context::record_event(
                    state,
                    &candidate,
                    &Value::Object(request_body.clone()),
                    &event.body,
                );
                sent_output |= stream_event_activity(&event.body).0;
                if websocket.send_json(&event.body).is_err() {
                    return TurnResult::Closed;
                }
                if terminal_stream_event(&event.body) {
                    if event.body["type"] == "response.completed" {
                        crate::services::providers::persist_protocol_observation(state, &candidate);
                    }
                    break;
                }
            }
            DisconnectRace::Ready(Ok(None)) => break,
            DisconnectRace::Ready(Err(error)) => {
                if sent_output {
                    let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                    let failure = stream_failure_value(&error, &id);
                    let _ = websocket.send_json(&failure);
                } else {
                    let _ = websocket.send_json(&websocket_router_error(&error));
                }
                break;
            }
        }
    }
    TurnResult::Finished
}
