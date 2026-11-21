use super::{ObservedWebSocket, TurnResult};
use crate::api::aero::{RelayError, await_module, open};
use crate::api::failure_response::emp_websocket_error;
use crate::api::history_response::history_stream_error;
use crate::app::ServerState;
use crate::http::request::Request;
use crate::services::disconnect::{DisconnectMonitor, DisconnectRace};
use crate::services::events::terminal_stream_event;
use emp_transport::SseJsonParser;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) fn serve(
    state: &ServerState,
    config: &Value,
    body: &Value,
    request: (Request<'_>, &BTreeMap<String, String>),
    websocket: &mut ObservedWebSocket<'_, '_>,
    monitor: &mut DisconnectMonitor,
) -> TurnResult {
    let mut upstream = match open(
        state,
        config,
        body,
        request,
        websocket.observation,
        false,
        monitor,
    ) {
        Ok(upstream) => upstream,
        Err(RelayError::Disconnected) => return TurnResult::Closed,
        Err(RelayError::Request(status, code, message)) => {
            return send(websocket, &emp_websocket_error(status, code, message));
        }
        Err(RelayError::History(error)) => {
            let event = history_stream_error(&error, websocket.observation);
            return send(websocket, &event);
        }
    };
    let status = upstream.response.status();
    if !(200..300).contains(&status) {
        let result = await_module(state, monitor, upstream.response.read_limited(64 * 1024));
        let error = match result {
            DisconnectRace::Ready(Ok(bytes)) => serde_json::from_slice::<Value>(&bytes).ok(),
            DisconnectRace::Ready(Err(_)) => None,
            DisconnectRace::Disconnected => return TurnResult::Closed,
        };
        let error = error.as_ref().and_then(|value| value.get("error"));
        let code = error
            .and_then(|value| value["code"].as_str())
            .unwrap_or("aero_error");
        let message = error
            .and_then(|value| value["message"].as_str())
            .unwrap_or("The Aero module rejected the request.");
        return send(websocket, &emp_websocket_error(status, code, message));
    }
    let mut parser = SseJsonParser::new();
    let mut response_id = None;
    loop {
        let events = match await_module(state, monitor, upstream.response.next_chunk()) {
            DisconnectRace::Disconnected => {
                return if websocket.interrupted() {
                    TurnResult::Finished
                } else {
                    TurnResult::Closed
                };
            }
            DisconnectRace::Ready(Ok(Some(chunk))) => match parser.push(&chunk) {
                Ok(events) => events,
                Err(_) => break,
            },
            DisconnectRace::Ready(Ok(None) | Err(_)) => break,
        };
        for event in events {
            let event = Value::Object(event);
            if event["type"] == "response.created" {
                response_id = event["response"]["id"].as_str().map(str::to_owned);
            }
            if websocket.send_json(&event).is_err() {
                return TurnResult::Closed;
            }
            if terminal_stream_event(&event) {
                return TurnResult::Finished;
            }
        }
    }
    let error = emp_websocket_error(
        502,
        "aero_stream_incomplete",
        "The Aero stream ended before a terminal event.",
    );
    match response_id {
        Some(id) => send(
            websocket,
            &json!({"type":"response.failed","response":{
                "id":id, "object":"response", "status":"failed", "error":error["error"]
            }}),
        ),
        None => send(websocket, &error),
    }
}

fn send(websocket: &mut ObservedWebSocket<'_, '_>, event: &Value) -> TurnResult {
    match websocket.send_json(event) {
        Ok(()) => TurnResult::Finished,
        Err(_) => TurnResult::Closed,
    }
}
