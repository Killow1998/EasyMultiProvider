use super::event_gates::EventGate;
use super::*;
use emp_test_support::{Deadline, Observation, test_timeout};
use std::time::Duration;

fn hold_event(gate: &EventGate, name: &'static str) -> (Receiver<()>, Sender<()>) {
    let (entered, writing) = mpsc::channel();
    let (resume, release) = mpsc::channel();
    gate.arm(name, move || {
        let _ = entered.send(());
        let _ = release.recv();
    });
    (writing, resume)
}

fn channel(directory: PathBuf) -> (Journal, Receiver<Command>) {
    let journal = Journal::new(directory);
    let receiver = journal.queue.lock().unwrap().pending.take().unwrap();
    (journal, receiver)
}

#[cfg(unix)]
#[test]
fn writer_uses_resolved_service_lock_directory() {
    use std::{env, fs, process::Command};

    const CHILD: &str = "EMP_JOURNAL_TEST_RESOLVED_PATHS";
    if env::var_os(CHILD).is_none() {
        let directory = emp_test_support::fixture_dir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let home = root.join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(root.join("writer")).unwrap();
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::journal::tests::writer_uses_resolved_service_lock_directory",
            ])
            .current_dir(&root)
            .env("HOME", home)
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let root = env::current_dir().unwrap();
    let home = PathBuf::from(env::var_os("HOME").unwrap());
    for (input, expected) in [
        ("~/state", home.join("state")),
        ("relative-state", root.join("relative-state")),
    ] {
        env::set_current_dir(&root).unwrap();
        let input = Path::new(input);
        let owner = ServiceOwner::acquire(input).unwrap();
        assert!(expected.join("service.lock").is_file());
        let mut journal = Journal::new(input.join("logs"));
        env::set_current_dir(root.join("writer")).unwrap();
        journal.attach(Arc::new(owner));
        journal.close_with_event_blocking("info", "shutdown_complete", &Value::Null);

        let shards = parts(&expected.join("logs"));
        assert_eq!(shards.len(), 1);
        assert!(
            fs::read_to_string(&shards[0].2)
                .unwrap()
                .contains("shutdown_complete")
        );
        assert!(!input.exists());
    }
    assert!(!root.join("~/state").exists());
}

#[test]
fn read_before_attachment_preserves_startup_records_in_the_first_part() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let logs = root.join("logs");
    let mut journal = Journal::new(logs.clone());
    let startup = [
        ("process_start", json!({"version":"fixture"})),
        ("descriptor_budget", json!({"soft_limit":128})),
    ];
    for (name, fields) in &startup {
        journal.event("info", name, fields);
    }
    let (observations, probes) = journal.read_observations(64);
    assert!(observations.is_empty() && probes.is_empty());
    assert!(!logs.exists());

    journal.attach(Arc::new(ServiceOwner::acquire(&root).unwrap()));
    journal.close_with_event_blocking("info", "shutdown_complete", &Value::Null);

    let shards = parts(&logs);
    assert_eq!(shards.len(), 1);
    let records: Vec<Value> = std::fs::read_to_string(&shards[0].2)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), startup.len() + 1);
    for (index, (name, fields)) in startup.into_iter().enumerate() {
        assert_eq!(records[index]["event"], name);
        assert_eq!(records[index]["fields"], fields);
        assert_eq!(records[index]["sequence"], index + 1);
    }
    assert_eq!(records.last().unwrap()["event"], "shutdown_complete");
}

#[test]
fn full_queue_attaches_loss_to_next_admitted_event_and_flush() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut journal = Journal::new(root.join("logs"));
    let timeout = test_timeout(Duration::from_secs(5)).duration();
    let (writing, resume) = hold_event(&journal.event_gates.before_write, "first");
    journal.event("info", "first", &Value::Null);
    assert!(!journal.directory.exists());
    journal.attach(Arc::new(ServiceOwner::acquire(&root).unwrap()));
    writing.recv_timeout(timeout).unwrap();

    let (draining, finish) = hold_event(&journal.event_gates.before_write, "queued");
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
    let (journal, receiver) = channel(directory.path().canonicalize().unwrap());
    drop(receiver);
    journal.event("info", "first", &Value::Null);
    journal.event("info", "second", &Value::Null);
    assert_eq!(journal.confirm(None), 2);
    assert_eq!(journal.confirm(None), 0);
    assert_eq!(
        journal.confirm(Some(Event::new("info", "exit", &Value::Null))),
        1
    );
    assert_eq!(journal.confirm(None), 0);
}

#[test]
fn disconnect_before_dequeue_reports_the_accepted_record() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let (journal, receiver) = channel(directory.path().to_owned());
    let writer = Observation::spawn(move |probe| {
        probe.observe(|_| drop(receiver));
    });
    journal.event("info", "queued", &Value::Null);
    writer
        .begin(
            Deadline::after(test_timeout(Duration::from_secs(5))),
            "disconnect writer",
        )
        .wait()
        .unwrap();

    assert_eq!(journal.confirm(None), 1);
    assert_eq!(journal.confirm(None), 0);
}

#[test]
fn disconnect_before_flush_receipt_reports_unsettled_records() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let (journal, receiver) = channel(directory.path().to_owned());
    let journal = Arc::new(journal);
    let writer = Observation::spawn(move |probe| {
        probe.observe(|deadline| {
            let event = receiver.recv_timeout(deadline.remaining()).unwrap();
            assert!(matches!(event.action, Action::Event(_)));
            let flush = receiver.recv_timeout(deadline.remaining()).unwrap();
            assert!(matches!(
                flush.action,
                Action::Flush {
                    record: Some(_),
                    ..
                }
            ));
            drop(receiver);
        });
    });
    journal.event("info", "queued", &Value::Null);
    let flushing = Arc::clone(&journal);
    let caller = Observation::spawn(move |probe| {
        probe.observe(|_| flushing.confirm(Some(Event::new("info", "exit", &Value::Null))));
    });
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let result = caller.begin(deadline, "flush waits for its receipt");
    writer
        .begin(deadline, "disconnect after receiving flush")
        .wait()
        .unwrap();

    assert_eq!(result.wait().unwrap(), 2);
    assert_eq!(journal.confirm(None), 0);
}

#[test]
fn stopped_writer_counts_the_failed_record_and_later_records() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let logs = root.join("logs");
    std::fs::create_dir(&logs).unwrap();
    let (journal, receiver) = channel(logs);
    let mut writer = Writer::new(
        Arc::new(ServiceOwner::acquire(&root).unwrap()),
        journal.run_id.clone(),
        journal.event_gates.clone(),
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
    assert_eq!(journal.confirm(None), 0);
    drop(journal);
    worker.join().unwrap();
}

#[test]
fn concurrent_events_stamp_in_admission_order() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut journal = Journal::new(root.join("logs"));
    journal.attach(Arc::new(ServiceOwner::acquire(&root).unwrap()));
    let journal = Arc::new(journal);
    let first = Arc::clone(&journal);
    let producer = Observation::spawn(move |probe| {
        probe.observe(|_| first.event("info", "first_to_redact", &Value::Null));
    });
    let (redacted, resume) = hold_event(&journal.event_gates.before_enqueue, "first_to_redact");
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let first = producer.begin(deadline, "first producer enqueued");
    redacted.recv_timeout(deadline.remaining()).unwrap();
    journal.event("info", "first_to_enqueue", &Value::Null);
    resume.send(()).unwrap();
    first.wait().unwrap();
    journal.flush_blocking();

    let parts = parts(&journal.directory);
    assert_eq!(parts.len(), 1);
    let records: Vec<Value> = std::fs::read_to_string(&parts[0].2)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["event"], "first_to_enqueue");
    assert_eq!(records[1]["event"], "first_to_redact");
    assert_eq!(records[0]["sequence"], 1);
    assert_eq!(records[1]["sequence"], 2);
    assert!(records[0]["timestamp"].as_str().unwrap() <= records[1]["timestamp"].as_str().unwrap());
}
