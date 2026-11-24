use crate::api::failure_response::emp_websocket_error;
use crate::api::failure_response::websocket_router_error;
use crate::api::history_response::history_stream_error;
use crate::app::ServerState;
use crate::http::request::Request;
use crate::server::http::error::HttpReply;
use crate::server::http::proxy::require_proxy;
use crate::services::catalog::response_catalog_etag;
use crate::services::history::DestinationPrepareError;
use crate::services::history::prepare_destination_context;
use crate::services::history::prepare_history;
use crate::services::observation::request::{Phase, RequestObservation};
use crate::services::request_preparation::{PreparedRequest, RequestPreparationError};
use crate::util::projection_ids;
use crate::util::random_hex;
use emp_context::HistoryError;
use emp_transport::FailureClass;
use emp_transport::websocket_accept;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task::block_in_place;

mod aero;
mod claude;
mod control;

mod http_stream;
mod native_socket;
pub(crate) mod socket;
use socket::{Downstream, DownstreamEvent, legacy_websocket_io};

use native_socket::{NativeSession, NativeTurnResult, native_stream_error_value};

/// The downstream sender owns delivery evidence; upstream accounting is separate.
struct ObservedWebSocket<'a> {
    inner: &'a Downstream,
    observation: &'a mut RequestObservation,
    control: Option<std::sync::Arc<control::Control>>,
    last_response_id: &'a mut Option<String>,
    codex: bool,
}
impl ObservedWebSocket<'_> {
    async fn warmup(&mut self) -> Result<(), emp_transport::WebSocketError> {
        let id = "";
        let usage = serde_json::json!({"input_tokens":0,"input_tokens_details":Value::Null,"output_tokens":0,"output_tokens_details":Value::Null,"total_tokens":0});
        self.send_json(&serde_json::json!({"type":"response.created","response":{"id":id}}))
            .await?;
        self.send_json(&serde_json::json!({"type":"response.completed","response":{"id":id,"object":"response","status":"completed","output":[],"usage":usage}})).await
    }

    async fn reject(
        &mut self,
        state: &ServerState,
        route: &emp_domain::ResolvedRoute,
        body: &Value,
        headers: &BTreeMap<String, String>,
        event: &Value,
    ) {
        let mut outcome = crate::services::request_outcome::RequestOutcome::new(
            state,
            route,
            body,
            headers,
            None,
            "responses",
        )
        .started_at(self.observation.execution_started())
        .transport("websocket");
        outcome.observe(event);
        let _ = self.send_json(event).await;
    }
    async fn send_json(&mut self, event: &Value) -> Result<(), emp_transport::WebSocketError> {
        self.send(event, false).await
    }
    async fn send_upstream_json(
        &mut self,
        event: &Value,
    ) -> Result<(), emp_transport::WebSocketError> {
        self.send(event, true).await
    }
    async fn send_stateless_json(
        &mut self,
        event: &mut Value,
        upstream: bool,
    ) -> Result<(), emp_transport::WebSocketError> {
        if event["type"] != "response.completed" {
            return self.send(event, upstream).await;
        }
        let id = std::mem::replace(&mut event["response"]["id"], Value::String(String::new()));
        let result = self.send(event, upstream).await;
        *self.last_response_id = id.as_str().map(str::to_owned);
        event["response"]["id"] = id;
        result
    }
    async fn send(
        &mut self,
        event: &Value,
        upstream: bool,
    ) -> Result<(), emp_transport::WebSocketError> {
        if !self.codex
            && crate::services::events::codex_event(event["type"].as_str().unwrap_or_default())
        {
            return Ok(());
        }
        let formatted = self.observation.errors().event(event, upstream);
        let event = formatted.as_ref().unwrap_or(event);
        let result = match &self.control {
            Some(control) => match control.send(self.inner, event).await {
                Ok(false) => return Ok(()),
                Ok(true) => Ok(()),
                Err(error) => Err(error),
            },
            None => self.inner.send_json(event).await,
        };
        if upstream {
            self.observation.upstream_event_written(event, result)?;
        } else {
            self.observation.event_written(event, result)?;
        }
        if crate::services::events::terminal_stream_event(event)
            && let Some(id) = event["response"]["id"].as_str()
        {
            *self.last_response_id = Some(id.to_owned());
        }
        Ok(())
    }
    fn interrupted(&self) -> bool {
        self.control.as_ref().is_some_and(|control| {
            control
                .interrupted
                .load(std::sync::atomic::Ordering::Acquire)
        })
    }
}

struct Turn {
    config: Value,
    route: emp_domain::ResolvedRoute,
    body: Value,
    headers: BTreeMap<String, String>,
    ids: emp_router::ProjectionIds,
    scope: (Option<String>, Option<String>),
}

enum TurnResult {
    Finished,
    Closed,
}

pub(crate) fn connection_has_upgrade_token(value: &str) -> bool {
    value
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
}

pub(crate) fn prepare_upgrade(
    request: Request<'_>,
    key_error: impl FnOnce(emp_transport::WebSocketError) -> String,
) -> Result<String, HttpReply> {
    if request
        .header("Upgrade")
        .is_none_or(|value| !value.eq_ignore_ascii_case("websocket"))
        || !connection_has_upgrade_token(request.header("Connection").unwrap_or_default())
        || request.header("Sec-WebSocket-Version") != Some("13")
    {
        return Err(HttpReply::error(400, "invalid websocket upgrade", None));
    }
    websocket_accept(request.header("Sec-WebSocket-Key").unwrap_or_default())
        .map_err(|error| HttpReply::error(400, &key_error(error), None))
}

pub(crate) fn serve_responses_websocket(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &Arc<ServerState>,
    now: f64,
    connection_id: Option<&str>,
) {
    let accept = match require_proxy(request, state, now)
        .map_err(|error| error.reply())
        .and_then(|()| prepare_upgrade(request, |error| error.to_string()))
    {
        Ok(accept) => accept,
        Err(reply) => {
            let _ = stream.write_all(&reply.wire_response());
            let _ = stream.flush();
            return;
        }
    };
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }
    let runtime = state.backend.transport.runtime.handle();
    let Ok(io) = legacy_websocket_io(stream, runtime) else {
        return;
    };
    let monitor = legacy_websocket_io(stream, runtime).ok().map(Arc::new);
    let permit = state.connection_admission.acquire_websocket();
    runtime.block_on(run_responses(
        state,
        request,
        connection_id,
        io,
        &body_prefix,
        monitor,
        permit.is_some(),
    ));
}

pub(crate) async fn run_responses(
    state: &Arc<ServerState>,
    request: Request<'_>,
    connection_id: Option<&str>,
    io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    prefix: &[u8],
    monitor: Option<Arc<tokio::net::TcpStream>>,
    admitted: bool,
) {
    let Ok(websocket) = Downstream::new(
        io,
        prefix,
        Arc::clone(&state.backend.transport.request_limits),
    ) else {
        return;
    };
    if !admitted {
        state.backend.diagnostics.journal.event(
            "warning",
            "request_rejected",
            &serde_json::json!({"transport":"websocket", "reason":"connection_capacity"}),
        );
        websocket
            .close(1013, "too many websocket connections")
            .await;
        return;
    }
    let mut native_session = NativeSession::default();
    let session = responses_turns(
        state,
        request,
        connection_id,
        &websocket,
        monitor.as_ref(),
        &mut native_session,
    );
    session.await;
    native_session.close_upstream().await;
}

async fn wait_for_drain(state: &ServerState) {
    tokio::select! {
        _ = state.wait_for_shutdown() => {}
        _ = state.connection_admission.wait_for_websocket_drain() => {}
    }
}

async fn responses_turns(
    state: &Arc<ServerState>,
    request: Request<'_>,
    connection_id: Option<&str>,
    websocket: &Downstream,
    monitor: Option<&Arc<tokio::net::TcpStream>>,
    native_session: &mut NativeSession,
) {
    let mut last_response_id = None;
    let incoming = request.incoming();
    loop {
        let next = match native_session.pending_request.take() {
            Some(text) => Some(text),
            None => {
                async {
                    loop {
                        let event = tokio::select! {
                            biased;
                            _ = wait_for_drain(state) => break,
                            event = websocket.read() => event,
                        };
                        match websocket.finish_read(event).await {
                            DownstreamEvent::Text(text) => return Some(text),
                            DownstreamEvent::Control => continue,
                            DownstreamEvent::Closed => return None,
                        }
                    }
                    websocket
                        .writer
                        .close(1001, "EMP connection is draining")
                        .await;
                    None
                }
                .await
            }
        };
        let Some(text) = next else {
            return;
        };
        let _request_budget = websocket.take_request_budget().await;
        if request
            .source
            .and_then(|source| source.caller.as_ref())
            .is_some_and(|caller| !matches!(caller.id.as_str(), "local" | "shared"))
            && crate::http::auth::proxy_identity(request, state, crate::util::system_now(), None)
                .is_none()
        {
            websocket.close(1008, "device key was revoked").await;
            return;
        }
        if state.connection_admission.is_quiescing() {
            websocket.close(1001, "EMP connection is draining").await;
            return;
        }
        let decoded_bytes = text.len() as u64;
        let decoded = serde_json::from_str::<Value>(&text);
        drop(text);
        if let Ok(value) = &decoded
            && value["type"] == "response.interrupt"
        {
            let late = value["response_id"]
                .as_str()
                .is_some_and(|id| Some(id) == last_response_id.as_deref())
                && value["mode"] == "discard_partial_items";
            state.backend.diagnostics.journal.event("info", "websocket_control", &serde_json::json!({
                "type":"response.interrupt", "late":late, "accepted":late, "connection_id":connection_id,
                "error_origin":if late {"none"} else {"emp"}, "error_code":if late {""} else {"invalid_request"}
            }));
            if !late
                && websocket
                    .send_json(&emp_websocket_error(
                        400,
                        "invalid_request",
                        "interrupt response_id does not identify an active response",
                    ))
                    .await
                    .is_err()
            {
                return;
            }
            continue;
        }
        let mut observation = RequestObservation::new(
            std::sync::Arc::clone(&state.backend.diagnostics),
            random_hex(8).ok(),
            connection_id,
            "websocket",
            "responses",
        )
        .with_ledger(
            std::sync::Arc::clone(&state.backend.usage.ledger),
            std::sync::Arc::clone(&state.backend.management_events),
        );
        let mut presentation = crate::services::error_presentation::ErrorPresentation::new(state);
        presentation.thread(incoming.get("thread-id").map(String::as_str));
        observation = observation.with_errors(presentation);
        let mut websocket = ObservedWebSocket {
            codex: crate::services::events::codex_client(&incoming),
            inner: websocket,
            observation: &mut observation,
            control: None,
            last_response_id: &mut last_response_id,
        };
        websocket.observation.phase(Phase::ReadBody);
        let Some(_permit) = state.updates.enter() else {
            let _ = websocket
                .send_json(&emp_websocket_error(
                    503,
                    "updating",
                    "EMP is installing an update. Please retry shortly.",
                ))
                .await;
            continue;
        };
        let mut request_body = match decoded {
            Ok(value @ Value::Object(_)) => value,
            _ => {
                let _ = websocket
                    .send_json(&emp_websocket_error(
                        400,
                        "invalid_request",
                        "websocket request must be a JSON object",
                    ))
                    .await;
                continue;
            }
        };
        websocket.observation.body(
            request_body.get("reasoning"),
            request_body.get("input"),
            request_body.get("client_metadata"),
            request_body.get("stream"),
        );
        if request_body
            .as_object_mut()
            .unwrap()
            .remove("type")
            .and_then(|value| value.as_str().map(str::to_owned))
            .as_deref()
            != Some("response.create")
        {
            let _ = websocket
                .send_json(&emp_websocket_error(
                    400,
                    "invalid_request",
                    "websocket request.type must be response.create",
                ))
                .await;
            continue;
        }
        let etag = response_catalog_etag(state).unwrap_or_default();
        if websocket.send_json(&serde_json::json!({"type":"codex.response.metadata","headers":{"x-models-etag":etag}})).await.is_err(){return;}
        websocket.observation.phase(Phase::ResolveRoute);
        if let Some(config) = crate::api::aero::selected_config(state, &request_body) {
            let generate = request_body.as_object_mut().unwrap().remove("generate");
            if generate == Some(Value::Bool(false)) {
                websocket.observation.diagnostic_only("aero");
                let result = if state.backend.modules.active(emp_modules::Module::Aero) {
                    websocket.warmup().await
                } else {
                    websocket
                        .send_json(&emp_websocket_error(
                            404,
                            "module_disabled",
                            "Enable the Aero module to use this model.",
                        ))
                        .await
                };
                if result.is_err() {
                    return;
                }
                continue;
            }
            request_body["stream"] = Value::Bool(true);
            let request_headers = websocket.observation.headers(incoming.clone());
            let (control, mut monitor) = control::TurnControl::start(state, &mut websocket);
            let result = aero::serve(
                state,
                &config,
                &request_body,
                (request, &request_headers),
                &mut websocket,
                &mut monitor,
            )
            .await;
            let result = control
                .finish(&mut websocket, &mut native_session.pending_request, result)
                .await;
            if matches!(result, TurnResult::Closed) {
                return;
            }
            continue;
        }
        let (config, route, mut request_body) = match block_in_place(|| {
            crate::services::memory_routing::prepare(state, request_body, &incoming)
        }) {
            Ok(PreparedRequest {
                config,
                route,
                body: body @ Value::Object(_),
            }) => (config, route, body),
            Err(RequestPreparationError::ModelRequired) => {
                let _ = websocket.send_json(&serde_json::json!({
                    "type":"error", "status":400,
                    "error":{"code":"invalid_request","origin":"emp","message":"[EMP] request.model is required"}
                })).await;
                continue;
            }
            Err(RequestPreparationError::Route(error)) => {
                let _ = websocket.send_json(&serde_json::json!({
                    "type":"error", "status":error.status(),
                    "error":{"code":"router_error","origin":"emp","message":crate::services::error_origin::message("emp", &error.to_string())}
                })).await;
                continue;
            }
            Err(RequestPreparationError::MemoryUnavailable(reason)) => {
                let _ = websocket
                    .send_json(&emp_websocket_error(
                        503,
                        "memory_routing_unavailable",
                        crate::services::memory_routing::error_message(reason),
                    ))
                    .await;
                continue;
            }
            Err(RequestPreparationError::TeamAccess(denial)) => {
                let error = denial.public_error(state);
                let _ = websocket
                    .send_json(&serde_json::json!({
                        "type":"error", "status":error.status, "error":error.body["error"]
                    }))
                    .await;
                continue;
            }
            Ok(_) | Err(_) => {
                let _ = websocket
                    .send_json(&emp_websocket_error(
                        500,
                        "internal_error",
                        "internal server error",
                    ))
                    .await;
                continue;
            }
        };
        websocket.observation.selected(&route);
        state
            .backend
            .claude_turns
            .selected(&route, &request_body, &incoming);
        websocket.observation.phase(Phase::ValidateInput);
        if crate::services::claude_cli::selected(&route)
            && let Err(error) = crate::services::claude_cli::preflight_input(&request_body)
        {
            if websocket
                .send_json(&crate::api::claude_response::websocket_value_for_route(
                    &error, &route,
                ))
                .await
                .is_err()
            {
                return;
            }
            continue;
        }
        let ids = match projection_ids() {
            Ok(ids) => ids,
            Err(_) => {
                let _ = websocket
                    .send_json(&emp_websocket_error(
                        500,
                        "internal_error",
                        "internal server error",
                    ))
                    .await;
                continue;
            }
        };
        let request_headers = websocket.observation.headers(incoming.clone());
        websocket.observation.phase(Phase::PrepareHistory);
        let request_scope = match emp_context::request_history_anchor(
            request_body.as_object().unwrap(),
            &request_headers,
        ) {
            Ok(anchor) => (anchor.thread_id, anchor.window_id),
            Err(error) => {
                let failed = history_stream_error(&error, websocket.observation);
                let _ = websocket.send_json(&failed).await;
                continue;
            }
        };
        websocket
            .observation
            .native_continuation()
            .downstream_decoded_bytes = Some(decoded_bytes);
        if let Some(reason) = native_session.check_scope(
            route.dialect,
            &request_scope,
            request_body.get("previous_response_id"),
            websocket.observation.native_continuation(),
        ) {
            websocket
                .reject(
                    state,
                    &route,
                    &request_body,
                    &request_headers,
                    &native_socket::previous_response_error(reason),
                )
                .await;
            continue;
        }
        request_body =
            match block_in_place(|| prepare_history(state, &route, request_body, &request_headers))
            {
                Ok(body @ Value::Object(_)) => body,
                Ok(_) => {
                    let error = HistoryError::new("invalid_history_projection");
                    let failed = history_stream_error(&error, websocket.observation);
                    if websocket.send_json(&failed).await.is_err() {
                        return;
                    }
                    continue;
                }
                Err(error) => {
                    let failed = history_stream_error(&error, websocket.observation);
                    if websocket.send_json(&failed).await.is_err() {
                        return;
                    }
                    continue;
                }
            };
        websocket.observation.phase(Phase::PrepareDestination);
        let destination_context = if let Some(socket) = monitor {
            let socket = Arc::clone(socket);
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut monitor =
                crate::services::disconnect::DisconnectMonitor::from_signal(receiver, interrupted);
            let watcher = tokio::spawn(async move {
                if socket.peek(&mut [0]).await.unwrap_or(0) == 0 {
                    let _ = sender.send(());
                } else {
                    std::future::pending::<()>().await;
                }
            });
            let result = block_in_place(|| {
                prepare_destination_context(
                    state,
                    &route,
                    request_body,
                    &request_headers,
                    Some(&mut monitor),
                )
            });
            watcher.abort();
            let _ = watcher.await;
            result
        } else {
            block_in_place(|| {
                prepare_destination_context(state, &route, request_body, &request_headers, None)
            })
        };
        request_body = match destination_context {
            Ok(body @ Value::Object(_)) => body,
            Ok(_) => {
                let error = HistoryError::new("invalid_history_projection");
                let failed = history_stream_error(&error, websocket.observation);
                if websocket.send_json(&failed).await.is_err() {
                    return;
                }
                continue;
            }
            Err(DestinationPrepareError::Collaboration {
                status,
                code,
                message,
            }) => {
                if websocket
                    .send_json(&emp_websocket_error(status, code, message))
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
            Err(DestinationPrepareError::Router(error)) => {
                if websocket
                    .send_json(&websocket_router_error(&error))
                    .await
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
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
            Err(DestinationPrepareError::Disconnected) => return,
            Err(DestinationPrepareError::History(reason)) => {
                let failed =
                    history_stream_error(&HistoryError::new(reason), websocket.observation);
                if websocket.send_json(&failed).await.is_err() {
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
                if websocket.send_json(&failed).await.is_err() {
                    return;
                }
                continue;
            }
        };
        websocket.observation.phase(Phase::Execute);
        let mut turn = Turn {
            config,
            route,
            body: request_body,
            headers: request_headers,
            ids,
            scope: request_scope,
        };
        let (native_turn_activity, native_usage) =
            if turn.route.dialect == emp_domain::Dialect::CodexNative {
                match native_session.serve(state, &turn, &mut websocket).await {
                    NativeTurnResult::Finished => continue,
                    NativeTurnResult::Closed => return,
                    NativeTurnResult::HttpFallback(activity, usage) => (activity, usage),
                }
            } else {
                (None, None)
            };
        let body = turn.body.as_object_mut().unwrap();
        body.remove("previous_response_id");
        let generate = body
            .get("generate")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        if !generate {
            if websocket.warmup().await.is_err() {
                return;
            }
            continue;
        }
        body.remove("generate");
        body.insert("stream".to_owned(), Value::Bool(true));
        let (control, mut monitor) = control::TurnControl::start(state, &mut websocket);
        let result = if crate::services::claude_cli::selected(&turn.route) {
            claude::serve(state, turn, &mut websocket, monitor).await
        } else if turn.route.dialect == emp_domain::Dialect::CodexNative {
            http_stream::serve_native(
                state,
                &turn,
                &mut websocket,
                &mut monitor,
                native_turn_activity,
                native_usage.map(|usage| *usage),
            )
            .await
        } else {
            http_stream::serve_external(state, &turn, &mut websocket, &mut monitor).await
        };
        let result = control
            .finish(&mut websocket, &mut native_session.pending_request, result)
            .await;
        if matches!(result, TurnResult::Closed) {
            return;
        }
    }
}
