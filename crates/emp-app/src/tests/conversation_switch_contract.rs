use super::{OneShotUpstream, canonical_root, post, session_header};
use crate::lifecycle::ServerHandle;
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr};

// The Python oracle used to compute the expected upstream `input` array. The
// Rust projection is now asserted directly: visible items pass through in
// order, hidden reasoning never crosses the destination dialect boundary.
fn expected_projected_input() -> Value {
    json!([
        {"type":"message","role":"user","content":[{"type":"input_text","text":"route-visible-user"}]},
        {"type":"function_call","call_id":"call_route_fixture","name":"route_fixture_tool","arguments":"{}"},
        {"type":"function_call_output","call_id":"call_route_fixture","output":"route-visible-tool-result"},
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":"route-visible-final"}]}
    ])
}

fn expected_sidechat_input() -> Value {
    // Sidechat history continuity: the parent's compaction summary becomes a
    // user-visible reference, the inherited checkpoint stays hidden, and the
    // side question follows.
    json!([
        {"type":"message","role":"user","content":[{"type":"input_text","text":"parent reference"}]},
        {"type":"message","role":"user","content":"Side conversation boundary: reference only"},
        {"type":"message","role":"user","content":"side question"}
    ])
}

#[test]
fn external_to_native_http_switch_preserves_visible_tool_history_and_headers() {
    let upstream = OneShotUpstream::start(json!({
        "id":"switch-native","object":"response","status":"completed",
        "model":"native-upstream","output":[]
    }));
    let directory = tempfile::tempdir().expect("native switch tempdir");
    let root = canonical_root(&directory);
    let config_path = root.join("config.json");
    let native_auth_path = root.join("codex/auth.json");
    std::fs::create_dir_all(native_auth_path.parent().expect("native auth parent"))
        .expect("create native auth parent");
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&json!({
            "providers":[{
                "id":"current-login","name":"Current login",
                "base_url":upstream.base_url(),"protocol":"responses","auth_mode":"forward"
            }],
            "models":[{
                "id":"native/alias","provider":"current-login",
                "upstream_id":"native-upstream","enabled":true
            }]
        }))
        .expect("encode native config"),
    )
    .expect("write native config");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "missing-test-codex",
        native_auth_path,
    )
    .expect("start native destination server");

    // Plain-history/tool-pair fixture with hidden reasoning that must not
    // cross the destination dialect boundary.
    let body = json!({
        "model":"native/alias","stream":false,
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"route-visible-user"}]},
            {"type":"reasoning","content":[{"type":"reasoning_text","text":"route-private-reasoning"}]},
            {"type":"function_call","call_id":"call_route_fixture","name":"route_fixture_tool","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_route_fixture","output":"route-visible-tool-result"},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"route-visible-final"}]}
        ]
    });
    let expected_input = expected_projected_input();
    let response = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&body).expect("request JSON"),
        &[
            &session_header(&server),
            "Authorization: Bearer caller",
            "thread-id: switch-thread",
            "x-openai-subagent: switch-subagent",
        ],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let (path, headers, observed) = upstream.observed();
    assert_eq!(path, "/v1/responses");
    assert_eq!(observed["model"], "native-upstream");
    assert_eq!(observed["input"], expected_input);
    assert_eq!(
        observed["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "reasoning")
            .count(),
        0
    );
    assert!(observed.to_string().contains("route-visible-tool-result"));
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer caller")
    );
    assert_eq!(
        headers.get("thread-id").map(String::as_str),
        Some("switch-thread")
    );
    assert_eq!(
        headers.get("x-openai-subagent").map(String::as_str),
        Some("switch-subagent")
    );
    server.shutdown().expect("shutdown native destination");
}

#[test]
fn short_to_long_native_history_fork_uses_exact_checkpoint_and_never_reads_later_parent_tail() {
    let upstream = OneShotUpstream::start(json!({
        "id":"switch-long","object":"response","status":"completed",
        "model":"long-model","output":[]
    }));
    let directory = tempfile::tempdir().expect("history switch tempdir");
    let root = canonical_root(&directory);
    let codex_home = root.join("codex");
    std::fs::create_dir_all(&codex_home).expect("create Codex home");
    let config_path = root.join("config.json");
    let thread_id = "01a00000-0000-7000-8000-000000000001";
    let child_id = "01a00000-0000-7000-8000-000000000002";
    let rollout_path = codex_home.join("parent.jsonl");
    let records = [
        json!({"type":"session_meta","payload":{"id":thread_id,"history_mode":"legacy"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"before"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":"parent reference"}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"before"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"compact"}}),
        json!({"type":"compacted","payload":{"message":"","replacement_history":[{"type":"compaction","encrypted_content":"inherited-checkpoint"}]}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"compact"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"later"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":"DO NOT LEAK LATER PARENT"}}),
        json!({"type":"compacted","payload":{"message":"","replacement_history":[{"type":"compaction","encrypted_content":"newer-checkpoint"}]}}),
    ];
    std::fs::write(
        &rollout_path,
        records
            .iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>(),
    )
    .expect("write parent rollout");
    let database = rusqlite::Connection::open(codex_home.join("state_5.sqlite"))
        .expect("open Codex state database");
    database
        .execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, model TEXT)",
            [],
        )
        .expect("create threads table");
    database
        .execute(
            "INSERT INTO threads VALUES (?1, ?2, 'legacy', 'gpt-native')",
            rusqlite::params![thread_id, rollout_path.to_str().unwrap()],
        )
        .expect("register parent rollout");
    drop(database);

    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&json!({
            "providers":[{
                "id":"long","name":"Long context","base_url":upstream.base_url(),
                "protocol":"responses","auth_mode":"api_key","api_key":"destination-secret"
            }],
            "models":[{
                "id":"long/model","provider":"long","upstream_id":"long-model",
                "enabled":true,"context_window":100000,"output_limit":4096,
                "capability_sources":{"context_window":{"source":"manual","confidence":1.0}}
            }]
        }))
        .expect("encode long-context config"),
    )
    .expect("write long-context config");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "missing-test-codex",
        codex_home.join("auth.json"),
    )
    .expect("start long-context destination server");
    let turn_metadata = json!({
        "thread_id":child_id,"turn_id":"child-turn","forked_from_thread_id":thread_id
    })
    .to_string();
    let body = json!({
        "model":"long/model",
        "input":[
            {"type":"compaction","encrypted_content":"inherited-checkpoint"},
            {"type":"message","role":"user","content":"Side conversation boundary: reference only"},
            {"type":"message","role":"user","content":"side question"}
        ],
        "client_metadata":{"x-codex-turn-metadata":turn_metadata}
    });
    let expected_input = expected_sidechat_input();
    let response = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&body).expect("sidechat request JSON"),
        &[
            &session_header(&server),
            &format!("thread-id: {child_id}"),
            &format!("x-codex-turn-metadata: {turn_metadata}"),
        ],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let (path, headers, observed) = upstream.observed();
    assert_eq!(path, "/v1/responses");
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer destination-secret")
    );
    assert!(headers.get("x-emp-request-id").is_some_and(|request_id| {
        request_id.len() == 16
            && request_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }));
    for internal_header in ["cookie", "thread-id", "x-codex-turn-metadata"] {
        assert!(
            !headers.contains_key(internal_header),
            "external destination received internal header {internal_header}: {headers:?}"
        );
    }
    let rendered = observed["input"].to_string();
    assert_eq!(observed["input"], expected_input);
    assert!(rendered.contains("parent reference"), "{rendered}");
    assert!(rendered.contains("Side conversation boundary: reference only"));
    assert!(rendered.contains("side question"));
    assert!(!rendered.contains("DO NOT LEAK LATER PARENT"), "{rendered}");
    assert!(!rendered.contains("inherited-checkpoint"), "{rendered}");
    assert!(!rendered.contains("newer-checkpoint"), "{rendered}");
    assert_eq!(observed["model"], "long-model");
    server
        .shutdown()
        .expect("shutdown long-context destination");
}
