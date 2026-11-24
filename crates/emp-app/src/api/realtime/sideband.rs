use super::call::{incoming_headers, native_headers, safe_forwarded_headers};
use super::{RealtimeError, valid_call_id};
use crate::VERSION;
use crate::api::websocket::prepare_upgrade;
#[cfg(any(test, not(feature = "server-candidate")))]
use crate::api::websocket::socket::legacy_websocket_io;
use crate::app::ServerState;
use crate::http::request::Request;
use crate::server::http::error::HttpReply;
use crate::server::http::proxy::require_proxy;
use emp_transport::{AsyncClientWebSocket, DEFAULT_WEBSOCKET_MESSAGE_BYTES};
use std::collections::BTreeMap;
#[cfg(any(test, not(feature = "server-candidate")))]
use std::io::Write;
#[cfg(any(test, not(feature = "server-candidate")))]
use std::net::TcpStream;
use std::time::Duration;
pub(crate) mod relay;
#[cfg(test)]
pub(crate) use relay::SIDEBAND_EVENT_CAPACITY;

pub(crate) const MAX_REALTIME_SIDEBAND_MESSAGE_BYTES: usize = DEFAULT_WEBSOCKET_MESSAGE_BYTES;
const NATIVE_SIDEBAND_ORIGIN: &str = "wss://api.openai.com";

/// Created after handshake validation; its admission permit transfers to the relay.
pub(crate) struct PreparedSideband {
    pub(crate) accept: String,
    pub(crate) url: String,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) proxy: Option<String>,
    pub(crate) max_message_bytes: usize,
    pub(crate) permit: crate::services::connection_admission::ConnectionPermit,
}

pub(crate) fn prepare_sideband(
    request: Request<'_>,
    call_id: &str,
    state: &ServerState,
    now: f64,
) -> Result<PreparedSideband, HttpReply> {
    require_proxy(request, state, now).map_err(|error| error.reply())?;
    if !valid_call_id(call_id) {
        return Err(RealtimeError::new(
            400,
            "realtime_invalid_call_id",
            "Realtime sideband call ID is invalid",
        )
        .into());
    }
    let accept = prepare_upgrade(request, |_| "invalid Sec-WebSocket-Key".into())?;
    let Some(permit) = state.lifecycle.admission.acquire_websocket() else {
        state.backend.diagnostics.journal.event("warning", "request_rejected", &serde_json::json!({"transport":"websocket", "reason":"connection_capacity", "route":"realtime_sideband"}));
        return Err(HttpReply::error(
            503,
            "Too many WebSocket connections",
            Some("realtime_capacity_unavailable"),
        )
        .with_header("Retry-After", "2"));
    };
    let incoming = incoming_headers(request);
    let Some(mut headers) = native_headers(state) else {
        return Err(super::native_subscription_unavailable().into());
    };
    let forwarded = safe_forwarded_headers(&incoming)?;
    headers.extend(forwarded);
    headers.insert("User-Agent".to_owned(), format!("EMP/{VERSION}"));
    let url = format!("{NATIVE_SIDEBAND_ORIGIN}/v1/live/{call_id}");
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
        })?;
    #[cfg(all(test, unix))]
    let (url, proxy) = if let Some(address) = state.test_sideband_address.get() {
        let path = url.strip_prefix(NATIVE_SIDEBAND_ORIGIN).unwrap();
        (format!("ws://{address}{path}"), None)
    } else {
        (url, proxy)
    };
    Ok(PreparedSideband {
        accept,
        url,
        headers,
        proxy,
        max_message_bytes: MAX_REALTIME_SIDEBAND_MESSAGE_BYTES,
        permit,
    })
}

#[cfg(any(test, not(feature = "server-candidate")))]
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

#[cfg(any(test, not(feature = "server-candidate")))]
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
            write_http_response(stream, &response.wire_response());
            return;
        }
        Ok(prepared) => prepared,
    };

    configure(&mut prepared);
    let runtime = state.backend.transport.runtime.handle();
    let mut upstream = match runtime.block_on(prepared.connect()) {
        Ok(upstream) => upstream,
        Err(reply) => {
            write_http_response(stream, &reply.wire_response());
            return;
        }
    };
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

#[cfg(any(test, not(feature = "server-candidate")))]
fn write_http_response(stream: &mut TcpStream, response: &[u8]) {
    let _ = stream.write_all(response);
    let _ = stream.flush();
}

impl PreparedSideband {
    pub(crate) async fn connect(&self) -> Result<AsyncClientWebSocket, HttpReply> {
        let mut upstream = match AsyncClientWebSocket::connect_with_proxy(
            &self.url,
            &self.headers,
            Duration::from_secs(15),
            self.proxy.as_deref(),
        )
        .await
        {
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
                return Err(RealtimeError::new(status, code, message).into());
            }
        };
        if let Err(error) = upstream.set_max_message_bytes(self.max_message_bytes) {
            return Err(RealtimeError::new(
                502,
                "native_realtime_transport_error",
                error.to_string(),
            )
            .into());
        }
        Ok(upstream)
    }
}

#[cfg(test)]
mod tests {
    use super::valid_call_id;
    use crate::api::websocket::connection_has_upgrade_token;

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
