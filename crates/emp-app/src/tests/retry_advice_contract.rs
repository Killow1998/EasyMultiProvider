//! Retry advice must survive the public HTTP and WebSocket boundaries.
use super::*;

#[test]
fn model_not_found_is_terminal_across_sources_and_client_transports() {
    for status in [400, 403, 404] {
        for protocol in [
            "native",
            "responses",
            "chat_completions",
            "anthropic_messages",
            "auto",
        ] {
            for delivery in ["http", "sse", "websocket"] {
                // Native WebSocket requires an upstream WebSocket, not this HTTP fixture.
                if protocol == "native" && delivery == "websocket" {
                    continue;
                }
                let upstream = OneShotUpstream::start_wire(
                    status,
                    "application/json",
                    None,
                    vec![br#"{"error":{"code":"model_not_found","message":"private upstream detail"}}"#.to_vec()],
                );
                let (_directory, server) = if protocol == "native" {
                    let (directory, server, _) = auto_review_contract::make_review_server(
                        &upstream.base_url(),
                        &[],
                        Some("regular"),
                    );
                    (directory, server)
                } else {
                    configured_protocol_server(&upstream.base_url(), protocol, "api_key")
                };
                let session = session_header(&server);
                let model = if protocol == "native" {
                    "codex-auto-review"
                } else {
                    "demo/model"
                };
                let mut body = json!({"model":model,"input":"fixture","stream":delivery != "http"});
                let failure = if delivery == "websocket" {
                    let headers = BTreeMap::from([(
                        "x-emp-session".to_owned(),
                        session.trim_start_matches("X-EMP-Session: ").to_owned(),
                    )]);
                    let mut socket = emp_transport::ClientWebSocket::connect(
                        &format!("ws://{}/v1/responses", server.local_addr()),
                        &headers,
                        Duration::from_secs(5),
                    )
                    .unwrap();
                    body["type"] = json!("response.create");
                    socket.send_json(&body).unwrap();
                    let event = std::iter::repeat_with(|| {
                        socket.receive_json().unwrap().expect("WebSocket event")
                    })
                    .take(4)
                    .find(|event| event["type"] == "error")
                    .expect("WebSocket failure");
                    assert_eq!(event["status"], status, "{protocol}: {event}");
                    event
                } else {
                    let wire = post(
                        &server,
                        "/v1/responses",
                        &serde_json::to_vec(&body).unwrap(),
                        &[&session],
                    );
                    assert_eq!(
                        wire.split_whitespace().nth(1),
                        Some(status.to_string().as_str()),
                        "{protocol}/{delivery}: {wire}"
                    );
                    serde_json::from_str::<Value>(wire.split("\r\n\r\n").nth(1).unwrap()).unwrap()
                };
                assert_eq!(
                    failure["error"]["code"], "model_not_found",
                    "{protocol}/{delivery}: {failure}"
                );
                assert!(
                    failure["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("model")
                );
                assert!(!failure.to_string().contains("private upstream detail"));
                upstream.observed();
                let report = request(&server, "/api/calls", &[&session]);
                let report: Value =
                    serde_json::from_str(report.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                let call = &report["records"][0];
                assert_eq!(call["attempts"], 1, "{protocol}/{delivery}: {call}");
                assert_eq!(call["retries"], json!([]));
                server.shutdown().unwrap();
            }
        }
    }
}

#[test]
fn upstream_retry_advice_reaches_http_and_websocket_clients() {
    for status in [429, 503] {
        for websocket in [false, true] {
            // Above EMP's local retry ceiling: expose the advice to Codex
            // immediately without waiting or replaying the inference.
            let upstream = OneShotUpstream::start_wire(
                status,
                "application/json",
                Some(301),
                vec![br#"{"error":{"message":"private upstream detail"}}"#.to_vec()],
            );
            let (_directory, server) = configured_server(&upstream.base_url());
            let session = session_header(&server);
            let body = json!({"model":"demo/model","input":"fixture","stream":true});
            if websocket {
                let headers = BTreeMap::from([(
                    "x-emp-session".to_owned(),
                    session.trim_start_matches("X-EMP-Session: ").to_owned(),
                )]);
                let mut socket = emp_transport::ClientWebSocket::connect(
                    &format!("ws://{}/v1/responses", server.local_addr()),
                    &headers,
                    Duration::from_secs(5),
                )
                .unwrap();
                let mut body = body;
                body["type"] = json!("response.create");
                socket.send_json(&body).unwrap();
                let mut failure = None;
                for _ in 0..4 {
                    let event = socket.receive_json().unwrap().expect("WebSocket event");
                    if event["type"] == "error" {
                        failure = Some(event);
                        break;
                    }
                }
                let failure = failure.expect("upstream failure");
                assert_eq!(failure["status"], status);
                assert_eq!(failure["error"]["retry_after_seconds"], 301);
                assert_eq!(failure["error"]["headers"]["Retry-After"], "301");
                assert!(!failure.to_string().contains("private upstream detail"));
            } else {
                let wire = post(
                    &server,
                    "/v1/responses",
                    &serde_json::to_vec(&body).unwrap(),
                    &[&session],
                );
                assert_eq!(
                    wire.split_whitespace().nth(1),
                    Some(status.to_string().as_str())
                );
                assert!(wire.contains("Retry-After: 301\r\n"), "{wire}");
                assert!(!wire.contains("private upstream detail"));
            }
            assert_eq!(upstream.observed().2["model"], "upstream-model");
            let report = request(&server, "/api/calls", &[&session]);
            let report: Value =
                serde_json::from_str(report.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            let call = &report["records"][0];
            assert_eq!(call["retry_after_seconds"], 301);
            assert_eq!(call["attempts"], 1);
            assert_eq!(call["retries"], json!([]));
            assert_eq!(call["state"], "failed");
            assert!(!report.to_string().contains("private upstream detail"));
            server.shutdown().unwrap();
        }
    }
}
