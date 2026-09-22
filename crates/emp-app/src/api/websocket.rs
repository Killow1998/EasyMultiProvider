//! Responses WebSocket turns and upstream connection ownership.
use crate::services::failures::stream_error_code;

use crate::app::ServerState;
use crate::http::auth::proxy_allowed;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::response::json_error_response;
use crate::http::response::status_text;
use crate::services::accounts::account_catalog_headers;
use crate::services::catalog::response_catalog_etag;
use crate::services::events::stream_event_activity;
use crate::services::events::terminal_stream_event;
use crate::services::failures::safe_failure_reason;
use crate::services::failures::stream_failure_value;
use crate::services::history::DestinationPrepareError;
use crate::services::history::history_stream_error;
use crate::services::history::prepare_destination_context;
use crate::services::history::prepare_history;
use crate::services::native;
use crate::services::providers::hydrate_provider_keys;
use crate::util::projection_ids;
use crate::util::random_hex;
use emp_codex::subscription_route_model;
use emp_core::resolve_route;
use emp_history::HistoryError;
use emp_router::ExternalRouter;
use emp_router::RouterError;
use emp_router::native_metadata::native_response_headers;
use emp_transport::ClientWebSocket;
use emp_transport::FailureClass;
use emp_transport::WebSocketConnection;
use emp_transport::public_failure_message;
use emp_transport::websocket_accept;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

fn websocket_router_error(error: &RouterError) -> Value {
    let failure = stream_failure_value(error, "resp_websocket_error");
    serde_json::json!({
        "type":"error", "status":error.status(),
        "error":failure["response"]["error"]
    })
}

fn native_stream_error_value(
    status: u16,
    error_class: FailureClass,
    failure_reason: Option<&str>,
    response_id: &str,
) -> Value {
    let mut error = serde_json::json!({
        "code":stream_error_code(error_class),
        "message":format!("HTTP {status}: {}",public_failure_message(error_class,failure_reason,status)),
        "status":status,"error_class":error_class.as_str()
    });
    if let Some(reason) = failure_reason {
        error["failure_reason"] = Value::String(safe_failure_reason(reason));
    }
    serde_json::json!({"type":"response.failed","response":{"id":response_id,"object":"response","status":"failed","error":error}})
}

fn native_websocket_identity_headers(
    headers: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter(|(name, _)| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization" | "chatgpt-account-id"
            )
        })
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect()
}

struct NativeUpstreamConnection {
    url: String,
    proxy: Option<String>,
    identity: BTreeMap<String, String>,
    client: ClientWebSocket,
}

pub(crate) fn serve_responses_websocket(
    stream: &mut TcpStream,
    request: Request<'_>,
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
    let _ = stream.set_read_timeout(None);
    let mut websocket = WebSocketConnection::new(stream);
    let mut native_upstream: Option<NativeUpstreamConnection> = None;
    let mut last_native_response_id: Option<String> = None;
    let mut last_native_scope = (None, None);
    let mut http_only_routes = std::collections::BTreeSet::new();
    loop {
        let text = match websocket.receive_text() {
            Ok(Some(value)) => value,
            Ok(None) => return,
            Err(error) => {
                websocket.close(error.close_code(), &error.to_string());
                return;
            }
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
        let Some(model) = request_body
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            let _=websocket.send_json(&serde_json::json!({"type":"error","status":400,"error":{"code":"invalid_request","message":"request.model is required"}}));
            continue;
        };
        let mut config = match state.backend.configuration.config.lock() {
            Ok(config) => config.clone(),
            Err(_) => {
                let _=websocket.send_json(&serde_json::json!({"type":"error","status":500,"error":{"code":"internal_error","message":"internal server error"}}));
                continue;
            }
        };
        hydrate_provider_keys(&mut config, &state.backend.configuration.vault);
        if let Some(config) = config.as_object_mut() {
            config.insert(
                "_native_auth_path".to_owned(),
                Value::String(
                    state
                        .backend
                        .accounts
                        .native_auth_path
                        .to_string_lossy()
                        .into_owned(),
                ),
            );
        }
        let route = match resolve_route(&config, model, |config, slug, account| {
            subscription_route_model(config, slug, account, |account| {
                account_catalog_headers(account, &state.backend.configuration.vault)
            })
        }) {
            Ok(route) => route,
            Err(error) => {
                let _=websocket.send_json(&serde_json::json!({"type":"error","status":error.status(),"error":{"code":"router_error","message":error.to_string()}}));
                continue;
            }
        };
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
                || request_scope != last_native_scope)
        {
            last_native_response_id = None;
            let _ = websocket.send_json(&serde_json::json!({"type":"error","error":{"code":"previous_response_not_found","message":"Previous response was not found. Retrying the full request."}}));
            continue;
        }
        request_body = match prepare_history(
            state,
            &route,
            &Value::Object(request_body),
            &request_headers,
        ) {
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
        request_body = match prepare_destination_context(
            state,
            &route,
            &Value::Object(request_body.clone()),
            &request_headers,
        ) {
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
        if route.dialect == emp_core::Dialect::CodexNative {
            let plan = match native::websocket_plan(
                state,
                &route,
                &config,
                &request_body,
                &request_headers,
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    let _=websocket.send_json(&serde_json::json!({"type":"error","status":error.status,"error":error.body["error"]}));
                    continue;
                }
            };
            let previous = request_body
                .get("previous_response_id")
                .and_then(Value::as_str);
            let plan_identity = native_websocket_identity_headers(&plan.headers);
            let proxy = state
                .backend
                .transport
                .client
                .websocket_proxy_for(&plan.url);
            let route_matches = native_upstream.as_ref().is_some_and(|upstream| {
                upstream.url == plan.url
                    && proxy.as_ref().is_ok_and(|proxy| proxy == &upstream.proxy)
                    && upstream.identity == plan_identity
            });
            if previous
                .is_some_and(|id| !route_matches || last_native_response_id.as_deref() != Some(id))
            {
                last_native_response_id = None;
                let _=websocket.send_json(&serde_json::json!({"type":"error","error":{"code":"previous_response_not_found","message":"Previous response was not found. Retrying the full request."}}));
                continue;
            }
            if !route_matches {
                native_upstream = None;
                last_native_response_id = None;
            }
            let route_key = (
                plan.url.clone(),
                proxy.as_ref().ok().cloned().flatten(),
                plan_identity.clone(),
            );
            let mut connected_now = false;
            if native_upstream.is_none()
                && !http_only_routes.contains(&route_key)
                && state
                    .backend
                    .transport
                    .native_connections
                    .allowed(&route_key)
                && let Ok(selected_proxy) = &proxy
            {
                match ClientWebSocket::connect_with_proxy(
                    &plan.url,
                    &plan.headers,
                    Duration::from_secs(15),
                    selected_proxy.as_deref(),
                ) {
                    Ok(client) => {
                        native_upstream = Some(NativeUpstreamConnection {
                            url: plan.url.clone(),
                            proxy: selected_proxy.clone(),
                            identity: plan_identity.clone(),
                            client,
                        });
                        connected_now = true;
                    }
                    Err(error) => {
                        // No response.create frame has been sent. Match Python's
                        // HTTP fallback and avoid repeating failed handshakes.
                        if matches!(error.status(), 400 | 404 | 405 | 415 | 426 | 501) {
                            http_only_routes.insert(route_key.clone());
                        }
                        state
                            .backend
                            .transport
                            .native_connections
                            .defer(route_key.clone());
                    }
                }
            }
            if let Some(upstream) = native_upstream.as_mut() {
                let client = &mut upstream.client;
                {
                    let selected = native_response_headers(
                        &serde_json::json!({"headers":client.response_headers()}),
                        &plan.requested_model,
                        &plan.upstream_model,
                    );
                    let state_headers = selected
                        .into_iter()
                        .filter(|(name, _)| {
                            !name.eq_ignore_ascii_case("x-models-etag")
                                && (connected_now
                                    || name.eq_ignore_ascii_case("openai-model")
                                    || name.eq_ignore_ascii_case("x-openai-model"))
                        })
                        .collect::<serde_json::Map<_, _>>();
                    if !state_headers.is_empty()
                        && websocket
                            .send_json(&serde_json::json!({"type":"response.metadata","headers":state_headers}))
                            .is_err()
                    {
                        return;
                    }
                }
                if client.send_json(&plan.payload).is_err() {
                    native_upstream = None;
                    last_native_response_id = None;
                    let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                    let error =
                        native_stream_error_value(502, FailureClass::Network, Some("network"), &id);
                    let _ = websocket.send_json(&error);
                    continue;
                }
                let mut terminal = false;
                let mut completed_id = None;
                let mut projected_error = false;
                while let Ok(Some(event)) = client.receive_json() {
                    let event = match plan.project_event(&event) {
                        Ok(event) => event,
                        Err(error) => {
                            let _ = websocket.send_json(&serde_json::json!({
                                "type":"error",
                                "status":error.status,
                                "error":error.body["error"]
                            }));
                            projected_error = true;
                            break;
                        }
                    };
                    if event.get("type").and_then(Value::as_str) == Some("response.completed") {
                        completed_id = event
                            .get("response")
                            .and_then(|response| response.get("id"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                    }
                    terminal = terminal_stream_event(&event);
                    if websocket.send_json(&event).is_err() {
                        return;
                    }
                    if terminal {
                        break;
                    }
                }
                if terminal {
                    state
                        .backend
                        .transport
                        .native_connections
                        .available(&route_key);
                    last_native_response_id = completed_id;
                    last_native_scope = request_scope;
                    continue;
                }
                native_upstream = None;
                last_native_response_id = None;
                if projected_error {
                    continue;
                }
                if previous.is_some() {
                    let _=websocket.send_json(&serde_json::json!({"type":"error","error":{"code":"previous_response_not_found","message":"Previous response was not found. Retrying the full request."}}));
                } else {
                    let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                    let error = native_stream_error_value(
                        502,
                        FailureClass::StreamIncomplete,
                        Some("stream_incomplete"),
                        &id,
                    );
                    let _ = websocket.send_json(&error);
                }
                continue;
            }
            if previous.is_some() {
                let _=websocket.send_json(&serde_json::json!({"type":"error","error":{"code":"previous_response_not_found","message":"Previous response was not found. Retrying the full request."}}));
                continue;
            }
        }
        request_body.remove("previous_response_id");
        let generate = request_body
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
        request_body.remove("generate");
        request_body.insert("stream".to_owned(), Value::Bool(true));
        let mut sent_output = false;
        if route.dialect == emp_core::Dialect::CodexNative {
            let mut upstream = match native::open_stream_result(
                state,
                &route,
                &config,
                &request_body,
                &request_headers,
                &ids,
            ) {
                Ok(stream) => stream,
                Err(error) => {
                    let _=websocket.send_json(&serde_json::json!({"type":"error","status":error.status,"error":error.body["error"]}));
                    continue;
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
                    .send_json(
                        &serde_json::json!({"type":"response.metadata","headers":state_headers}),
                    )
                    .is_err()
            {
                return;
            }
            loop {
                match state
                    .backend
                    .transport
                    .runtime
                    .block_on(upstream.next_event())
                {
                    Ok(Some(event)) => {
                        sent_output |= stream_event_activity(&event.body).0;
                        if websocket.send_json(&event.body).is_err() {
                            return;
                        }
                        if terminal_stream_event(&event.body) {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        if sent_output {
                            let id = format!(
                                "resp_{}",
                                random_hex(16).unwrap_or_else(|_| "0".repeat(32))
                            );
                            let failure = stream_failure_value(&error, &id);
                            let _ = websocket.send_json(&failure);
                        } else {
                            let _ = websocket.send_json(&websocket_router_error(&error));
                        }
                        break;
                    }
                }
            }
        } else {
            let router = ExternalRouter::new(&state.backend.transport.client);
            let mut upstream = match state.backend.transport.runtime.block_on(router.open_stream(
                &route,
                &Value::Object(request_body.clone()),
                &request_headers,
                &ids,
            )) {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = websocket.send_json(&websocket_router_error(&error));
                    continue;
                }
            };
            loop {
                match state
                    .backend
                    .transport
                    .runtime
                    .block_on(upstream.next_event())
                {
                    Ok(Some(event)) => {
                        sent_output |= stream_event_activity(&event.body).0;
                        if websocket.send_json(&event.body).is_err() {
                            return;
                        }
                        if terminal_stream_event(&event.body) {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        if sent_output {
                            let id = format!(
                                "resp_{}",
                                random_hex(16).unwrap_or_else(|_| "0".repeat(32))
                            );
                            let failure = stream_failure_value(&error, &id);
                            let _ = websocket.send_json(&failure);
                        } else {
                            let _ = websocket.send_json(&websocket_router_error(&error));
                        }
                        break;
                    }
                }
            }
        }
    }
}
