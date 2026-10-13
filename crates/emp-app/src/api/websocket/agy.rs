//! agy proposals are buffered until the official CLI's structured terminal result.
use super::{Turn, TurnResult};
use crate::app::ServerState;
use crate::services::agy_cli::{self, Error};
use serde_json::{Value, json};

pub(super) fn serve(
    state: &ServerState,
    turn: &Turn,
    websocket: &mut super::ObservedWebSocket<'_, '_>,
    monitor: &mut crate::services::disconnect::DisconnectMonitor,
) -> TurnResult {
    let id = match crate::util::random_hex(16) {
        Ok(value) => format!("resp_{value}"),
        Err(_) => return TurnResult::Closed,
    };
    if websocket.send_json(&json!({"type":"response.created","response":{"id":id,"object":"response","status":"in_progress","output":[]}})).is_err() { return TurnResult::Closed; }
    let _activity =
        state
            .backend
            .activity
            .begin(crate::services::activity::ActivityIdentity::from_route(
                &turn.route,
            ));
    let body = Value::Object(turn.body.clone());
    let mut usage = crate::services::request_outcome::RequestOutcome::new(
        state,
        &turn.route,
        &body,
        &turn.headers,
        None,
        "responses",
    )
    .started_at(websocket.observation.execution_started())
    .transport("websocket");
    let completion = match agy_cli::execute(state, &turn.route, &body, Some(monitor), &mut usage) {
        Ok(value) => value,
        Err(Error("client_disconnected" | "agy_cli_interrupted")) => {
            return if websocket.interrupted() {
                TurnResult::Finished
            } else {
                TurnResult::Closed
            };
        }
        Err(error) => {
            return if websocket.send_json(&error.event()).is_ok() {
                TurnResult::Finished
            } else {
                TurnResult::Closed
            };
        }
    };
    let mut response = completion.body;
    response["id"] = id.into();
    usage.http_status(completion.status);
    usage.observe(&response);
    usage.finish();
    crate::services::context::record(state, &turn.route, &body, true);
    let events = match emp_router::response_json_stream_events(response, &turn.ids, false) {
        Ok(events) => events,
        Err(_) => {
            let _ = websocket.send_json(&Error("agy_cli_invalid_output").event());
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
        if crate::services::events::terminal_stream_event(&event) {
            break;
        }
    }
    TurnResult::Finished
}
