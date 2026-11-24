use super::call::{incoming_headers, native_headers, safe_forwarded_headers};
use super::{RealtimeError, valid_call_id};
use crate::VERSION;
use crate::api::websocket::prepare_upgrade;
use crate::app::ServerState;
use crate::http::request::Request;
use crate::server::http::error::HttpReply;
use crate::server::http::proxy::require_proxy;
use emp_transport::{AsyncClientWebSocket, DEFAULT_WEBSOCKET_MESSAGE_BYTES};
use std::collections::BTreeMap;
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
    #[cfg(test)]
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
