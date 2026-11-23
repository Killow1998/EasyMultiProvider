use super::*;

#[test]
fn idle_restore_distinguishes_unchecked_history_from_confirmed_pending_checkpoints() {
    let fixture = fixture(false, false, true);
    let missing_id = "44444444-4444-4444-8444-444444444444";
    let busy_id = "55555555-5555-4555-8555-555555555555";
    let busy_path = fixture
        .parent_path
        .with_file_name(format!("{busy_id}.jsonl"));
    let busy_bytes = format!(
        "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{busy_id}\",\"history_mode\":\"legacy\"}}}}\n\
         {{\"type\":\"response_item\",\"payload\":{{\"type\":\"reasoning\",\"encrypted_content\":\"native-ciphertext\"}}}}\n"
    );
    fs::write(&busy_path, &busy_bytes).unwrap();
    let connection = Connection::open(fixture.home.join("state_1.sqlite")).unwrap();
    for (id, path) in [
        (missing_id, fixture.home.join("sessions/deleted.jsonl")),
        (busy_id, busy_path.clone()),
    ] {
        connection
            .execute(
                "INSERT INTO threads (id, rollout_path, history_mode) VALUES (?1, ?2, 'legacy')",
                params![id, path.to_string_lossy()],
            )
            .unwrap();
    }
    drop(connection);
    let directory = fixture.home.join(LOCK_DIRECTORY);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join(COORDINATION_LOCK), []).unwrap();
    let writer = File::create(directory.join(format!("{busy_id}.lock"))).unwrap();
    writer.lock().unwrap();

    let (lock, report) = repair_idle_before_native_restore(&fixture.home).unwrap();
    let report = serde_json::to_value(report).unwrap();
    assert_eq!(
        report["threads_deferred"], 1,
        "Only the confirmed EMP checkpoint is pending"
    );
    assert_eq!(
        report["threads_skipped"], 2,
        "Busy or unavailable files have not been classified"
    );
    assert_eq!(report["skipped_reasons"]["active_codex_writer"], 1);
    assert_eq!(report["skipped_reasons"]["lineage_source_missing"], 1);
    assert_eq!(report["deferred_reasons"]["unfinished_turn_in_history"], 1);
    assert_eq!(fs::read(&busy_path).unwrap(), busy_bytes.as_bytes());
    assert_eq!(
        fs::read(&fixture.parent_path).unwrap(),
        fixture.parent_plain
    );
    drop((lock, writer));
}

#[test]
fn stale_paths_find_existing_history_and_a_later_restore_discovers_returned_history() {
    let fixture = fixture(false, true, false);
    let missing = fixture.home.join("sessions/removed.jsonl");
    Connection::open(fixture.home.join("state_1.sqlite"))
        .unwrap()
        .execute(
            "UPDATE threads SET rollout_path = ?1",
            params![missing.to_string_lossy()],
        )
        .unwrap();
    let child = fixture.child_path.as_ref().unwrap();
    let saved_child = fs::read(child).unwrap();
    fs::remove_file(child).unwrap();
    let directory = fixture.home.join(LOCK_DIRECTORY);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join(COORDINATION_LOCK), []).unwrap();

    let (lock, report) = repair_idle_before_native_restore(&fixture.home).unwrap();
    assert_eq!(
        (
            report.threads_repaired,
            report.threads_deferred,
            report.threads_skipped
        ),
        (1, 0, 1)
    );
    assert_eq!(
        report.skipped_reasons.get("lineage_source_missing"),
        Some(&1)
    );
    drop(lock);

    fs::write(child, saved_child).unwrap();
    let (_, report) = repair_idle_before_native_restore(&fixture.home).unwrap();
    assert_eq!((report.threads_repaired, report.threads_skipped), (1, 0));
    let repaired = records(&fs::read(child).unwrap());
    assert!(
        streamed_history(&repaired, latest_checkpoint(&repaired))
            .iter()
            .any(|item| {
                item.to_string()
                    .contains("Portable EMP summary: keep the task plan")
            })
    );
}

#[test]
fn ambiguous_stale_paths_never_choose_or_rewrite_a_duplicate() {
    let fixture = fixture(false, false, false);
    let duplicate = fixture
        .home
        .join("archived_sessions")
        .join(format!("rollout-2026-10-22-{PARENT_ID}.jsonl"));
    fs::create_dir_all(duplicate.parent().unwrap()).unwrap();
    fs::write(&duplicate, &fixture.parent_plain).unwrap();
    Connection::open(fixture.home.join("state_1.sqlite"))
        .unwrap()
        .execute(
            "UPDATE threads SET rollout_path = 'sessions/missing.jsonl'",
            [],
        )
        .unwrap();
    let error = repair_before_native_restore(&fixture.home).unwrap_err();
    assert_eq!(error.reason(), "lineage_source_ambiguous");
    assert_eq!(
        fs::read(&fixture.parent_path).unwrap(),
        fixture.parent_plain
    );
    assert_eq!(fs::read(duplicate).unwrap(), fixture.parent_plain);
}

#[cfg(unix)]
#[test]
fn stale_path_discovery_rejects_symlinks_without_touching_either_history() {
    let fixture = fixture(false, false, false);
    let linked = fixture.home.join("sessions/linked-history.jsonl");
    std::os::unix::fs::symlink(&fixture.parent_path, &linked).unwrap();
    Connection::open(fixture.home.join("state_1.sqlite"))
        .unwrap()
        .execute(
            "UPDATE threads SET rollout_path = 'sessions/missing.jsonl'",
            [],
        )
        .unwrap();
    let error = repair_before_native_restore(&fixture.home).unwrap_err();
    assert_eq!(error.reason(), "session_directory_unsafe");
    assert_eq!(
        fs::read(&fixture.parent_path).unwrap(),
        fixture.parent_plain
    );
    assert!(
        fs::symlink_metadata(linked)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
