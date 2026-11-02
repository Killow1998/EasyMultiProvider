use std::time::Duration;

use emp_transport::{
    FailureClass, FailurePhase, HttpFailureInput, NetworkFailureKind, UpstreamFailure,
    external_backoff_delay, external_http_retry_allowed, http_failure, network_failure,
    protocol_fallback_allowed, public_failure_message, retry_allowed, status_error_class,
};

#[test]
fn statuses_and_proxy_evidence_keep_502_and_503_distinct() {
    assert_eq!(status_error_class(Some(502)), FailureClass::Upstream5xx);
    let origin = http_failure(HttpFailureInput {
        status: 502,
        detail: "temporarily unavailable",
        upstream_code: None,
        proxy_evidence: false,
        retry_after_seconds: None,
    });
    assert_eq!(
        (origin.status, origin.error_class),
        (502, FailureClass::Upstream5xx)
    );
    let proxy = http_failure(HttpFailureInput {
        status: 502,
        detail: "gateway unavailable",
        upstream_code: None,
        proxy_evidence: true,
        retry_after_seconds: None,
    });
    assert_eq!(
        (proxy.status, proxy.error_class),
        (503, FailureClass::ProxyUnavailable)
    );
    assert_eq!(proxy.failure_reason.as_deref(), Some("proxy_unavailable"));
}

#[test]
fn network_failures_have_stable_content_free_classes() {
    let dns = network_failure(NetworkFailureKind::Dns, FailurePhase::Connect, false);
    let tls = network_failure(NetworkFailureKind::Tls, FailurePhase::Connect, false);
    let proxy = network_failure(
        NetworkFailureKind::ConnectionRefused,
        FailurePhase::Connect,
        true,
    );
    assert_eq!(
        (dns.status, dns.error_class),
        (503, FailureClass::DnsFailure)
    );
    assert_eq!(
        (tls.status, tls.error_class),
        (502, FailureClass::TlsFailure)
    );
    assert_eq!(
        (proxy.status, proxy.error_class),
        (503, FailureClass::ProxyUnavailable)
    );
    assert!(
        !serde_json::to_string(&[dns, tls, proxy])
            .unwrap()
            .contains("secret")
    );
}

#[test]
fn external_retries_allow_bounded_rate_limit_pressure() {
    let transport = UpstreamFailure::new(FailureClass::ConnectTimeout, 504, FailurePhase::Connect);
    assert!(retry_allowed(&transport, 0, true, false, false));
    assert!(!retry_allowed(&transport, 1, true, false, false));
    assert!(!retry_allowed(&transport, 0, true, true, false));
    assert!(!retry_allowed(&transport, 0, true, false, true));

    let rate_limit = http_failure(HttpFailureInput {
        status: 429,
        detail: "busy",
        upstream_code: None,
        proxy_evidence: false,
        retry_after_seconds: Some(5),
    });
    assert!(external_http_retry_allowed(
        &rate_limit,
        0,
        false,
        false,
        false
    ));
    assert!(external_http_retry_allowed(
        &rate_limit,
        1,
        false,
        false,
        false
    ));
    assert!(!external_http_retry_allowed(
        &rate_limit,
        2,
        false,
        false,
        false
    ));
    // Free routes never retry rate limits.
    assert!(!external_http_retry_allowed(
        &rate_limit,
        0,
        false,
        false,
        true
    ));
    // Retry-After up to the 300 s ceiling is honored.
    let long_rate_limit = http_failure(HttpFailureInput {
        retry_after_seconds: Some(300),
        ..HttpFailureInput {
            status: 429,
            detail: "busy",
            upstream_code: None,
            proxy_evidence: false,
            retry_after_seconds: None,
        }
    });
    assert!(external_http_retry_allowed(
        &long_rate_limit,
        0,
        false,
        false,
        false
    ));
    let excessive_rate_limit = http_failure(HttpFailureInput {
        retry_after_seconds: Some(301),
        ..HttpFailureInput {
            status: 429,
            detail: "busy",
            upstream_code: None,
            proxy_evidence: false,
            retry_after_seconds: None,
        }
    });
    assert!(!external_http_retry_allowed(
        &excessive_rate_limit,
        0,
        false,
        false,
        false
    ));
    // Capacity 429s are retryable; quota exhaustion is terminal.
    let capacity = http_failure(HttpFailureInput {
        status: 429,
        detail: "upstream overloaded",
        upstream_code: None,
        proxy_evidence: false,
        retry_after_seconds: None,
    });
    assert!(external_http_retry_allowed(
        &capacity, 0, false, false, false
    ));
    let quota = http_failure(HttpFailureInput {
        status: 429,
        detail: "insufficient credits",
        upstream_code: None,
        proxy_evidence: false,
        retry_after_seconds: None,
    });
    assert!(!external_http_retry_allowed(&quota, 0, false, false, false));
    // Output or tool activity always blocks the retry.
    assert!(!external_http_retry_allowed(
        &rate_limit,
        0,
        true,
        false,
        false
    ));
    assert!(!external_http_retry_allowed(
        &rate_limit,
        0,
        false,
        true,
        false
    ));
}

#[test]
fn external_backoff_grows_with_jitter() {
    let first = external_backoff_delay(0);
    let second = external_backoff_delay(1);
    let third = external_backoff_delay(2);
    let capped = external_backoff_delay(8);
    // Base 500 ms, 1 s, 2 s; jitter only shrinks (0–25%), so bounds hold.
    assert!(first >= Duration::from_millis(375) && first <= Duration::from_millis(500));
    assert!(second >= Duration::from_millis(750) && second <= Duration::from_millis(1_000));
    assert!(third >= Duration::from_millis(1_500) && third <= Duration::from_millis(2_000));
    assert!(capped >= Duration::from_millis(6_000) && capped <= Duration::from_millis(8_000));
}

#[test]
fn protocol_fallback_requires_explicit_rejection_before_output() {
    for status in [404, 405, 415, 501] {
        assert!(protocol_fallback_allowed(status, false, false));
    }
    for status in [408, 429, 500, 502, 503, 504] {
        assert!(!protocol_fallback_allowed(status, false, false));
    }
    assert!(!protocol_fallback_allowed(404, true, false));
    assert!(!protocol_fallback_allowed(404, false, true));
}

#[test]
fn public_messages_never_echo_upstream_content() {
    assert_eq!(
        public_failure_message(FailureClass::Upstream5xx, Some("private secret"), 502),
        "The upstream service returned HTTP 502."
    );
    assert_eq!(
        public_failure_message(
            FailureClass::MalformedTerminal,
            Some("sse_invalid_json"),
            502,
        ),
        "The upstream stream contained invalid JSON."
    );
}

#[test]
fn busy_request_errors_keep_their_status_and_do_not_add_gateway_retries() {
    for (status, detail) in [
        (
            400,
            "Concurrency limit exceeded for account, please retry later",
        ),
        (409, "Too many pending requests, please retry later"),
        (422, "当前并发请求过多，请稍后重试"),
    ] {
        let failure = http_failure(HttpFailureInput {
            status,
            detail,
            upstream_code: None,
            proxy_evidence: false,
            retry_after_seconds: Some(7),
        });
        assert_eq!(failure.status, status);
        assert_eq!(failure.error_class.as_str(), "upstream_capacity");
        assert_eq!(failure.failure_reason.as_deref(), Some("upstream_capacity"));
        assert_eq!(failure.retry_after_seconds, Some(7));
        assert!(
            public_failure_message(
                failure.error_class,
                failure.failure_reason.as_deref(),
                status
            )
            .contains("you can retry your request")
        );
        assert!(!external_http_retry_allowed(
            &failure, 0, false, false, false
        ));
    }
    for (status, detail) in [
        (
            400,
            "Invalid request parameters: concurrency_limit must be an integer",
        ),
        (400, "Insufficient credits, please retry later"),
        (400, "Monthly usage limit reached, try again later"),
        (400, "Maximum context length exceeded, please retry later"),
        (401, "Temporarily unavailable, invalid API key"),
        (422, "额度不足，超过并发上限，请稍后重试"),
    ] {
        let failure = http_failure(HttpFailureInput {
            status,
            detail,
            upstream_code: None,
            proxy_evidence: false,
            retry_after_seconds: None,
        });
        assert_ne!(
            failure.failure_reason.as_deref(),
            Some("upstream_capacity"),
            "{detail}"
        );
        assert!(
            !public_failure_message(
                failure.error_class,
                failure.failure_reason.as_deref(),
                status
            )
            .contains("you can retry your request"),
            "{detail}"
        );
    }
}

#[test]
fn body_size_limits_are_distinct_from_context_and_rate_limits() {
    for (status, detail) in [
        (
            400,
            "Request body is 40.1 MB, over the 32 MB limit for one request (33554432 bytes)",
        ),
        (413, "Payload too large"),
        (400, "Request exceeds the maximum allowed number of bytes"),
    ] {
        let failure = http_failure(HttpFailureInput {
            status,
            detail,
            upstream_code: None,
            proxy_evidence: false,
            retry_after_seconds: None,
        });
        assert_eq!(failure.status, status);
        assert_eq!(failure.error_class.as_str(), "request_too_large");
        assert_eq!(failure.failure_reason.as_deref(), Some("request_too_large"));
        let message = public_failure_message(
            failure.error_class,
            failure.failure_reason.as_deref(),
            status,
        );
        assert!(message.contains("byte-size limit"), "{message}");
        assert!(!external_http_retry_allowed(
            &failure, 0, false, false, false
        ));
    }
    for (status, detail, reason) in [
        (
            400,
            "Maximum context length exceeded",
            "context_length_exceeded",
        ),
        (
            413,
            "Input too large: maximum context length is 100000 tokens",
            "context_length_exceeded",
        ),
        (
            429,
            "Request too large for tokens per minute (TPM)",
            "rate_limited",
        ),
        (400, "Input too large", "upstream_rejected"),
    ] {
        let failure = http_failure(HttpFailureInput {
            status,
            detail,
            upstream_code: None,
            proxy_evidence: false,
            retry_after_seconds: None,
        });
        assert_ne!(
            failure.error_class.as_str(),
            "request_too_large",
            "{detail}"
        );
        assert_eq!(failure.failure_reason.as_deref(), Some(reason), "{detail}");
    }
}
