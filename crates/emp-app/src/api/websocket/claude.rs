//! Single-step Claude completion projected into downstream WebSocket events.
use super::{Turn, TurnResult};
use crate::app::ServerState;
use crate::services::events::terminal_stream_event;
use crate::services::failures::websocket_router_error;
use emp_transport::WebSocketConnection;
use serde_json::Value;
use std::net::TcpStream;

pub(super) fn serve(
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
    let started = std::time::Instant::now();
    let mut monitor = monitor_stream
        .and_then(|probe| crate::services::disconnect::DisconnectMonitor::start(probe).ok());
    let _activity_guard = state.backend.activity.begin(
        crate::services::activity::ActivityIdentity::from_route(route),
        &state.backend.accounts.quota_revision,
        &state.backend.accounts.quota_condition,
    );
    let completion = match crate::services::claude_cli::execute_complete(
        state,
        route,
        &Value::Object(request_body.clone()),
        request_headers,
        ids,
        monitor.as_mut(),
    ) {
        Ok(crate::services::claude_cli::ClaudeCliResult::Completed(completion)) => completion,
        Err(crate::services::claude_cli::ClaudeCliError::Disconnected) => {
            let mut usage = crate::services::observation::Observation::new(
                state,
                route,
                &Value::Object(request_body.clone()),
                request_headers,
                None,
                "responses",
            )
            .started_at(started)
            .transport("websocket");
            usage.disconnected();
            return TurnResult::Closed;
        }
        Err(error) => {
            if let crate::services::claude_cli::ClaudeCliError::Router(router_error) = &error {
                let mut usage = crate::services::observation::Observation::new(
                    state,
                    route,
                    &Value::Object(request_body.clone()),
                    request_headers,
                    None,
                    "responses",
                )
                .started_at(started)
                .transport("websocket");
                usage.router_error(router_error);
            }
            if websocket.send_json(&error.websocket_value()).is_err() {
                return TurnResult::Closed;
            }
            return TurnResult::Finished;
        }
    };
    let selected_route = completion.route;
    let response_value = completion.response.body;
    let mut usage = crate::services::observation::Observation::new(
        state,
        &selected_route,
        &Value::Object(request_body.clone()),
        request_headers,
        None,
        "responses",
    )
    .started_at(completion.request_started)
    .transport("websocket");
    usage.http_status(completion.response.status);
    usage.observe(&response_value);
    usage.finish();
    if response_value["status"] == "completed" {
        crate::services::context::record(
            state,
            &selected_route,
            &Value::Object(request_body.clone()),
            true,
        );
    }
    crate::services::providers::persist_protocol_observation(state, &selected_route);
    let events = match emp_router::response_json_stream_events(response_value, ids, false) {
        Ok(events) => events,
        Err(error) => {
            if websocket
                .send_json(&websocket_router_error(&error))
                .is_err()
            {
                return TurnResult::Closed;
            }
            return TurnResult::Finished;
        }
    };
    for event in events {
        if websocket.send_json(&event).is_err() {
            return TurnResult::Closed;
        }
        if terminal_stream_event(&event) {
            break;
        }
    }
    TurnResult::Finished
}
