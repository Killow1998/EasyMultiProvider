//! Shared durable-rollout fixtures for same-input fast/full comparisons.
use super::*;

pub(super) fn write_rollout(directory: &std::path::Path, records: &[Value]) -> std::path::PathBuf {
    let rollout = directory.join("rollout.jsonl");
    std::fs::write(
        &rollout,
        records
            .iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>(),
    )
    .unwrap();
    rollout
}

pub(super) fn jsonl_bytes(records: &[Value]) -> Vec<u8> {
    records
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>()
        .into_bytes()
}

pub(super) fn write_records(path: &std::path::Path, records: &[Value]) -> std::path::PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, jsonl_bytes(records)).unwrap();
    path.to_path_buf()
}

pub(super) fn write_zstd(path: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let compressed = zstd::stream::encode_all(bytes, 0).unwrap();
    std::fs::write(path, compressed).unwrap();
    path.to_path_buf()
}

pub(super) fn session_meta(thread: &str, mode: &str) -> Value {
    json!({"ordinal":0,"type":"session_meta","payload":{"id":thread,"history_mode":mode}})
}

pub(super) fn state_database(directory: &std::path::Path, rollout: &std::path::Path) {
    state_database_with_mode(directory, rollout, "paginated");
}

pub(super) fn state_database_with_mode(
    directory: &std::path::Path,
    rollout: &std::path::Path,
    history_mode: &str,
) {
    let database = directory.join("state_5.sqlite");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, model TEXT)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO threads VALUES (?1, ?2, ?3, ?4)",
            params![THREAD, rollout.to_str().unwrap(), history_mode, MODEL],
        )
        .unwrap();
}

pub(super) fn anchor() -> HistoryAnchor {
    HistoryAnchor {
        thread_id: Some(THREAD.to_owned()),
        turn_id: Some(TURN.to_owned()),
        ..HistoryAnchor::default()
    }
}

pub(super) fn visible_text(snapshot: &HistorySnapshot) -> String {
    snapshot
        .items
        .iter()
        .map(|item| match &item.content {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
            Value::Object(fields) => fields
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("|")
}

pub(super) fn build_home(records: &[Value]) -> tempfile::TempDir {
    let directory = tempdir().unwrap();
    let rollout = write_rollout(directory.path(), records);
    state_database(directory.path(), &rollout);
    directory
}

pub(super) fn checkpoint(text: &str) -> Value {
    json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number":1,
        "replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}]}})
}

pub(super) fn user(ordinal: u64, text: &str) -> Value {
    json!({"ordinal":ordinal,"type":"event_msg","payload":{"type":"user_message","message":text}})
}

pub(super) fn started(ordinal: u64, turn: &str) -> Value {
    json!({"ordinal":ordinal,"type":"event_msg","payload":{"type":"task_started","turn_id":turn}})
}

pub(super) fn completed(ordinal: u64, turn: &str) -> Value {
    json!({"ordinal":ordinal,"type":"event_msg","payload":{"type":"task_complete","turn_id":turn}})
}

pub(super) fn meta() -> Value {
    json!({"ordinal":0,"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated"}})
}

// --- Critical 1: the walker must never read past the frozen captured end. ---

pub(super) fn assert_fast_matches_full(records: &[Value], name: &str) {
    // Same durable rollout, two strategies: the hidden `force_full` seam
    // replays byte-identical input without the reverse-base fast path, so
    // any divergence is a real fast-path bug rather than a fixture rewrite.
    let directory = build_home(records);
    let fast = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history_with_strategy(&anchor(), false);
    let full = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history_with_strategy(&anchor(), true);
    match (&fast, &full) {
        (Ok(fast), Ok(full)) => {
            assert_eq!(fast.items, full.items, "{name}: items diverge");
            assert_eq!(
                fast.source_model, full.source_model,
                "{name}: models diverge"
            );
        }
        (Err(fast), Err(full)) => assert_eq!(
            fast.reason(),
            full.reason(),
            "{name}: error reasons diverge"
        ),
        (fast, full) => panic!("{name}: fast={fast:?} full={full:?} disagree"),
    }
}

pub(super) fn assert_both_reject(records: &[Value], reason: &str) {
    let directory = build_home(records);
    for force_full in [false, true] {
        let error = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history_with_strategy(&anchor(), force_full)
            .unwrap_err();
        assert_eq!(error.reason(), reason, "force_full={force_full}");
    }
}

pub(super) fn checkpoint_with_metadata(metadata: Value) -> Value {
    json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number":1,
        "replacement_history":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"a"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"b"}]}
        ],
        "replacement_history_metadata":metadata}})
}
