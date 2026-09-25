//! Native Responses failure translation and public error shapes.
use emp_protocol::collaboration::CollaborationError;
use emp_protocol::native_responses::NativeProjectionError;
use emp_transport::{
    FailureClass, HttpFailureInput, HttpTransportErrorKind, http_failure, public_failure_message,
    status_error_class,
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
        let mut error = json!({"code": code, "type": class.as_str(), "message": message.into()});
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
            body: json!({"error": {"message":message.into()}}),
            headers: BTreeMap::new(),
        }
    }

    pub(super) fn transport(class: FailureClass, status: u16, reason: Option<&str>) -> Self {
        Self::classified(
            status,
            class,
            format!("transport failure: class={}", class.as_str()),
            reason,
            None,
        )
    }

    pub(super) fn context(headers: BTreeMap<String, String>) -> Self {
        let mut error = Self::classified(
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
                body: json!({"error": {"code":"history_reconstruction_failed", "message":"History reconstruction failed. Continue in the original task or start a new task.",
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

pub(super) fn read_error(kind: HttpTransportErrorKind) -> NativeHttpError {
    match kind {
        HttpTransportErrorKind::ResponseTooLarge => {
            NativeHttpError::router(502, "upstream Responses 响应 is too large")
        }
        HttpTransportErrorKind::ReadTimeout => {
            NativeHttpError::router(504, "upstream request timed out")
        }
        _ => NativeHttpError::plain(500, "internal server error"),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn error_detail(content_type: &str, raw: &[u8]) -> (String, String) {
    let media = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let decoded = String::from_utf8_lossy(raw);
    let stripped = decoded.trim_start().to_lowercase();
    if matches!(media.as_str(), "text/html" | "application/xhtml+xml")
        || stripped.starts_with("<html")
        || stripped.starts_with("<!doctype html")
    {
        return (
            if media.is_empty() {
                "text/html".to_owned()
            } else {
                media
            },
            "HTML error page omitted; the upstream gateway or WAF may have rejected the request"
                .to_owned(),
        );
    }
    let mut detail = decoded.to_string();
    if (media.contains("json") || stripped.starts_with(['{', '[']))
        && let Ok(value) = serde_json::from_str::<Value>(&decoded)
    {
        let nested = if value.is_object() {
            value
                .get("error")
                .filter(|value| truthy(value))
                .or_else(|| value.get("message").filter(|value| truthy(value)))
                .unwrap_or(&value)
        } else {
            &value
        };
        let nested = if nested.is_object() {
            ["message", "type", "code"]
                .iter()
                .find_map(|key| nested.get(key).filter(|value| truthy(value)))
                .unwrap_or(nested)
        } else {
            nested
        };
        detail = nested
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| nested.to_string());
    }
    detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if detail.chars().count() > 512 {
        detail = detail
            .chars()
            .take(509)
            .collect::<String>()
            .trim_end()
            .to_owned()
            + "...";
    }
    (
        if media.is_empty() {
            "unknown content type".to_owned()
        } else {
            media
        },
        detail,
    )
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
    let (_, detail) = error_detail(content_type, raw);
    let failure = http_failure(HttpFailureInput {
        status,
        detail: &detail,
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
    let message = public_failure_message(failure.error_class, reason, failure.status);
    let mut error = NativeHttpError::classified(
        failure.status,
        failure.error_class,
        message,
        reason,
        if failure.error_class == FailureClass::ProxyUnavailable {
            None
        } else {
            failure.retry_after_seconds
        },
    );
    selected.extend(error.headers);
    error.headers = selected;
    error
}
