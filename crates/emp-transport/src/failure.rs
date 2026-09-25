use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePhase {
    Connect,
    FirstEvent,
    Streaming,
    TerminalValidation,
}

impl FailurePhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::FirstEvent => "first_event",
            Self::Streaming => "streaming",
            Self::TerminalValidation => "terminal_validation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    None,
    Auth,
    PaymentRequired,
    RateLimit,
    ProtocolRejection,
    Upstream5xx,
    Timeout,
    Upstream504,
    ConnectTimeout,
    FirstEventTimeout,
    FirstOutputTimeout,
    IdleAfterOutput,
    LocalDeadline,
    Network,
    ProxyUnavailable,
    DnsFailure,
    TlsFailure,
    RouterError,
    ProtocolError,
    StreamError,
    StreamIncomplete,
    ClientDisconnect,
    MalformedTerminal,
    ProxyReset,
    OutputLimit,
    ContentFilter,
    ContextLengthExceeded,
    ExternalCompactionFailed,
    HistoryReconstructionFailed,
    UpstreamCapacity,
}

impl FailureClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Auth => "auth",
            Self::PaymentRequired => "payment_required",
            Self::RateLimit => "rate_limit",
            Self::ProtocolRejection => "protocol_rejection",
            Self::Upstream5xx => "upstream_5xx",
            Self::Timeout => "timeout",
            Self::Upstream504 => "upstream_504",
            Self::ConnectTimeout => "connect_timeout",
            Self::FirstEventTimeout => "first_event_timeout",
            Self::FirstOutputTimeout => "first_output_timeout",
            Self::IdleAfterOutput => "idle_after_output",
            Self::LocalDeadline => "local_deadline",
            Self::Network => "network",
            Self::ProxyUnavailable => "proxy_unavailable",
            Self::DnsFailure => "dns_failure",
            Self::TlsFailure => "tls_failure",
            Self::RouterError => "router_error",
            Self::ProtocolError => "protocol_error",
            Self::StreamError => "stream_error",
            Self::StreamIncomplete => "stream_incomplete",
            Self::ClientDisconnect => "client_disconnect",
            Self::MalformedTerminal => "malformed_terminal",
            Self::ProxyReset => "proxy_reset",
            Self::OutputLimit => "output_limit",
            Self::ContentFilter => "content_filter",
            Self::ContextLengthExceeded => "context_length_exceeded",
            Self::ExternalCompactionFailed => "external_compaction_failed",
            Self::HistoryReconstructionFailed => "history_reconstruction_failed",
            Self::UpstreamCapacity => "upstream_capacity",
        }
    }

    fn from_normalized(value: &str) -> Option<Self> {
        Some(match value {
            "none" => Self::None,
            "auth" => Self::Auth,
            "payment_required" => Self::PaymentRequired,
            "rate_limit" => Self::RateLimit,
            "protocol_rejection" => Self::ProtocolRejection,
            "upstream_5xx" => Self::Upstream5xx,
            "timeout" => Self::Timeout,
            "upstream_504" => Self::Upstream504,
            "connect_timeout" => Self::ConnectTimeout,
            "first_event_timeout" => Self::FirstEventTimeout,
            "first_output_timeout" => Self::FirstOutputTimeout,
            "idle_after_output" => Self::IdleAfterOutput,
            "local_deadline" => Self::LocalDeadline,
            "network" => Self::Network,
            "proxy_unavailable" => Self::ProxyUnavailable,
            "dns_failure" => Self::DnsFailure,
            "tls_failure" => Self::TlsFailure,
            "router_error" => Self::RouterError,
            "protocol_error" => Self::ProtocolError,
            "stream_error" => Self::StreamError,
            "stream_incomplete" => Self::StreamIncomplete,
            "client_disconnect" => Self::ClientDisconnect,
            "malformed_terminal" => Self::MalformedTerminal,
            "proxy_reset" => Self::ProxyReset,
            "output_limit" => Self::OutputLimit,
            "content_filter" => Self::ContentFilter,
            "context_length_exceeded" => Self::ContextLengthExceeded,
            "external_compaction_failed" => Self::ExternalCompactionFailed,
            "history_reconstruction_failed" => Self::HistoryReconstructionFailed,
            "upstream_capacity" => Self::UpstreamCapacity,
            _ => return None,
        })
    }
}

fn safe_token(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

pub fn normalize_error_class(value: Option<&str>, fallback: FailureClass) -> FailureClass {
    value
        .map(safe_token)
        .as_deref()
        .and_then(FailureClass::from_normalized)
        .unwrap_or(fallback)
}

pub fn status_error_class(status: Option<u16>) -> FailureClass {
    match status {
        None => FailureClass::Network,
        Some(401 | 403) => FailureClass::Auth,
        Some(402) => FailureClass::PaymentRequired,
        Some(429) => FailureClass::RateLimit,
        Some(404 | 405 | 415 | 501) => FailureClass::ProtocolRejection,
        Some(504) => FailureClass::Upstream504,
        Some(408) => FailureClass::Timeout,
        Some(500..=599) => FailureClass::Upstream5xx,
        Some(_) => FailureClass::RouterError,
    }
}

pub fn protocol_fallback_allowed(
    status: u16,
    output_emitted: bool,
    terminal_event_observed: bool,
) -> bool {
    matches!(status, 404 | 405 | 415 | 501) && !output_emitted && !terminal_event_observed
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpstreamFailure {
    pub error_class: FailureClass,
    pub status: u16,
    pub phase: FailurePhase,
    pub terminal_event: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}

impl UpstreamFailure {
    pub fn new(error_class: FailureClass, status: u16, phase: FailurePhase) -> Self {
        Self {
            error_class,
            status,
            phase,
            terminal_event: false,
            failure_reason: None,
            retry_after_seconds: None,
        }
    }

    fn with_reason(mut self, reason: &str) -> Self {
        self.failure_reason = Some(safe_token(reason));
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkFailureKind {
    Timeout,
    Tls,
    Dns,
    ConnectionRefused,
    ConnectionReset,
    Other,
}

pub fn network_failure(
    kind: NetworkFailureKind,
    phase: FailurePhase,
    proxy_configured: bool,
) -> UpstreamFailure {
    match kind {
        NetworkFailureKind::Timeout => {
            UpstreamFailure::new(FailureClass::ConnectTimeout, 504, FailurePhase::Connect)
                .with_reason("connect_timeout")
        }
        NetworkFailureKind::Tls => {
            UpstreamFailure::new(FailureClass::TlsFailure, 502, phase).with_reason("tls_failure")
        }
        NetworkFailureKind::Dns => {
            UpstreamFailure::new(FailureClass::DnsFailure, 503, phase).with_reason("dns_failure")
        }
        NetworkFailureKind::ConnectionRefused if proxy_configured => {
            UpstreamFailure::new(FailureClass::ProxyUnavailable, 503, phase)
                .with_reason("proxy_unavailable")
        }
        NetworkFailureKind::ConnectionReset if proxy_configured => {
            UpstreamFailure::new(FailureClass::ProxyReset, 502, phase).with_reason("proxy_reset")
        }
        _ => UpstreamFailure::new(FailureClass::Network, 503, phase)
            .with_reason("network_unavailable"),
    }
}

fn http_failure_reason(status: u16, detail: &str) -> &'static str {
    let mut text = String::new();
    let mut separated = true;
    for character in detail.to_ascii_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            text.push(character);
            separated = false;
        } else if !separated {
            text.push(' ');
            separated = true;
        }
    }
    if text.ends_with(' ') {
        text.pop();
    }
    let contains_any = |words: &[&str]| words.iter().any(|word| text.contains(word));
    match status {
        401 | 403 => "auth_rejected",
        402 => "payment_required",
        413 => "request_too_large",
        _ if contains_any(&["request too large", "payload too large", "input too large"]) => {
            "request_too_large"
        }
        _ if contains_any(&["context length", "context window", "maximum context"]) => {
            "context_length_exceeded"
        }
        429 if contains_any(&["quota", "credit", "balance", "insufficient"]) => "quota_exhausted",
        429 if contains_any(&["capacity", "overloaded", "saturated"]) => "upstream_capacity",
        429 => "rate_limited",
        504 => "upstream_504",
        500 | 502 | 503 => "upstream_unavailable",
        _ => "upstream_rejected",
    }
}

pub struct HttpFailureInput<'a> {
    pub status: u16,
    pub detail: &'a str,
    pub proxy_evidence: bool,
    pub retry_after_seconds: Option<u64>,
}

pub fn http_failure(input: HttpFailureInput<'_>) -> UpstreamFailure {
    if matches!(input.status, 502..=504) && input.proxy_evidence {
        return UpstreamFailure::new(
            FailureClass::ProxyUnavailable,
            if input.status == 504 { 504 } else { 503 },
            FailurePhase::Connect,
        )
        .with_reason("proxy_unavailable");
    }
    let mut failure = UpstreamFailure::new(
        status_error_class(Some(input.status)),
        input.status,
        FailurePhase::TerminalValidation,
    )
    .with_reason(http_failure_reason(input.status, input.detail));
    if matches!(input.status, 429 | 503) {
        failure.retry_after_seconds = input.retry_after_seconds;
    }
    failure
}

pub fn retry_allowed(
    failure: &UpstreamFailure,
    attempt: usize,
    replayable: bool,
    output_emitted: bool,
    tool_activity: bool,
) -> bool {
    attempt == 0
        && replayable
        && !output_emitted
        && !tool_activity
        && !failure.terminal_event
        && matches!(
            failure.error_class,
            FailureClass::ConnectTimeout
                | FailureClass::FirstEventTimeout
                | FailureClass::Network
                | FailureClass::ProxyReset
        )
}

/// External requests retry twice before any output: 429 (never on `:free`
/// routes; quota exhaustion stays terminal), 504, and 5xx-capacity. The
/// caller supplies an exponential backoff base of 500 ms capped at 8 s with
/// ±25% jitter when no `Retry-After` applies.
pub fn external_http_retry_allowed(
    failure: &UpstreamFailure,
    attempt: usize,
    output_emitted: bool,
    tool_activity: bool,
    free_route: bool,
) -> bool {
    const MAX_EXTERNAL_ATTEMPTS: usize = 3;
    const MAX_RETRY_AFTER_SECONDS: u64 = 300;
    if attempt >= MAX_EXTERNAL_ATTEMPTS - 1
        || output_emitted
        || tool_activity
        || failure.terminal_event
    {
        return false;
    }
    let retryable = failure.status == 504
        || (failure.status == 429
            && !free_route
            && matches!(
                failure.failure_reason.as_deref(),
                Some("rate_limited") | Some("upstream_capacity")
            ));
    retryable && failure.retry_after_seconds.unwrap_or(1) <= MAX_RETRY_AFTER_SECONDS
}

/// Exponential backoff with ±25% jitter for attempts without usable
/// `Retry-After`: 500 ms, 1 s, 2 s — each capped at 8 s.
pub fn external_backoff_delay(attempt: usize) -> Duration {
    const BASE_MS: u64 = 500;
    const CAP_MS: u64 = 8_000;
    let mut nonce = [0u8; 1];
    // Jitter shrinks the delay by 0–25%; getrandom failure falls back to 0.
    let jitter_byte = getrandom::getrandom(&mut nonce)
        .ok()
        .map(|_| nonce[0])
        .unwrap_or(0);
    let shrink = u64::from(jitter_byte % 64);
    let base = BASE_MS.saturating_mul(1 << attempt.min(4)).min(CAP_MS);
    Duration::from_millis(base.saturating_sub(base / 4 * shrink / 64))
}

pub fn public_failure_message(
    error_class: FailureClass,
    failure_reason: Option<&str>,
    status: u16,
) -> String {
    let reason = failure_reason.map(safe_token);
    match error_class {
        FailureClass::ProxyUnavailable => "Configured proxy is unavailable.".to_owned(),
        FailureClass::DnsFailure => "The upstream host name could not be resolved.".to_owned(),
        FailureClass::TlsFailure => "The secure connection to the upstream failed.".to_owned(),
        FailureClass::Network => {
            "The network connection to the upstream is unavailable.".to_owned()
        }
        FailureClass::ConnectTimeout
        | FailureClass::FirstEventTimeout
        | FailureClass::FirstOutputTimeout
        | FailureClass::IdleAfterOutput
        | FailureClass::LocalDeadline
        | FailureClass::Upstream504
        | FailureClass::Timeout => "The upstream request timed out.".to_owned(),
        FailureClass::RateLimit => "The upstream rate limit was reached.".to_owned(),
        FailureClass::Auth => "The upstream rejected the account credentials.".to_owned(),
        FailureClass::StreamIncomplete => {
            "The upstream stream ended without a valid completion event.".to_owned()
        }
        FailureClass::Upstream5xx => {
            format!("The upstream service returned HTTP {status}.")
        }
        FailureClass::MalformedTerminal => match reason.as_deref() {
            Some("sse_event_too_large") => {
                "An upstream stream event exceeded EMP's event size limit.".to_owned()
            }
            Some("sse_invalid_json") => "The upstream stream contained invalid JSON.".to_owned(),
            Some("sse_non_object") => {
                "The upstream stream contained a non-object event.".to_owned()
            }
            Some("sse_non_bytes") => {
                "The upstream stream returned an invalid data type.".to_owned()
            }
            Some("unexpected_terminal_status") => {
                "The upstream completion event had an unexpected status.".to_owned()
            }
            _ => "EMP could not parse the upstream response stream.".to_owned(),
        },
        _ => "The upstream request failed before producing output.".to_owned(),
    }
}
