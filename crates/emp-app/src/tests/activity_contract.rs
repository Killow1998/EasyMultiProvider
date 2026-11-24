//! Real HTTP and management SSE contracts for request activity.
use super::*;
use std::sync::mpsc::SyncSender;

struct HeldReply {
    status: u16,
    body: Vec<u8>,
}

type ObservedRequest = (usize, String, BTreeMap<String, String>, Value);

struct HeldUpstream {
    address: SocketAddr,
    releases: Vec<SyncSender<HeldReply>>,
    worker: Option<JoinHandle<()>>,
}

impl HeldUpstream {
    fn start(connections: usize) -> Observation<ObservedRequest, Self> {
        Observation::with_worker(|probe| Self::serve(connections, probe))
    }

    fn serve(connections: usize, probe: Probe<ObservedRequest>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind held upstream");
        let address = listener.local_addr().expect("held upstream address");
        listener.set_nonblocking(true).unwrap();
        let (release_senders, release_receivers): (Vec<_>, Vec<_>) =
            (0..connections).map(|_| mpsc::sync_channel(1)).unzip();
        let worker = thread::spawn(move || {
            let mut handlers = Vec::with_capacity(connections);
            for (index, release) in release_receivers.into_iter().enumerate() {
                let Some(observed) = probe.next_request() else {
                    break;
                };
                let mut stream = accept_before(&listener, observed.deadline());
                let (path, headers, body) = receive_upstream_request(&mut stream);
                observed.complete((index, path, headers, body));
                handlers.push(thread::spawn(move || {
                    let reply = release.recv_timeout(Duration::from_secs(8)).unwrap_or_else(|_| {
                        HeldReply {
                            status: 200,
                            body: success_response_body(),
                        }
                    });
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                reply.status,
                                status_text(reply.status),
                                reply.body.len()
                            )
                            .as_bytes(),
                        )
                        .expect("write held upstream response head");
                    stream
                        .write_all(&reply.body)
                        .expect("write held upstream response body");
                }));
            }
            for handler in handlers {
                let _ = handler.join();
            }
        });
        Self {
            address,
            releases: release_senders,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn release(&self, index: usize, status: u16, body: Vec<u8>) {
        self.releases[index]
            .send(HeldReply { status, body })
            .expect("release held upstream response");
    }
}

impl Drop for HeldUpstream {
    fn drop(&mut self) {
        self.releases.clear();
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.expect("join held upstream");
            }
        }
    }
}

fn success_response_body() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id":"resp_activity_fixture",
        "object":"response",
        "status":"completed",
        "model":"upstream-model",
        "output":[{
            "id":"msg_activity_fixture",
            "type":"message",
            "status":"completed",
            "role":"assistant",
            "content":[{"type":"output_text","text":"fixture answer","annotations":[]}]
        }],
        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
    }))
    .expect("encode successful upstream response")
}

fn activity_request_body(stream: bool) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model":"demo/model",
        "input":"activity-prompt-must-not-appear-in-snapshot",
        "stream":stream
    }))
    .expect("encode activity request")
}

fn next_activity_snapshot(
    events: &mut SseReader,
    mut matches: impl FnMut(&Value) -> bool,
) -> Value {
    loop {
        let frame = read_sse_frame(events);
        if frame.starts_with("event: activity-updated\ndata: ") {
            let body = frame
                .strip_prefix("event: activity-updated\ndata: ")
                .expect("activity frame prefix")
                .trim();
            let snapshot: Value = serde_json::from_str(body).expect("activity frame JSON");
            if matches(&snapshot) {
                return snapshot;
            }
        } else if !frame.starts_with(':') && !frame.starts_with("event: usage-updated\ndata: ") {
            panic!("request activity emitted an unrelated management event: {frame}");
        }
    }
}

fn only_route(snapshot: &Value) -> &Value {
    let routes = snapshot["routes"].as_array().expect("snapshot routes");
    assert_eq!(
        routes.len(),
        1,
        "snapshot contains only the active public route"
    );
    &routes[0]
}

fn assert_public_route_shape(route: &Value) {
    assert_eq!(
        route
            .as_object()
            .expect("route object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "account_id",
            "in_flight",
            "last_finished",
            "model_id",
            "provider_id"
        ]
    );
}

#[test]
fn held_http_requests_report_public_route_and_concurrent_count_then_finish() {
    let upstream = HeldUpstream::start(2);
    let (_directory, server) =
        configured_protocol_server(&upstream.worker().base_url(), "responses", "api_key");
    let session = session_header(&server);
    let mut events = open_quota_events(&server, &session);
    let body = activity_request_body(false);

    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let observed = upstream.begin(deadline, "first activity request");
    let mut first = open_post_stream(
        &server,
        "/v1/responses",
        &body,
        &[&session, "Content-Type: application/json"],
    );
    let (first_index, path, headers, upstream_body) = observed.wait().unwrap();
    assert_eq!(first_index, 0);
    assert_eq!(path, "/v1/responses");
    assert_eq!(headers["authorization"], "Bearer upstream-secret");
    assert_eq!(upstream_body["model"], "upstream-model");

    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    set_sse_deadline(&mut events, &deadline);
    let observed = upstream.begin(deadline, "second activity request");
    let mut second = open_post_stream(
        &server,
        "/v1/responses",
        &body,
        &[&session, "Content-Type: application/json"],
    );
    let (second_index, path, _, _) = observed.wait().unwrap();
    assert_eq!(second_index, 1);
    assert_eq!(path, "/v1/responses");

    let active = next_activity_snapshot(&mut events, |snapshot| {
        snapshot["routes"][0]["in_flight"] == 2
    });
    let route = only_route(&active);
    assert_public_route_shape(route);
    assert_eq!(route["model_id"], "demo/model");
    assert_eq!(route["provider_id"], "demo");
    assert_eq!(route["account_id"], Value::Null);
    assert_eq!(route["in_flight"], 2);
    assert_eq!(route["last_finished"], Value::Null);
    let encoded = serde_json::to_string(&active).expect("encode activity snapshot");
    assert!(!encoded.contains("activity-prompt-must-not-appear"));
    assert!(!encoded.contains("upstream-secret"));
    assert!(!encoded.contains("127.0.0.1"));

    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    set_sse_deadline(&mut events, &deadline);
    upstream.worker().release(0, 200, success_response_body());
    upstream.worker().release(1, 200, success_response_body());
    let first_response = thread::spawn(move || response_until_close(&mut first));
    let second_response = thread::spawn(move || response_until_close(&mut second));
    assert!(
        first_response
            .join()
            .expect("join first request")
            .starts_with("HTTP/1.1 200 OK\r\n")
    );
    assert!(
        second_response
            .join()
            .expect("join second request")
            .starts_with("HTTP/1.1 200 OK\r\n")
    );

    let finished = next_activity_snapshot(&mut events, |snapshot| {
        snapshot["routes"][0]["in_flight"] == 0
            && snapshot["routes"][0]["last_finished"].as_u64().is_some()
    });
    assert_eq!(only_route(&finished)["in_flight"], 0);
    assert!(only_route(&finished)["last_finished"].as_u64().is_some());
    server.shutdown().expect("shutdown activity test server");
}

#[test]
fn upstream_error_finishes_the_activity_guard() {
    let upstream = HeldUpstream::start(1);
    let (_directory, server) =
        configured_protocol_server(&upstream.worker().base_url(), "responses", "api_key");
    let session = session_header(&server);
    let mut events = open_quota_events(&server, &session);
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    set_sse_deadline(&mut events, &deadline);
    let observed = upstream.begin(deadline, "activity request before upstream error");
    let mut downstream = open_post_stream(
        &server,
        "/v1/responses",
        &activity_request_body(false),
        &[&session, "Content-Type: application/json"],
    );
    let (_, path, _, _) = observed.wait().unwrap();
    assert_eq!(path, "/v1/responses");

    let active = next_activity_snapshot(&mut events, |snapshot| {
        snapshot["routes"][0]["in_flight"] == 1
    });
    assert_eq!(only_route(&active)["model_id"], "demo/model");
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    set_sse_deadline(&mut events, &deadline);
    upstream.worker().release(
        0,
        503,
        br#"{"error":{"message":"fixture upstream failure"}}"#.to_vec(),
    );
    let response = response_until_close(&mut downstream);
    assert!(!response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");

    let finished = next_activity_snapshot(&mut events, |snapshot| {
        snapshot["routes"][0]["in_flight"] == 0
            && snapshot["routes"][0]["last_finished"].as_u64().is_some()
    });
    assert_eq!(only_route(&finished)["in_flight"], 0);
    server.shutdown().expect("shutdown activity test server");
}

#[test]
fn client_cancellation_releases_a_streaming_activity_guard() {
    let upstream = OneShotUpstream::serve_observed(|stream, probe| {
        if probe.observe(|_| true) {
            probe.observe(|deadline| upstream_closed_before(stream, deadline));
        }
    });
    let (_directory, server) =
        configured_protocol_server(&upstream.worker().base_url(), "responses", "api_key");
    let session = session_header(&server);
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let observed = upstream.begin(deadline, "stream request reached fake upstream");
    let downstream = open_post_stream(
        &server,
        "/v1/responses",
        &activity_request_body(true),
        &[&session, "Content-Type: application/json"],
    );
    assert!(observed.wait().unwrap());
    let (path, _, _) = upstream.worker().observed();
    assert_eq!(path, "/v1/responses");

    // Dispatch has reached the held upstream. Observe cancellation separately
    // from SSE delivery, which the held-request contracts already exercise.
    let active = server
        .state
        .backend
        .activity
        .snapshot(crate::util::system_now() as u64);
    assert_eq!(only_route(&active)["in_flight"], 1);
    assert_eq!(only_route(&active)["model_id"], "demo/model");
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let cancellation = upstream.begin(deadline, "upstream cancellation");
    drop(downstream);
    assert!(
        cancellation.wait().unwrap(),
        "downstream cancellation did not close the pending upstream request"
    );
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let finished = loop {
        let snapshot = server
            .state
            .backend
            .activity
            .snapshot(crate::util::system_now() as u64);
        let route = only_route(&snapshot);
        if route["in_flight"] == 0 && route["last_finished"].as_u64().is_some() {
            break deadline.accept(snapshot, "activity guard release").unwrap();
        }
        assert!(!deadline.is_expired(), "activity guard was not released");
        #[expect(
            clippy::disallowed_methods,
            reason = "活动请求完成通过状态快照观察，以固定间隔轮询"
        )]
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(only_route(&finished)["in_flight"], 0);
    server.shutdown().expect("shutdown activity test server");
}

#[test]
fn completed_http_call_is_persisted_with_original_model_usage_and_delivery() {
    let upstream = HeldUpstream::start(1);
    let (_directory, server) =
        configured_protocol_server(&upstream.worker().base_url(), "responses", "api_key");
    let session = session_header(&server);
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let observed = upstream.begin(deadline, "upstream received activity request");
    let mut socket = open_post_stream(
        &server,
        "/v1/responses",
        &activity_request_body(false),
        &[
            &session,
            "Content-Type: application/json",
            "thread-id: fixture-thread",
        ],
    );
    let (index, _, _, _) = observed.wait().unwrap();
    let mut body: Value = serde_json::from_slice(&success_response_body()).unwrap();
    body["model"] = json!("reported-revision");
    upstream
        .worker()
        .release(index, 200, serde_json::to_vec(&body).unwrap());
    assert!(response_until_close(&mut socket).starts_with("HTTP/1.1 200"));
    assert!(request(&server, "/api/calls", &[]).starts_with("HTTP/1.1 401"));
    let response = request(&server, "/api/calls?start=0&end=9999999999", &[&session]);
    assert!(response.starts_with("HTTP/1.1 200"));
    let report: Value = serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let row = &report["records"][0];
    assert_eq!(row["client_model"], "demo/model");
    assert_eq!(row["upstream_model"], "upstream-model");
    assert_eq!(row["response_model"], "reported-revision");
    assert_eq!(row["model_name_status"], "different");
    assert_eq!(row["session_id"], "fixture-thread");
    assert_eq!(row["input_tokens"], 1);
    assert_eq!(row["output_tokens"], 1);
    assert_eq!(row["ttft_ms"], Value::Null);
    assert_eq!(row["delivery"]["delivery"], "terminal_written");
    assert!(
        !report
            .to_string()
            .contains("activity-prompt-must-not-appear")
    );
    assert!(!report.to_string().contains("upstream-secret"));
}

#[test]
fn real_sse_records_raw_model_conflicts_reasoning_ttft_and_cache_usage() {
    let upstream = HeldUpstream::start(1);
    let (_directory, server) =
        configured_protocol_server(&upstream.worker().base_url(), "responses", "api_key");
    let session = session_header(&server);
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let observed = upstream.begin(deadline, "upstream received activity request");
    let mut socket = open_post_stream(
        &server,
        "/v1/responses",
        &activity_request_body(true),
        &[&session, "Content-Type: application/json"],
    );
    let (index, _, _, _) = observed.wait().unwrap();
    let mut terminal: Value = serde_json::from_slice(&success_response_body()).unwrap();
    terminal["model"] = json!("terminal-model");
    terminal["usage"] =
        json!({"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":50}});
    let events = [
        json!({"type":"response.created","response":{"id":"resp_activity_fixture","object":"response","status":"in_progress","model":"first-model","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"reasoning_fixture","type":"reasoning","summary":[]}}),
        json!({"type":"response.completed","response":terminal}),
    ];
    let raw = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {}\n\n",
                event["type"].as_str().unwrap(),
                event
            )
        })
        .collect::<String>();
    #[expect(
        clippy::disallowed_methods,
        reason = "固定延迟上游完成响应，保留活动时长计量窗口"
    )]
    thread::sleep(Duration::from_millis(20));
    upstream.worker().release(index, 200, raw.into_bytes());
    let delivered = response_until_close(&mut socket);
    assert!(delivered.contains("response.completed"), "{delivered}");
    let wire = request(&server, "/api/calls?start=0&end=9999999999", &[&session]);
    let report: Value = serde_json::from_str(wire.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let row = &report["records"][0];
    assert_eq!(row["response_model"], "terminal-model");
    assert_eq!(row["model_name_status"], "conflict");
    assert!(row["ttft_ms"].is_number());
    assert!(row["duration_ms"].as_u64().unwrap() >= 20);
    assert_eq!(row["input_tokens"], 100);
    assert_eq!(row["output_tokens"], 20);
    assert_eq!(row["cached_input_tokens"], 50);
    assert_eq!(row["model_declarations"].as_array().unwrap().len(), 2);
}
