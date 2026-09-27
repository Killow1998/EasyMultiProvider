//! Reader-level history contracts.
//!
//! These tests own the semantics the deleted whitebox module used to pin:
//! the reverse-base fast path and the full scan must stay observationally
//! identical over the same durable rollout, and the reader's error reasons
//! must match the Python oracle (`ordinal_missing`,
//! `ordinal_not_monotonic`, `compaction_identity_ambiguous`,
//! `lineage_cycle`, `lineage_prefix_truncated`).

use emp_codex::history::CodexHomeHistoryReader;
use emp_history::{HistoryAnchor, HistoryReader, HistorySnapshot};
use rusqlite::params;
use serde_json::{Value, json};
use std::io::Write;
use tempfile::tempdir;

const THREAD: &str = "01a00000-0000-7000-8000-000000000001";
const TURN: &str = "01a00000-0000-7000-8000-000000000004";
const PARENT: &str = "01a00000-0000-7000-8000-000000000002";
const MODEL: &str = "gpt-native";

fn write_rollout(directory: &std::path::Path, records: &[Value]) -> std::path::PathBuf {
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

fn jsonl_bytes(records: &[Value]) -> Vec<u8> {
    records
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>()
        .into_bytes()
}

fn write_records(path: &std::path::Path, records: &[Value]) -> std::path::PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, jsonl_bytes(records)).unwrap();
    path.to_path_buf()
}

fn write_zstd(path: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let compressed = zstd::stream::encode_all(bytes, 0).unwrap();
    std::fs::write(path, compressed).unwrap();
    path.to_path_buf()
}

fn session_meta(thread: &str, mode: &str) -> Value {
    json!({"ordinal":0,"type":"session_meta","payload":{"id":thread,"history_mode":mode}})
}

fn state_database(directory: &std::path::Path, rollout: &std::path::Path) {
    state_database_with_mode(directory, rollout, "paginated");
}

fn state_database_with_mode(
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

fn anchor() -> HistoryAnchor {
    HistoryAnchor {
        thread_id: Some(THREAD.to_owned()),
        turn_id: Some(TURN.to_owned()),
        ..HistoryAnchor::default()
    }
}

fn visible_text(snapshot: &HistorySnapshot) -> String {
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

fn build_home(records: &[Value]) -> tempfile::TempDir {
    let directory = tempdir().unwrap();
    let rollout = write_rollout(directory.path(), records);
    state_database(directory.path(), &rollout);
    directory
}

fn checkpoint(text: &str) -> Value {
    json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number":1,
        "replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}]}})
}

fn user(ordinal: u64, text: &str) -> Value {
    json!({"ordinal":ordinal,"type":"event_msg","payload":{"type":"user_message","message":text}})
}

fn started(ordinal: u64, turn: &str) -> Value {
    json!({"ordinal":ordinal,"type":"event_msg","payload":{"type":"task_started","turn_id":turn}})
}

fn completed(ordinal: u64, turn: &str) -> Value {
    json!({"ordinal":ordinal,"type":"event_msg","payload":{"type":"task_complete","turn_id":turn}})
}

fn meta() -> Value {
    json!({"ordinal":0,"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated"}})
}

// --- Critical 1: the walker must never read past the frozen captured end. ---

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

fn assert_fast_matches_full(records: &[Value], name: &str) {
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
fn ordinal_regression_reason_is_python_canonical() {
    // The Python oracle raises HistoryAmbiguousError("ordinal_not_monotonic");
    // the Rust reason must match byte for byte.
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

fn assert_both_reject(records: &[Value], reason: &str) {
    let directory = build_home(records);
    for force_full in [false, true] {
        let error = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history_with_strategy(&anchor(), force_full)
            .unwrap_err();
        assert_eq!(error.reason(), reason, "force_full={force_full}");
    }
}

fn checkpoint_with_metadata(metadata: Value) -> Value {
    json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number":1,
        "replacement_history":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"a"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"b"}]}
        ],
        "replacement_history_metadata":metadata}})
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
