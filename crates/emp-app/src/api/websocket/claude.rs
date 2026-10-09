//! Single-step Claude completion projected into downstream WebSocket events.
use super::{Turn, TurnResult};
use crate::api::failure_response::websocket_router_error;
use crate::app::ServerState;
use crate::services::events::terminal_stream_event;
use serde_json::Value;

pub(super) fn serve(
    state: &ServerState,
    turn: &Turn,
    websocket: &mut super::ObservedWebSocket<'_, '_>,
    monitor: &mut crate::services::disconnect::DisconnectMonitor,
) -> TurnResult {
    let Turn {
        route,
        body: request_body,
        headers: request_headers,
        ids,
        ..
    } = turn;
    // Expose an ID before the buffered CLI call, so Codex can steer while waiting.
    let id = format!(
        "resp_{}",
        crate::util::random_hex(16).unwrap_or_else(|_| "0".repeat(32))
    );
    if websocket
        .send_json(&serde_json::json!({"type":"response.created", "response":{
            "id":id, "object":"response", "status":"in_progress", "output":[]
        }}))
        .is_err()
    {
        return TurnResult::Closed;
    }
    let _activity_guard =
        state
            .backend
            .activity
            .begin(crate::services::activity::ActivityIdentity::from_route(
                route,
            ));
    let completion = match crate::services::claude_cli::execute_complete(
        state,
        route,
        &Value::Object(request_body.clone()),
        request_headers,
        Some(monitor),
    ) {
        Ok(completion) => completion,
        Err(
            crate::services::claude_cli::ClaudeCliError::Disconnected
            | crate::services::claude_cli::ClaudeCliError::Failure("claude_cli_interrupted"),
        ) => {
            let mut usage = crate::services::request_outcome::RequestOutcome::new(
                state,
                route,
                &Value::Object(request_body.clone()),
                request_headers,
                None,
                "responses",
            )
            .started_at(websocket.observation.execution_started())
            .transport("websocket");
            if websocket.interrupted() {
                usage.interrupted();
                return TurnResult::Finished;
            }
            usage.disconnected();
            return TurnResult::Closed;
        }
        Err(error) => {
            if let crate::services::claude_cli::ClaudeCliError::Router(router_error) = &error {
                let mut usage = crate::services::request_outcome::RequestOutcome::new(
                    state,
                    route,
                    &Value::Object(request_body.clone()),
                    request_headers,
                    None,
                    "responses",
                )
                .started_at(websocket.observation.execution_started())
                .transport("websocket");
                usage.router_error(router_error);
            }
            if websocket
                .send_json(&crate::api::claude_response::websocket_value_for_route(
                    &error, route,
                ))
                .is_err()
            {
                return TurnResult::Closed;
            }
            return TurnResult::Finished;
        }
    };
    let selected_route = completion.route;
    let mut response_value = completion.response.body;
    response_value["id"] = Value::String(id);
    let mut usage = crate::services::request_outcome::RequestOutcome::new(
        state,
        &selected_route,
        &Value::Object(request_body.clone()),
        request_headers,
        None,
        "responses",
    )
    .started_at(websocket.observation.execution_started())
    .transport("websocket");
    usage.http_status(completion.response.status);
    usage.upstream_observation(&completion.response.observation);
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
        if event["type"] == "response.created" {
            continue;
        }
        if websocket.send_json(&event).is_err() {
            return TurnResult::Closed;
        }
        if terminal_stream_event(&event) {
            break;
        }
    }
    TurnResult::Finished
}
