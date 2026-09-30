use super::support::*;
use super::*;

#[test]
fn compressed_rollout_full_replay_matches_visible_history_contract() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl.zst");
    let records = [meta(), user(1, "compressed item"), started(990, TURN)];
    write_zstd(&path, &jsonl_bytes(&records));
    state_database(directory.path(), &path);
    let snapshot = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&anchor())
        .unwrap();
    assert!(visible_text(&snapshot).contains("compressed item"));
}

#[test]
fn sqlite_and_session_history_modes_are_validated() {
    let directory = tempdir().unwrap();
    let path = write_rollout(
        directory.path(),
        &[
            session_meta(THREAD, "legacy"),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"legacy item"}}),
        ],
    );
    state_database_with_mode(directory.path(), &path, "legacy");
    let legacy = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&HistoryAnchor {
            thread_id: Some(THREAD.to_owned()),
            ..HistoryAnchor::default()
        })
        .unwrap();
    assert!(visible_text(&legacy).contains("legacy item"));

    let directory = tempdir().unwrap();
    let path = write_rollout(directory.path(), &[meta(), user(1, "mode mismatch")]);
    state_database_with_mode(directory.path(), &path, "future-mode");
    let error = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&HistoryAnchor {
            thread_id: Some(THREAD.to_owned()),
            ..HistoryAnchor::default()
        })
        .unwrap_err();
    assert_eq!(error.reason(), "invalid_history_mode");

    let directory = tempdir().unwrap();
    let path = write_rollout(
        directory.path(),
        &[session_meta(THREAD, "legacy"), user(1, "mode mismatch")],
    );
    state_database_with_mode(directory.path(), &path, "paginated");
    let error = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&HistoryAnchor {
            thread_id: Some(THREAD.to_owned()),
            ..HistoryAnchor::default()
        })
        .unwrap_err();
    assert_eq!(error.reason(), "history_mode_mismatch");

    let directory = tempdir().unwrap();
    let path = write_rollout(
        directory.path(),
        &[
            session_meta(THREAD, "future-mode"),
            user(1, "bad session mode"),
        ],
    );
    state_database(directory.path(), &path);
    let error = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&HistoryAnchor {
            thread_id: Some(THREAD.to_owned()),
            ..HistoryAnchor::default()
        })
        .unwrap_err();
    assert_eq!(error.reason(), "invalid_history_mode");
}

#[test]
fn replacement_history_metadata_must_be_array() {
    assert_both_reject(
        &[
            meta(),
            checkpoint_with_metadata(json!({"not":"a list"})),
            started(990, TURN),
        ],
        "invalid_replacement_history_metadata",
    );
}

#[test]
fn replacement_history_metadata_length_must_match() {
    assert_both_reject(
        &[
            meta(),
            checkpoint_with_metadata(json!([{}])),
            started(990, TURN),
        ],
        "invalid_replacement_history_metadata",
    );
    // A parallel array (and an explicit null) stays valid on both paths.
    for metadata in [json!([{}, {}]), Value::Null] {
        assert_fast_matches_full(
            &[
                meta(),
                checkpoint_with_metadata(metadata),
                started(990, TURN),
            ],
            "valid metadata",
        );
    }
}
