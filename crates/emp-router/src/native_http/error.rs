//! Native Responses failure translation and public error shapes.
use emp_protocol::collaboration::CollaborationError;
use emp_protocol::native_responses::NativeProjectionError;
use emp_transport::{
    FailureClass, HttpFailureInput, HttpTransportErrorKind, http_failure, status_error_class,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;

pub struct NativeHttpError {
    pub status: u16,
    pub body: Value,
    pub headers: BTreeMap<String, String>,
}

impl fmt::Debug for NativeHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeHttpError")
            .field("status", &self.status)
            .field("body", &self.body)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl NativeHttpError {
    fn classified(
        origin: &'static str,
        status: u16,
        class: FailureClass,
        message: impl Into<String>,
        reason: Option<&str>,
        retry: Option<u64>,
    ) -> Self {
        let code = if class == FailureClass::RateLimit {
            "rate_limit_exceeded"
        } else {
            reason.unwrap_or(class.as_str())
        };
        let label = match origin {
            "upstream" => "Upstream service",
            "transport" => "EMP connection",
            _ => "EMP",
        };
        let mut error = json!({"code": code, "type": class.as_str(), "origin":origin, "message":format!("[{label}] {}", message.into())});
        if let Some(reason) = reason {
            error["failure_reason"] = reason.into();
        }
        let mut headers = BTreeMap::new();
        if let Some(retry) = retry {
            error["retry_after_seconds"] = retry.into();
            headers.insert("Retry-After".to_owned(), retry.to_string());
        }
        Self {
            status,
            body: json!({"error": error}),
            headers,
        }
    }

    pub fn router(status: u16, message: impl Into<String>) -> Self {
        Self::classified(
            "emp",
            status,
            status_error_class(Some(status)),
            message,
            None,
            None,
        )
    }

    pub(super) fn plain(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({"error": {"origin":"emp", "message":format!("[EMP] {}",message.into())}}),
            headers: BTreeMap::new(),
        }
    }

    pub(super) fn transport(class: FailureClass, status: u16, reason: Option<&str>) -> Self {
        Self::classified(
            "transport",
            status,
            class,
            format!("transport failure: class={}", class.as_str()),
            reason,
            None,
        )
    }

    pub(super) fn context(headers: BTreeMap<String, String>) -> Self {
        let mut error = Self::classified(
            "emp",
            413,
            FailureClass::ContextLengthExceeded,
            "context length exceeded: estimated input unknown tokens, safe input limit unknown; provider unknown, model unknown; next action: reduce input or use native remote compaction",
            None,
            None,
        );
        error.headers = headers;
        error
    }
}

pub(super) fn projection_error(error: NativeProjectionError) -> NativeHttpError {
    match error {
        NativeProjectionError::Projection(error)
            if error.failure_class() == "invalid_compaction" =>
        {
            NativeHttpError {
                status: 409,
                body: json!({"error": {"code":"history_reconstruction_failed", "origin":"emp", "message":"[EMP] History reconstruction failed. Continue in the original task or start a new task.",
                "error_class":"history_reconstruction_failed", "reason":"history_projection_incomplete"}}),
                headers: BTreeMap::new(),
            }
        }
        NativeProjectionError::Projection(error) => NativeHttpError::router(422, error.to_string()),
        NativeProjectionError::UnhashableItemType => {
            NativeHttpError::plain(500, "internal server error")
        }
    }
}

pub(super) fn collaboration_error(error: CollaborationError) -> NativeHttpError {
    match error {
        CollaborationError::NamespaceCollision => NativeHttpError::classified(
            "emp",
            422,
            FailureClass::RouterError,
            error.to_string(),
            error.failure_reason(),
            None,
        ),
        CollaborationError::UnexpectedEncryptedArguments => {
            NativeHttpError::plain(400, error.to_string())
        }
        _ => NativeHttpError::plain(500, "internal server error"),
    }
}

pub(super) fn send_error(kind: HttpTransportErrorKind) -> NativeHttpError {
    let (class, status, reason) = match kind {
        HttpTransportErrorKind::DnsFailure => (FailureClass::DnsFailure, 503, "dns_failure"),
        HttpTransportErrorKind::TlsFailure => (FailureClass::TlsFailure, 502, "tls_failure"),
        HttpTransportErrorKind::ProxyUnavailable => {
            (FailureClass::ProxyUnavailable, 503, "proxy_unavailable")
        }
        HttpTransportErrorKind::ConnectTimeout | HttpTransportErrorKind::ReadTimeout => {
            (FailureClass::ConnectTimeout, 504, "connect_timeout")
        }
        HttpTransportErrorKind::Network => (FailureClass::Network, 503, "network"),
        _ => return NativeHttpError::router(502, "upstream request failed"),
    };
    NativeHttpError::transport(class, status, Some(reason))
}

pub(super) fn read_error(kind: HttpTransportErrorKind) -> NativeHttpError {
    match kind {
        HttpTransportErrorKind::ResponseTooLarge => {
            NativeHttpError::router(502, "upstream Responses 响应 is too large")
        }
        HttpTransportErrorKind::ReadTimeout => {
            NativeHttpError::transport(FailureClass::Timeout, 504, Some("read_timeout"))
        }
        HttpTransportErrorKind::ConnectTimeout => {
            NativeHttpError::transport(FailureClass::Timeout, 504, Some("connect_timeout"))
        }
        HttpTransportErrorKind::Network => {
            NativeHttpError::transport(FailureClass::Network, 500, Some("network"))
        }
        _ => NativeHttpError::plain(500, "internal server error"),
    }
}

fn proxy_evidence(headers: &str, detail: &str) -> bool {
    let text = format!("{headers} {detail}").to_lowercase();
    [
        "cannot connect to proxy",
        "proxy connect",
        "proxy connection",
        "proxy error",
        "proxy server",
        "tunnel connection failed",
        "proxy-agent",
        "x-squid-error",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

pub(super) fn upstream_http_error(
    status: u16,
    content_type: &str,
    raw: &[u8],
    proxy_headers: &str,
    retry: Option<u64>,
    mut selected: BTreeMap<String, String>,
) -> NativeHttpError {
    let (code, detail) = crate::upstream_rejection::details(status, content_type, raw);
    let failure = http_failure(HttpFailureInput {
        status,
        detail: &detail,
        upstream_code: code.as_deref(),
        proxy_evidence: proxy_evidence(proxy_headers, &detail),
        retry_after_seconds: retry,
    });
    let reason = failure.failure_reason.as_deref().map(|reason| {
        if reason == "upstream_504" {
            "upstream_rejected"
        } else {
            reason
        }
    });
    let mut error = NativeHttpError::classified(
        "upstream",
        failure.status,
        failure.error_class,
        detail,
        reason,
        if failure.error_class == FailureClass::ProxyUnavailable {
            None
        } else {
            failure.retry_after_seconds
        },
    );
    if let Some(code) = code {
        error.body["error"]["upstream_code"] = code.into();
    }
    selected.extend(error.headers);
    error.headers = selected;
    error
}
