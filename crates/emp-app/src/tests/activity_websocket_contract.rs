//! Real WebSocket activity lifecycle checks.
use super::*;
use std::sync::mpsc::Sender;

type ObservedRequest = (String, BTreeMap<String, String>, Value);

struct HeldActivityUpstream {
    address: SocketAddr,
    release: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl HeldActivityUpstream {
    fn start(request_count: usize) -> Observation<ObservedRequest, Self> {
        Observation::with_worker(|probe| Self::serve(request_count, probe))
    }

    fn serve(request_count: usize, probe: Probe<ObservedRequest>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind held upstream");
        let address = listener.local_addr().expect("held upstream address");
        listener.set_nonblocking(true).unwrap();
        let (release, releases) = mpsc::channel();
        let worker = thread::spawn(move || {
            for index in 0..request_count {
                let Some(observed) = probe.next_request() else {
                    break;
                };
                let mut stream = accept_before(&listener, observed.deadline());
                let request = receive_upstream_request(&stream, observed.deadline());
                observed.complete(request);

                let id = format!("resp_activity_{index}");
                let created = json!({
                    "type":"response.created",
                    "response":{"id":id,"object":"response","status":"in_progress","model":"upstream-model"}
                });
                let completed = json!({
                    "type":"response.completed",
                    "response":{"id":id,"object":"response","status":"completed","model":"upstream-model","output":[]}
                });
                let prefix = format!(
                    "event: response.created\ndata: {}\n\n",
                    serde_json::to_string(&created).expect("encode created event")
                );
                let terminal = format!(
                    "event: response.completed\ndata: {}\n\n",
                    serde_json::to_string(&completed).expect("encode completed event")
                );
                let content_length = prefix.len() + terminal.len();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
                )
                .expect("write held upstream response head");
                stream
                    .write_all(prefix.as_bytes())
                    .expect("write response.created event");
                stream.flush().expect("flush response.created event");
                match releases.recv_timeout(Duration::from_secs(10)) {
                    Ok(()) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Err(error) => panic!("release held upstream response: {error}"),
                }
                stream
                    .write_all(terminal.as_bytes())
                    .expect("write response.completed event");
                stream.flush().expect("flush response.completed event");
            }
        });

        Self {
            address,
            release: Some(release),
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn release_one(&self) {
        self.release
            .as_ref()
            .unwrap()
            .send(())
            .expect("release upstream response");
    }
}

impl Drop for HeldActivityUpstream {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(worker) = emp_test_support::drop_check(
            self.worker.take().ok_or("missing worker"),
            "join held upstream",
        ) {
            emp_test_support::join_check(worker, "join held upstream");
        }
    }
}

fn next_activity_snapshot(reader: &mut SseReader, in_flight: u64) -> Value {
    loop {
        let frame = read_sse_frame(reader);
        if sse::event(&frame) != Some("activity-updated") {
            continue;
        }
        let Some(data) = sse::data(&frame) else {
            continue;
        };
        let snapshot: Value = serde_json::from_str(&data).expect("activity snapshot JSON");
        if snapshot["routes"][0]["in_flight"] == in_flight {
            return snapshot;
        }
    }
}

fn send_turn(socket: &mut emp_test_support::websocket::ClientWebSocket, input: &str) {
    socket
        .send_json(&json!({
            "type":"response.create",
            "model":"demo/model",
            "input":input
        }))
        .expect("send WebSocket response.create");
}

fn receive_completed_turn(
    socket: &mut emp_test_support::websocket::ClientWebSocket,
    deadline: &Deadline,
) {
    for _ in 0..16 {
        let event = websocket_event_before(socket, deadline);
        if event["type"] == "response.completed" {
            return;
        }
        assert_ne!(event["type"], "error", "upstream turn failed: {event}");
    }
    panic!("WebSocket turn did not emit response.completed");
}

fn assert_one_active_route(snapshot: &Value) {
    let routes = snapshot["routes"].as_array().expect("activity routes");
    assert_eq!(routes.len(), 1, "{snapshot}");
    assert_eq!(routes[0]["model_id"], "demo/model");
    assert_eq!(routes[0]["provider_id"], "demo");
    assert_eq!(routes[0]["account_id"], Value::Null);
    assert_eq!(routes[0]["in_flight"], 1);
    assert!(routes[0]["last_finished"].is_null() || routes[0]["last_finished"].as_u64().is_some());
}

fn assert_one_finished_route(snapshot: &Value) {
    let routes = snapshot["routes"].as_array().expect("activity routes");
    assert_eq!(routes.len(), 1, "{snapshot}");
    assert_eq!(routes[0]["model_id"], "demo/model");
    assert_eq!(routes[0]["provider_id"], "demo");
    assert_eq!(routes[0]["account_id"], Value::Null);
    assert_eq!(routes[0]["in_flight"], 0);
    assert!(routes[0]["last_finished"].as_u64().is_some(), "{snapshot}");
}

#[test]
fn websocket_idle_connection_is_inactive_and_each_turn_has_its_own_activity() {
    let upstream = HeldActivityUpstream::start(2);
    let (_directory, server) =
        configured_protocol_server(&upstream.worker().base_url(), "responses", "api_key");
    let url = format!("ws://{}/v1/responses", server.local_addr());
    let session = session_header(&server);
    let headers = BTreeMap::from([(
        "x-emp-session".to_owned(),
        session.trim_start_matches("X-EMP-Session: ").to_owned(),
    )]);
    let mut socket = emp_test_support::websocket::ClientWebSocket::connect(
        &url,
        &headers,
        Duration::from_secs(5),
    )
    .expect("connect idle WebSocket");
    let mut activity = open_quota_events(&server, &session);

    for input in ["first activity turn", "second activity turn"] {
        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        set_sse_deadline(&mut activity, &deadline);
        let observed = upstream.begin(deadline, "held upstream activity turn");
        send_turn(&mut socket, input);
        let (path, _, body) = observed.wait().unwrap();
        assert_eq!(path, "/v1/responses");
        assert_eq!(body["stream"], true);
        assert_one_active_route(&next_activity_snapshot(&mut activity, 1));
        upstream.worker().release_one();
        receive_completed_turn(&mut socket, &deadline);
        assert_one_finished_route(&next_activity_snapshot(&mut activity, 0));
    }

    drop(socket);
    drop(activity);
    server
        .shutdown()
        .expect("shutdown activity WebSocket server");
}
