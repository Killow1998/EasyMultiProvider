use emp_transport::{
    FailureClass, FailurePhase, HttpFailureInput, NetworkFailureKind, http_failure, network_failure,
    public_failure_message, status_error_class,
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
