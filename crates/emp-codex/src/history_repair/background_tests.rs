use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn worker(fixture: &Fixture) -> IdleHistoryRepair {
    let directory = fixture.home.join(LOCK_DIRECTORY);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join(COORDINATION_LOCK), []).unwrap();
    IdleHistoryRepair::new(&fixture.home, Arc::new(AtomicBool::new(false))).unwrap()
}

#[test]
fn background_converts_one_idle_thread_at_a_time_and_reuses_inspection() {
    for compressed in [false, true] {
        let fixture = fixture(compressed, true, false);
        let mut worker = worker(&fixture);
        assert_eq!(worker.step().unwrap().unwrap().threads_repaired, 1);
        assert_eq!(
            fs::read(fixture.child_path.as_ref().unwrap()).unwrap(),
            *fixture.child_plain.as_ref().unwrap()
        );
        assert_eq!(worker.step().unwrap().unwrap().threads_repaired, 1);
        assert!(worker.step().unwrap().is_none());
        for id in [PARENT_ID, CHILD_ID] {
            let snapshot = resumed_snapshot(&fixture.home, id);
            assert!(snapshot.items.iter().any(|item| {
                item.content
                    .to_string()
                    .contains("Portable EMP summary: keep the task plan")
            }));
            assert!(
                !snapshot
                    .items
                    .iter()
                    .any(|item| item.content.to_string().contains("emp1:"))
            );
            let info = inventory::threads(&fixture.home, Some(id))
                .unwrap()
                .unwrap()
                .remove(0);
            let bytes = fs::read(info.path).unwrap();
            let bytes = if compressed && id == PARENT_ID {
                zstd::stream::decode_all(bytes.as_slice()).unwrap()
            } else {
                bytes
            };
            let records = records(&bytes);
            let history = streamed_history(&records, latest_checkpoint(&records));
            assert!(
                history
                    .iter()
                    .any(|item| item["call_id"] == "call_parent_tool")
            );
            assert!(
                history
                    .iter()
                    .any(|item| item["encrypted_content"] == "native-ciphertext-must-stay-opaque")
            );
        }
        worker.refresh().unwrap();
        while let Some(report) = worker.step().unwrap() {
            assert_eq!(report.threads_repaired, 0);
        }
        worker.refresh().unwrap();
        assert!(
            worker.step().unwrap().is_none(),
            "unchanged histories should not be scanned again"
        );
        let original = if compressed {
            zstd::stream::decode_all(fs::read(&fixture.parent_path).unwrap().as_slice()).unwrap()
        } else {
            fs::read(&fixture.parent_path).unwrap()
        };
        assert!(original.starts_with(&fixture.parent_plain));
    }
}

#[test]
fn background_scans_without_coordination_and_rechecks_live_descendants_at_commit() {
    let fixture = fixture(false, true, false);
    let mut worker = worker(&fixture);
    let info = inventory::threads(&fixture.home, Some(PARENT_ID))
        .unwrap()
        .unwrap()
        .remove(0);
    let coordination = acquire_coordination_lock(&fixture.home, false).unwrap();
    let staged = worker.stage(&info).unwrap().unwrap();
    assert!(
        pending_manifests(&fixture.home, &fixture.home.join(REPAIR_DIRECTORY))
            .unwrap()
            .is_empty()
    );
    drop(coordination);
    let writer = File::create(
        fixture
            .home
            .join(LOCK_DIRECTORY)
            .join(format!("{CHILD_ID}.lock")),
    )
    .unwrap();
    writer.lock().unwrap();
    let report = worker.commit(staged).unwrap();
    assert_eq!((report.threads_repaired, report.threads_skipped), (0, 1));
    assert_eq!(
        fs::read(&fixture.parent_path).unwrap(),
        fixture.parent_plain
    );
    assert_eq!(
        fs::read(fixture.child_path.as_ref().unwrap()).unwrap(),
        *fixture.child_plain.as_ref().unwrap()
    );
    while let Some(report) = worker.step().unwrap() {
        assert_eq!(report.threads_repaired, 0);
    }
    drop(writer);
    worker.refresh().unwrap();
    let mut repaired = 0;
    while let Some(report) = worker.step().unwrap() {
        repaired += report.threads_repaired;
    }
    assert_eq!(repaired, 2);
}

#[test]
fn background_does_not_overwrite_history_changed_after_scanning() {
    let fixture = fixture(false, false, false);
    let worker = worker(&fixture);
    let info = inventory::threads(&fixture.home, Some(PARENT_ID))
        .unwrap()
        .unwrap()
        .remove(0);
    let staged = worker.stage(&info).unwrap().unwrap();
    let mut changed = fixture.parent_plain.clone();
    append_record(
        &mut changed,
        18,
        record(
            "event_msg",
            json!({"type":"thread_name_updated","name":"keep newer history"}),
        ),
    );
    fs::write(&fixture.parent_path, &changed).unwrap();
    assert_eq!(
        worker.commit(staged).unwrap_err().reason(),
        "repair_stale_snapshot"
    );
    assert_eq!(fs::read(&fixture.parent_path).unwrap(), changed);
    assert_eq!(
        fs::read_dir(fixture.home.join(REPAIR_DIRECTORY))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn background_refresh_discovers_a_new_archive_directory() {
    let fixture = fixture(false, false, false);
    let mut worker = worker(&fixture);
    let archive = fixture.home.join("archived_sessions");
    fs::create_dir(&archive).unwrap();
    let path = archive.join(format!("{PARENT_ID}.jsonl"));
    fs::rename(&fixture.parent_path, &path).unwrap();
    Connection::open(fixture.home.join("state_1.sqlite"))
        .unwrap()
        .execute(
            "UPDATE threads SET rollout_path = ?1 WHERE id = ?2",
            params![path.to_string_lossy(), PARENT_ID],
        )
        .unwrap();
    worker.refresh().unwrap();
    assert_eq!(worker.step().unwrap().unwrap().threads_repaired, 1);
    assert!(fs::read(path).unwrap().starts_with(&fixture.parent_plain));
}

#[test]
fn background_cancellation_discards_staged_files_without_changing_history() {
    let fixture = fixture(false, false, false);
    let _ = worker(&fixture);
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut worker = IdleHistoryRepair::new(&fixture.home, Arc::clone(&shutdown)).unwrap();
    let info = inventory::threads(&fixture.home, Some(PARENT_ID))
        .unwrap()
        .unwrap()
        .remove(0);
    let staged = worker.stage(&info).unwrap().unwrap();
    shutdown.store(true, Ordering::Release);
    assert_eq!(
        worker.commit(staged).unwrap_err().reason(),
        "history_repair_cancelled"
    );
    assert_eq!(
        worker.step().unwrap_err().reason(),
        "history_repair_cancelled"
    );
    assert_eq!(
        fs::read(&fixture.parent_path).unwrap(),
        fixture.parent_plain
    );
    assert_eq!(
        fs::read_dir(fixture.home.join(REPAIR_DIRECTORY))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn closed_failed_turns_remain_readable_after_background_conversion() {
    let fixture = fixture(false, false, false);
    let mut bytes = fixture.parent_plain.clone();
    append_record(
        &mut bytes,
        18,
        record(
            "event_msg",
            json!({"type":"task_started","turn_id":"failed"}),
        ),
    );
    append_record(
        &mut bytes,
        19,
        record(
            "event_msg",
            json!({"type":"task_complete","turn_id":"failed","error":{"message":"upstream unavailable"}}),
        ),
    );
    fs::write(&fixture.parent_path, &bytes).unwrap();
    let mut worker = worker(&fixture);
    let report = worker.step().unwrap().unwrap();
    assert_eq!((report.threads_repaired, report.threads_deferred), (1, 0));
    assert!(fs::read(&fixture.parent_path).unwrap().starts_with(&bytes));
    let records = records(&fs::read(&fixture.parent_path).unwrap());
    let history = streamed_history(&records, latest_checkpoint(&records));
    assert!(history.iter().any(|item| {
        item["content"]
            .to_string()
            .contains("Portable EMP summary: keep the task plan")
    }));
    assert!(
        records
            .iter()
            .rev()
            .take(4)
            .any(|item| item["payload"]["error"]["message"] == "upstream unavailable")
    );
}
