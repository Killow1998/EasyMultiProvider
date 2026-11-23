use emp_transport::{FailureClass, UpstreamFailure};
use std::time::Duration;

pub fn protocol_fallback_allowed(
    status: u16,
    output_emitted: bool,
    terminal_event_observed: bool,
) -> bool {
    matches!(status, 404 | 405 | 415 | 501) && !output_emitted && !terminal_event_observed
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

/// At most two retries before output: 504, or non-free 429 capacity/rate limits.
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

/// Exponential backoff when Retry-After is absent, shortened by up to 25% jitter.
pub fn external_backoff_delay(attempt: usize) -> Duration {
    const BASE_MS: u64 = 500;
    const CAP_MS: u64 = 8_000;
    let mut nonce = [0u8; 1];
    let jitter_byte = getrandom::getrandom(&mut nonce)
        .ok()
        .map(|_| nonce[0])
        .unwrap_or(0);
    let shrink = u64::from(jitter_byte % 64);
    let base = BASE_MS.saturating_mul(1 << attempt.min(4)).min(CAP_MS);
    Duration::from_millis(base.saturating_sub(base / 4 * shrink / 64))
}
