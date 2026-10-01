use super::support::*;
use super::*;

#[test]
fn fast_and_full_replay_matrix() {
    let failed = |ordinal: u64, turn: &str| json!({"ordinal":ordinal,"type":"event_msg","payload":{"type":"task_complete","turn_id":turn,"error":{"code":"boom"}}});
    let cases: Vec<(&str, Vec<Value>)> = vec![
        (
            "checkpoint plus suffix turns",
            vec![
                meta(),
                user(1, "before checkpoint"),
                started(2, "pre"),
                completed(3, "pre"),
                checkpoint("checkpoint base"),
                user(901, "after checkpoint"),
                started(902, "mid"),
                completed(903, "mid"),
                started(990, TURN),
            ],
        ),
        (
            "failed turn after checkpoint",
            vec![
                meta(),
                user(1, "early"),
                started(2, "pre"),
                completed(3, "pre"),
                checkpoint("checkpoint base"),
                started(901, "doomed"),
                user(902, "doomed request"),
                failed(903, "doomed"),
                user(904, "recovered"),
                started(990, TURN),
            ],
        ),
        (
            "interrupted turn with rollback marker",
            vec![
                meta(),
                started(1, "kept"),
                user(2, "kept turn"),
                completed(3, "kept"),
                started(4, "stale"),
                user(5, "stale turn"),
                json!({"ordinal":6,"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
                user(7, "after rollback"),
                started(990, TURN),
            ],
        ),
        (
            "anchor before checkpoint requires full replay",
            vec![
                meta(),
                user(1, "ancient"),
                started(2, "ancient-turn"),
                completed(3, "ancient-turn"),
                checkpoint("checkpoint base"),
                started(901, "later"),
                completed(902, "later"),
                started(990, TURN),
            ],
        ),
        (
            "regressed ordinal is terminal on both paths",
            vec![
                meta(),
                user(5, "later"),
                user(4, "earlier"),
                started(990, TURN),
            ],
        ),
    ];
    for (name, records) in cases {
        assert_fast_matches_full(&records, name);
    }
}

// --- Reason-exact contracts retained from the deleted whitebox module. ---

#[test]
fn fast_and_full_validate_prefix_identity_mode_and_ordinals() {
    let cases = [
        (
            "identity conflict before checkpoint",
            vec![
                meta(),
                json!({"ordinal":1,"type":"session_meta","payload":{"id":PARENT,"history_mode":"paginated"}}),
                checkpoint("base"),
                started(990, TURN),
            ],
            "thread_mismatch",
        ),
        (
            "mode mismatch before checkpoint",
            vec![
                meta(),
                json!({"ordinal":1,"type":"session_meta","payload":{"id":THREAD,"history_mode":"legacy"}}),
                checkpoint("base"),
                started(990, TURN),
            ],
            "history_mode_mismatch",
        ),
        (
            "ordinal regression before checkpoint",
            vec![
                meta(),
                user(5, "later ordinal"),
                user(4, "earlier ordinal"),
                checkpoint("base"),
                started(990, TURN),
            ],
            "ordinal_not_monotonic",
        ),
    ];
    for (name, records, expected_reason) in cases {
        let directory = build_home(&records);
        let reader = CodexHomeHistoryReader::new(directory.path());
        let fast = reader.read_visible_history_with_strategy(&anchor(), false);
        let full = reader.read_visible_history_with_strategy(&anchor(), true);
        assert_eq!(
            fast.as_ref().unwrap_err().reason(),
            expected_reason,
            "{name}"
        );
        assert_eq!(
            full.as_ref().unwrap_err().reason(),
            expected_reason,
            "{name}"
        );
    }
}

#[test]
fn reverse_base_preserves_prefix_model_and_role_dedup_past_initial_window() {
    let huge = "x".repeat(17 * 1024 * 1024);
    let records = [
        meta(),
        started(1, "A"),
        json!({"ordinal":2,"type":"turn_context","turn_id":"A","payload":{"model":"gpt-prefix"}}),
        json!({"ordinal":3,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"old assistant"}],"turn_id":"A"}}),
        completed(4, "A"),
        json!({"ordinal":5,"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":huge}],"turn_id":"A"}}),
        checkpoint("self-contained base"),
        json!({"ordinal":901,"type":"event_msg","payload":{"type":"assistant_message","message":"duplicate assistant"}}),
        started(990, TURN),
    ];
    let directory = build_home(&records);
    let reader = CodexHomeHistoryReader::new(directory.path());
    let fast = reader
        .read_visible_history_with_strategy(&anchor(), false)
        .unwrap();
    let full = reader
        .read_visible_history_with_strategy(&anchor(), true)
        .unwrap();
    assert_eq!(fast.items, full.items);
    assert_eq!(fast.source_model, full.source_model);
    assert_eq!(fast.source_model.as_deref(), Some("gpt-prefix"));
    let text = visible_text(&fast);
    assert!(text.contains("self-contained base"), "{text}");
    assert!(!text.contains("duplicate assistant"), "{text}");
    assert!(!text.contains("old assistant"), "{text}");
}

#[test]
fn anchor_before_checkpoint_falls_back_cleanly() {
    // The anchor turn predates the compaction base: visible history is the
    // bytes before the anchor, so everything after it (including the
    // checkpoint and later turns) is excluded.
    let records = [
        meta(),
        user(1, "ancient"),
        started(2, "ancient-turn"),
        completed(3, "ancient-turn"),
        checkpoint("checkpoint base"),
        started(901, "later"),
        completed(902, "later"),
        started(990, TURN),
    ];
    // Anchor on the ancient turn: the fast path cannot see it.
    let ancient_anchor = HistoryAnchor {
        thread_id: Some(THREAD.to_owned()),
        turn_id: Some("ancient-turn".to_owned()),
        ..HistoryAnchor::default()
    };
    let snapshot = CodexHomeHistoryReader::new(build_home(&records).path())
        .read_visible_history(&ancient_anchor)
        .unwrap();
    let text = visible_text(&snapshot);
    assert!(text.contains("ancient"), "{text}");
    assert!(!text.contains("checkpoint base"), "{text}");
}

#[test]
fn opaque_checkpoint_and_rollback_match_full_semantics() {
    // Opaque replacement items are not self-contained: the full scan owns
    // the case and the same defect must surface regardless of path.
    let records = [
        meta(),
        user(1, "pre compaction"),
        started(2, "pre"),
        completed(3, "pre"),
        json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number":7,
        "replacement_history":[
            {"type":"compaction","encrypted_content":"gAAAA-opaque"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"opaque base"}]}
        ]}}),
        user(901, "post opaque"),
        started(990, TURN),
    ];
    assert_fast_matches_full(&records, "opaque checkpoint");
}

// --- Round-2 review regressions. ---

#[test]
fn interrupted_checkpoint_owner_forces_full() {
    // A turn with NO task_complete at all is unsuccessful in full replay,
    // so a checkpoint it owns must not seed the fast path.
    let records = [
        meta(),
        user(1, "GOOD HISTORY"),
        started(2, "turnB"),
        checkpoint("BAD CHECKPOINT"),
        // No task_complete for turnB — interrupted.
        started(990, TURN),
    ];
    let snapshot = CodexHomeHistoryReader::new(build_home(&records).path())
        .read_visible_history(&anchor())
        .unwrap();
    let text = visible_text(&snapshot);
    assert!(text.contains("GOOD HISTORY"), "{text}");
    assert!(!text.contains("BAD CHECKPOINT"), "{text}");
}

#[test]
fn reverse_window_missing_turn_start_forces_full() {
    // A 17 MiB record pushes the initial reverse window past the checkpoint
    // owner's task_started. The shared prefix control scan must recover that
    // state and make the same decision as full replay on this exact file.
    let huge = "x".repeat(17 * 1024 * 1024);
    let records = [
        meta(),
        user(1, "GOOD HISTORY"),
        started(2, "turnB"),
        json!({"ordinal":3,"type":"response_item","payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":huge}],"turn_id":"turnB"}}),
        checkpoint("BAD CHECKPOINT"),
        json!({"ordinal":901,"type":"event_msg","payload":{"type":"task_complete","turn_id":"turnB","error":{"code":"boom"}}}),
        started(990, TURN),
    ];
    let directory = build_home(&records);
    let reader = CodexHomeHistoryReader::new(directory.path());
    let fast = reader
        .read_visible_history_with_strategy(&anchor(), false)
        .unwrap();
    let full = reader
        .read_visible_history_with_strategy(&anchor(), true)
        .unwrap();
    assert_eq!(fast.items, full.items);
    assert_eq!(fast.source_model, full.source_model);
    let text = visible_text(&fast);
    assert!(text.contains("GOOD HISTORY"), "{text}");
    assert!(!text.contains("BAD CHECKPOINT"), "{text}");
}

#[test]
fn fast_preserves_precheckpoint_source_model() {
    // The reverse base seeds the suffix scan with the pre-checkpoint model
    // contexts; the newest successful one is the source model, exactly as
    // the full scan resolves it.
    let records = [
        meta(),
        started(1, "A"),
        json!({"ordinal":2,"type":"turn_context","turn_id":"A","payload":{"model":"gpt-native"}}),
        user(3, "hi"),
        completed(4, "A"),
        checkpoint("checkpoint base"),
        started(990, TURN),
    ];
    let directory = build_home(&records);
    let fast = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history_with_strategy(&anchor(), false)
        .unwrap();
    let full = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history_with_strategy(&anchor(), true)
        .unwrap();
    assert_eq!(fast.source_model.as_deref(), Some("gpt-native"));
    assert_eq!(fast.source_model, full.source_model);
}

#[test]
fn fast_does_not_observe_source_model_after_anchor() {
    // The full scan stops observing once the anchor closes history; the
    // fast suffix must not pick up model contexts past the boundary.
    let records = [
        meta(),
        started(1, "early"),
        json!({"ordinal":2,"type":"turn_context","turn_id":"early","payload":{"model":"early-model"}}),
        completed(3, "early"),
        checkpoint("checkpoint base"),
        started(990, TURN),
        started(991, "future"),
        json!({"ordinal":992,"type":"turn_context","turn_id":"future","payload":{"model":"future-model"}}),
        completed(993, "future"),
    ];
    let directory = build_home(&records);
    let fast = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history_with_strategy(&anchor(), false)
        .unwrap();
    let full = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history_with_strategy(&anchor(), true)
        .unwrap();
    assert_eq!(fast.source_model.as_deref(), Some("early-model"));
    assert_eq!(fast.source_model, full.source_model);
}

#[test]
fn suffix_rollback_after_checkpoint_matches_full() {
    // checkpoint -> turn A -> thread_rolled_back(1) -> fresh turn anchor:
    // the fast suffix must apply the rollback to its own seed exactly like
    // the full scan, so the stale turn disappears on both paths.
    let records = [
        meta(),
        started(1, "A"),
        user(2, "stale turn"),
        checkpoint("checkpoint base"),
        json!({"ordinal":901,"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
        user(902, "fresh turn"),
        started(990, TURN),
    ];
    assert_fast_matches_full(&records, "rollback after checkpoint");
    let snapshot = CodexHomeHistoryReader::new(build_home(&records).path())
        .read_visible_history(&anchor())
        .unwrap();
    let text = visible_text(&snapshot);
    assert!(text.contains("fresh turn"), "{text}");
    assert!(!text.contains("stale turn"), "{text}");
}

#[test]
fn ordinal_regression_reason_is_stable() {
    let records = [
        meta(),
        user(5, "later"),
        user(4, "earlier"),
        started(990, TURN),
    ];
    let directory = build_home(&records);
    let error = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&anchor())
        .unwrap_err();
    assert_eq!(error.reason(), "ordinal_not_monotonic");
}

// --- replacement_history_metadata must be a parallel array. ---
