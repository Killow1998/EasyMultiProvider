//! Authentication, admission and upstream target selection for Voice sidebands.

use super::call::{incoming_headers, native_headers, safe_forwarded_headers};
use super::{valid_call_id, RealtimeError};
use crate::app::ServerState;
use crate::http::auth::{proxy_allowed, same_origin};
use crate::http::request::Request;
use crate::http::response::{json_error_response, status_text};
use crate::VERSION;
use emp_transport::{
    websocket_accept, ClientWebSocket, ClientWebSocketError, ClientWebSocketPump, PumpCommand,
    PumpEvent, WebSocketConnection, WebSocketPoll, WebSocketPumpConfig,
    DEFAULT_PUMP_CHANNEL_CAPACITY,
};
use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpStream;
use std::sync::mpsc::{RecvTimeoutError, TrySendError};
use std::time::Duration;

pub(crate) const MAX_REALTIME_SIDEBAND_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const NATIVE_SIDEBAND_BASE: &str = "wss://api.openai.com/v1/live/";

/// All immutable state needed after the downstream WebSocket handshake has
/// been validated. Ownership of the permit follows the eventual relay.
pub(crate) struct PreparedSideband {
    pub(crate) accept: String,
    pub(crate) url: String,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) proxy: Option<String>,
    pub(crate) max_message_bytes: usize,
    _permit: crate::services::connection_admission::ConnectionPermit,
}

pub(crate) fn prepare_sideband(
    request: Request<'_>,
    call_id: &str,
    state: &ServerState,
    now: f64,
) -> Result<PreparedSideband, Vec<u8>> {
    if !proxy_allowed(request, state, now) {
        let status = if same_origin(request, state.port) {
            401
        } else {
            403
        };
        return Err(RealtimeError::new(
            status,
            "realtime_caller_unauthorized",
            "Caller authentication is required for Codex Voice",
        )
        .wire_response());
    }
    if !valid_call_id(call_id) {
        return Err(RealtimeError::new(
            400,
            "realtime_invalid_call_id",
            "Realtime sideband call ID is invalid",
        )
        .wire_response());
    }
    if !request
        .header("Upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        || !connection_has_upgrade_token(request.header("Connection").unwrap_or_default())
        || request.header("Sec-WebSocket-Version") != Some("13")
    {
        return Err(plain_error(400, "invalid websocket upgrade"));
    }
    let accept = match request
        .header("Sec-WebSocket-Key")
        .and_then(|key| websocket_accept(key).ok())
    {
        Some(value) => value,
        None => return Err(plain_error(400, "invalid Sec-WebSocket-Key")),
    };
    let Some(permit) = state.connection_admission.acquire_websocket() else {
        return Err(json_error_response(
            503,
            status_text(503),
            "Too many WebSocket connections",
            Some("realtime_capacity_unavailable"),
            &[("Retry-After", "2")],
        ));
    };
    let incoming = incoming_headers(request);
    let Some(mut headers) = native_headers(state) else {
        return Err(RealtimeError::new(
            401,
            "native_subscription_unavailable",
            "A current native ChatGPT subscription login is required for Codex Voice",
        )
        .wire_response());
    };
    let forwarded = safe_forwarded_headers(&incoming).map_err(|error| error.wire_response())?;
    headers.extend(forwarded);
    headers.insert("User-Agent".to_owned(), format!("EMP/{VERSION}"));
    let url = format!("{NATIVE_SIDEBAND_BASE}{call_id}");
    let proxy = state
        .backend
        .transport
        .client
        .websocket_proxy_for(&url)
        .map_err(|_| {
            RealtimeError::new(
                502,
                "native_realtime_transport_error",
                "Native realtime sideband connection failed",
            )
            .wire_response()
        })?;
    Ok(PreparedSideband {
        accept,
        url,
        headers,
        proxy,
        max_message_bytes: MAX_REALTIME_SIDEBAND_MESSAGE_BYTES,
        _permit: permit,
    })
}

pub(crate) fn serve_realtime_sideband(
    stream: &mut TcpStream,
    request: Request<'_>,
    call_id: &str,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) {
    serve_realtime_sideband_with_connector(
        stream,
        request,
        call_id,
        body_prefix,
        state,
        now,
        |url, headers, timeout, proxy| {
            ClientWebSocket::connect_with_proxy(url, headers, timeout, proxy)
        },
    );
}

#[cfg(test)]
pub(crate) fn serve_realtime_sideband_with_test_connector(
    stream: &mut TcpStream,
    request: Request<'_>,
    call_id: &str,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
    connect: impl FnOnce(
        &str,
        &BTreeMap<String, String>,
        Duration,
        Option<&str>,
    ) -> Result<ClientWebSocket, ClientWebSocketError>,
) {
    serve_realtime_sideband_with_connector(
        stream,
        request,
        call_id,
        body_prefix,
        state,
        now,
        connect,
    );
}

fn serve_realtime_sideband_with_connector(
    stream: &mut TcpStream,
    request: Request<'_>,
    call_id: &str,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
    connect: impl FnOnce(
        &str,
        &BTreeMap<String, String>,
        Duration,
        Option<&str>,
    ) -> Result<ClientWebSocket, ClientWebSocketError>,
) {
    let prepared = match prepare_sideband(request, call_id, state, now) {
        Err(response) => {
            write_http_response(stream, &response);
            return;
        }
        Ok(prepared) => prepared,
    };

    let upstream = match connect(
        &prepared.url,
        &prepared.headers,
        Duration::from_secs(15),
        prepared.proxy.as_deref(),
    ) {
        Ok(client) => client,
        Err(error) => {
            let status = error.status();
            let (code, message) = match status {
                401 | 403 => ("native_subscription_auth_failed", error.to_string()),
                404 | 405 | 426 | 501 => ("native_realtime_unsupported", error.to_string()),
                _ => ("native_realtime_transport_error", error.to_string()),
            };
            let response = RealtimeError::new(status, code, message).wire_response();
            write_http_response(stream, &response);
            return;
        }
    };
    let mut pump = match ClientWebSocketPump::spawn(
        upstream,
        WebSocketPumpConfig {
            outbound_capacity: DEFAULT_PUMP_CHANNEL_CAPACITY,
            inbound_capacity: DEFAULT_PUMP_CHANNEL_CAPACITY,
            max_message_bytes: prepared.max_message_bytes,
        },
    ) {
        Ok(pump) => pump,
        Err(error) => {
            let response =
                RealtimeError::new(502, "native_realtime_transport_error", error.to_string())
                    .wire_response();
            write_http_response(stream, &response);
            return;
        }
    };

    if stream
        .set_read_timeout(Some(Duration::from_millis(10)))
        .is_err()
    {
        shutdown_pump(&mut pump, 1011, "websocket setup failed");
        let response = RealtimeError::new(
            500,
            "realtime_websocket_unavailable",
            "Realtime sideband socket setup failed",
        )
        .wire_response();
        write_http_response(stream, &response);
        return;
    }
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        prepared.accept
    );
    if stream.write_all(head.as_bytes()).is_err() || stream.flush().is_err() {
        shutdown_pump(&mut pump, 1001, "downstream disconnected");
        return;
    }

    let mut downstream = match WebSocketConnection::new_with_prefix(stream, &body_prefix) {
        Ok(connection) => connection,
        Err(error) => {
            shutdown_pump(&mut pump, 1009, "websocket message too large");
            let _ = error;
            return;
        }
    };
    if downstream
        .set_max_message_bytes(prepared.max_message_bytes)
        .is_err()
    {
        shutdown_pump(&mut pump, 1011, "websocket setup failed");
        downstream.close(1011, "websocket setup failed");
        return;
    }
    if downstream
        .set_poll_timeout(Duration::from_millis(10))
        .is_err()
    {
        shutdown_pump(&mut pump, 1011, "websocket setup failed");
        downstream.close(1011, "websocket setup failed");
        return;
    }

    let mut pending_command = None;
    let mut relay_finished = false;
    while !relay_finished {
        if let Some(command) = pending_command.take() {
            match pump.try_send(command) {
                Ok(()) => {}
                Err(TrySendError::Full(command)) => pending_command = Some(command),
                Err(TrySendError::Disconnected(_)) => {
                    downstream.close(1011, "native sideband disconnected");
                    break;
                }
            }
        }

        for _ in 0..DEFAULT_PUMP_CHANNEL_CAPACITY {
            match pump.recv_timeout(Duration::ZERO) {
                Ok(PumpEvent::Text(text)) => {
                    if downstream.send_json_bytes(text.as_bytes()).is_err() {
                        shutdown_pump(&mut pump, 1001, "downstream disconnected");
                        relay_finished = true;
                        break;
                    }
                }
                Ok(PumpEvent::Closed { code }) => {
                    downstream.close(valid_close_code(code.unwrap_or(1000)), "");
                    relay_finished = true;
                    break;
                }
                Ok(PumpEvent::Failure { .. }) => {
                    downstream.close(1011, "native sideband failed");
                    relay_finished = true;
                    break;
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    downstream.close(1011, "native sideband disconnected");
                    relay_finished = true;
                    break;
                }
            }
        }
        if relay_finished {
            break;
        }

        if pending_command.is_some() {
            match pump.recv_timeout(Duration::from_millis(10)) {
                Ok(event) => relay_finished = !relay_event(event, &mut downstream, &mut pump),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    downstream.close(1011, "native sideband disconnected");
                    relay_finished = true;
                }
            }
            continue;
        }

        match downstream.poll_text() {
            Ok(WebSocketPoll::Pending) => match pump.recv_timeout(Duration::from_millis(10)) {
                Ok(event) => relay_finished = !relay_event(event, &mut downstream, &mut pump),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    downstream.close(1011, "native sideband disconnected");
                    relay_finished = true;
                }
            },
            Ok(WebSocketPoll::Text(text)) => {
                pending_command = Some(PumpCommand::Text(text));
            }
            Ok(WebSocketPoll::Ping(payload)) => {
                if downstream.send_pong(&payload).is_err() {
                    shutdown_pump(&mut pump, 1011, "downstream websocket failed");
                    relay_finished = true;
                }
            }
            Ok(WebSocketPoll::Closed { code }) => {
                shutdown_pump(&mut pump, valid_close_code(code.unwrap_or(1000)), "");
                relay_finished = true;
            }
            Err(error) => {
                let code = valid_close_code(error.close_code());
                let reason = error.to_string();
                downstream.close(code, &reason);
                shutdown_pump(&mut pump, code, "downstream websocket failed");
                relay_finished = true;
            }
        }
    }
    shutdown_pump(&mut pump, 1000, "");
}

fn relay_event(
    event: PumpEvent,
    downstream: &mut WebSocketConnection<'_, TcpStream>,
    pump: &mut ClientWebSocketPump,
) -> bool {
    match event {
        PumpEvent::Text(text) => {
            if downstream.send_json_bytes(text.as_bytes()).is_ok() {
                true
            } else {
                shutdown_pump(pump, 1001, "downstream disconnected");
                false
            }
        }
        PumpEvent::Closed { code } => {
            downstream.close(valid_close_code(code.unwrap_or(1000)), "");
            false
        }
        PumpEvent::Failure { .. } => {
            downstream.close(1011, "native sideband failed");
            false
        }
    }
}

fn shutdown_pump(pump: &mut ClientWebSocketPump, code: u16, reason: &str) {
    let mut close = Some(PumpCommand::Close {
        code,
        reason: reason.to_owned(),
    });
    loop {
        if let Some(command) = close.take() {
            match pump.try_send(command) {
                Ok(()) => {}
                Err(TrySendError::Full(command)) => close = Some(command),
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
        match pump.recv_timeout(Duration::from_millis(10)) {
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    pump.join();
}

fn valid_close_code(code: u16) -> u16 {
    if (1000..=1014).contains(&code) && !(1004..=1006).contains(&code)
        || (3000..=4999).contains(&code)
    {
        code
    } else {
        1011
    }
}

fn write_http_response(stream: &mut TcpStream, response: &[u8]) {
    let _ = stream.write_all(response);
    let _ = stream.flush();
}

fn plain_error(status: u16, message: &str) -> Vec<u8> {
    json_error_response(status, status_text(status), message, None, &[])
}

fn connection_has_upgrade_token(value: &str) -> bool {
    value
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
}

#[cfg(test)]
mod tests {
    use super::{connection_has_upgrade_token, valid_call_id};

    #[test]
    fn accepts_only_python_realtime_call_id_forms() {
        for value in [
            "rtc_voice_123",
            "rtc_a.b~c-d_9",
            "01234567-89ab-CDEF-0123-456789abcdef",
        ] {
            assert!(valid_call_id(value), "{value}");
        }
        for value in [
            "",
            "rtc_",
            "rtc_bad/id",
            "rtc_bad%2Fid",
            "not-a-call-id",
            "0123456789abcdef",
        ] {
            assert!(!valid_call_id(value), "{value}");
        }
    }

    #[test]
    fn connection_tokens_are_case_insensitive_and_trimmed() {
        assert!(connection_has_upgrade_token("keep-alive,  UpGrAdE , close"));
        assert!(!connection_has_upgrade_token("keep-alive, close"));
    }
}
