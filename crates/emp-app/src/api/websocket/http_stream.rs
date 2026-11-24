use super::{Turn, TurnResult};
use crate::api::failure_response::{
    stream_failure_value, stream_failure_value_for_route, websocket_router_error,
    websocket_router_error_for_route,
};
use crate::app::ServerState;
use crate::services::disconnect::DisconnectRace;
use crate::services::events::{stream_event_activity, terminal_stream_event};
use crate::services::{activity::ActivityGuard, native};
use crate::util::random_hex;
use serde_json::Value;
use tokio::task::block_in_place;

pub(super) async fn serve_native(
    state: &ServerState,
    turn: &Turn,
    websocket: &mut super::ObservedWebSocket<'_>,
    monitor: &mut crate::services::disconnect::DisconnectMonitor,
    mut native_turn_activity: Option<ActivityGuard<'_>>,
    previous_usage: Option<crate::services::request_outcome::RequestOutcome<'_>>,
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
    let usage = previous_usage.unwrap_or_else(|| {
        crate::services::request_outcome::RequestOutcome::new(
            state,
            route,
            request_body,
            request_headers,
            None,
            "responses",
        )
        .started_at(websocket.observation.execution_started())
        .transport("websocket")
    });
    let (mut upstream, mut usage) = match native::open_stream_result_with_monitor(
        state,
        route,
        config,
        request_body,
        request_headers,
        ids,
        Some(monitor),
        Some(usage),
    )
    .await
    {
        Ok(native::CancellableNativeStreamOpen::Opened(stream)) => *stream,
        Ok(native::CancellableNativeStreamOpen::Disconnected) => {
            return if monitor.is_interrupted() {
                TurnResult::Finished
            } else {
                TurnResult::Closed
            };
        }
        Err(error) => {
            let _=websocket.send_json(&serde_json::json!({"type":"error","status":error.status,"error":error.body["error"]})).await;
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
    let (mut upstream, candidate, usage) = match crate::services::external::open_stream_async(
        state,
        route,
        request_body,
        request_headers,
        ids,
        Some(monitor),
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
            let _ = websocket.send_json(&event).await;
            return TurnResult::Finished;
        }
    };
    let mut usage = usage
        .started_at(websocket.observation.execution_started())
        .transport("websocket");
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
                    if event.body["type"] == "response.completed" {
                        block_in_place(|| {
                            crate::services::providers::persist_protocol_observation(
                                state, &candidate,
                            )
                        });
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
