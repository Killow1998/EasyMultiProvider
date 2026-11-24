use super::call::{incoming_headers, native_headers, safe_forwarded_headers};
use super::{RealtimeError, valid_call_id};
use crate::VERSION;
use crate::api::websocket::socket::legacy_websocket_io;
use crate::app::ServerState;
use crate::http::request::Request;
use crate::http::response::{json_error_response, status_text};
use crate::server::http::proxy::require_proxy;
use emp_transport::{AsyncClientWebSocket, DEFAULT_WEBSOCKET_MESSAGE_BYTES, websocket_accept};
use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;
mod relay;
#[cfg(test)]
pub(crate) use relay::SIDEBAND_EVENT_CAPACITY;

pub(crate) const MAX_REALTIME_SIDEBAND_MESSAGE_BYTES: usize = DEFAULT_WEBSOCKET_MESSAGE_BYTES;
const NATIVE_SIDEBAND_BASE: &str = "wss://api.openai.com/v1/live/";

/// Created after handshake validation; its admission permit transfers to the relay.
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
    if let Err(error) = require_proxy(request, state, now) {
        return Err(error.wire_response());
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
        state.backend.diagnostics.journal.event("warning", "request_rejected", &serde_json::json!({"transport":"websocket", "reason":"connection_capacity", "route":"realtime_sideband"}));
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
    serve_sideband(stream, request, call_id, body_prefix, state, now, |_| {});
}

#[cfg(test)]
pub(crate) fn serve_realtime_sideband_with_test_connector(
    stream: &mut TcpStream,
    request: Request<'_>,
    call_id: &str,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
    configure: impl FnOnce(&mut PreparedSideband),
) {
    serve_sideband(stream, request, call_id, body_prefix, state, now, configure);
}

fn serve_sideband(
    stream: &mut TcpStream,
    request: Request<'_>,
    call_id: &str,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
    configure: impl FnOnce(&mut PreparedSideband),
) {
    let mut prepared = match prepare_sideband(request, call_id, state, now) {
        Err(response) => {
            write_http_response(stream, &response);
            return;
        }
        Ok(prepared) => prepared,
    };

    configure(&mut prepared);
    let runtime = state.backend.transport.runtime.handle();
    let mut upstream = match runtime.block_on(AsyncClientWebSocket::connect_with_proxy(
        &prepared.url,
        &prepared.headers,
        Duration::from_secs(15),
        prepared.proxy.as_deref(),
    )) {
        Ok(client) => client,
        Err(error) => {
            let (status, code, message) = if error.is_upgrade_rejection() {
                let status = error.status();
                let code = match status {
                    401 | 403 => "native_subscription_auth_failed",
                    404 | 405 | 426 | 501 => "native_realtime_unsupported",
                    _ => "native_realtime_transport_error",
                };
                (status, code, error.to_string())
            } else {
                (
                    502,
                    "native_realtime_transport_error",
                    "Native realtime sideband connection failed".to_owned(),
                )
            };
            let response = RealtimeError::new(status, code, message).wire_response();
            write_http_response(stream, &response);
            return;
        }
    };
    if let Err(error) = upstream.set_max_message_bytes(prepared.max_message_bytes) {
        let response =
            RealtimeError::new(502, "native_realtime_transport_error", error.to_string())
                .wire_response();
        write_http_response(stream, &response);
        return;
    }
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        prepared.accept
    );
    if stream.write_all(head.as_bytes()).is_err() || stream.flush().is_err() {
        runtime.block_on(upstream.close(1001, "downstream disconnected"));
        return;
    }
    let socket = match legacy_websocket_io(stream, runtime) {
        Ok(socket) => socket,
        Err(_) => {
            runtime.block_on(upstream.close(1011, "websocket setup failed"));
            return;
        }
    };
    runtime.block_on(relay::run(
        state,
        upstream,
        socket,
        &body_prefix,
        prepared.max_message_bytes,
    ));
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
