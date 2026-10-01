use super::support::*;
use super::*;

#[test]
fn lineage_cycle_is_rejected() {
    // The rollout's session_meta history_base points at its own thread id:
    // the lineage walk revisits the same rollout and must fail closed
    // instead of looping.
    let directory = tempdir().unwrap();
    let child = write_rollout(
        directory.path(),
        &[
            json!({"ordinal":0,"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated",
                "history_base":{"thread_id":THREAD,"end_ordinal_exclusive":10,"end_byte_offset":0}}}),
            started(990, TURN),
        ],
    );
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
    drop(connection);
    let error = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&anchor())
        .unwrap_err();
    assert_eq!(error.reason(), "lineage_cycle");
}

#[test]
fn frozen_lineage_byte_offset_is_enforced() {
    let directory = tempdir().unwrap();
    // An ancestor truncated below the frozen byte offset must fail closed.
    // The ancestor is named <thread-id>.jsonl so the reader's filename
    // fallback resolves it without a threads row.
    write_records(
        &directory
            .path()
            .join("sessions/2026/09/27/01a00000-0000-7000-8000-000000000002.jsonl"),
        &[
            json!({"ordinal":0,"type":"session_meta","payload":{"id":"01a00000-0000-7000-8000-000000000002","history_mode":"paginated"}}),
            user(1, "ancestor history"),
        ],
    );
    let child = write_rollout(
        directory.path(),
        &[
            json!({"ordinal":0,"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated",
                "history_base":{"thread_id":"01a00000-0000-7000-8000-000000000002","end_ordinal_exclusive":2,"end_byte_offset":1_000_000}}}),
            started(990, TURN),
        ],
    );
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
    drop(connection);
    let error = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&anchor())
        .unwrap_err();
    assert_eq!(error.reason(), "lineage_prefix_truncated");
}

#[test]
fn invalid_history_base_bounds_are_rejected() {
    let invalid_bases = [
        json!({"thread_id":PARENT,"end_ordinal_exclusive":-1,"end_byte_offset":0}),
        json!({"thread_id":PARENT,"end_ordinal_exclusive":1,"end_byte_offset":2_147_483_649u64}),
        json!({"thread_id":"not-a-uuid","end_ordinal_exclusive":1,"end_byte_offset":0}),
        json!({"thread_id":PARENT,"end_byte_offset":0}),
    ];
    for history_base in invalid_bases {
        let directory = tempdir().unwrap();
        let child = write_rollout(
            directory.path(),
            &[
                json!({"ordinal":0,"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated",
            "history_base":history_base}}),
            ],
        );
        state_database(directory.path(), &child);
        let error = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history(&HistoryAnchor {
                thread_id: Some(THREAD.to_owned()),
                ..HistoryAnchor::default()
            })
            .unwrap_err();
        assert_eq!(error.reason(), "invalid_history_base");
    }
}

#[test]
fn lineage_depth_has_a_distinct_failure_from_cycles() {
    let directory = tempdir().unwrap();
    let id = |index: usize| format!("01a00000-0000-7000-8000-{index:012x}");
    let child_parent = id(100);
    let child_meta = json!({"ordinal":0,"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated",
        "history_base":{"thread_id":child_parent,"end_ordinal_exclusive":10,"end_byte_offset":0}}});
    let child = write_rollout(directory.path(), &[child_meta]);
    state_database(directory.path(), &child);
    for index in 100..131 {
        let parent = id(index);
        let next = id(index + 1);
        write_records(
            &directory
                .path()
                .join(format!("sessions/2026/09/27/{parent}.jsonl")),
            &[
                json!({"ordinal":0,"type":"session_meta","payload":{"id":parent,"history_mode":"paginated",
                "history_base":{"thread_id":next,"end_ordinal_exclusive":10,"end_byte_offset":0}}}),
            ],
        );
    }
    let error = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&HistoryAnchor {
            thread_id: Some(THREAD.to_owned()),
            ..HistoryAnchor::default()
        })
        .unwrap_err();
    assert_eq!(error.reason(), "lineage_depth_exceeded");
}

#[test]
fn ancestor_lookup_rejects_ambiguous_session_files() {
    let directory = tempdir().unwrap();
    let child = write_rollout(
        directory.path(),
        &[
            json!({"ordinal":0,"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated",
            "history_base":{"thread_id":PARENT,"end_ordinal_exclusive":10,"end_byte_offset":0}}}),
        ],
    );
    state_database(directory.path(), &child);
    for root in ["sessions", "archived_sessions"] {
        write_records(
            &directory
                .path()
                .join(format!("{root}/2026/09/27/{PARENT}.jsonl")),
            &[session_meta(PARENT, "paginated"), user(1, "parent")],
        );
    }
    let error = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&HistoryAnchor {
            thread_id: Some(THREAD.to_owned()),
            ..HistoryAnchor::default()
        })
        .unwrap_err();
    assert_eq!(error.reason(), "lineage_source_ambiguous");
}

#[cfg(unix)]
#[test]
fn ancestor_lookup_does_not_follow_session_symlinks() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let child = write_rollout(
        directory.path(),
        &[
            json!({"ordinal":0,"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated",
            "history_base":{"thread_id":PARENT,"end_ordinal_exclusive":10,"end_byte_offset":0}}}),
        ],
    );
    state_database(directory.path(), &child);
    let outside = write_records(
        &directory.path().join("outside.jsonl"),
        &[session_meta(PARENT, "paginated"), user(1, "outside")],
    );
    let link = directory
        .path()
        .join(format!("sessions/2026/09/27/{PARENT}.jsonl"));
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(outside, link).unwrap();
    let error = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&HistoryAnchor {
            thread_id: Some(THREAD.to_owned()),
            ..HistoryAnchor::default()
        })
        .unwrap_err();
    assert_eq!(error.reason(), "source_missing");
}

#[cfg(unix)]
#[test]
fn ancestor_database_path_rejects_symlinked_session_directories() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let child = write_rollout(
        directory.path(),
        &[
            json!({"ordinal":0,"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated",
        "history_base":{"thread_id":PARENT,"end_ordinal_exclusive":10,"end_byte_offset":0}}}),
        ],
    );
    state_database(directory.path(), &child);

    let parent = write_records(
        &directory
            .path()
            .join(format!("sessions/real/{PARENT}.jsonl")),
        &[session_meta(PARENT, "paginated"), user(1, "parent")],
    );
    let alias = directory.path().join("sessions/alias");
    symlink(parent.parent().unwrap(), &alias).unwrap();
    let linked_path = alias.join(format!("{PARENT}.jsonl"));
    let connection = rusqlite::Connection::open(directory.path().join("state_5.sqlite")).unwrap();
    connection
        .execute(
            "INSERT INTO threads VALUES (?1, ?2, 'paginated', ?3)",
            params![PARENT, linked_path.to_str().unwrap(), MODEL],
        )
        .unwrap();

    let error = CodexHomeHistoryReader::new(directory.path())
        .read_visible_history(&HistoryAnchor {
            thread_id: Some(THREAD.to_owned()),
            ..HistoryAnchor::default()
        })
        .unwrap_err();
    assert_eq!(error.reason(), "rollout_outside_session_root");
}

#[cfg(unix)]
#[test]
fn ancestor_database_path_resolves_through_a_symlinked_codex_home() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let real = directory.path().join("real");
    std::fs::create_dir_all(real.join("sessions")).unwrap();
    let home = directory.path().join("home");
    symlink(&real, &home).unwrap();
    write_rollout(
        &real,
        &[
            json!({"ordinal":0,"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated",
            "history_base":{"thread_id":PARENT,"end_ordinal_exclusive":10,"end_byte_offset":0}}}),
        ],
    );
    state_database(&real, &home.join("rollout.jsonl"));
    write_records(
        &real.join(format!("sessions/2026/09/27/{PARENT}.jsonl")),
        &[session_meta(PARENT, "paginated"), user(1, "parent")],
    );
    let parent = home.join(format!("sessions/2026/09/27/{PARENT}.jsonl"));
    let connection = rusqlite::Connection::open(real.join("state_5.sqlite")).unwrap();
    connection
        .execute(
            "INSERT INTO threads VALUES (?1, ?2, 'paginated', ?3)",
            params![PARENT, parent.to_str().unwrap(), MODEL],
        )
        .unwrap();

    let snapshot = CodexHomeHistoryReader::new(&home)
        .read_visible_history(&HistoryAnchor {
            thread_id: Some(THREAD.to_owned()),
            ..HistoryAnchor::default()
        })
        .unwrap();
    assert!(!snapshot.items.is_empty());
}

#[test]
fn frozen_prefix_replay_matches_full_on_the_same_plain_and_zstd_inputs() {
    let inherited_text = "inherited ".repeat(2_000);
    let prefix_records = [
        session_meta(PARENT, "paginated"),
        started(1, "kept"),
        json!({"ordinal":2,"type":"turn_context","turn_id":"kept","payload":{"model":"base-model"}}),
        completed(3, "kept"),
        user(4, &inherited_text),
    ];
    let parent_prefix = jsonl_bytes(&prefix_records);
    let prefix_offset = parent_prefix.len() as u64;
    let parent_records = [
        prefix_records.as_slice(),
        &[user(5, "future history"), started(6, "future"),
            json!({"ordinal":7,"type":"turn_context","turn_id":"future","payload":{"model":"future-model"}}),
            completed(8, "future")],
    ]
    .concat();
    let parent_bytes = jsonl_bytes(&parent_records);

    for compressed in [false, true] {
        let directory = tempdir().unwrap();
        let extension = if compressed { "jsonl.zst" } else { "jsonl" };
        let parent_path = directory
            .path()
            .join(format!("sessions/2026/09/27/{PARENT}.{extension}"));
        if compressed {
            write_zstd(&parent_path, &parent_bytes);
        } else {
            std::fs::create_dir_all(parent_path.parent().unwrap()).unwrap();
            std::fs::write(&parent_path, &parent_bytes).unwrap();
        }
        if compressed {
            assert!((std::fs::metadata(&parent_path).unwrap().len()) < prefix_offset);
        }
        let child_meta = json!({"ordinal":0,"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated",
            "history_base":{"thread_id":PARENT,"end_ordinal_exclusive":5,"end_byte_offset":prefix_offset}}});
        let child = write_rollout(directory.path(), &[child_meta, started(990, TURN)]);
        state_database(directory.path(), &child);

        let reader = CodexHomeHistoryReader::new(directory.path());
        let fast = reader
            .read_visible_history_with_strategy(&anchor(), false)
            .unwrap();
        let full = reader
            .read_visible_history_with_strategy(&anchor(), true)
            .unwrap();
        assert_eq!(fast.items, full.items, "compressed={compressed}");
        assert_eq!(
            fast.source_model, full.source_model,
            "compressed={compressed}"
        );
        let text = visible_text(&fast);
        assert!(
            text.contains("inherited"),
            "compressed={compressed}: {text}"
        );
        assert!(
            !text.contains("future history"),
            "compressed={compressed}: {text}"
        );
        assert_eq!(fast.source_model.as_deref(), Some("base-model"));
    }
}
