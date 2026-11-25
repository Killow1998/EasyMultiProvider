use super::*;
use emp_test_support::test_timeout;
use std::time::Duration;

fn hold_event(journal: &Journal, name: &'static str) -> (Receiver<()>, Sender<()>) {
    let (entered, writing) = mpsc::channel();
    let (resume, release) = mpsc::channel();
    journal.gate_next_event(name, move || {
        let _ = entered.send(());
        let _ = release.recv();
    });
    (writing, resume)
}

#[test]
fn full_queue_attaches_loss_to_next_admitted_event_and_flush() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let journal = Journal::new(directory.path().canonicalize().unwrap());
    let timeout = test_timeout(Duration::from_secs(5));
    let (writing, resume) = hold_event(&journal, "first");
    journal.event("info", "first", &Value::Null);
    writing.recv_timeout(timeout).unwrap();

    let (draining, finish) = hold_event(&journal, "queued");
    for _ in 0..QUEUE_CAPACITY {
        journal.event("info", "queued", &Value::Null);
    }
    journal.event("info", "overflow", &Value::Null);
    resume.send(()).unwrap();
    draining.recv_timeout(timeout).unwrap();
    journal.event("info", "next_admitted", &Value::Null);
    journal.event("info", "tail_overflow", &Value::Null);
    finish.send(()).unwrap();

    assert_eq!(journal.confirm(None), 2);
    assert_eq!(journal.confirm(None), 0);
    let parts = parts(&journal.directory);
    assert_eq!(parts.len(), 1);
    let records: Vec<Value> = std::fs::read_to_string(&parts[0].2)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), QUEUE_CAPACITY + 4);
    assert_eq!(records[0]["event"], "first");
    assert!(
        records[1..=QUEUE_CAPACITY]
            .iter()
            .all(|record| record["event"] == "queued")
    );
    for (index, event) in [
        "journal_records_dropped",
        "next_admitted",
        "journal_records_dropped",
    ]
    .into_iter()
    .enumerate()
    {
        let sequence = QUEUE_CAPACITY + index + 2;
        let record = &records[sequence - 1];
        assert_eq!(record["event"], event);
        assert_eq!(record["sequence"], sequence);
        if event == "journal_records_dropped" {
            assert_eq!(record["fields"]["count"], 1);
        }
    }
}

#[test]
fn disconnected_writer_counts_drops_until_flush() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let (journal, receiver) = Journal::channel(directory.path().canonicalize().unwrap());
    drop(receiver);
    journal.event("info", "first", &Value::Null);
    journal.event("info", "second", &Value::Null);
    assert_eq!(journal.confirm(None), 2);
    assert_eq!(journal.confirm(None), 0);
    assert_eq!(
        journal.confirm(Some(Event::new("info", "exit", &Value::Null))),
        1
    );
}

#[test]
fn stopped_writer_counts_the_failed_record_and_later_records() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let (journal, receiver) = Journal::channel(directory.path().canonicalize().unwrap());
    let mut writer = Writer::new(
        journal.directory.clone(),
        journal.run_id.clone(),
        journal.event_gate.clone(),
    );
    writer.bytes = MAX_PART;
    std::fs::create_dir(writer.path(2)).unwrap();
    let queue = Arc::clone(&journal.queue);
    let worker = std::thread::spawn(move || writer.run(receiver, &queue));
    journal.event("info", "rotation_fails", &Value::Null);
    journal.event("info", "after_failure", &Value::Null);
    assert_eq!(journal.confirm(None), 2);
    assert_eq!(journal.confirm(None), 0);
    journal.event("info", "still_stopped", &Value::Null);
    assert_eq!(journal.confirm(None), 1);
    drop(journal);
    worker.join().unwrap();
}
