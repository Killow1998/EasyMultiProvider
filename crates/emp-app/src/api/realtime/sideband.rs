//! Authentication, admission and upstream target selection for Voice sidebands.

use super::call::{incoming_headers, native_headers, safe_forwarded_headers};
use super::{RealtimeError, valid_call_id};
use crate::VERSION;
use crate::app::ServerState;
use crate::http::auth::{proxy_allowed, same_origin};
use crate::http::request::Request;
use crate::http::response::{json_error_response, status_text};
use emp_transport::websocket_accept;
use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpStream;

pub(crate) const MAX_REALTIME_SIDEBAND_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const NATIVE_SIDEBAND_BASE: &str = "wss://api.openai.com/v1/live/";

/// All immutable state needed after the downstream WebSocket handshake has
/// been validated. Ownership of the permit follows the eventual relay.
#[allow(dead_code)] // The next slice hands these fields to the upstream owner-pump.
pub(crate) struct PreparedSideband {
    pub(crate) accept: String,
    pub(crate) call_id: String,
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
        call_id: call_id.to_owned(),
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
    _body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) {
    let response = match prepare_sideband(request, call_id, state, now) {
        Err(response) => response,
        Ok(_prepared) => RealtimeError::new(
            503,
            "native_realtime_transport_error",
            "Native realtime sideband connection failed",
        )
        .wire_response(),
    };
    let _ = stream.write_all(&response);
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
