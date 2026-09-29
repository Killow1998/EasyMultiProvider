use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use emp_history::HistoryAnchor;
use rusqlite::params;
use serde_json::json;
use std::io::{BufWriter, Write};

const PARENT_ID: &str = "11111111-1111-4111-8111-111111111111";
const CHILD_ID: &str = "22222222-2222-4222-8222-222222222222";
const GRANDCHILD_ID: &str = "33333333-3333-4333-8333-333333333333";

struct Fixture {
    _temporary: tempfile::TempDir,
    home: PathBuf,
    parent_path: PathBuf,
    parent_plain: Vec<u8>,
    child_path: Option<PathBuf>,
    child_plain: Option<Vec<u8>>,
}

fn append_record(bytes: &mut Vec<u8>, ordinal: u64, mut value: Map<String, Value>) {
    value.insert("ordinal".to_owned(), json!(ordinal));
    bytes.extend_from_slice(&serde_json::to_vec(&Value::Object(value)).unwrap());
    bytes.push(b'\n');
}

fn record(kind: &str, payload: Value) -> Map<String, Value> {
    Map::from_iter([
        ("type".to_owned(), json!(kind)),
        ("payload".to_owned(), payload),
    ])
}

fn fixture(compressed_parent: bool, with_child: bool, unfinished: bool) -> Fixture {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("codex");
    let session_dir = home.join("sessions/2026/09/29");
    fs::create_dir_all(&session_dir).unwrap();
    let parent_path = session_dir.join(if compressed_parent {
        format!("{PARENT_ID}.jsonl.zst")
    } else {
        format!("{PARENT_ID}.jsonl")
    });

    let emp_summary = "Portable EMP summary: keep the task plan";
    let emp_item = json!({
        "type":"compaction",
        "id":"cmp_emp_keep_id",
        "encrypted_content":format!("emp1:{}", URL_SAFE.encode(emp_summary.as_bytes()))
    });
    let native_item = json!({
        "type":"compaction",
        "id":"cmp_native_keep_id",
        "encrypted_content":"native-ciphertext-must-stay-opaque"
    });
    let user_item = json!({
        "type":"message",
        "id":"msg_parent_user",
        "role":"user",
        "content":[{"type":"input_text","text":"parent request"}]
    });
    let tool_call = json!({
        "type":"function_call",
        "id":"fc_parent_tool",
        "call_id":"call_parent_tool",
        "name":"collect_fact",
        "arguments":"{\"key\":\"value\"}"
    });
    let tool_output = json!({
        "type":"function_call_output",
        "id":"fo_parent_tool",
        "call_id":"call_parent_tool",
        "output":"collected"
    });
    let replacement = vec![
        user_item.clone(),
        tool_call.clone(),
        tool_output.clone(),
        emp_item.clone(),
        native_item.clone(),
    ];
    let replacement_metadata = vec![
        json!({"client_authored":true}),
        json!({"client_authored":false}),
        json!({"client_authored":false}),
        json!({"client_authored":false}),
        json!({"client_authored":false}),
    ];

    let mut parent_plain = Vec::new();
    append_record(
        &mut parent_plain,
        0,
        record(
            "session_meta",
            json!({"id":PARENT_ID,"history_mode":"paginated"}),
        ),
    );
    append_record(
        &mut parent_plain,
        1,
        record(
            "event_msg",
            json!({"type":"task_started","turn_id":"parent-turn"}),
        ),
    );
    for (ordinal, item, metadata) in [
        (2, user_item, json!({"client_authored":true})),
        (3, tool_call, json!({"client_authored":false})),
        (4, tool_output, json!({"client_authored":false})),
        (5, emp_item, json!({"client_authored":false})),
        (6, native_item, json!({"client_authored":false})),
    ] {
        let mut line = record("response_item", item);
        line.insert("metadata".to_owned(), metadata);
        append_record(&mut parent_plain, ordinal, line);
    }
    if !unfinished {
        append_record(
            &mut parent_plain,
            7,
            record(
                "event_msg",
                json!({"type":"task_complete","turn_id":"parent-turn"}),
            ),
        );
    }
    append_record(
        &mut parent_plain,
        8,
        record(
            "compacted",
            json!({
                "message":"old remote summary",
                "replacement_history":replacement,
                "replacement_history_metadata":replacement_metadata,
                "window_number":0,
                "first_window_id":"019b3f6e-0000-7000-8000-000000000001",
                "previous_window_id":null,
                "window_id":"019b3f6e-0000-7000-8000-000000000002",
                "compaction_response_id":"resp_old_compaction",
                "latest_token_usage_record":{"input_tokens":3},
                "retained_context":{"entries":[]},
                "guardian_history":null,
                "mcp_resource_origins":{"items":[]},
                "resume_metadata":{
                    "multi_agent_version":"v2",
                    "last_started_turn_id":"parent-turn",
                    "previous_turn_settings":{"model":"gpt-5","comp_hash":null,"realtime_active":false}
                }
            }),
        ),
    );
    append_record(
        &mut parent_plain,
        9,
        record(
            "world_state",
            json!({"full":true,"state":{"cwd":"/tmp","task":"retain"}}),
        ),
    );
    append_record(
        &mut parent_plain,
        10,
        record(
            "turn_context",
            json!({"turn_id":"parent-turn","model":"gpt-5","comp_hash":"model-hash","realtime_active":false}),
        ),
    );
    append_record(
        &mut parent_plain,
        11,
        record(
            "retained_context",
            json!({"type":"verified_answer","turn_id":"parent-turn","call_id":"ask-1","questions":[]}),
        ),
    );
    append_record(
        &mut parent_plain,
        12,
        record(
            "event_msg",
            json!({"type":"thread_settings_applied","thread_settings":{"model":"gpt-5"}}),
        ),
    );
    append_record(
        &mut parent_plain,
        13,
        record(
            "security_risk_score",
            json!({"risk_score":0.1,"reason":"synthetic fixture"}),
        ),
    );
    append_record(
        &mut parent_plain,
        14,
        record(
            "realtime_item",
            json!({"type":"audio","id":"realtime-fixture-item"}),
        ),
    );
    append_record(
        &mut parent_plain,
        15,
        record(
            "inter_agent_communication",
            json!({"from":"agent-a","to":"agent-b","message":"retain"}),
        ),
    );
    append_record(
        &mut parent_plain,
        16,
        record(
            "inter_agent_communication_metadata",
            json!({"channel":"fixture","sequence":1}),
        ),
    );
    append_record(
        &mut parent_plain,
        17,
        record(
            "token_usage_record",
            json!({"input_tokens":7,"output_tokens":2}),
        ),
    );
    if compressed_parent {
        let compressed = zstd::stream::encode_all(parent_plain.as_slice(), 3).unwrap();
        fs::write(&parent_path, compressed).unwrap();
    } else {
        fs::write(&parent_path, &parent_plain).unwrap();
    }

    let child_path = with_child.then(|| session_dir.join(format!("{CHILD_ID}.jsonl")));
    let child_plain = if with_child {
        let child_path = child_path.as_ref().unwrap();
        let mut child_plain = Vec::new();
        append_record(
            &mut child_plain,
            18,
            record(
                "session_meta",
                json!({
                    "id":CHILD_ID,
                    "history_mode":"paginated",
                    "history_base":{
                        "thread_id":PARENT_ID,
                        "end_ordinal_exclusive":18,
                        "end_byte_offset":parent_plain.len()
                    }
                }),
            ),
        );
        append_record(
            &mut child_plain,
            19,
            record(
                "event_msg",
                json!({"type":"task_started","turn_id":"child-turn"}),
            ),
        );
        append_record(
            &mut child_plain,
            20,
            record(
                "response_item",
                json!({"type":"message","id":"msg_child_user","role":"user","content":[{"type":"input_text","text":"child continuation"}]}),
            ),
        );
        append_record(
            &mut child_plain,
            21,
            record(
                "event_msg",
                json!({"type":"task_complete","turn_id":"child-turn"}),
            ),
        );
        fs::write(child_path, &child_plain).unwrap();
        Some(child_plain)
    } else {
        None
    };

    let database = Connection::open(home.join("state_1.sqlite")).unwrap();
    database
        .execute(
            "CREATE TABLE threads (id TEXT NOT NULL, rollout_path TEXT NOT NULL, history_mode TEXT)",
            [],
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO threads (id, rollout_path, history_mode) VALUES (?1, ?2, 'paginated')",
            params![PARENT_ID, parent_path.to_string_lossy()],
        )
        .unwrap();
    if let Some(child_path) = &child_path {
        database
            .execute(
                "INSERT INTO threads (id, rollout_path, history_mode) VALUES (?1, ?2, 'paginated')",
                params![CHILD_ID, child_path.to_string_lossy()],
            )
            .unwrap();
    }

    Fixture {
        _temporary: temporary,
        home,
        parent_path,
        parent_plain,
        child_path,
        child_plain,
    }
}

fn add_grandchild(fixture: &Fixture) -> (PathBuf, Vec<u8>) {
    let session_dir = fixture.parent_path.parent().unwrap();
    let child_plain = fixture.child_plain.as_ref().unwrap();
    let grandchild_path = session_dir.join(format!("{GRANDCHILD_ID}.jsonl"));
    let mut grandchild_plain = Vec::new();
    append_record(
        &mut grandchild_plain,
        22,
        record(
            "session_meta",
            json!({
                "id":GRANDCHILD_ID,
                "history_mode":"paginated",
                "history_base":{
                    "thread_id":CHILD_ID,
                    "end_ordinal_exclusive":22,
                    "end_byte_offset":child_plain.len()
                }
            }),
        ),
    );
    append_record(
        &mut grandchild_plain,
        23,
        record(
            "event_msg",
            json!({"type":"task_started","turn_id":"grandchild-turn"}),
        ),
    );
    append_record(
        &mut grandchild_plain,
        24,
        record(
            "response_item",
            json!({"type":"message","id":"msg_grandchild_user","role":"user","content":[{"type":"input_text","text":"grandchild continuation"}]}),
        ),
    );
    append_record(
        &mut grandchild_plain,
        25,
        record(
            "event_msg",
            json!({"type":"task_complete","turn_id":"grandchild-turn"}),
        ),
    );
    fs::write(&grandchild_path, &grandchild_plain).unwrap();
    let database = Connection::open(fixture.home.join("state_1.sqlite")).unwrap();
    database
        .execute(
            "INSERT INTO threads (id, rollout_path, history_mode) VALUES (?1, ?2, 'paginated')",
            params![GRANDCHILD_ID, grandchild_path.to_string_lossy()],
        )
        .unwrap();
    (grandchild_path, grandchild_plain)
}

fn rename_rollout_as_codex(home: &Path, path: &Path, thread_id: &str, timestamp: &str) -> PathBuf {
    let renamed = path
        .parent()
        .unwrap()
        .join(format!("rollout-{timestamp}-{thread_id}.jsonl"));
    fs::rename(path, &renamed).unwrap();
    Connection::open(home.join("state_1.sqlite"))
        .unwrap()
        .execute(
            "UPDATE threads SET rollout_path = ?1 WHERE id = ?2",
            params![renamed.to_string_lossy(), thread_id],
        )
        .unwrap();
    renamed
}

fn decoded(path: &Path) -> Vec<u8> {
    let bytes = fs::read(path).unwrap();
    if path.extension().and_then(|value| value.to_str()) == Some("zst") {
        zstd::stream::decode_all(bytes.as_slice()).unwrap()
    } else {
        bytes
    }
}

fn records(bytes: &[u8]) -> Vec<Value> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

fn latest_checkpoint(records: &[Value]) -> &Value {
    records
        .iter()
        .rev()
        .find(|record| record["type"] == "compacted")
        .expect("compacted checkpoint")
}

fn streamed_history<'a>(records: &'a [Value], checkpoint: &Value) -> Vec<&'a Value> {
    let checkpoint_ordinal = checkpoint["ordinal"].as_u64().unwrap();
    records
        .iter()
        .filter(|record| {
            record["type"] == "response_item"
                && record["ordinal"]
                    .as_u64()
                    .is_some_and(|ordinal| ordinal > checkpoint_ordinal)
        })
        .map(|record| &record["payload"])
        .collect()
}

fn streamed_metadata_count(records: &[Value], checkpoint: &Value) -> usize {
    let checkpoint_ordinal = checkpoint["ordinal"].as_u64().unwrap();
    records
        .iter()
        .filter(|record| {
            record["type"] == "response_item"
                && record["ordinal"]
                    .as_u64()
                    .is_some_and(|ordinal| ordinal > checkpoint_ordinal)
                && record["metadata"].is_object()
        })
        .count()
}

fn assert_canonical_replacement_suffix(records: &[Value], checkpoint: &Value) {
    let checkpoint_ordinal = checkpoint["ordinal"].as_u64().unwrap();
    let mut suffix_count = 0;
    for record in records.iter().filter(|record| {
        record["ordinal"]
            .as_u64()
            .is_some_and(|ordinal| ordinal > checkpoint_ordinal)
    }) {
        suffix_count += 1;
        assert!(
            record["timestamp"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        assert!(record["type"].as_str().is_some());
        assert!(record["payload"].is_object());
        if record["type"] == "response_item" {
            assert!(record["metadata"].is_object());
        }
    }
    assert!(suffix_count > 0);
}

fn resumed_snapshot(home: &Path, thread_id: &str) -> emp_history::HistorySnapshot {
    CodexHomeHistoryReader::new(home)
        .read_visible_history(&HistoryAnchor {
            thread_id: Some(thread_id.to_owned()),
            ..HistoryAnchor::default()
        })
        .unwrap()
}

fn make_parent_legacy(fixture: &mut Fixture) {
    let newline = fixture
        .parent_plain
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("session meta line");
    let mut session_meta: Value = serde_json::from_slice(&fixture.parent_plain[..newline]).unwrap();
    session_meta["payload"]
        .as_object_mut()
        .unwrap()
        .remove("history_mode");
    let mut legacy = serde_json::to_vec(&session_meta).unwrap();
    legacy.push(b'\n');
    legacy.extend_from_slice(&fixture.parent_plain[newline + 1..]);
    fs::write(&fixture.parent_path, &legacy).unwrap();
    fixture.parent_plain = legacy;
    Connection::open(fixture.home.join("state_1.sqlite"))
        .unwrap()
        .execute(
            "UPDATE threads SET history_mode = NULL WHERE id = ?1",
            [PARENT_ID],
        )
        .unwrap();
}

fn assert_retained_tail(records: &[Value], checkpoint: &Value) {
    let checkpoint_ordinal = checkpoint["ordinal"].as_u64().unwrap();
    for kind in [
        "world_state",
        "turn_context",
        "retained_context",
        "security_risk_score",
        "realtime_item",
        "inter_agent_communication",
        "inter_agent_communication_metadata",
    ] {
        assert!(
            records.iter().any(|record| {
                record["type"] == kind
                    && record["ordinal"]
                        .as_u64()
                        .is_some_and(|ordinal| ordinal > checkpoint_ordinal)
            }),
            "{kind} should remain after the replacement checkpoint"
        );
    }
    assert!(records.iter().any(|record| {
        record["type"] == "retained_context" && record["payload"]["call_id"] == "ask-1"
    }));
    assert!(records.iter().any(|record| {
        record["type"] == "security_risk_score" && record["payload"]["risk_score"] == 0.1
    }));
    assert!(records.iter().any(|record| {
        record["type"] == "realtime_item" && record["payload"]["id"] == "realtime-fixture-item"
    }));
    assert!(records.iter().any(|record| {
        record["type"] == "inter_agent_communication" && record["payload"]["message"] == "retain"
    }));
    assert!(records.iter().any(|record| {
        record["type"] == "inter_agent_communication_metadata"
            && record["payload"]["channel"] == "fixture"
    }));
    assert!(records.iter().any(|record| {
        record["type"] == "event_msg"
            && record["payload"]["type"] == "thread_settings_applied"
            && record["ordinal"].as_u64().unwrap() > checkpoint_ordinal
    }));
}

#[test]
fn repairs_zstd_parent_and_depth_two_paginated_descendants_without_losing_history() {
    let fixture = fixture(true, true, false);
    let (grandchild_path, grandchild_original) = add_grandchild(&fixture);
    let original_parent_file = fs::read(&fixture.parent_path).unwrap();
    let (writer_lock, report) = repair_before_native_restore(&fixture.home).unwrap();
    assert_eq!(report.threads_repaired, 3);
    assert_eq!(report.checkpoints_converted, 3);

    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.home.join(LOCK_DIRECTORY).join(COORDINATION_LOCK))
        .unwrap();
    assert!(matches!(
        lock_file.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));

    let repaired_parent = decoded(&fixture.parent_path);
    assert!(repaired_parent.starts_with(&fixture.parent_plain));
    let parent_records = records(&repaired_parent);
    let parent_checkpoint = latest_checkpoint(&parent_records);
    assert_canonical_replacement_suffix(&parent_records, parent_checkpoint);
    assert!(
        parent_checkpoint["payload"]["replacement_history"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let parent_history = streamed_history(&parent_records, parent_checkpoint);
    let portable = parent_history
        .iter()
        .find(|item| item["id"] == "cmp_emp_keep_id")
        .expect("EMP item id retained");
    assert_eq!(portable["type"], "message");
    assert_eq!(portable["role"], "user");
    assert_eq!(
        portable["content"][0]["text"],
        "Portable EMP summary: keep the task plan"
    );
    assert!(portable.get("encrypted_content").is_none());
    assert!(parent_history.iter().any(|item| {
        item["id"] == "cmp_native_keep_id"
            && item["encrypted_content"] == "native-ciphertext-must-stay-opaque"
    }));
    assert!(
        parent_history
            .iter()
            .any(|item| item["id"] == "fc_parent_tool")
    );
    assert!(
        parent_history
            .iter()
            .any(|item| item["id"] == "fo_parent_tool")
    );
    let tool_call = parent_history
        .iter()
        .find(|item| item["type"] == "function_call")
        .unwrap();
    let tool_output = parent_history
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    assert_eq!(tool_call["call_id"], tool_output["call_id"]);
    assert_eq!(
        streamed_metadata_count(&parent_records, parent_checkpoint),
        parent_history.len()
    );
    assert_eq!(parent_checkpoint["payload"]["window_number"], 1);
    assert_eq!(
        parent_checkpoint["payload"]["message"],
        "Portable EMP summary: keep the task plan"
    );
    assert_eq!(
        parent_checkpoint["payload"]["latest_token_usage_record"]["input_tokens"],
        7
    );
    assert_retained_tail(&parent_records, parent_checkpoint);
    let parent_snapshot = resumed_snapshot(&fixture.home, PARENT_ID);
    assert_eq!(parent_snapshot.thread_id, PARENT_ID);
    assert!(parent_snapshot.items.iter().any(|item| {
        item.item_id.as_deref() == Some("msg_parent_user")
            && item.content.to_string().contains("parent request")
    }));
    let parent_call = parent_snapshot
        .items
        .iter()
        .find(|item| item.item_id.as_deref() == Some("fc_parent_tool"))
        .unwrap();
    let parent_output = parent_snapshot
        .items
        .iter()
        .find(|item| item.item_id.as_deref() == Some("fo_parent_tool"))
        .unwrap();
    assert_eq!(parent_call.call_id, parent_output.call_id);
    assert!(
        parent_snapshot
            .items
            .iter()
            .any(|item| item.content.to_string().contains("Portable EMP summary"))
    );

    let child_path = fixture.child_path.as_ref().unwrap();
    let child_original = fixture.child_plain.as_ref().unwrap();
    let repaired_child = fs::read(child_path).unwrap();
    assert!(repaired_child.starts_with(child_original));
    let child_records = records(&repaired_child);
    let child_checkpoint = latest_checkpoint(&child_records);
    assert_canonical_replacement_suffix(&child_records, child_checkpoint);
    let child_history = streamed_history(&child_records, child_checkpoint);
    assert!(child_history.iter().any(|item| {
        item["id"] == "cmp_emp_keep_id"
            && item["type"] == "message"
            && item["content"][0]["text"] == "Portable EMP summary: keep the task plan"
    }));
    assert!(
        child_history
            .iter()
            .any(|item| item["id"] == "msg_child_user")
    );
    assert!(
        child_history
            .iter()
            .any(|item| item["id"] == "cmp_native_keep_id")
    );
    assert_retained_tail(&child_records, child_checkpoint);
    let child_snapshot = resumed_snapshot(&fixture.home, CHILD_ID);
    assert_eq!(child_snapshot.thread_id, CHILD_ID);
    assert!(
        child_snapshot
            .items
            .iter()
            .any(|item| item.item_id.as_deref() == Some("msg_child_user"))
    );
    assert!(
        child_snapshot
            .items
            .iter()
            .any(|item| item.content.to_string().contains("Portable EMP summary"))
    );

    let repaired_grandchild = fs::read(&grandchild_path).unwrap();
    assert!(repaired_grandchild.starts_with(&grandchild_original));
    let grandchild_records = records(&repaired_grandchild);
    let grandchild_checkpoint = latest_checkpoint(&grandchild_records);
    assert_canonical_replacement_suffix(&grandchild_records, grandchild_checkpoint);
    let grandchild_history = streamed_history(&grandchild_records, grandchild_checkpoint);
    assert!(grandchild_history.iter().any(|item| {
        item["id"] == "cmp_emp_keep_id"
            && item["type"] == "message"
            && item["content"][0]["text"] == "Portable EMP summary: keep the task plan"
    }));
    assert!(grandchild_history.iter().any(|item| {
        item["id"] == "cmp_native_keep_id"
            && item["encrypted_content"] == "native-ciphertext-must-stay-opaque"
    }));
    assert!(
        grandchild_history
            .iter()
            .any(|item| item["id"] == "msg_child_user")
    );
    assert!(
        grandchild_history
            .iter()
            .any(|item| item["id"] == "msg_grandchild_user")
    );
    let grandchild_call = grandchild_history
        .iter()
        .find(|item| item["type"] == "function_call")
        .unwrap();
    let grandchild_output = grandchild_history
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    assert_eq!(grandchild_call["call_id"], grandchild_output["call_id"]);
    assert_eq!(
        streamed_metadata_count(&grandchild_records, grandchild_checkpoint),
        grandchild_history.len()
    );
    assert_retained_tail(&grandchild_records, grandchild_checkpoint);
    let grandchild_snapshot = resumed_snapshot(&fixture.home, GRANDCHILD_ID);
    assert_eq!(grandchild_snapshot.thread_id, GRANDCHILD_ID);
    assert!(
        grandchild_snapshot
            .items
            .iter()
            .any(|item| item.item_id.as_deref() == Some("msg_grandchild_user"))
    );
    let grandchild_call = grandchild_snapshot
        .items
        .iter()
        .find(|item| item.item_id.as_deref() == Some("fc_parent_tool"))
        .unwrap();
    let grandchild_output = grandchild_snapshot
        .items
        .iter()
        .find(|item| item.item_id.as_deref() == Some("fo_parent_tool"))
        .unwrap();
    assert_eq!(grandchild_call.call_id, grandchild_output.call_id);

    let repair_root = fixture.home.join(REPAIR_DIRECTORY);
    let manifest_path = fs::read_dir(&repair_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("manifest.json");
    let manifest: RepairManifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest.status, "committed");
    let parent_entry = manifest
        .entries
        .iter()
        .find(|entry| entry.thread_id == PARENT_ID)
        .unwrap();
    assert_eq!(
        fs::read(fixture.home.join(&parent_entry.backup)).unwrap(),
        original_parent_file
    );

    let parent_after_first = fs::read(&fixture.parent_path).unwrap();
    let child_after_first = fs::read(child_path).unwrap();
    let grandchild_after_first = fs::read(&grandchild_path).unwrap();
    drop(writer_lock);
    drop(lock_file);
    let (second_lock, second_report) = repair_before_native_restore(&fixture.home).unwrap();
    assert_eq!(second_report.threads_repaired, 0);
    assert_eq!(second_report.checkpoints_converted, 0);
    assert_eq!(fs::read(&fixture.parent_path).unwrap(), parent_after_first);
    assert_eq!(fs::read(child_path).unwrap(), child_after_first);
    assert_eq!(fs::read(&grandchild_path).unwrap(), grandchild_after_first);
    drop(second_lock);
}

#[test]
fn repairs_paginated_lineage_with_codex_timestamped_rollout_names() {
    let mut fixture = fixture(false, true, false);
    let (grandchild_path, _) = add_grandchild(&fixture);
    fixture.parent_path = rename_rollout_as_codex(
        &fixture.home,
        &fixture.parent_path,
        PARENT_ID,
        "2026-09-29T09-04-13",
    );
    let child_path = fixture.child_path.as_ref().unwrap().clone();
    fixture.child_path = Some(rename_rollout_as_codex(
        &fixture.home,
        &child_path,
        CHILD_ID,
        "2026-09-29T09-04-13",
    ));
    let grandchild_path = rename_rollout_as_codex(
        &fixture.home,
        &grandchild_path,
        GRANDCHILD_ID,
        "2026-09-29T09-04-14",
    );

    let (lock, report) = repair_before_native_restore(&fixture.home).unwrap();
    assert_eq!(report.threads_repaired, 3);
    assert_eq!(report.checkpoints_converted, 3);
    for thread_id in [PARENT_ID, CHILD_ID, GRANDCHILD_ID] {
        let snapshot = resumed_snapshot(&fixture.home, thread_id);
        assert_eq!(snapshot.thread_id, thread_id);
        assert!(snapshot.items.iter().any(|item| {
            item.content
                .to_string()
                .contains("Portable EMP summary: keep the task plan")
        }));
    }
    assert!(grandchild_path.is_file());
    drop(lock);
}

#[test]
fn active_codex_writer_blocks_without_changing_rollout_bytes() {
    let fixture = fixture(false, false, false);
    let original = fs::read(&fixture.parent_path).unwrap();
    let lock_directory = fixture.home.join(LOCK_DIRECTORY);
    fs::create_dir_all(&lock_directory).unwrap();
    let writer = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_directory.join(format!("{PARENT_ID}.lock")))
        .unwrap();
    writer.lock().unwrap();
    let error = repair_before_native_restore(&fixture.home).unwrap_err();
    assert_eq!(error.reason(), "active_codex_writer");
    assert_eq!(fs::read(&fixture.parent_path).unwrap(), original);
}

#[test]
fn unfinished_turn_fails_closed_before_any_rollout_change() {
    let fixture = fixture(false, false, true);
    let original = fs::read(&fixture.parent_path).unwrap();
    let error = repair_before_native_restore(&fixture.home).unwrap_err();
    assert_eq!(error.reason(), "unfinished_turn_in_history");
    assert_eq!(fs::read(&fixture.parent_path).unwrap(), original);
    let repair_root = fixture.home.join(REPAIR_DIRECTORY);
    assert_eq!(fs::read_dir(repair_root).unwrap().count(), 0);
}

#[test]
fn rollback_event_fails_closed_before_any_lineage_rollout_change() {
    let fixture = fixture(false, true, false);
    let child_path = fixture.child_path.as_ref().unwrap();
    let original_child = fs::read(child_path).unwrap();
    let mut parent_with_rollback = fs::read(&fixture.parent_path).unwrap();
    append_record(
        &mut parent_with_rollback,
        18,
        record(
            "event_msg",
            json!({"type":"thread_rolled_back","turn_id":"parent-turn"}),
        ),
    );
    fs::write(&fixture.parent_path, &parent_with_rollback).unwrap();
    let error = repair_before_native_restore(&fixture.home).unwrap_err();
    assert_eq!(error.reason(), "rollback_or_interrupted_turn");
    assert_eq!(
        fs::read(&fixture.parent_path).unwrap(),
        parent_with_rollback
    );
    assert_eq!(fs::read(child_path).unwrap(), original_child);
    assert_eq!(
        fs::read_dir(fixture.home.join(REPAIR_DIRECTORY))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn rollback_in_child_fails_closed_before_parent_or_child_changes() {
    let fixture = fixture(false, true, false);
    let child_path = fixture.child_path.as_ref().unwrap();
    let original_parent = fs::read(&fixture.parent_path).unwrap();
    let mut child_with_rollback = fs::read(child_path).unwrap();
    append_record(
        &mut child_with_rollback,
        22,
        record(
            "event_msg",
            json!({"type":"turn_aborted","turn_id":"child-turn"}),
        ),
    );
    fs::write(child_path, &child_with_rollback).unwrap();
    let error = repair_before_native_restore(&fixture.home).unwrap_err();
    assert_eq!(error.reason(), "rollback_or_interrupted_turn");
    assert_eq!(fs::read(&fixture.parent_path).unwrap(), original_parent);
    assert_eq!(fs::read(child_path).unwrap(), child_with_rollback);
    assert_eq!(
        fs::read_dir(fixture.home.join(REPAIR_DIRECTORY))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn preparing_manifest_recovery_removes_only_verified_stage_and_keeps_backup() {
    let fixture = fixture(false, false, false);
    let target_relative = fixture
        .parent_path
        .strip_prefix(&fixture.home)
        .unwrap()
        .to_path_buf();
    let transaction_dir = fixture.home.join(REPAIR_DIRECTORY).join("repair-preparing");
    fs::create_dir_all(&transaction_dir).unwrap();
    let backup_relative = PathBuf::from(format!(
        "{REPAIR_DIRECTORY}/repair-preparing/{PARENT_ID}.original"
    ));
    let stage_path = fixture
        .parent_path
        .with_file_name(".emp-history-repair-preparing.stage");
    let stage_relative = stage_path
        .strip_prefix(&fixture.home)
        .unwrap()
        .to_path_buf();
    let before = fs::read(&fixture.parent_path).unwrap();
    let after = [before.as_slice(), b"staged"].concat();
    fs::write(fixture.home.join(&backup_relative), &before).unwrap();
    fs::write(&stage_path, &after).unwrap();
    let manifest_path = transaction_dir.join("manifest.json");
    let mut manifest = RepairManifest {
        format_version: 1,
        status: "preparing".to_owned(),
        entries: vec![RepairManifestEntry {
            thread_id: PARENT_ID.to_owned(),
            target: target_relative,
            backup: backup_relative.clone(),
            stage: stage_relative,
            before_sha256: sha256(&before),
            after_sha256: sha256(&after),
            before_bytes: before.len() as u64,
            after_bytes: after.len() as u64,
        }],
    };
    write_manifest(&manifest_path, &manifest).unwrap();
    recover_preparing(&fixture.home, &manifest_path, manifest).unwrap();
    manifest = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest.status, "aborted");
    assert!(!stage_path.exists());
    assert_eq!(
        fs::read(fixture.home.join(backup_relative)).unwrap(),
        before
    );
    assert_eq!(fs::read(&fixture.parent_path).unwrap(), before);
}

#[test]
fn prepared_recovery_accepts_already_published_target_without_missing_stage() {
    let fixture = fixture(false, false, false);
    let target_relative = fixture
        .parent_path
        .strip_prefix(&fixture.home)
        .unwrap()
        .to_path_buf();
    let transaction_dir = fixture.home.join(REPAIR_DIRECTORY).join("repair-prepared");
    fs::create_dir_all(&transaction_dir).unwrap();
    let backup_relative = PathBuf::from(format!(
        "{REPAIR_DIRECTORY}/repair-prepared/{PARENT_ID}.original"
    ));
    let stage_relative = PathBuf::from(format!(
        "sessions/2026/09/29/.emp-history-repair-prepared-{PARENT_ID}.stage"
    ));
    let before = fs::read(&fixture.parent_path).unwrap();
    let after = [before.as_slice(), b"published"].concat();
    fs::write(fixture.home.join(&backup_relative), &before).unwrap();
    fs::write(&fixture.parent_path, &after).unwrap();
    let manifest_path = transaction_dir.join("manifest.json");
    let manifest = RepairManifest {
        format_version: 1,
        status: "prepared".to_owned(),
        entries: vec![RepairManifestEntry {
            thread_id: PARENT_ID.to_owned(),
            target: target_relative,
            backup: backup_relative,
            stage: stage_relative,
            before_sha256: sha256(&before),
            after_sha256: sha256(&after),
            before_bytes: before.len() as u64,
            after_bytes: after.len() as u64,
        }],
    };
    write_manifest(&manifest_path, &manifest).unwrap();
    recover_pending(
        &fixture.home,
        &fixture.home.join(REPAIR_DIRECTORY),
        vec![(manifest_path.clone(), manifest)],
    )
    .unwrap();
    let committed: RepairManifest =
        serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    assert_eq!(committed.status, "committed");
    assert_eq!(fs::read(&fixture.parent_path).unwrap(), after);
}

#[test]
fn unrelated_unfinished_unknown_history_does_not_block_restore() {
    const UNRELATED_ID: &str = "44444444-4444-4444-8444-444444444444";
    let fixture = fixture(false, false, false);
    let session_dir = fixture.parent_path.parent().unwrap();
    let unrelated_path = session_dir.join(format!("{UNRELATED_ID}.jsonl"));
    let mut unrelated = Vec::new();
    append_record(
        &mut unrelated,
        0,
        record(
            "session_meta",
            json!({"id":UNRELATED_ID,"history_mode":"paginated"}),
        ),
    );
    append_record(
        &mut unrelated,
        1,
        record(
            "event_msg",
            json!({"type":"task_started","turn_id":"abandoned-turn"}),
        ),
    );
    for ordinal in 2..258 {
        append_record(
            &mut unrelated,
            ordinal,
            record(
                "future_codex_record",
                json!({"sequence":ordinal,"payload":"unrelated"}),
            ),
        );
    }
    fs::write(&unrelated_path, &unrelated).unwrap();
    Connection::open(fixture.home.join("state_1.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO threads (id, rollout_path, history_mode) VALUES (?1, ?2, 'paginated')",
            params![UNRELATED_ID, unrelated_path.to_string_lossy()],
        )
        .unwrap();

    let (lock, report) = repair_before_native_restore(&fixture.home).unwrap();
    assert_eq!(report.threads_repaired, 1);
    assert_eq!(report.checkpoints_converted, 1);
    assert_eq!(fs::read(&unrelated_path).unwrap(), unrelated);
    drop(lock);
}

#[test]
fn superseded_emp_marker_is_not_replayed_or_rewritten() {
    let fixture = fixture(false, false, false);
    let mut superseded = fixture.parent_plain.clone();
    append_record(
        &mut superseded,
        18,
        record(
            "compacted",
            json!({
                "message":"native replacement",
                "replacement_history":[{
                    "type":"message",
                    "id":"msg_native_replacement",
                    "role":"user",
                    "content":[{"type":"input_text","text":"already portable"}]
                }],
                "replacement_history_metadata":[{}],
                "window_number":2,
                "window_id":"019b3f6e-0000-7000-8000-000000000003"
            }),
        ),
    );
    fs::write(&fixture.parent_path, &superseded).unwrap();

    let (lock, report) = repair_before_native_restore(&fixture.home).unwrap();
    assert_eq!(report.threads_repaired, 0);
    assert_eq!(report.checkpoints_converted, 0);
    assert_eq!(fs::read(&fixture.parent_path).unwrap(), superseded);
    drop(lock);
}

#[test]
fn child_cutoff_before_parent_emp_marker_does_not_rewrite_child() {
    let fixture = fixture(false, true, false);
    let child_path = fixture.child_path.as_ref().unwrap();
    let original_child = fs::read(child_path).unwrap();
    let parent_cutoff = fixture
        .parent_plain
        .split_inclusive(|byte| *byte == b'\n')
        .take(5)
        .map(|line| line.len() as u64)
        .sum::<u64>();
    let meta_end = original_child
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("child session meta line");
    let mut meta: Value = serde_json::from_slice(&original_child[..meta_end]).unwrap();
    meta["payload"]["history_base"]["end_ordinal_exclusive"] = json!(5);
    meta["payload"]["history_base"]["end_byte_offset"] = json!(parent_cutoff);
    let mut bounded_child = serde_json::to_vec(&meta).unwrap();
    bounded_child.push(b'\n');
    bounded_child.extend_from_slice(&original_child[meta_end + 1..]);
    fs::write(child_path, &bounded_child).unwrap();

    let (lock, report) = repair_before_native_restore(&fixture.home).unwrap();
    assert_eq!(report.threads_repaired, 1);
    assert_eq!(report.checkpoints_converted, 1);
    assert_eq!(fs::read(child_path).unwrap(), bounded_child);
    drop(lock);
}

#[test]
fn affected_unknown_history_still_fails_closed() {
    let fixture = fixture(false, false, false);
    let original = fs::read(&fixture.parent_path).unwrap();
    let mut with_unknown_emp = original.clone();
    append_record(
        &mut with_unknown_emp,
        18,
        record(
            "future_codex_record",
            json!({"encrypted_content":format!("emp1:{}", URL_SAFE.encode(b"hidden"))}),
        ),
    );
    fs::write(&fixture.parent_path, &with_unknown_emp).unwrap();

    let error = repair_before_native_restore(&fixture.home).unwrap_err();
    assert_eq!(error.reason(), "unsupported_rollout_record");
    assert_eq!(fs::read(&fixture.parent_path).unwrap(), with_unknown_emp);
    assert_eq!(
        fs::read_dir(fixture.home.join(REPAIR_DIRECTORY))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn streams_and_resumes_rollout_larger_than_128_mib() {
    let mut fixture = fixture(false, false, false);
    make_parent_legacy(&mut fixture);
    let last_ordinal = records(&fixture.parent_plain).last().unwrap()["ordinal"]
        .as_u64()
        .unwrap();
    let payload = "x".repeat(1024 * 1024);
    let mut output = BufWriter::new(
        OpenOptions::new()
            .append(true)
            .open(&fixture.parent_path)
            .unwrap(),
    );
    for index in 0..129u64 {
        output
            .write_all(b"{\"type\":\"token_usage_record\",\"payload\":{\"padding\":\"")
            .unwrap();
        output.write_all(payload.as_bytes()).unwrap();
        output.write_all(b"\"},\"ordinal\":").unwrap();
        write!(output, "{}", last_ordinal + index + 1).unwrap();
        output.write_all(b"}\n").unwrap();
    }
    output.flush().unwrap();
    output.get_ref().sync_all().unwrap();
    drop(output);

    let original = file_fingerprint(&fixture.parent_path).unwrap();
    assert!(original.0 > 128 * 1024 * 1024);
    let (lock, report) = repair_before_native_restore(&fixture.home).unwrap();
    assert_eq!(report.threads_repaired, 1);
    assert_eq!(report.checkpoints_converted, 1);
    let snapshot = resumed_snapshot(&fixture.home, PARENT_ID);
    assert_eq!(snapshot.thread_id, PARENT_ID);
    assert!(
        snapshot
            .items
            .iter()
            .any(|item| item.content.to_string().contains("Portable EMP summary"))
    );

    let repair_root = fixture.home.join(REPAIR_DIRECTORY);
    let manifest_path = fs::read_dir(&repair_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("manifest.json");
    let manifest: RepairManifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let backup = fixture.home.join(&manifest.entries[0].backup);
    assert_eq!(file_fingerprint(&backup).unwrap(), original);
    drop(lock);
}
