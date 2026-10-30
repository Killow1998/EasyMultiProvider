//! Authenticated API forwarding uses instance keys, never upstream credentials.
use super::*;

#[test]
fn gateway_key_survives_login_changes_and_cannot_open_management_endpoints() {
    let (directory, server) = test_server();
    let key = server.state.backend.gateway_key.clone();
    let auth_path = &server.state.backend.accounts.native_auth_path;
    let original = br#"{"auth_mode":"apikey","OPENAI_API_KEY":"native-fixture"}"#;
    emp_state::atomic_write_private_state(auth_path, original).unwrap();
    let authorization = format!("Authorization: Bearer {key}");
    let forwarded = "Host: 192.168.1.20:14200";
    assert!(
        request(&server, "/v1/models", &[&authorization, forwarded]).starts_with("HTTP/1.1 200")
    );
    assert!(request(&server, "/v1/models", &[forwarded]).starts_with("HTTP/1.1 403"));
    assert!(
        request(
            &server,
            "/v1/models",
            &[&authorization, forwarded, "Origin: https://foreign.invalid"]
        )
        .starts_with("HTTP/1.1 403")
    );
    for path in ["/api/gateway", "/api/config", "/api/support-report"] {
        assert!(request(&server, path, &[&authorization]).starts_with("HTTP/1.1 401"));
        assert!(request(&server, path, &[&authorization, forwarded]).starts_with("HTTP/1.1 403"));
    }
    let response = request(&server, "/api/gateway", &[&session_header(&server)]);
    assert!(response.contains("Cache-Control: no-store"));
    let connection: Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(connection["api_key"], key);
    assert_eq!(std::fs::read(auth_path).unwrap(), original);
    emp_state::atomic_write_private_state(
        auth_path,
        br#"{"tokens":{"access_token":"another-subscription","account_id":"fixture"}}"#,
    )
    .unwrap();
    assert!(
        request(&server, "/v1/models", &[&authorization, forwarded]).starts_with("HTTP/1.1 200")
    );
    assert_eq!(
        emp_state::gateway_key::load_or_create(&directory.path().join("state/gateway-key"))
            .unwrap(),
        key
    );
    assert!(!request(&server, "/", &[]).contains(&key));
    assert!(!request(&server, "/api/config", &[&session_header(&server)]).contains(&key));
    server.shutdown().unwrap();
}

#[test]
fn jev_decisions_forward_the_body_and_provider_key_without_entering_the_model_catalog() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let endpoint = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let (send, receive) = mpsc::channel();
    let worker = thread::spawn(move || {
        for status in [200, 429] {
            let (mut stream, _) = listener.accept().unwrap();
            let incoming = receive_upstream_request(&mut stream);
            send.send(incoming).unwrap();
            let body = if status == 200 {
                br#"{"model":"typesafe/jev-1.14","answers":{"effort":"low"},"usage":{"input_tokens":5}}"#.as_slice()
            } else {
                br#"{"error":{"message":"fixture quota","code":429}}"#.as_slice()
            };
            stream
                .write_all(&crate::http::response::response(
                    &format!("HTTP/1.1 {status} {}", status_text(status)),
                    "application/json",
                    body,
                    if status == 429 {
                        &[("Retry-After", "7")]
                    } else {
                        &[]
                    },
                ))
                .unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.json");
    std::fs::write(&config, serde_json::to_vec(&json!({
        "providers":[{"id":"openrouter","base_url":endpoint,"auth_mode":"api_key","api_key":"provider-fixture"}],
        "models":[{"id":"openrouter/jev","provider":"openrouter","upstream_id":"typesafe/jev-1.14","enabled":true}]
    })).unwrap()).unwrap();
    let server =
        ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config).unwrap();
    let key = &server.state.backend.gateway_key;
    let authorization = format!("Authorization: Bearer {key}");
    let headers = [
        &authorization[..],
        "Host: localhost:14200",
        "X-EMP-Provider: openrouter",
    ];
    let body = br#"{"model":"typesafe/jev-1.14","input":"public fixture","questions":[{"name":"effort","choices":["low","xhigh"]}]}"#;
    assert!(post(&server, "/v1/decisions", body, &[]).starts_with("HTTP/1.1 401"));
    assert!(
        post(
            &server,
            "/v1/decisions",
            br#"{"model":"chat-model"}"#,
            &headers
        )
        .starts_with("HTTP/1.1 400")
    );
    assert!(post(&server, "/v1/decisions", body, &[&authorization]).starts_with("HTTP/1.1 400"));
    for expected_status in [200, 429] {
        let response = post(&server, "/v1/decisions", body, &headers);
        assert!(
            response.starts_with(&format!("HTTP/1.1 {expected_status}")),
            "{response}"
        );
        assert!(!response.contains(key));
        if expected_status == 429 {
            assert!(response.contains("Retry-After: 7\r\n"));
        }
        let (path, upstream_headers, payload) =
            receive.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(path, "/api/alpha/decisions");
        assert_eq!(payload, serde_json::from_slice::<Value>(body).unwrap());
        assert_eq!(upstream_headers["authorization"], "Bearer provider-fixture");
        assert!(!upstream_headers.values().any(|value| value.contains(key)));
    }
    assert!(!request(&server, "/v1/models", &headers).contains("typesafe/jev"));
    server.shutdown().unwrap();
    worker.join().unwrap();
}
