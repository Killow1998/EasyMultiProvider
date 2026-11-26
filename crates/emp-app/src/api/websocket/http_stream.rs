use super::{Turn, TurnResult};
use crate::api::failure_response::{
    stream_failure_value, stream_failure_value_for_route, websocket_router_error,
    websocket_router_error_for_route,
};
use crate::app::ServerState;
use crate::services::disconnect::DisconnectRace;
use crate::services::events::{stream_event_activity, terminal_stream_event};
use crate::services::providers::save_learned_protocol;
use crate::services::{native, request_outcome::RequestGuard};
use crate::util::random_hex;
use emp_state::{config::LearnedProtocol, observed_at_now};
use serde_json::Value;
use tokio::task::block_in_place;

pub(super) async fn serve_native(
    state: &ServerState,
    turn: &Turn,
    websocket: &mut super::ObservedWebSocket<'_>,
    monitor: &mut crate::services::disconnect::DisconnectMonitor,
    usage: &mut RequestGuard<'_>,
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
    let execution_started = websocket.observation.execution_started();
    let mut upstream = match native::open_stream(
        state,
        route,
        config,
        request_body,
        request_headers,
        ids,
        Some(monitor),
        usage,
    )
    .await
    {
        Ok(Some(stream)) => stream,
        Ok(None) => {
            usage.websocket(execution_started);
            native::record_open_disconnect(usage, monitor);
            return if monitor.is_interrupted() {
                TurnResult::Finished
            } else {
                TurnResult::Closed
            };
        }
        Err(error) => {
            usage.finish();
            let _=websocket.send_json(&serde_json::json!({"type":"error","status":error.status,"error":error.body["error"]})).await;
            return TurnResult::Finished;
        }
    };
    usage.websocket(execution_started);
    let state_headers = upstream
        .headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("x-models-etag"))
        .map(|(name, value)| (name.clone(), Value::String(value.clone())))
        .collect::<serde_json::Map<_, _>>();
    if !state_headers.is_empty()
        && websocket
            .send_json(&serde_json::json!({"type":"response.metadata","headers":state_headers}))
            .await
            .is_err()
    {
        return TurnResult::Closed;
    }
    loop {
        let polled = monitor.race(upstream.next_event()).await;
        match polled {
            DisconnectRace::Disconnected => {
                if websocket.interrupted() {
                    usage.interrupted();
                    return TurnResult::Finished;
                }
                if monitor.is_draining() {
                    usage.drained();
                } else {
                    usage.disconnected();
                }
                return TurnResult::Closed;
            }
            DisconnectRace::Ready(Ok(Some(event))) => {
                usage.upstream_observation(&upstream.observation);
                usage.observe(&event.body);
                block_in_place(|| {
                    crate::services::context::record_event(state, route, request_body, &event.body)
                });
                sent_output |= stream_event_activity(&event.body).0;
                if websocket.send_upstream_json(&event.body).await.is_err() {
                    return TurnResult::Closed;
                }
                if terminal_stream_event(&event.body) {
                    break;
                }
            }
            DisconnectRace::Ready(Ok(None)) => break,
            DisconnectRace::Ready(Err(error)) => {
                usage.router_error(&error);
                if sent_output {
                    let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                    let failure = stream_failure_value(&error, &id);
                    let _ = websocket.send_json(&failure).await;
                } else {
                    let _ = websocket.send_json(&websocket_router_error(&error)).await;
                }
                break;
            }
        }
    }
    TurnResult::Finished
}

pub(super) async fn serve_external(
    state: &ServerState,
    turn: &Turn,
    websocket: &mut super::ObservedWebSocket<'_>,
    monitor: &mut crate::services::disconnect::DisconnectMonitor,
    usage: &mut RequestGuard<'_>,
) -> TurnResult {
    let Turn {
        route,
        body: request_body,
        headers: request_headers,
        ids,
        ..
    } = turn;
    let mut sent_output = false;
    let (mut upstream, candidate) = match crate::services::external::open_stream(
        state,
        route,
        request_body,
        request_headers,
        ids,
        Some(monitor),
        usage,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            if matches!(
                error,
                crate::services::external::ExternalRequestError::Disconnected
            ) {
                return if websocket.interrupted() {
                    TurnResult::Finished
                } else {
                    TurnResult::Closed
                };
            }
            let event = crate::api::failure_response::external_websocket_error(error);
            usage.finish();
            let _ = websocket.send_json(&event).await;
            return TurnResult::Finished;
        }
    };
    usage.websocket(websocket.observation.execution_started());
    loop {
        let polled = monitor.race(upstream.next_event()).await;
        usage.upstream_observation(&upstream.observation);
        match polled {
            DisconnectRace::Disconnected => {
                if websocket.interrupted() {
                    usage.interrupted();
                    return TurnResult::Finished;
                }
                usage.disconnected();
                return TurnResult::Closed;
            }
            DisconnectRace::Ready(Ok(Some(mut event))) => {
                let learned = if event.body["type"] == "response.completed" {
                    LearnedProtocol::from_route(route, candidate.protocol, observed_at_now())
                } else {
                    None
                };
                usage.observe(&event.body);
                block_in_place(|| {
                    crate::services::context::record_event(
                        state,
                        &candidate,
                        request_body,
                        &event.body,
                    )
                });
                sent_output |= stream_event_activity(&event.body).0;
                if websocket
                    .send_stateless_json(&mut event.body, true)
                    .await
                    .is_err()
                {
                    return TurnResult::Closed;
                }
                if terminal_stream_event(&event.body) {
                    if learned.is_some() {
                        block_in_place(|| save_learned_protocol(state, learned));
                    }
                    break;
                }
            }
            DisconnectRace::Ready(Ok(None)) => break,
            DisconnectRace::Ready(Err(error)) => {
                usage.router_error(&error);
                if sent_output {
                    let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                    let failure = stream_failure_value_for_route(&error, &id, &candidate, true);
                    let _ = websocket.send_json(&failure).await;
                } else {
                    let _ = websocket
                        .send_json(&websocket_router_error_for_route(&error, &candidate))
                        .await;
                }
                break;
            }
        }
    }
    TurnResult::Finished
}
