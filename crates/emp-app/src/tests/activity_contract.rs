//! Real HTTP and management SSE contracts for request activity.
use super::*;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::SyncSender;

struct HeldReply {
    status: u16,
    body: Vec<u8>,
}

struct HeldUpstream {
    address: SocketAddr,
    connections: usize,
    accepted: Arc<AtomicUsize>,
    observed: Receiver<(usize, String, BTreeMap<String, String>, Value)>,
    releases: Vec<SyncSender<HeldReply>>,
    worker: Option<JoinHandle<()>>,
}

impl HeldUpstream {
    fn start(connections: usize) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind held upstream");
        let address = listener.local_addr().expect("held upstream address");
        let (observed_sender, observed) = mpsc::sync_channel(connections.max(1));
        let (release_senders, release_receivers): (Vec<_>, Vec<_>) =
            (0..connections).map(|_| mpsc::sync_channel(1)).unzip();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_worker = Arc::clone(&accepted);
        let worker = thread::spawn(move || {
            let mut handlers = Vec::with_capacity(connections);
            for (index, release) in release_receivers.into_iter().enumerate() {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                accepted_worker.fetch_add(1, Ordering::AcqRel);
                let observed_sender = observed_sender.clone();
                handlers.push(thread::spawn(move || {
                    let (path, headers, body) = receive_upstream_request(&mut stream);
                    let _ = observed_sender.send((index, path, headers, body));
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
            connections,
            accepted,
            observed,
            releases: release_senders,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn observed(&self) -> (usize, String, BTreeMap<String, String>, Value) {
        self.observed
            .recv_timeout(Duration::from_secs(5))
            .expect("upstream received dispatched request")
    }

    fn release(&self, index: usize, status: u16, body: Vec<u8>) {
        self.releases[index]
            .send(HeldReply { status, body })
            .expect("release held upstream response");
    }
}

impl Drop for HeldUpstream {
    fn drop(&mut self) {
        for release in &self.releases {
            let _ = release.send(HeldReply {
                status: 200,
                body: success_response_body(),
            });
        }
        let accepted = self.accepted.load(Ordering::Acquire);
        for _ in accepted..self.connections {
            if let Ok(mut stream) = TcpStream::connect(self.address) {
                let _ =
                    stream.write_all(b"POST /v1/responses HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}");
            }
        }
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join held upstream");
        }
    }
}

struct CancelableUpstream {
    address: SocketAddr,
    observed: Receiver<(String, BTreeMap<String, String>, Value)>,
    closed: Receiver<bool>,
    worker: Option<JoinHandle<()>>,
}

impl CancelableUpstream {
    fn start() -> Self {
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind cancelable upstream");
        let address = listener.local_addr().expect("cancelable upstream address");
        let (observed_sender, observed) = mpsc::sync_channel(1);
        let (closed_sender, closed) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept cancelable request");
            let request = receive_upstream_request(&mut stream);
            let _ = observed_sender.send(request);
            stream
                .set_read_timeout(Some(Duration::from_millis(50)))
                .expect("set cancellation probe timeout");
            let deadline = Instant::now() + Duration::from_secs(5);
            let closed_by_client = loop {
                let mut byte = [0_u8; 1];
                match stream.peek(&mut byte) {
                    Ok(0) => break true,
                    Ok(_) => continue,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        if Instant::now() >= deadline {
                            break false;
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                        ) =>
                    {
                        break true;
                    }
                    Err(_) => break false,
                }
            };
            let _ = closed_sender.send(closed_by_client);
        });
        Self {
            address,
            observed,
            closed,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
}

impl Drop for CancelableUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join cancelable upstream");
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
    events: &mut BufReader<TcpStream>,
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
        } else if !frame.starts_with(':') {
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
        configured_protocol_server(&upstream.base_url(), "responses", "api_key");
    let session = session_header(&server);
    let mut events = open_quota_events(&server, &session);
    let body = activity_request_body(false);

    let mut first = open_post_stream(
        &server,
        "/v1/responses",
        &body,
        &[&session, "Content-Type: application/json"],
    );
    let (first_index, path, headers, upstream_body) = upstream.observed();
    assert_eq!(first_index, 0);
    assert_eq!(path, "/v1/responses");
    assert_eq!(headers["authorization"], "Bearer upstream-secret");
    assert_eq!(upstream_body["model"], "upstream-model");

    let mut second = open_post_stream(
        &server,
        "/v1/responses",
        &body,
        &[&session, "Content-Type: application/json"],
    );
    let (second_index, path, _, _) = upstream.observed();
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

    upstream.release(0, 200, success_response_body());
    upstream.release(1, 200, success_response_body());
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
        configured_protocol_server(&upstream.base_url(), "responses", "api_key");
    let session = session_header(&server);
    let mut events = open_quota_events(&server, &session);
    let mut downstream = open_post_stream(
        &server,
        "/v1/responses",
        &activity_request_body(false),
        &[&session, "Content-Type: application/json"],
    );
    let (_, path, _, _) = upstream.observed();
    assert_eq!(path, "/v1/responses");

    let active = next_activity_snapshot(&mut events, |snapshot| {
        snapshot["routes"][0]["in_flight"] == 1
    });
    assert_eq!(only_route(&active)["model_id"], "demo/model");
    upstream.release(
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
    let upstream = CancelableUpstream::start();
    let (_directory, server) =
        configured_protocol_server(&upstream.base_url(), "responses", "api_key");
    let session = session_header(&server);
    let mut events = open_quota_events(&server, &session);
    let downstream = open_post_stream(
        &server,
        "/v1/responses",
        &activity_request_body(true),
        &[&session, "Content-Type: application/json"],
    );
    let (path, _, _) = upstream
        .observed
        .recv_timeout(Duration::from_secs(5))
        .expect("stream request reached fake upstream");
    assert_eq!(path, "/v1/responses");

    let active = next_activity_snapshot(&mut events, |snapshot| {
        snapshot["routes"][0]["in_flight"] == 1
    });
    assert_eq!(only_route(&active)["model_id"], "demo/model");
    drop(downstream);
    assert!(
        upstream
            .closed
            .recv_timeout(Duration::from_secs(5))
            .expect("upstream observes client cancellation"),
        "downstream cancellation did not close the pending upstream request"
    );
    let finished = next_activity_snapshot(&mut events, |snapshot| {
        snapshot["routes"][0]["in_flight"] == 0
            && snapshot["routes"][0]["last_finished"].as_u64().is_some()
    });
    assert_eq!(only_route(&finished)["in_flight"], 0);
    server.shutdown().expect("shutdown activity test server");
}
