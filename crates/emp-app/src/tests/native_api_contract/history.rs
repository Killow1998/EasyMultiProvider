//! External checkpoints are decoded at the native request boundary only.
use super::support::*;
use super::*;

#[test]
fn external_compaction_switches_to_native_without_forwarding_emp_ciphertext() {
    let summary = OneShotUpstream::start(json!({"id":"summary","object":"chat.completion",
        "choices":[{"index":0,"message":{"role":"assistant","content":"retained summary"},"finish_reason":"stop"}]}));
    let (_external_home, external) = configured_server(&summary.base_url());
    let compact = post(
        &external,
        "/v1/responses/compact",
        &serde_json::to_vec(&json!({
            "model":"demo/model", "input":"earlier conversation"
        }))
        .unwrap(),
        &[&session_header(&external)],
    );
    assert!(compact.starts_with("HTTP/1.1 200"), "{compact}");
    let compact: Value = serde_json::from_slice(response_parts(&compact).1).unwrap();
    let checkpoint = compact["output"][0].clone();
    assert!(
        checkpoint["encrypted_content"]
            .as_str()
            .unwrap()
            .starts_with("emp1:")
    );
    external.shutdown().unwrap();

    let upstream = NativeUpstream::start(1);
    let (_native_home, native) = native_alias_server(&upstream.base_url());
    let opaque = json!({"type":"reasoning", "id":"rs_native", "encrypted_content":"opaque-native-ciphertext"});
    let call =
        json!({"type":"function_call", "call_id":"call_fixture", "name":"read", "arguments":"{}"});
    let result =
        json!({"type":"function_call_output", "call_id":"call_fixture", "output":"file contents"});
    let wire = post(
        &native,
        "/v1/responses",
        &serde_json::to_vec(&json!({
            "model":"native/alias", "input":[checkpoint, opaque, call, result], "stream":false
        }))
        .unwrap(),
        &[
            &session_header(&native),
            "Authorization: Bearer fixture-only",
        ],
    );
    assert!(wire.starts_with("HTTP/1.1 200"), "{wire}");
    let observed = upstream.next();
    assert!(!observed.body.to_string().contains("emp1:"));
    assert!(
        observed.body["input"][0]
            .to_string()
            .contains("retained summary")
    );
    assert_eq!(observed.body["input"][1], opaque);
    assert_eq!(observed.body["input"][2], call);
    assert_eq!(observed.body["input"][3], result);
    native.shutdown().unwrap();
}

#[test]
fn failed_legacy_checkpoint_returns_targeted_repair_advice_without_changing_history() {
    let (root, server) = native_alias_server("http://127.0.0.1:9/v1");
    let saved = root.path().join("codex/sessions/preserved.jsonl");
    std::fs::create_dir_all(saved.parent().unwrap()).unwrap();
    std::fs::write(
        &saved,
        b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"unrelated\"}}\n",
    )
    .unwrap();
    let original = std::fs::read(&saved).unwrap();
    let cookie = session_header(&server);
    for stream in [false, true] {
        let body = serde_json::to_vec(&json!({"model":"native/alias","stream":stream,
            "input":[{"type":"compaction","encrypted_content":"emp1:!"}]}))
        .unwrap();
        let response = post_stream(
            &server,
            "/v1/responses",
            &body,
            &[&cookie, "thread-id: 01234567-89ab-cdef-0123-456789abcdef"],
        );
        assert!(
            response.starts_with(if stream {
                "HTTP/1.1 200"
            } else {
                "HTTP/1.1 409"
            }),
            "{response}"
        );
        assert!(
            response
                .contains("EMP migrate-history --thread-id 01234567-89ab-cdef-0123-456789abcdef"),
            "{response}"
        );
        assert!(response.contains("EMP 对话历史处理失败"), "{response}");
    }
    assert_eq!(std::fs::read(&saved).unwrap(), original);
    server.shutdown().unwrap();
}
