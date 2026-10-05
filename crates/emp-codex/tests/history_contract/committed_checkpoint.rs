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
