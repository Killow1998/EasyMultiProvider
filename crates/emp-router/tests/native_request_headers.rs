//! Native request credentials and context: which caller headers are allowed
//! to reach a native upstream, how account/forward credentials take
//! precedence, and which spoofable security headers are dropped.

use emp_router::native_request::{NativeAuth, request_headers};
use std::collections::BTreeMap;

fn selected() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("Authorization".to_owned(), "Bearer selected".to_owned()),
        ("chatgpt-account-id".to_owned(), "selected-owner".to_owned()),
    ])
}

fn hostile_incoming() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("AUTHORIZATION".to_owned(), "Bearer caller".to_owned()),
        ("CHATGPT-ACCOUNT-ID".to_owned(), "caller-owner".to_owned()),
        ("Cookie".to_owned(), "private-cookie".to_owned()),
        ("Proxy-Authorization".to_owned(), "private-proxy".to_owned()),
        ("User-Agent".to_owned(), "untrusted-agent".to_owned()),
        ("Accept".to_owned(), "application/untrusted".to_owned()),
        (
            "Content-Type".to_owned(),
            "application/untrusted".to_owned(),
        ),
        ("X-EMP-Request-ID".to_owned(), "0123456789abcdef".to_owned()),
        ("Thread-ID".to_owned(), "thread-fixture".to_owned()),
        (
            "x-openai-subagent".to_owned(),
            "subagent-fixture".to_owned(),
        ),
        ("openai-beta".to_owned(), "beta-fixture".to_owned()),
    ])
}

#[test]
fn forward_mode_sends_caller_credentials_but_never_cookies_or_spoofed_basics() {
    let headers =
        request_headers(NativeAuth::Forward, &hostile_incoming(), false).expect("forward headers");
    assert_eq!(headers["Authorization"], "Bearer caller");
    assert_eq!(headers["chatgpt-account-id"], "caller-owner");
    assert_eq!(headers["Content-Type"], "application/json");
    assert_eq!(headers["Accept"], "application/json");
    assert!(!headers.contains_key("Cookie"));
    assert!(!headers.contains_key("Proxy-Authorization"));
    // EMP identifies itself; the caller's User-Agent/Accept/Content-Type
    // never reach the upstream, while context headers do.
    assert_eq!(
        headers["User-Agent"],
        format!("EMP/{}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(headers["thread-id"], "thread-fixture");
    assert_eq!(headers["x-openai-subagent"], "subagent-fixture");
    assert_eq!(headers["OpenAI-Beta"], "beta-fixture");
}

#[test]
fn account_mode_replaces_caller_credentials_with_the_selected_account() {
    let headers = request_headers(NativeAuth::Account(&selected()), &hostile_incoming(), true)
        .expect("account headers");
    assert_eq!(headers["Authorization"], "Bearer selected");
    assert_eq!(headers["chatgpt-account-id"], "selected-owner");
    assert_eq!(headers["thread-id"], "thread-fixture");
    assert_eq!(headers["Accept"], "text/event-stream");
    assert!(!headers.contains_key("Cookie"));
}

#[test]
fn an_unparseable_request_id_is_never_forwarded() {
    for id in [
        "0123456789abcde",  // too short
        "0123456789ABCDEF", // uppercase outside the hex allowlist
        "0123456789abcdeg", // not hex
        "",                 // empty
    ] {
        let incoming = BTreeMap::from([
            ("Authorization".to_owned(), "Bearer caller".to_owned()),
            ("X-EMP-Request-ID".to_owned(), id.to_owned()),
        ]);
        let headers = request_headers(NativeAuth::Forward, &incoming, false).expect("headers");
        assert!(!headers.contains_key("X-EMP-Request-ID"), "{id}");
    }
    let incoming = BTreeMap::from([
        ("Authorization".to_owned(), "Bearer caller".to_owned()),
        ("X-EMP-Request-ID".to_owned(), "0123456789abcdef".to_owned()),
    ]);
    let headers = request_headers(NativeAuth::Forward, &incoming, false).expect("headers");
    assert_eq!(headers["X-EMP-Request-ID"], "0123456789abcdef");
}

#[test]
fn missing_forward_credentials_fail_with_401_and_a_stable_message() {
    for incoming in [
        BTreeMap::new(),
        BTreeMap::from([("authorization".to_owned(), String::new())]),
    ] {
        let error = request_headers(NativeAuth::Forward, &incoming, false)
            .expect_err("forward without credentials must fail");
        assert_eq!(error.status(), 401);
        assert!(
            error.to_string().contains("Authorization"),
            "the error names the missing header"
        );
    }
}

#[test]
fn implicit_native_without_a_login_falls_back_to_caller_auth() {
    // Implicit(None): no readable native login -> the caller's bearer is the
    // fallback so the request can still proceed in forward-like mode.
    let headers = request_headers(NativeAuth::Implicit(None), &hostile_incoming(), false)
        .expect("implicit fallback headers");
    assert_eq!(headers["Authorization"], "Bearer caller");
    assert_eq!(headers["chatgpt-account-id"], "caller-owner");

    // Implicit(Some(login)): a resolved native login replaces the caller's
    // credentials entirely; an empty login adds none and never falls back.
    let empty = BTreeMap::new();
    let headers = request_headers(
        NativeAuth::Implicit(Some(&empty)),
        &hostile_incoming(),
        false,
    )
    .expect("resolved native login headers");
    assert!(!headers.contains_key("Authorization"));
    assert!(!headers.contains_key("chatgpt-account-id"));
}
