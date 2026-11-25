//! Steering cancels HTTP streams while preserving the downstream WS connection.
use super::support::*;
use super::*;
use emp_test_support::websocket::ClientWebSocket;
use emp_test_support::websocket::WebSocketPoll;
use emp_test_support::{Deadline, test_timeout};
use std::time::Instant;

fn event(client: &mut ClientWebSocket, kind: &str) -> Value {
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    event_before(client, kind, &deadline).unwrap()
}

fn event_before(
    client: &mut ClientWebSocket,
    kind: &str,
    deadline: &Deadline,
) -> Result<Value, String> {
    let mut last_event = Value::Null;
    while !deadline.is_expired() {
        match client
            .poll_receive_text_with_timeout(deadline.remaining())
            .map_err(|error| format!("waiting for {kind}, last event {last_event}: {error:?}"))?
        {
            WebSocketPoll::Text(text) => {
                let value: Value = serde_json::from_str(&text).map_err(|error| {
                    format!("invalid event while waiting for {kind}: {error}: {text}")
                })?;
                if value["type"] == kind {
                    return deadline
                        .accept(value, kind)
                        .map_err(|error| format!("{kind} arrived after deadline: {error}"));
                }
                if matches!(value["type"].as_str(), Some("error" | "response.failed")) {
                    return Err(format!("waiting for {kind}, received {value}"));
                }
                last_event = value;
            }
            WebSocketPoll::Pending => {}
            other => return Err(format!("waiting for {kind}, received {other:?}")),
        }
    }
    Err(format!("waiting for {kind}, last event {last_event}"))
}

fn interrupt_and_observe(
    client: &mut ClientWebSocket,
    interrupt: &Value,
    upstream: &Observation<bool>,
    upstream_deadline: Deadline,
) -> (Result<Value, String>, std::io::Result<bool>) {
    let downstream_deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let cancellation = upstream.begin(upstream_deadline, "HTTP upstream cancellation");
    client.send_json(interrupt).unwrap();
    let receipt = event_before(client, "response.incomplete", &downstream_deadline);
    (receipt, cancellation.wait())
}

#[test]
fn steering_cancels_native_fallback_and_external_http_then_accepts_the_next_turn() {
    for native in [false, true] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = Observation::spawn(move |probe| {
            let Some(dispatch) = probe.next_request() else {
                return;
            };
            if native {
                let mut socket = accept_before(&listener, dispatch.deadline());
                let raw = read_request_head(&mut socket).unwrap();
                assert!(
                    parse_request(&raw.head)
                        .unwrap()
                        .header("Upgrade")
                        .is_some()
                );
                socket.write_all(b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
            let mut first = Some(dispatch);
            for (index, id) in ["resp_steer", "resp_after"].iter().enumerate() {
                let Some(dispatch) = first.take().or_else(|| probe.next_request()) else {
                    return;
                };
                let mut socket = accept_before(&listener, dispatch.deadline());
                let request = receive_native_request(&mut socket);
                assert_eq!(
                    request.body["input"],
                    if index == 0 { "first" } else { "after" }
                );
                assert!(request.body.get("previous_response_id").is_none());
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
                write!(socket, "data: {}\n\n", json!({"type":"response.created", "response":{"id":id,"status":"in_progress","model":"upstream"}})).unwrap();
                socket.flush().unwrap();
                dispatch.complete(true);
                if index == 0 {
                    probe.observe(|deadline| upstream_closed_before(&socket, deadline));
                } else {
                    write!(socket, "data: {}\n\n", json!({"type":"response.completed", "response":{"id":id,"status":"completed","model":"upstream","output":[]}})).unwrap();
                    socket.flush().unwrap();
                }
            }
        });
        let directory = emp_test_support::fixture_dir().unwrap();
        let config = canonical_root(&directory).join("config.json");
        let (_native_directory, server) = if native {
            let (directory, server) = native_alias_server(&format!("http://{address}/v1"));
            (Some(directory), server)
        } else {
            std::fs::write(&config, serde_json::to_vec(&json!({
                "providers":[{"id":"demo","name":"Demo","base_url":format!("http://{address}/v1"),"protocol":"responses","auth_mode":"api_key","api_key":"fake"}],
                "models":[{"id":"demo/model","provider":"demo","upstream_id":"upstream","enabled":true}]
            })).unwrap()).unwrap();
            (
                None,
                ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
                    .unwrap(),
            )
        };
        let model = if native { "native/alias" } else { "demo/model" };
        let mut client = ClientWebSocket::connect(
            &format!("ws://{}/v1/responses", server.local_addr()),
            &BTreeMap::from([
                ("X-EMP-Session".into(), server.session_token().to_owned()),
                ("Authorization".into(), "Bearer fake".into()),
            ]),
            Duration::from_secs(5),
        )
        .unwrap();
        let first = upstream.begin(
            Deadline::after(test_timeout(Duration::from_secs(5))),
            "first interrupt fixture request",
        );
        client
            .send_json(&json!({"type":"response.create","model":model,"input":"first"}))
            .unwrap();
        assert_eq!(
            event(&mut client, "response.created")["response"]["id"],
            "resp_steer"
        );
        assert!(first.wait().unwrap());
        client.send_json(&json!({"type":"response.interrupt","response_id":"someone_else","mode":"discard_partial_items"})).unwrap();
        assert_eq!(
            event(&mut client, "error")["error"]["code"],
            "invalid_request"
        );
        let interrupt = json!({"type":"response.interrupt","response_id":"resp_steer","mode":"discard_partial_items"});
        let (receipt, closed) = interrupt_and_observe(
            &mut client,
            &interrupt,
            &upstream,
            Deadline::after(test_timeout(Duration::from_secs(2))),
        );
        let incomplete = receipt.unwrap_or_else(|error| {
            panic!("interrupt receipt: {error}; upstream cancellation: {closed:?}")
        });
        assert!(
            closed.unwrap_or_else(|error| {
                panic!("upstream cancellation: {error}; interrupt receipt: {incomplete}")
            }),
            "steering retained HTTP inference; interrupt receipt: {incomplete}"
        );
        assert_eq!(
            incomplete["response"]["incomplete_details"]["reason"],
            "interrupted"
        );
        assert_eq!(incomplete["response"]["id"], "resp_steer");
        assert!(
            incomplete["response"].get("usage").is_none(),
            "unreported tokens must not become zero"
        );
        // An incremental request must ask Codex for full history rather than
        // pretending the cancelled HTTP upstream has a resumable WS state.
        client.send_json(&json!({"type":"response.create","model":model,"input":"after","previous_response_id":"resp_steer"})).unwrap();
        assert_eq!(
            event(&mut client, "error")["error"]["code"],
            "previous_response_not_found"
        );
        client.send_json(&interrupt).unwrap(); // late control, then prefetched next frame.
        let next = upstream.begin(
            Deadline::after(test_timeout(Duration::from_secs(5))),
            "next interrupt fixture request",
        );
        client
            .send_json(&json!({"type":"response.create","model":model,"input":"after"}))
            .unwrap();
        assert_eq!(
            event(&mut client, "response.completed")["response"]["id"],
            if native { "resp_after" } else { "" }
        );
        assert!(next.wait().unwrap());
        if !native {
            client.send_json(&json!({"type":"response.interrupt","response_id":"resp_after","mode":"discard_partial_items"})).unwrap();
            client
                .send_json(
                    &json!({"type":"response.create","model":model,"generate":false,"input":[]}),
                )
                .unwrap();
            assert_eq!(
                event(&mut client, "response.completed")["response"]["id"],
                ""
            );
        }
        client.close();
        server.shutdown().unwrap();
        drop(upstream);
    }
}

#[test]
fn interrupt_can_collect_an_on_time_upstream_close_after_a_later_receipt() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let http_client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (http_upstream, _) = listener.accept().unwrap();
    let (eof_observed, before_receipt) = mpsc::channel();

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = Observation::spawn(move |probe| {
        probe.observe(|deadline| {
            let stream = super::interrupt::complete_upgrade(accept_before(&listener, deadline));
            let mut io = deadline.io(&stream);
            let mut socket = WebSocketConnection::new(&mut io);
            assert_eq!(
                upstream_websocket_request(&mut socket, deadline)["type"],
                "response.interrupt"
            );
            drop(http_client);
            before_receipt
                .recv_timeout(deadline.remaining())
                .expect("observe HTTP EOF before sending the interrupt receipt");
            socket
                .send_json(&json!({"type":"response.incomplete"}))
                .unwrap();
        });
    });
    let peer_done = peer.begin(
        Deadline::after(test_timeout(Duration::from_secs(5))),
        "interrupt peer sent receipt",
    );
    let mut client = ClientWebSocket::connect(
        &format!("ws://{address}/v1/responses"),
        &BTreeMap::new(),
        test_timeout(Duration::from_secs(5)).duration(),
    )
    .unwrap();
    let upstream_budget = test_timeout(Duration::from_secs(2));
    let (trigger_io, io_trigger) = mpsc::channel();
    let observation = Observation::spawn(move |probe| {
        let Some(request) = probe.next_request() else {
            return;
        };
        let io_started: Instant = io_trigger.recv().unwrap();
        let io_deadline = Deadline::starting_at(io_started, upstream_budget);
        assert_eq!(io_deadline.io(&http_upstream).peek(&mut [0]).unwrap(), 0);
        let observed_at = Instant::now() - upstream_budget.duration();
        request.complete_at(observed_at, true);
        eof_observed.send(()).unwrap();
    });
    let io_started = Instant::now();
    let observation_started = io_started - upstream_budget.duration();
    let upstream_deadline = Deadline::starting_at(observation_started, upstream_budget);
    assert!(upstream_deadline.is_expired());
    trigger_io.send(io_started).unwrap();
    let (receipt, closed) = interrupt_and_observe(
        &mut client,
        &json!({"type":"response.interrupt"}),
        &observation,
        upstream_deadline,
    );
    assert_eq!(receipt.unwrap()["type"], "response.incomplete");
    assert!(closed.unwrap());
    peer_done.wait().unwrap();
}
