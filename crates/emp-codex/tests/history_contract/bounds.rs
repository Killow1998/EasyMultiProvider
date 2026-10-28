use super::support::*;
use super::*;

#[test]
fn fast_suffix_missing_ordinal_matches_full() {
    let records = [
        meta(),
        checkpoint("base"),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"NO ORDINAL"}}),
        started(990, TURN),
    ];
    let fast =
        CodexHomeHistoryReader::new(build_home(&records).path()).read_visible_history(&anchor());
    // Force the full scan by making the compaction ineligible (no window
    // number): the reverse probe then finds no base.
    let full_records_full = [
        records[0].clone(),
        json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number_absent":1,
            "replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"base"}]}]}}),
        records[2].clone(),
        records[3].clone(),
    ];
    let full = CodexHomeHistoryReader::new(build_home(&full_records_full).path())
        .read_visible_history(&anchor());
    match (&fast, &full) {
        (Err(fast), Err(full)) => {
            assert_eq!(fast.reason(), "ordinal_missing", "fast reason");
            assert_eq!(full.reason(), "ordinal_missing", "full reason");
        }
        (fast, full) => panic!("both paths must reject: fast={fast:?} full={full:?}"),
    }
}

#[test]
fn fast_suffix_cross_base_ordinal_regression_matches_full() {
    let records = [
        meta(),
        user(850, "pre"),
        checkpoint("base"),
        user(850, "after regression"),
        started(990, TURN),
    ];
    let fast =
        CodexHomeHistoryReader::new(build_home(&records).path()).read_visible_history(&anchor());
    // Full path: identical file but the compaction is made ineligible so the
    // reverse probe finds nothing and the full scan reports the defect.
    let full_records = [
        records[0].clone(),
        records[1].clone(),
        json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number_absent":1,
            "replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"base"}]}]}}),
        records[3].clone(),
        records[4].clone(),
    ];
    let full = CodexHomeHistoryReader::new(build_home(&full_records).path())
        .read_visible_history(&anchor());
    match (&fast, &full) {
        (Err(fast), Err(full)) => assert_eq!(
            fast.reason(),
            full.reason(),
            "fast ({}) and full ({}) diverge",
            fast.reason(),
            full.reason()
        ),
        (fast, full) => panic!("both paths must reject: fast={fast:?} full={full:?}"),
    }
}

#[test]
fn failed_checkpoint_owner_turn_forces_full() {
    let records = [
        meta(),
        user(1, "GOOD HISTORY"),
        started(2, "turnB"),
        json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number":1,
            "replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"BAD CHECKPOINT"}]}]}}),
        json!({"ordinal":901,"type":"event_msg","payload":{"type":"task_complete","turn_id":"turnB","error":{"code":"boom"}}}),
        started(990, TURN),
    ];
    let snapshot = CodexHomeHistoryReader::new(build_home(&records).path())
        .read_visible_history(&anchor())
        .unwrap();
    let text = visible_text(&snapshot);
    assert!(text.contains("GOOD HISTORY"), "{text}");
    assert!(!text.contains("BAD CHECKPOINT"), "{text}");
}
