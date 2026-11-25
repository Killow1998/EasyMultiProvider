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
fn worker_start_failure_closes_admission_before_the_journal_receipt() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_path = root.join("config.json");
    let (entered, writing) = mpsc::channel();
    let (resume, release) = mpsc::channel();
    let startup = Observation::spawn(move |probe| {
        probe.observe(|_| {
            ServerHandle::start_with_workers(
                StartupContext {
                    host: "127.0.0.1".parse().unwrap(),
                    port: 0,
                    config_path: &config_path,
                    codex_binary: "missing-test-codex",
                    native_auth_path: config_path.with_file_name("codex").join("auth.json"),
                    open_browser: false,
                    markers: UpdateStartupMarkers::default(),
                    http_client_override: None,
                    admission: ConnectionAdmissionConfig::default(),
                    enable_catalog_refresh_worker: false,
                    external_key: false,
                    official_models: emp_state::official_registry::bundled_snapshot,
                },
                |server, _| {
                    let state = Arc::clone(&server.state);
                    let address = server.local_addr;
                    server.state.backend.diagnostics.journal.gate_next_event(
                        "startup_failure",
                        move || {
                            let _ = entered.send((address, state));
                            let _ = release.recv();
                        },
                    );
                    Err(io::Error::from(io::ErrorKind::WouldBlock).into())
                },
            )
            .err()
        });
    });
    let resume = resume;
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let result = startup.begin(deadline, "worker startup failure returns after shutdown");
    let (address, state) = writing.recv_timeout(deadline.remaining()).unwrap();
    assert!(state.backend.diagnostics.journal.flush_pending_for_test());
    assert!(state.lifecycle.admission.acquire_request().is_none());
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
        matches!(result.wait().unwrap(), Some(AppError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock)
    );
    let records = crate::tests::live_journal(&root);
    assert!(
        records
            .iter()
            .any(|record| record["event"] == "startup_failure")
    );
    assert_eq!(records.last().unwrap()["event"], "shutdown_complete");
    assert!(ServiceOwner::acquire(&root.join("state")).is_ok());
}
