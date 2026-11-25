use super::*;
use emp_test_support::{Deadline, Observation, test_timeout};
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn first_journal_part_starts_with_buffered_events_in_order() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let server =
        ServerHandle::start_with_config("127.0.0.1".parse().unwrap(), 0, &root.join("config.json"))
            .unwrap();
    server.shutdown().unwrap();
    let mut parts = std::fs::read_dir(root.join("state/logs")).unwrap();
    let first = parts.next().unwrap().unwrap().path();
    assert!(
        first
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with("-p001.jsonl")
    );
    assert!(parts.next().is_none());
    let records: Vec<serde_json::Value> = std::fs::read_to_string(first)
        .unwrap()
        .lines()
        .take(3)
        .map(|record| serde_json::from_str(record).unwrap())
        .collect();
    assert_eq!(records[0]["event"], "process_start");
    assert_eq!(records[1]["event"], "descriptor_budget");
    assert_eq!(records[2]["event"], "proxy_selected");
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record["sequence"], index + 1);
    }
    assert!(records.windows(2).all(|pair| {
        pair[0]["timestamp"].as_str().unwrap() <= pair[1]["timestamp"].as_str().unwrap()
    }));
}

#[test]
fn contender_leaves_the_owners_journal_parts_unchanged() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = root.join("config.json");
    let host = "127.0.0.1".parse().unwrap();
    let owner = ServerHandle::start_with_config(host, 0, &config).unwrap();
    let journal = &owner.state.backend.diagnostics.journal;
    let (entered, writing) = mpsc::channel();
    let (resume, release) = mpsc::channel();
    journal.gate_next_event("hold_owner", move || {
        let _ = entered.send(());
        let _ = release.recv();
    });
    journal.event("info", "hold_owner", &serde_json::Value::Null);
    writing
        .recv_timeout(test_timeout(Duration::from_secs(5)).duration())
        .unwrap();

    let parts = || {
        std::fs::read_dir(root.join("state/logs"))
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), std::fs::read(entry.path()).unwrap())
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let before = parts();
    assert!(!before.is_empty());
    let contender = ServerHandle::start_with_config(host, 0, &config);
    let after = parts();
    resume.send(()).unwrap();
    owner.shutdown().unwrap();
    assert!(matches!(contender, Err(AppError::ServiceOwned)));
    assert_eq!(after, before);
}

#[test]
fn price_worker_start_failure_closes_admission_and_signals_usage_before_the_journal_receipt() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_path = root.join("config.json");
    let server =
        ServerHandle::start_with_config("127.0.0.1".parse().unwrap(), 0, &config_path).unwrap();
    let address = server.local_addr;
    let state = Arc::clone(&server.state);
    let journal = &state.backend.diagnostics.journal;
    let (scan_started, scanning) = mpsc::channel();
    journal.gate_next_event("usage_scan_started", move || {
        let _ = scan_started.send(());
    });
    let (entered, writing) = mpsc::channel();
    let (resume, release) = mpsc::channel();
    let startup = Observation::spawn(move |probe| {
        probe.observe(|&deadline| {
            let hooks = &server.state.lifecycle.test_hooks;
            *hooks.price_spawn_failure.lock().unwrap() = Some(Box::new(move |journal| {
                if let Err(error) = scanning.recv_timeout(deadline.remaining()) {
                    return io::Error::other(error);
                }
                journal.gate_next_event("startup_failure", move || {
                    let _ = entered.send(());
                    let _ = release.recv();
                });
                io::ErrorKind::WouldBlock.into()
            }));
            super::super::run_started_server(server, false, UpdateStartupMarkers::default())
        });
    });
    let resume = resume;
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let result = startup.begin(
        deadline,
        "price worker startup failure returns after shutdown",
    );
    let receipt = writing.recv_timeout(deadline.remaining());
    if receipt.is_err() {
        state.request_shutdown();
    }
    receipt.expect("startup_failure reached the journal writer");
    assert!(journal.flush_pending_for_test());
    assert!(state.lifecycle.admission.acquire_request().is_none());
    assert!(
        state
            .backend
            .usage
            .history
            .stop
            .load(std::sync::atomic::Ordering::Acquire)
    );
    let mut reply = Vec::new();
    let request = (|| -> io::Result<()> {
        let mut stream = deadline.io(deadline.connect(address)?);
        write!(
            stream,
            "GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
        )?;
        stream.read_to_end(&mut reply)?;
        Ok(())
    })();
    match request {
        Ok(()) => assert!(reply.is_empty(), "{reply:?}"),
        Err(error) => assert!(
            matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
            ),
            "{error}"
        ),
    }
    resume.send(()).unwrap();
    assert!(
        matches!(result.wait().unwrap(), Err(AppError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock)
    );
    let records = crate::tests::live_journal(&root);
    assert!(
        records
            .iter()
            .any(|record| record["event"] == "startup_failure"
                && record["fields"]["stage"] == "background_workers")
    );
    assert_eq!(records.last().unwrap()["event"], "shutdown_complete");
    assert!(ServiceOwner::acquire(&root.join("state")).is_ok());
}
