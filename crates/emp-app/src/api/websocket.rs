//! Responses WebSocket turns and upstream connection ownership.

use crate::api::failure_response::websocket_router_error;
use crate::api::history_response::history_stream_error;
use crate::app::ServerState;
use crate::http::auth::proxy_allowed;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::response::json_error_response;
use crate::http::response::status_text;
use crate::services::catalog::response_catalog_etag;
use crate::services::history::DestinationPrepareError;
use crate::services::history::prepare_destination_context;
use crate::services::history::prepare_history;
use crate::services::request_preparation::{
    PreparedRequest, RequestOperation, RequestPreparationError, prepare_request,
};
use crate::util::projection_ids;
use crate::util::random_hex;
use emp_history::HistoryError;
use emp_transport::FailureClass;
use emp_transport::WebSocketConnection;
use emp_transport::websocket_accept;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpStream;

mod claude;
mod http_stream;
mod native_socket;

use native_socket::{NativeSession, NativeTurnResult, native_stream_error_value};

struct Turn {
    config: Value,
    route: emp_core::ResolvedRoute,
    body: serde_json::Map<String, Value>,
    headers: BTreeMap<String, String>,
    ids: emp_router::ProjectionIds,
    scope: (Option<String>, Option<String>),
}

enum TurnResult {
    Finished,
    Closed,
}

pub(crate) fn serve_responses_websocket(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) {
    if !proxy_allowed(request, state, now) {
        let status = if same_origin(request, state.port) {
            401
        } else {
            403
        };
        let response = json_error_response(
            status,
            status_text(status),
            "proxy caller authentication is required",
            None,
            &[],
        );
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        return;
    }
    let connection_tokens = request
        .header("Connection")
        .unwrap_or_default()
        .split(',')
        .map(|value| value.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    if request
        .header("Upgrade")
        .is_none_or(|value| !value.eq_ignore_ascii_case("websocket"))
        || !connection_tokens.iter().any(|value| value == "upgrade")
        || request.header("Sec-WebSocket-Version") != Some("13")
    {
        let response = json_error_response(
            400,
            status_text(400),
            "invalid websocket upgrade",
            None,
            &[],
        );
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        return;
    }
    let accept = match websocket_accept(request.header("Sec-WebSocket-Key").unwrap_or_default()) {
        Ok(value) => value,
        Err(error) => {
            let response =
                json_error_response(400, status_text(400), &error.to_string(), None, &[]);
            let _ = stream.write_all(&response);
            let _ = stream.flush();
            return;
        }
    };
    let incoming = request
        .headers
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }
    let monitor_stream = stream.try_clone().ok();
    let mut websocket = match WebSocketConnection::new_with_prefix(stream, &body_prefix) {
        Ok(websocket) => websocket,
        Err(_) => return,
    };
    let Some(_websocket_permit) = state.connection_admission.acquire_websocket() else {
        state.backend.diagnostics.journal.event(
            "warning",
            "request_rejected",
            &serde_json::json!({"transport":"websocket", "reason":"connection_capacity"}),
        );
        websocket.close(1013, "too many websocket connections");
        return;
    };
    let mut native_session = NativeSession::default();
    loop {
        let text = match websocket.receive_text() {
            Ok(Some(value)) => value,
            Ok(None) => return,
            Err(error) => {
                websocket.close(error.close_code(), &error.to_string());
                return;
            }
        };
        let Some(_permit) = state.updates.enter() else {
            let _ = websocket.send_json(&serde_json::json!({
                "type":"error",
                "status":503,
                "error":{"code":"updating","message":"EMP is installing an update. Please retry shortly."}
            }));
            continue;
        };
        let mut request_body = match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(value)) => value,
            _ => {
                let _=websocket.send_json(&serde_json::json!({"type":"error","status":400,"error":{"code":"invalid_request","message":"websocket request must be a JSON object"}}));
                continue;
            }
        };
        if request_body
            .remove("type")
            .and_then(|value| value.as_str().map(str::to_owned))
            .as_deref()
            != Some("response.create")
        {
            let _=websocket.send_json(&serde_json::json!({"type":"error","status":400,"error":{"code":"invalid_request","message":"websocket request.type must be response.create"}}));
            continue;
        }
        let etag = response_catalog_etag(state).unwrap_or_default();
        if websocket.send_json(&serde_json::json!({"type":"codex.response.metadata","headers":{"x-models-etag":etag}})).is_err(){return;}
        let (config, route, mut request_body) = match prepare_request(
            state,
            Value::Object(request_body),
            RequestOperation::Responses,
        ) {
            Ok(PreparedRequest {
                config,
                route,
                body: Value::Object(body),
            }) => (config, route, body),
            Err(RequestPreparationError::ModelRequired) => {
                let _ = websocket.send_json(&serde_json::json!({
                    "type":"error", "status":400,
                    "error":{"code":"invalid_request","message":"request.model is required"}
                }));
                continue;
            }
            Err(RequestPreparationError::Route(error)) => {
                let _ = websocket.send_json(&serde_json::json!({
                    "type":"error", "status":error.status(),
                    "error":{"code":"router_error","message":error.to_string()}
                }));
                continue;
            }
            Ok(_) | Err(_) => {
                let _ = websocket.send_json(&serde_json::json!({
                    "type":"error", "status":500,
                    "error":{"code":"internal_error","message":"internal server error"}
                }));
                continue;
            }
        };
        if crate::services::claude_cli::selected(&route)
            && let Err(error) =
                crate::services::claude_cli::preflight_input(&Value::Object(request_body.clone()))
        {
            if websocket
                .send_json(&crate::api::claude_response::websocket_value_for_route(
                    &error, &route,
                ))
                .is_err()
            {
                return;
            }
            continue;
        }
        let ids = match projection_ids() {
            Ok(ids) => ids,
            Err(_) => {
                let _=websocket.send_json(&serde_json::json!({"type":"error","status":500,"error":{"code":"internal_error","message":"internal server error"}}));
                continue;
            }
        };
        let mut request_headers = incoming.clone();
        if let Ok(id) = random_hex(8) {
            request_headers.insert("X-EMP-Request-ID".to_owned(), id);
        }
        let request_scope =
            match emp_history::request_history_anchor(&request_body, &request_headers) {
                Ok(anchor) => (anchor.thread_id, anchor.window_id),
                Err(error) => {
                    let _ = websocket.send_json(&history_stream_error(&error));
                    continue;
                }
            };
        if request_body.contains_key("previous_response_id")
            && (route.dialect != emp_core::Dialect::CodexNative
                || request_scope != native_session.last_scope)
        {
            native_session.last_response_id = None;
            let _ = websocket.send_json(&serde_json::json!({"type":"error","error":{"code":"previous_response_not_found","message":"Previous response was not found. Retrying the full request."}}));
            continue;
        }
        request_body =
            match prepare_history(state, &route, Value::Object(request_body), &request_headers) {
                Ok(Value::Object(body)) => body,
                Ok(_) => {
                    let error = HistoryError::new("invalid_history_projection");
                    if websocket.send_json(&history_stream_error(&error)).is_err() {
                        return;
                    }
                    continue;
                }
                Err(error) => {
                    if websocket.send_json(&history_stream_error(&error)).is_err() {
                        return;
                    }
                    continue;
                }
            };
        let destination_context = {
            let mut monitor = monitor_stream.as_ref().and_then(|probe| {
                crate::services::disconnect::DisconnectMonitor::start(probe).ok()
            });
            prepare_destination_context(
                state,
                &route,
                Value::Object(request_body),
                &request_headers,
                monitor.as_mut(),
            )
        };
        request_body = match destination_context {
            Ok(Value::Object(body)) => body,
            Ok(_) => {
                let error = HistoryError::new("invalid_history_projection");
                if websocket.send_json(&history_stream_error(&error)).is_err() {
                    return;
                }
                continue;
            }
            Err(DestinationPrepareError::Router(error)) => {
                if websocket
                    .send_json(&websocket_router_error(&error))
                    .is_err()
                {
                    return;
                }
                continue;
            }
            Err(DestinationPrepareError::ClaudeCli(error)) => {
                if websocket
                    .send_json(&crate::api::claude_response::websocket_value_for_route(
                        &error, &route,
                    ))
                    .is_err()
                {
                    return;
                }
                continue;
            }
            Err(DestinationPrepareError::Disconnected) => return,
            Err(DestinationPrepareError::History(reason)) => {
                if websocket
                    .send_json(&history_stream_error(&HistoryError::new(reason)))
                    .is_err()
                {
                    return;
                }
                continue;
            }
            Err(DestinationPrepareError::Context(_)) => {
                let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                let failed = native_stream_error_value(
                    413,
                    FailureClass::ContextLengthExceeded,
                    Some("context_length_exceeded"),
                    &id,
                );
                if websocket.send_json(&failed).is_err() {
                    return;
                }
                continue;
            }
        };
        let mut turn = Turn {
            config,
            route,
            body: request_body,
            headers: request_headers,
            ids,
            scope: request_scope,
        };
        let native_turn_activity = if turn.route.dialect == emp_core::Dialect::CodexNative {
            match native_session.serve(state, &turn, &mut websocket) {
                NativeTurnResult::Finished => continue,
                NativeTurnResult::Closed => return,
                NativeTurnResult::HttpFallback(activity) => activity,
            }
        } else {
            None
        };
        turn.body.remove("previous_response_id");
        let generate = turn
            .body
            .get("generate")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        if !generate {
            let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
            let usage = serde_json::json!({"input_tokens":0,"input_tokens_details":Value::Null,"output_tokens":0,"output_tokens_details":Value::Null,"total_tokens":0});
            if websocket
                .send_json(&serde_json::json!({"type":"response.created","response":{"id":id}}))
                .is_err()
            {
                return;
            }
            if websocket.send_json(&serde_json::json!({"type":"response.completed","response":{"id":id,"object":"response","status":"completed","output":[],"usage":usage}})).is_err(){return;}
            continue;
        }
        turn.body.remove("generate");
        turn.body.insert("stream".to_owned(), Value::Bool(true));
        let result = if crate::services::claude_cli::selected(&turn.route) {
            claude::serve(state, &turn, &mut websocket, monitor_stream.as_ref())
        } else if turn.route.dialect == emp_core::Dialect::CodexNative {
            http_stream::serve_native(
                state,
                &turn,
                &mut websocket,
                monitor_stream.as_ref(),
                native_turn_activity,
            )
        } else {
            http_stream::serve_external(state, &turn, &mut websocket, monitor_stream.as_ref())
        };
        if matches!(result, TurnResult::Closed) {
            return;
        }
    }
}
