use super::*;
use serde_json::json;

const THREAD: &str = "11111111-1111-4111-8111-111111111111";
const TURN: &str = "22222222-2222-4222-8222-222222222222";

fn start() -> Value {
    json!({"type":"event_msg","payload":{"type":"task_started","turn_id":TURN}})
}
fn tool(id: &str, from: u64, to: u64) -> Value {
    json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":THREAD,
        "turn_id":TURN,"started_at_ms":from,"completed_at_ms":to,
        "item":{"type":"CommandExecution","id":id,"command":"PRIVATE","aggregated_output":"PRIVATE"}}})
}
fn write(path: &Path, events: &[Value]) {
    let text = events
        .iter()
        .map(|event| format!("{event}\n"))
        .collect::<String>();
    std::fs::write(path, text).unwrap();
}

#[test]
fn parallel_intervals_are_unioned_deduplicated_and_refreshed_without_changing_sources() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("rollout.jsonl");
    let database = root.path().join("state_5.sqlite");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT);")
        .unwrap();
    connection
        .execute(
            "INSERT INTO threads VALUES(?,?)",
            [THREAD, path.to_str().unwrap()],
        )
        .unwrap();
    drop(connection);
    let mut events = vec![
        start(),
        tool("one", 1000, 4000),
        tool("two", 2000, 6000),
        tool("one", 1000, 4000),
    ];
    write(&path, &events);
    let source = std::fs::read(&path).unwrap();
    let reader = ToolTimings::default();
    let queries = vec![(THREAD.into(), TURN.into())];
    assert_eq!(reader.resolve(root.path(), &queries)[&queries[0]], 5000);
    assert_eq!(reader.resolve(root.path(), &queries)[&queries[0]], 5000);
    assert_eq!(std::fs::read(&path).unwrap(), source);
    events.push(tool("three", 7000, 8000));
    write(&path, &events);
    assert_eq!(reader.resolve(root.path(), &queries)[&queries[0]], 6000);
    let outside = tempfile::NamedTempFile::new().unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE threads SET rollout_path=?",
            [outside.path().to_str().unwrap()],
        )
        .unwrap();
    assert!(reader.resolve(root.path(), &queries).is_empty());
}

#[test]
fn missing_invalid_or_foreign_client_timing_is_unknown_instead_of_zero() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("rollout.jsonl");
    let mut missing = tool("one", 1000, 2000);
    missing["payload"]
        .as_object_mut()
        .unwrap()
        .remove("started_at_ms");
    let mut foreign = tool("one", 1000, 2000);
    foreign["payload"]["thread_id"] = json!("33333333-3333-4333-8333-333333333333");
    for events in [
        vec![tool("one", 1000, 2000)],
        vec![start(), missing],
        vec![start(), tool("one", 2000, 1000)],
        vec![start(), foreign],
    ] {
        write(&path, &events);
        assert!(
            read(&path, THREAD, path.metadata().unwrap().len())
                .unwrap()
                .is_empty()
        );
    }
    write(&path, &[start()]);
    assert_eq!(
        read(&path, THREAD, path.metadata().unwrap().len()).unwrap()[TURN],
        0
    );
}
