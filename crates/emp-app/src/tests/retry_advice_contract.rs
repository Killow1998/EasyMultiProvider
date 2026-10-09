//! Retry advice must survive the public HTTP and WebSocket boundaries.
use super::*;

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
            server.shutdown().unwrap();
        }
    }
}
