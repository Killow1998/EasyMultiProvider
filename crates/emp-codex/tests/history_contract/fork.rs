use super::support::*;
use super::*;

#[test]
fn duplicate_exact_compaction_is_ambiguous() {
    // Fork replay matches the anchor's exact compaction ciphertext against
    // every parent record; two matches mean the boundary is ambiguous and
    // must fail closed rather than pick one.
    let directory = tempdir().unwrap();
    let duplicated = json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number":1,
    "replacement_history":[
        {"type":"compaction","encrypted_content":"abc"},
        {"type":"message","role":"user","content":[{"type":"input_text","text":"x"}]}
    ]}});
    let compaction = duplicated["payload"]["replacement_history"][0]
        .as_object()
        .unwrap()
        .clone();
    let parent = directory.path().join("parent.jsonl");
    std::fs::write(
        &parent,
        [
            json!({"ordinal":0,"type":"session_meta","payload":{"id":PARENT,"history_mode":"paginated"}}),
            user(1, "before"),
            duplicated.clone(),
            duplicated,
        ]
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>(),
    )
    .unwrap();
    // Fork leg fires when the child rollout's session_meta id mismatches
    // the anchor thread (thread_mismatch), so the child file carries the
    // parent's id and the resume leg fails before replay.
    let child = directory.path().join("child.jsonl");
    std::fs::write(
        &child,
        format!(
            "{}\n",
            json!({"ordinal":0,"type":"session_meta","payload":{"id":PARENT,"history_mode":"paginated"}})
        ),
    )
    .unwrap();
    let database = directory.path().join("state_5.sqlite");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, model TEXT)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO threads VALUES (?1, ?2, 'paginated', ?3)",
            params![THREAD, child.to_str().unwrap(), MODEL],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO threads VALUES (?1, ?2, 'paginated', ?3)",
            params![PARENT, parent.to_str().unwrap(), MODEL],
        )
        .unwrap();
    drop(connection);
    let error = CodexHomeHistoryReader::new(directory.path())
        .read_compaction_history(
            &HistoryAnchor {
                thread_id: Some(THREAD.to_owned()),
                forked_from_thread_id: Some(PARENT.to_owned()),
                ..HistoryAnchor::default()
            },
            &compaction,
        )
        .unwrap_err();
    assert_eq!(error.reason(), "compaction_identity_ambiguous");
}

#[test]
fn ordinal_only_ancestor_never_seeds_from_a_post_fork_checkpoint() {
    // A `history_base` without `end_byte_offset` bounds the ancestor by
    // ordinal alone; a compaction the parent wrote after the fork must not
    // become the child's inherited history.
    let directory = tempdir().unwrap();
    let parent_path = directory
        .path()
        .join(format!("sessions/2026/09/27/{PARENT}.jsonl"));
    write_records(
        &parent_path,
        &[
            session_meta(PARENT, "paginated"),
            started(1, "kept"),
            user(2, "before fork"),
            completed(3, "kept"),
            user(5, "after fork"),
            checkpoint("post-fork checkpoint"),
        ],
    );
    let child_meta = json!({"ordinal":0,"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated",
        "history_base":{"thread_id":PARENT,"end_ordinal_exclusive":5}}});
    let child = write_rollout(directory.path(), &[child_meta, started(990, TURN)]);
    state_database(directory.path(), &child);

    let reader = CodexHomeHistoryReader::new(directory.path());
    let fast = reader
        .read_visible_history_with_strategy(&anchor(), false)
        .unwrap();
    let full = reader
        .read_visible_history_with_strategy(&anchor(), true)
        .unwrap();
    let text = visible_text(&full);
    assert!(text.contains("before fork"), "{text}");
    assert!(!text.contains("post-fork"), "{text}");
    assert_eq!(fast.items, full.items, "fast: {}", visible_text(&fast));
}

#[test]
fn child_opaque_checkpoint_resolves_against_inherited_history_on_both_paths() {
    // An opaque child checkpoint resolves against the merged lineage
    // history; the reverse base alone cannot see the inherited items.
    let directory = tempdir().unwrap();
    let parent_path = directory
        .path()
        .join(format!("sessions/2026/09/27/{PARENT}.jsonl"));
    write_records(
        &parent_path,
        &[
            session_meta(PARENT, "paginated"),
            started(1, "kept"),
            user(2, "inherited"),
            completed(3, "kept"),
        ],
    );
    let child_meta = json!({"ordinal":0,"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated",
        "history_base":{"thread_id":PARENT,"end_ordinal_exclusive":4}}});
    let child = write_rollout(
        directory.path(),
        &[
            child_meta,
            started(10, "child"),
            completed(11, "child"),
            json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number":2,
            "replacement_history":[
                {"type":"compaction","encrypted_content":"gAAAA-opaque"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"child base"}]}
            ]}}),
            started(990, TURN),
        ],
    );
    state_database(directory.path(), &child);
    let reader = CodexHomeHistoryReader::new(directory.path());
    let fast = reader.read_visible_history_with_strategy(&anchor(), false);
    let full = reader.read_visible_history_with_strategy(&anchor(), true);
    match (&fast, &full) {
        (Ok(fast), Ok(full)) => assert_eq!(fast.items, full.items),
        (Err(fast), Err(full)) => assert_eq!(fast.reason(), full.reason()),
        _ => panic!("fast {fast:?} vs full {full:?}"),
    }
}
