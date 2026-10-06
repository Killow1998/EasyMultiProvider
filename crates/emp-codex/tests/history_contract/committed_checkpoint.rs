use super::support::*;
use super::*;
use std::collections::BTreeMap;

fn failed_turn_checkpoint() -> Vec<Value> {
    vec![
        meta(),
        started(1, "checkpoint-owner"),
        user(2, "visible work before the committed checkpoint"),
        json!({"ordinal":3,"type":"response_item","payload":{
            "type":"reasoning","text":"private reasoning must stay hidden"}}),
        json!({"ordinal":4,"type":"compacted","payload":{
        "message":"","window_number":1,"replacement_history":[
            {"type":"compaction","encrypted_content":"fixture-committed-checkpoint"}
        ]}}),
        user(5, "failed suffix must not be replayed"),
        json!({"ordinal":6,"type":"event_msg","payload":{
            "type":"task_complete","turn_id":"checkpoint-owner","error":{"code":"failure"}}}),
        started(7, "later-success"),
        user(8, "later turn is already in the request tail"),
        completed(9, "later-success"),
        started(10, TURN),
    ]
}

#[test]
fn model_switch_recovers_exact_committed_checkpoint_from_a_failed_turn() {
    for compressed in [false, true] {
        let directory = tempdir().unwrap();
        let records = failed_turn_checkpoint();
        let path = if compressed {
            write_zstd(
                &directory.path().join("rollout.jsonl.zst"),
                &jsonl_bytes(&records),
            )
        } else {
            write_rollout(directory.path(), &records)
        };
        state_database(directory.path(), &path);
        let reader = CodexHomeHistoryReader::new(directory.path());
        // Ordinary resume still excludes failed turns. Only the exact
        // checkpoint supplied by the switching client authorizes its prefix.
        let resumed = reader.read_visible_history(&anchor()).unwrap();
        assert!(!visible_text(&resumed).contains("visible work before"));
        let request = json!({"model":"fixture/claude-opus","input":[
            {"type":"compaction","encrypted_content":"fixture-committed-checkpoint"},
            {"type":"message","role":"user","content":"later turn is already in the request tail"}
        ],"client_metadata":{"x-codex-turn-metadata":json!({"thread_id":THREAD,"turn_id":TURN}).to_string()}});
        let prepared = emp_history::prepare_owned(request, &BTreeMap::new(), false, &reader)
            .expect("the committed checkpoint survives its owner's later failure");
        let text = prepared.to_string();
        assert!(text.contains("visible work before the committed checkpoint"));
        assert_eq!(
            text.matches("later turn is already in the request tail")
                .count(),
            1
        );
        assert!(!text.contains("failed suffix"));
        assert!(!text.contains("private reasoning"));
        assert!(!text.contains("encrypted_content"));
    }
}

#[test]
fn failed_turn_recovery_requires_a_unique_matching_checkpoint() {
    for duplicate in [false, true] {
        let mut records = failed_turn_checkpoint();
        if duplicate {
            let mut copied = records[4].clone();
            copied["ordinal"] = json!(11);
            records.push(copied);
        }
        let directory = build_home(&records);
        let reader = CodexHomeHistoryReader::new(directory.path());
        let checkpoint = json!({"type":"compaction","encrypted_content":if duplicate {
            "fixture-committed-checkpoint"
        } else {
            "unmatched-checkpoint"
        }});
        let error = reader
            .read_compaction_history(&anchor(), checkpoint.as_object().unwrap())
            .unwrap_err();
        assert_eq!(
            error.reason(),
            if duplicate {
                "compaction_identity_ambiguous"
            } else {
                "compaction_identity_missing"
            }
        );
    }
}

#[test]
fn committed_checkpoint_recovery_does_not_reuse_an_older_summary() {
    let mut earlier = checkpoint("earlier successful summary");
    earlier["ordinal"] = json!(3);
    let mut records = vec![
        meta(),
        started(1, "earlier-success"),
        user(2, "earlier work"),
        earlier,
        completed(4, "earlier-success"),
    ];
    records.extend(
        failed_turn_checkpoint()
            .into_iter()
            .skip(1)
            .map(|mut record| {
                record["ordinal"] = json!(record["ordinal"].as_u64().unwrap() + 4);
                record
            }),
    );
    let directory = build_home(&records);
    let reader = CodexHomeHistoryReader::new(directory.path());
    let request = json!({"input":[
        {"type":"compaction","encrypted_content":"fixture-committed-checkpoint"},
        {"role":"user","content":"continue after checkpoint"}
    ],"client_metadata":{"x-codex-turn-metadata":json!({"thread_id":THREAD,"turn_id":TURN}).to_string()}});
    let prepared = emp_history::prepare_owned(request, &BTreeMap::new(), false, &reader).unwrap();
    let text = prepared.to_string();
    assert!(text.contains("earlier successful summary"), "{text}");
    assert!(
        text.contains("visible work before the committed checkpoint"),
        "{text}"
    );
    assert!(!text.contains("failed suffix"), "{text}");

    let unknown = json!({"type":"compaction","encrypted_content":"unknown-checkpoint"});
    assert_eq!(
        reader
            .read_compaction_history(&anchor(), unknown.as_object().unwrap())
            .unwrap_err()
            .reason(),
        "compaction_identity_missing"
    );
    let mut duplicate = records[8].clone();
    duplicate["ordinal"] = json!(15);
    records.push(duplicate);
    write_rollout(directory.path(), &records);
    let requested = json!({"type":"compaction","encrypted_content":"fixture-committed-checkpoint"});
    assert_eq!(
        reader
            .read_compaction_history(&anchor(), requested.as_object().unwrap())
            .unwrap_err()
            .reason(),
        "compaction_identity_ambiguous"
    );
}

#[test]
fn committed_child_checkpoint_recovery_preserves_inherited_history() {
    let directory = tempdir().unwrap();
    let parent_path = directory
        .path()
        .join(format!("sessions/2026/09/27/{PARENT}.jsonl"));
    write_records(
        &parent_path,
        &[
            session_meta(PARENT, "paginated"),
            started(1, "parent-success"),
            user(2, "inherited work before the fork"),
            completed(3, "parent-success"),
            user(4, "parent work after the fork must stay hidden"),
        ],
    );
    let mut records = failed_turn_checkpoint();
    records[0]["payload"]["history_base"] = json!({
        "thread_id":PARENT, "end_ordinal_exclusive":4
    });
    let path = write_rollout(directory.path(), &records);
    state_database(directory.path(), &path);
    let reader = CodexHomeHistoryReader::new(directory.path());
    let request = json!({"input":[
        {"type":"compaction","encrypted_content":"fixture-committed-checkpoint"},
        {"role":"user","content":"continue child"}
    ],"client_metadata":{"x-codex-turn-metadata":json!({"thread_id":THREAD,"turn_id":TURN}).to_string()}});
    let prepared = emp_history::prepare_owned(request, &BTreeMap::new(), false, &reader).unwrap();
    let text = prepared.to_string();
    assert!(text.contains("inherited work before the fork"), "{text}");
    assert!(
        text.contains("visible work before the committed checkpoint"),
        "{text}"
    );
    assert!(!text.contains("after the fork must stay hidden"), "{text}");
    assert!(!text.contains("failed suffix"), "{text}");
}

#[test]
fn inherited_committed_checkpoint_recovery_stops_at_the_fork() {
    let directory = tempdir().unwrap();
    let parent_path = directory
        .path()
        .join(format!("sessions/2026/09/27/{PARENT}.jsonl"));
    let mut parent_records = failed_turn_checkpoint();
    parent_records[0]["payload"]["id"] = json!(PARENT);
    let mut later = parent_records[4].clone();
    later["ordinal"] = json!(11);
    later["payload"]["replacement_history"][0]["encrypted_content"] = json!("post-fork-checkpoint");
    parent_records.push(later);
    write_records(&parent_path, &parent_records);
    let child_meta = json!({"ordinal":0,"type":"session_meta","payload":{
        "id":THREAD, "history_mode":"paginated",
        "history_base":{"thread_id":PARENT,"end_ordinal_exclusive":5}
    }});
    let path = write_rollout(
        directory.path(),
        &[
            child_meta,
            started(1, "child-success"),
            user(2, "child's visible tail"),
            completed(3, "child-success"),
            started(4, TURN),
        ],
    );
    state_database(directory.path(), &path);
    let reader = CodexHomeHistoryReader::new(directory.path());
    let request = json!({"input":[
        {"type":"compaction","encrypted_content":"fixture-committed-checkpoint"},
        {"role":"user","content":"child's visible tail"}
    ],"client_metadata":{"x-codex-turn-metadata":json!({"thread_id":THREAD,"turn_id":TURN}).to_string()}});
    let prepared = emp_history::prepare_owned(request, &BTreeMap::new(), false, &reader).unwrap();
    let text = prepared.to_string();
    assert!(
        text.contains("visible work before the committed checkpoint"),
        "{text}"
    );
    assert_eq!(text.matches("child's visible tail").count(), 1, "{text}");
    assert!(!text.contains("failed suffix"), "{text}");
    let unavailable = json!({"type":"compaction","encrypted_content":"post-fork-checkpoint"});
    assert_eq!(
        reader
            .read_compaction_history(&anchor(), unavailable.as_object().unwrap())
            .unwrap_err()
            .reason(),
        "compaction_identity_missing"
    );
}
