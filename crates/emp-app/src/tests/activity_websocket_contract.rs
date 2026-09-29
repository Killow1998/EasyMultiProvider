//! Real WebSocket activity lifecycle checks.
use super::*;
use std::sync::mpsc::{Receiver, Sender};

struct HeldActivityUpstream {
    address: SocketAddr,
    requests: Receiver<(String, BTreeMap<String, String>, Value)>,
    release: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl HeldActivityUpstream {
    fn start(request_count: usize) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind held upstream");
        let address = listener.local_addr().expect("held upstream address");
        let (request_sender, requests) = mpsc::sync_channel(request_count);
        let (release, releases) = mpsc::channel();
        let worker = thread::spawn(move || {
            for index in 0..request_count {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let (path, headers, body) = receive_upstream_request(&mut stream);
                let _ = request_sender.send((path, headers, body));

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
                releases
                    .recv_timeout(Duration::from_secs(10))
                    .expect("release held upstream response");
                stream
                    .write_all(terminal.as_bytes())
                    .expect("write response.completed event");
                stream.flush().expect("flush response.completed event");
            }
        });

        Self {
            address,
            requests,
            release,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn next_request(&self) -> (String, BTreeMap<String, String>, Value) {
        self.requests
            .recv_timeout(Duration::from_secs(5))
            .expect("held upstream request")
    }

    fn release_one(&self) {
        self.release.send(()).expect("release upstream response");
    }
}

impl Drop for HeldActivityUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            for _ in 0..2 {
                let _ = self.release.send(());
            }
            if !worker.is_finished()
                && let Ok(mut stream) = TcpStream::connect(self.address)
            {
                let _ = stream.write_all(
                    b"POST /v1/chat/completions HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}",
                );
            }
            worker.join().expect("join held upstream");
        }
    }
}

fn next_activity_snapshot(reader: &mut BufReader<TcpStream>) -> Value {
    loop {
        let frame = read_sse_frame(reader);
        let Some(data) = frame.strip_prefix("event: activity-updated\ndata: ") else {
            continue;
        };
        return serde_json::from_str(data.trim()).expect("activity snapshot JSON");
    }
}

fn send_turn(socket: &mut emp_transport::ClientWebSocket, input: &str) {
    socket
        .send_json(&json!({
            "type":"response.create",
            "model":"demo/model",
            "input":input
        }))
        .expect("send WebSocket response.create");
}

fn receive_completed_turn(socket: &mut emp_transport::ClientWebSocket) {
    for _ in 0..16 {
        let event = socket
            .receive_json()
            .expect("receive WebSocket event")
            .expect("WebSocket remains open");
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
        configured_protocol_server(&upstream.base_url(), "responses", "api_key");
    let url = format!("ws://{}/v1/responses", server.local_addr());
    let session = session_header(&server);
    let headers = BTreeMap::from([(
        "x-emp-session".to_owned(),
        session.trim_start_matches("X-EMP-Session: ").to_owned(),
    )]);
    let mut socket =
        emp_transport::ClientWebSocket::connect(&url, &headers, Duration::from_secs(5))
            .expect("connect idle WebSocket");
    let mut activity = open_quota_events(&server, &session);

    send_turn(&mut socket, "first activity turn");
    let (path, _, body) = upstream.next_request();
    assert_eq!(path, "/v1/responses");
    assert_eq!(body["stream"], true);
    assert_one_active_route(&next_activity_snapshot(&mut activity));
    upstream.release_one();
    receive_completed_turn(&mut socket);
    assert_one_finished_route(&next_activity_snapshot(&mut activity));

    send_turn(&mut socket, "second activity turn");
    let (path, _, body) = upstream.next_request();
    assert_eq!(path, "/v1/responses");
    assert_eq!(body["stream"], true);
    assert_one_active_route(&next_activity_snapshot(&mut activity));
    upstream.release_one();
    receive_completed_turn(&mut socket);
    assert_one_finished_route(&next_activity_snapshot(&mut activity));

    drop(socket);
    drop(activity);
    server
        .shutdown()
        .expect("shutdown activity WebSocket server");
}
