use super::{OneShotUpstream, canonical_root, post, session_cookie_header};
use crate::lifecycle::ServerHandle;
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr};
use std::process::{Command, Stdio};

fn python_oracle(script: &str, input: &Value) -> Value {
    let python = std::env::var("EMP_PYTHON_INTEROP")
        .expect("EMP_PYTHON_INTEROP must point to the official Python environment");
    let root = std::env::var("EMP_PYTHON_ORACLE_ROOT")
        .expect("EMP_PYTHON_ORACLE_ROOT must point to the Python 0.11.10 oracle");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .env("EMP_PYTHON_ORACLE_ROOT", &root)
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start Python 0.11.10 projection oracle");
    serde_json::to_writer(child.stdin.take().expect("oracle stdin"), input)
        .expect("write oracle fixture");
    let output = child.wait_with_output().expect("wait for oracle");
    assert!(
        output.status.success(),
        "Python oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python oracle JSON")
}

fn projected_input(provider: &Value, body: &Value) -> Value {
    python_oracle(
        r#"
import json, os, sys
root = os.environ['EMP_PYTHON_ORACLE_ROOT']
sys.path.insert(0, root)
import easy_multi_provider
assert easy_multi_provider.__version__ == '0.11.10', easy_multi_provider.__version__
from easy_multi_provider.dialects import project_request
case=json.load(sys.stdin)
print(json.dumps(project_request(case['provider'],case['body'])['input'],ensure_ascii=False,sort_keys=True))
"#,
        &json!({ "provider":provider, "body":body }),
    )
}

fn sidechat_expected_input(body: &Value, headers: &Value) -> Value {
    python_oracle(
        r#"
import json, os, sys
root = os.environ['EMP_PYTHON_ORACLE_ROOT']
sys.path.insert(0, root)
import easy_multi_provider
assert easy_multi_provider.__version__ == '0.11.10', easy_multi_provider.__version__
from easy_multi_provider.codex_history import HistoryCursor, HistorySnapshot, VisibleItem
from easy_multi_provider.dialects import project_request
from easy_multi_provider.history_continuity import HistoryContinuityEngine
case=json.load(sys.stdin)
class Reader:
    def read_compaction_history(self, anchor, compaction):
        return HistorySnapshot(anchor=anchor, items=(
            VisibleItem(kind='user_message', content='parent reference'),
            VisibleItem(kind='compaction_marker', content=''),
        ), cursor=HistoryCursor(thread_id=anchor.thread_id),
            source='fixture', source_model='gpt-native')
provider={'protocol':'responses','auth_mode':'api_key'}
prepared=HistoryContinuityEngine(Reader()).prepare(
    {}, provider, {}, case['body']['model'], case['body'], case['headers'])
print(json.dumps(project_request(provider,prepared)['input'],ensure_ascii=False,sort_keys=True))
"#,
        &json!({"body":body,"headers":headers}),
    )
}

#[test]
fn external_to_native_http_switch_preserves_python_visible_tool_history_and_headers() {
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

    // This is the shared plain-history/tool-pair fixture from Python
    // test_v06_switch_matrix, including hidden reasoning that must not cross
    // the destination dialect boundary.
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
    let expected_input = projected_input(
        &json!({"protocol":"responses","auth_mode":"forward"}),
        &body,
    );
    let response = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&body).expect("request JSON"),
        &[
            &session_cookie_header(&server),
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
    let expected_input = sidechat_expected_input(&body, &json!({"thread-id":child_id}));
    let response = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&body).expect("sidechat request JSON"),
        &[
            &session_cookie_header(&server),
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
