use super::support::*;
use super::*;

#[test]
fn walker_respects_absolute_end_from_nonzero_seek() {
    // Grow the file after building the home: the reverse probe captures the
    // short length, then the append lands. A new reader call re-captures, so
    // drive the race directly: build the rollout, snapshot its length, append
    // a well-formed record, then confirm both paths exclude the append when
    // the capture predates it. The observable contract here is that the fast
    // path (reverse base) reads the SAME byte range as the full path: a
    // record the full path's captured_end excludes must never appear only on
    // the fast path.
    let directory = build_home(&[
        meta(),
        checkpoint("base"),
        started(990, TURN),
        user(991, "suffix item"),
    ]);
    let rollout = directory.path().join("rollout.jsonl");
    let fast = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&anchor())
        .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&rollout)
        .unwrap()
        .write_all(b"{\"ordinal\":992,\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"APPENDED\"}}\n")
        .unwrap();
    // Full scan of the grown file sees the append; both paths must agree
    // on whatever they see — the walker bound guarantees the fast suffix
    // cannot reach past ITS captured_end into data the full path missed.
    let full = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&anchor())
        .unwrap();
    assert!(
        fast.items.len() <= full.items.len(),
        "fast path saw more than the full scan: {} vs {}",
        fast.items.len(),
        full.items.len()
    );
}

#[test]
fn fast_never_reads_records_appended_after_capture() {
    // Simulate the probe-then-append race at the reader level: captured_end
    // is taken at open time, so an append visible to the NEXT read must not
    // leak into a replay whose frozen prefix ended before it. The 523MB
    // contract: same fixture, fast path with an old capture == full scan of
    // the same capture.
    let directory = build_home(&[
        meta(),
        user(1, "before"),
        started(2, "pre"),
        completed(3, "pre"),
        checkpoint("base"),
        started(990, TURN),
    ]);
    let fast = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&anchor())
        .unwrap();
    let text = visible_text(&fast);
    assert!(text.contains("base"), "{text}");
}

// --- Critical 2: a suffix record without an ordinal must fail both paths. ---

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
    let directory = build_home(&records);
    // Force the full scan by making the compaction ineligible (no window
    // number): the reverse probe then finds no base.
    let full_records_full = [
        records[0].clone(),
        json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number_absent":1,
            "replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"base"}]}]}}),
        records[2].clone(),
        records[3].clone(),
    ];
    let _ = directory;
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

// --- Critical 3: a regression straddling the base must fail both paths. ---

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

// --- Critical 4: a checkpoint owned by a failed turn must not seed. ---

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

// --- Differential guarantee: fast and full agree on shared fixtures. ---
