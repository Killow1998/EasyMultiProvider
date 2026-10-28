//! External route error conversion.

use super::*;

pub(super) fn upstream_error(
    status: u16,
    raw: &[u8],
    retry_after_seconds: Option<u64>,
) -> RouterError {
    let code = serde_json::from_slice::<Value>(raw)
        .ok()
        .and_then(|body| body.get("error")?.get("code")?.as_str().map(str::to_owned));
    let failure = http_failure(HttpFailureInput {
        status,
        detail: &String::from_utf8_lossy(raw),
        upstream_code: code.as_deref(),
        proxy_evidence: false,
        retry_after_seconds,
    });
    RouterError::new(
        RouterErrorKind::Upstream,
        failure.status,
        failure.error_class,
        failure.failure_reason,
        failure.retry_after_seconds,
        "upstream request failed",
    )
}

impl From<ResponsesValidationError> for RouterError {
    fn from(error: ResponsesValidationError) -> Self {
        responses_validation_error(error)
    }
}

pub(super) fn invalid_request(message: &'static str) -> RouterError {
    RouterError::new(
        RouterErrorKind::InvalidRequest,
        422,
        FailureClass::RouterError,
        Some("invalid_request".to_owned()),
        None,
        message,
    )
}

pub(super) fn unresolved_protocol() -> RouterError {
    RouterError::new(
        RouterErrorKind::UnsupportedProtocol,
        501,
        FailureClass::ProtocolRejection,
        Some("protocol_not_negotiated".to_owned()),
        None,
        "automatic protocol must be negotiated before external routing",
    )
}

pub(super) fn protocol_error(error: ProtocolError) -> RouterError {
    RouterError::new(
        RouterErrorKind::Protocol,
        error.status(),
        match error.error_class() {
            "invalid_request" => FailureClass::RouterError,
            "stream_incomplete" => FailureClass::StreamIncomplete,
            _ => FailureClass::ProtocolError,
        },
        Some(error.error_class().to_owned()),
        None,
        error.message(),
    )
}

pub(super) fn anthropic_error(error: AnthropicError) -> RouterError {
    RouterError::new(
        RouterErrorKind::Protocol,
        error.status(),
        match error.error_class() {
            "invalid_request" => FailureClass::RouterError,
            "stream_incomplete" => FailureClass::StreamIncomplete,
            _ => FailureClass::ProtocolError,
        },
        Some(error.error_class().to_owned()),
        None,
        error.message,
    )
}

pub(super) fn portable_request_error(error: PortableProjectionError) -> RouterError {
    RouterError::new(
        RouterErrorKind::InvalidRequest,
        422,
        FailureClass::RouterError,
        Some(error.failure_class().to_owned()),
        None,
        "portable Responses request projection failed",
    )
}

pub(super) fn portable_response_error(_error: PortableProjectionError) -> RouterError {
    RouterError::new(
        RouterErrorKind::Protocol,
        502,
        FailureClass::ProtocolError,
        None,
        None,
        "external Responses response projection failed",
    )
}

pub(super) fn responses_validation_error(error: ResponsesValidationError) -> RouterError {
    RouterError::new(
        RouterErrorKind::Protocol,
        502,
        FailureClass::ProtocolError,
        None,
        None,
        error.message(),
    )
}

pub(super) fn transport_error(error: HttpTransportError) -> RouterError {
    let (status, error_class, reason) = match error.kind() {
        HttpTransportErrorKind::InvalidRequest => {
            (422, FailureClass::RouterError, "invalid_request")
        }
        HttpTransportErrorKind::ClientBuild => (503, FailureClass::Network, "client_build_failed"),
        HttpTransportErrorKind::ConnectTimeout => {
            (504, FailureClass::ConnectTimeout, "connect_timeout")
        }
        HttpTransportErrorKind::ReadTimeout => (504, FailureClass::Timeout, "read_timeout"),
        HttpTransportErrorKind::ResponseTooLarge => {
            (502, FailureClass::ProtocolError, "upstream_body_too_large")
        }
        HttpTransportErrorKind::Network => (503, FailureClass::Network, "network"),
        HttpTransportErrorKind::DnsFailure => (503, FailureClass::DnsFailure, "dns_failure"),
        HttpTransportErrorKind::TlsFailure => (502, FailureClass::TlsFailure, "tls_failure"),
        HttpTransportErrorKind::ProxyUnavailable => {
            (503, FailureClass::ProxyUnavailable, "proxy_unavailable")
        }
        HttpTransportErrorKind::RedirectDisabled => {
            (502, FailureClass::ProtocolError, "redirect_disabled")
        }
    };
    RouterError::new(
        RouterErrorKind::Transport,
        status,
        error_class,
        Some(reason.to_owned()),
        None,
        "upstream transport failed",
    )
}

pub(super) fn tool_request_error(message: &'static str) -> RouterError {
    RouterError::new(
        RouterErrorKind::InvalidRequest,
        422,
        FailureClass::RouterError,
        None,
        None,
        message,
    )
}
pub(super) fn tool_response_error(message: &'static str) -> RouterError {
    RouterError::new(
        RouterErrorKind::Protocol,
        502,
        FailureClass::ProtocolError,
        None,
        None,
        message,
    )
}
