use super::realtime_contract::{RealtimeUpstream, UpstreamResponse, app_server_for};
use super::*;
use crate::api::realtime::sideband::{MAX_REALTIME_SIDEBAND_MESSAGE_BYTES, prepare_sideband};

fn server_fixture(with_native_auth: bool) -> (TempDir, TempDir, RealtimeUpstream, ServerHandle) {
    let fixture = tempfile::tempdir().expect("fixture directory");
    let native_auth = canonical_root(&fixture).join("codex/auth.json");
    std::fs::create_dir_all(native_auth.parent().unwrap()).expect("native auth directory");
    if with_native_auth {
        std::fs::write(
            &native_auth,
            br#"{"tokens":{"access_token":"native-secret","account_id":"acct-native"}}"#,
        )
        .expect("write native auth");
    }
    let upstream = RealtimeUpstream::start(UpstreamResponse {
        status: 404,
        reason: "Not Found",
        content_type: "application/json",
        location: None,
        body: b"",
    });
    let (app_directory, server) = app_server_for(&upstream, &native_auth);
    (fixture, app_directory, upstream, server)
}

fn upgrade_headers(cookie: &str) -> Vec<String> {
    vec![
        cookie.to_owned(),
        "Upgrade: websocket".to_owned(),
        "Connection: keep-alive, UpGrAdE".to_owned(),
        "Sec-WebSocket-Version: 13".to_owned(),
        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==".to_owned(),
    ]
}

fn request_with_owned_headers(server: &ServerHandle, path: &str, headers: &[String]) -> String {
    let headers = headers.iter().map(String::as_str).collect::<Vec<_>>();
    request(server, path, &headers)
}

fn status_and_body(wire: &str) -> (u16, Value) {
    let separator = wire.find("\r\n\r\n").expect("HTTP separator");
    let status = wire[..separator]
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse::<u16>().ok())
        .expect("HTTP status");
    let body = serde_json::from_str(&wire[separator + 4..]).expect("JSON error body");
    (status, body)
}

#[test]
fn sideband_rejects_caller_before_call_id_or_upstream() {
    let (_fixture, _app_directory, upstream, server) = server_fixture(true);
    let headers = vec![
        "Authorization: Bearer invalid".to_owned(),
        "Upgrade: websocket".to_owned(),
        "Connection: Upgrade".to_owned(),
        "Sec-WebSocket-Version: 13".to_owned(),
        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==".to_owned(),
    ];
    let (status, payload) = status_and_body(&request_with_owned_headers(
        &server,
        "/v1/live/not-a-call-id",
        &headers,
    ));
    assert_eq!(status, 401);
    assert_eq!(payload["error"]["code"], "realtime_caller_unauthorized");
    assert!(upstream.no_request());
    server.shutdown().expect("shutdown server");
}

#[test]
fn sideband_validates_call_id_and_upgrade_before_admission() {
    let (_fixture, _app_directory, upstream, server) = server_fixture(true);
    let cookie = session_cookie_header(&server);
    let headers = upgrade_headers(&cookie);
    let (status, payload) = status_and_body(&request_with_owned_headers(
        &server,
        "/v1/live/not-a-call-id",
        &headers,
    ));
    assert_eq!(status, 400);
    assert_eq!(payload["error"]["code"], "realtime_invalid_call_id");
    assert_eq!(server.state.connection_admission.active_websockets(), 0);

    let headers = vec![cookie.clone()];
    let (status, payload) = status_and_body(&request_with_owned_headers(
        &server,
        "/v1/live/rtc_valid_call",
        &headers,
    ));
    assert_eq!(status, 400);
    assert_eq!(payload["error"]["message"], "invalid websocket upgrade");
    assert_eq!(server.state.connection_admission.active_websockets(), 0);

    let mut headers = upgrade_headers(&cookie);
    headers.retain(|header| !header.starts_with("Sec-WebSocket-Key:"));
    headers.push("Sec-WebSocket-Key: not-base64".to_owned());
    let (status, payload) = status_and_body(&request_with_owned_headers(
        &server,
        "/v1/live/rtc_valid_call",
        &headers,
    ));
    assert_eq!(status, 400);
    assert_eq!(payload["error"]["message"], "invalid Sec-WebSocket-Key");
    assert_eq!(server.state.connection_admission.active_websockets(), 0);
    assert!(upstream.no_request());
    server.shutdown().expect("shutdown server");
}

#[test]
fn sideband_capacity_is_reserved_after_upgrade_validation_before_native_connect() {
    let (_fixture, _app_directory, upstream, server) = server_fixture(false);
    let permits = (0..224)
        .map(|_| {
            server
                .state
                .connection_admission
                .acquire_websocket()
                .expect("fill adaptive websocket pool")
        })
        .collect::<Vec<_>>();
    assert_eq!(server.state.connection_admission.active_websockets(), 224);
    let cookie = session_cookie_header(&server);
    let headers = upgrade_headers(&cookie);
    let wire = request_with_owned_headers(&server, "/v1/live/rtc_capacity_test", &headers);
    let (status, payload) = status_and_body(&wire);
    assert_eq!(status, 503);
    assert_eq!(payload["error"]["code"], "realtime_capacity_unavailable");
    assert!(wire.lines().any(|line| line == "Retry-After: 2"));
    assert!(
        request(
            &server,
            "/api/request-limits",
            &[&session_cookie_header(&server)]
        )
        .starts_with("HTTP/1.1 200 OK\r\n")
    );
    assert!(upstream.no_request());
    drop(permits);
    assert_eq!(server.state.connection_admission.active_websockets(), 0);
    server.shutdown().expect("shutdown server");
}

#[test]
fn sideband_target_uses_native_credentials_allowlisted_headers_proxy_and_message_cap() {
    let (_fixture, _app_directory, upstream, server) = server_fixture(true);
    let cookie = session_cookie_header(&server);
    let raw = format!(
        "GET /v1/live/rtc_voice_proxy HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{}\r\nAuthorization: Bearer caller-secret\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nOpenAI-Alpha: quicksilver=v2\r\nSession-Id: session-voice\r\nX-Ignored-Secret: do-not-forward\r\n\r\n",
        server.local_addr().port(),
        cookie
    );
    let request = parse_request(&raw).expect("parse fixture upgrade");
    let prepared = prepare_sideband(
        request,
        "rtc_voice_proxy",
        &server.state,
        crate::util::system_now(),
    )
    .expect("prepare sideband connection");
    assert_eq!(prepared.call_id, "rtc_voice_proxy");
    assert_eq!(prepared.url, "wss://api.openai.com/v1/live/rtc_voice_proxy");
    assert_eq!(prepared.accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    assert_eq!(
        prepared.max_message_bytes,
        MAX_REALTIME_SIDEBAND_MESSAGE_BYTES
    );
    assert_eq!(prepared.headers["Authorization"], "Bearer native-secret");
    assert_eq!(prepared.headers["chatgpt-account-id"], "acct-native");
    assert_eq!(prepared.headers["OpenAI-Alpha"], "quicksilver=v2");
    assert_eq!(prepared.headers["session-id"], "session-voice");
    assert!(!prepared.headers.contains_key("X-Ignored-Secret"));
    assert!(
        !prepared
            .headers
            .values()
            .any(|value| value == "Bearer caller-secret")
    );
    // The proxy comes only from HttpClient's selected route; target construction
    // does not log or otherwise expose it.
    assert_eq!(
        prepared.proxy,
        server
            .state
            .backend
            .transport
            .client
            .websocket_proxy_for(&prepared.url)
            .unwrap()
    );
    assert_eq!(server.state.connection_admission.active_websockets(), 1);
    drop(prepared);
    assert_eq!(server.state.connection_admission.active_websockets(), 0);
    assert!(upstream.no_request());
    server.shutdown().expect("shutdown server");
}

#[test]
fn sideband_without_native_subscription_is_rejected_after_caller_auth() {
    let (_fixture, _app_directory, upstream, server) = server_fixture(false);
    let cookie = session_cookie_header(&server);
    let headers = upgrade_headers(&cookie);
    let (status, payload) = status_and_body(&request_with_owned_headers(
        &server,
        "/v1/live/rtc_missing_native",
        &headers,
    ));
    assert_eq!(status, 401);
    assert_eq!(payload["error"]["code"], "native_subscription_unavailable");
    assert_eq!(server.state.connection_admission.active_websockets(), 0);
    assert!(upstream.no_request());
    server.shutdown().expect("shutdown server");
}
