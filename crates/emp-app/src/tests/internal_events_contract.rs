//! Observable command receipts, management events and privacy-safe failures.
use super::*;

pub(crate) fn live_journal(root: &Path) -> Vec<Value> {
    read_journal(root, true)
}

pub(super) fn final_journal(root: &Path) -> Vec<Value> {
    read_journal(root, false)
}

fn read_journal(root: &Path, complete_lines_only: bool) -> Vec<Value> {
    std::fs::read_dir(root.join("state/logs"))
        .unwrap()
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            bytes
                .split_inclusive(|&byte| byte == b'\n')
                .filter(|line| !complete_lines_only || line.ends_with(b"\n"))
                .map(|line| {
                    serde_json::from_slice(line).unwrap_or_else(|error| {
                        panic!(
                            "invalid diagnostic journal record in {}: {error}",
                            path.display()
                        )
                    })
                })
                .collect::<Vec<Value>>()
        })
        .collect()
}

#[test]
fn final_journal_reads_a_complete_record_without_its_newline() {
    let root = tempfile::tempdir().unwrap();
    let logs = root.path().join("state/logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("events.jsonl"),
        b"{\"id\":1}\n{\"marker\":\"PRIVATE\"}",
    )
    .unwrap();

    assert_eq!(live_journal(root.path()), [json!({"id": 1})]);
    assert_eq!(
        final_journal(root.path()),
        [json!({"id": 1}), json!({"marker": "PRIVATE"})]
    );
}

#[test]
#[should_panic(expected = "invalid diagnostic journal record")]
fn final_journal_rejects_a_corrupt_last_record() {
    let root = tempfile::tempdir().unwrap();
    let logs = root.path().join("state/logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join("events.jsonl"), b"{\"id\":1}\n{\"marker\":\"\xe4").unwrap();

    assert_eq!(live_journal(root.path()), [json!({"id": 1})]);
    final_journal(root.path());
}

#[test]
fn http_journal_pairs_receipts_and_omits_query_identity_and_content() {
    let (directory, server) = test_server();
    assert!(request(&server, "/healthz?private-query", &[]).starts_with("HTTP/1.1 200"));
    assert!(
        post(
            &server,
            "/api/accounts/private-account/quota",
            br#"{"secret":"private-body"}"#,
            &["Authorization: Bearer private-header"]
        )
        .starts_with("HTTP/1.1 401")
    );
    let mut malformed = TcpStream::connect(server.local_addr()).unwrap();
    malformed.write_all(b"malformed\r\n\r\n").unwrap();
    assert!(complete_response(&mut malformed).starts_with("HTTP/1.1 400"));
    server.shutdown().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let records = loop {
        let records = final_journal(directory.path());
        if records
            .iter()
            .filter(|record| record["event"] == "http_request_completed")
            .count()
            == 3
        {
            break records;
        }
        assert!(Instant::now() < deadline, "missing completed HTTP receipts");
        #[expect(
            clippy::disallowed_methods,
            reason = "HTTP 完成回执异步写入诊断日志，以固定间隔轮询"
        )]
        thread::sleep(Duration::from_millis(5));
    };
    let text = serde_json::to_string(&records).unwrap();
    for private in [
        "private-query",
        "private-account",
        "private-body",
        "private-header",
    ] {
        assert!(!text.contains(private), "journal disclosed {private}");
    }
    for (status, path) in [
        (200, "/healthz"),
        (401, "/api/accounts/{account}/quota"),
        (400, "/unknown"),
    ] {
        let done = records
            .iter()
            .find(|record| {
                record["event"] == "http_request_completed" && record["fields"]["status"] == status
            })
            .unwrap();
        assert_eq!(done["fields"]["path"], path);
        let id = &done["fields"]["request_id"];
        assert!(id.as_str().is_some_and(|id| id.len() == 16));
        assert_eq!(
            records
                .iter()
                .filter(|record| record["event"] == "http_request_started"
                    && &record["fields"]["request_id"] == id)
                .count(),
            1
        );
    }
}

#[test]
fn startup_failures_are_recorded_before_backend_or_listener_exists() {
    for invalid_config in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.json");
        std::fs::write(
            &config,
            if invalid_config {
                b"private-bad-json".as_slice()
            } else {
                b"{}".as_slice()
            },
        )
        .unwrap();
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let result = ServerHandle::start_with_config(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            occupied.local_addr().unwrap().port(),
            &config,
        );
        assert!(result.is_err());
        let records = final_journal(directory.path());
        let failure = records
            .iter()
            .find(|record| record["event"] == "startup_failure")
            .unwrap();
        assert_eq!(
            failure["fields"]["error_class"],
            if invalid_config {
                "config_error"
            } else {
                "io_error"
            }
        );
        assert!(
            !serde_json::to_string(&records)
                .unwrap()
                .contains("private-bad-json")
        );
        assert!(
            !records
                .iter()
                .any(|record| record["event"] == "service_listening")
        );
    }
}

#[test]
fn history_scan_has_a_worker_receipt_and_publishes_completion_over_sse() {
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let home = root.join("codex");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::write(home.join("sessions/rollout-fixture.jsonl"), b"{}\n").unwrap();
    // Public price fetching is forced through a closed loopback proxy. This
    // fixture cannot call a real provider, price server or quota account.
    let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let proxy = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let client = emp_transport::HttpClient::new(emp_transport::HttpClientPolicy::new(
        emp_transport::ProxyPolicy::explicit(Some(proxy)),
        emp_transport::TimeoutPolicy::default(),
    ))
    .unwrap();
    let server = ServerHandle::start_with_config_options_and_http_client(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &root.join("config.json"),
        "missing-test-codex",
        home.join("auth.json"),
        client,
    )
    .unwrap();
    let session = session_header(&server);
    let mut events = TcpStream::connect(server.local_addr()).unwrap();
    events
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        events,
        "GET /api/accounts/events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{session}\r\n\r\n",
        server.local_addr().port()
    )
    .unwrap();
    let (head, mut reader) = read_sse_head(BufReader::new(events));
    assert!(head.starts_with("HTTP/1.1 200"));
    // Queue before starting the worker: admission must not imply completion.
    let queued = post(&server, "/api/usage/scan", b"{}", &[&session]);
    assert!(queued.starts_with("HTTP/1.1 202"));
    let status: Value = serde_json::from_str(queued.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(status["command_id"], 1);
    assert_eq!(status["acknowledged"], 0);
    assert_eq!(status["completed"], 0);
    let workers = crate::services::usage::workers(&server.state).unwrap();
    let completed = loop {
        let frame = read_sse_frame(&mut reader);
        if frame.contains("event: usage-updated") {
            let reply = request(&server, "/api/usage", &[&session]);
            let payload: Value =
                serde_json::from_str(reply.split_once("\r\n\r\n").unwrap().1).unwrap();
            if payload["history"]["completed"] == 1 {
                break payload["history"].clone();
            }
        }
    };
    assert_eq!(completed["acknowledged"], 1);
    assert_eq!(completed["files"], 1);
    assert_eq!(completed["errors"], 0);
    assert_eq!(completed["queued"], false);
    let queued = post(&server, "/api/usage/scan", b"{}", &[&session]);
    let receipt: Value = serde_json::from_str(queued.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(receipt["command_id"], 2);
    loop {
        if read_sse_frame(&mut reader).contains("event: usage-updated") {
            let reply = request(&server, "/api/usage", &[&session]);
            let payload: Value =
                serde_json::from_str(reply.split_once("\r\n\r\n").unwrap().1).unwrap();
            if payload["history"]["completed"] == 2 {
                break;
            }
        }
    }
    server.state.backend.usage.stop();
    assert!(post(&server, "/api/usage/scan", b"{}", &[&session]).starts_with("HTTP/1.1 503"));
    server.state.request_shutdown();
    server.state.backend.usage.stop();
    for worker in workers {
        worker.join().unwrap();
    }
    drop(reader);
    server.shutdown().unwrap();
    let records = final_journal(&root);
    assert!(
        records
            .iter()
            .filter(|record| record["event"] == "price_refresh_started")
            .count()
            <= 1,
        "manual scan must not interrupt the price-refresh backoff"
    );
    for event in [
        "usage_scan_queued",
        "usage_scan_started",
        "usage_scan_finished",
    ] {
        assert!(
            records
                .iter()
                .any(|record| record["event"] == event && record["fields"]["command_id"] == 1)
        );
    }
}
