//! Steering cancels HTTP streams while preserving the downstream WS connection.
use super::support::*;
use super::*;
use emp_test_support::{Deadline, test_timeout};
use emp_transport::{ClientWebSocket, WebSocketPoll};

fn event(client: &mut ClientWebSocket, kind: &str) -> Value {
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    loop {
        assert!(!deadline.is_expired(), "waiting for {kind}");
        let remaining = deadline
            .remaining()
            .min(test_timeout(Duration::from_millis(20)));
        match client.poll_receive_text_with_timeout(remaining).unwrap() {
            WebSocketPoll::Text(text) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == kind {
                    return deadline.accept(value, kind).unwrap();
                }
                assert_ne!(value["type"], "error", "{value}");
                assert_ne!(value["type"], "response.failed", "{value}");
            }
            WebSocketPoll::Pending => {}
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

#[test]
fn steering_cancels_native_fallback_and_external_http_then_accepts_the_next_turn() {
    for native in [false, true] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (probe, cancelled) = mpsc::channel();
        let worker = thread::spawn(move || {
            let accept = || {
                let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
                loop {
                    assert!(!deadline.is_expired(), "upstream request did not arrive");
                    match listener.accept() {
                        Ok((socket, _)) => {
                            return deadline.accept(socket, "upstream accept").unwrap();
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            #[expect(
                                clippy::disallowed_methods,
                                reason = "假上游以固定间隔重试非阻塞 accept，避免空转"
                            )]
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("{error}"),
                    }
                }
            };
            if native {
                let mut socket = accept();
                let raw = read_request_head(&mut socket).unwrap();
                assert!(
                    parse_request(&raw.head)
                        .unwrap()
                        .header("Upgrade")
                        .is_some()
                );
                socket.write_all(b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
            for (index, id) in ["resp_steer", "resp_after"].iter().enumerate() {
                let mut socket = accept();
                let request = receive_native_request(&mut socket);
                assert_eq!(
                    request.body["input"],
                    if index == 0 { "first" } else { "after" }
                );
                assert!(request.body.get("previous_response_id").is_none());
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
                write!(socket, "data: {}\n\n", json!({"type":"response.created", "response":{"id":id,"status":"in_progress","model":"upstream"}})).unwrap();
                socket.flush().unwrap();
                if index == 0 {
                    report_cancellation(&cancelled, |deadline| {
                        upstream_closed_before(&socket, deadline)
                    });
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
        client
            .send_json(&json!({"type":"response.create","model":model,"input":"first"}))
            .unwrap();
        assert_eq!(
            event(&mut client, "response.created")["response"]["id"],
            "resp_steer"
        );
        client.send_json(&json!({"type":"response.interrupt","response_id":"someone_else","mode":"discard_partial_items"})).unwrap();
        assert_eq!(
            event(&mut client, "error")["error"]["code"],
            "invalid_request"
        );
        let interrupt = json!({"type":"response.interrupt","response_id":"resp_steer","mode":"discard_partial_items"});
        let deadline = Deadline::after(test_timeout(Duration::from_secs(2)));
        client.send_json(&interrupt).unwrap();
        let incomplete = event(&mut client, "response.incomplete");
        assert_eq!(
            incomplete["response"]["incomplete_details"]["reason"],
            "interrupted"
        );
        assert_eq!(incomplete["response"]["id"], "resp_steer");
        assert!(
            incomplete["response"].get("usage").is_none(),
            "unreported tokens must not become zero"
        );
        assert!(
            wait_for_cancellation(&probe, &deadline),
            "steering retained HTTP inference"
        );
        // An incremental request must ask Codex for full history rather than
        // pretending the cancelled HTTP upstream has a resumable WS state.
        client.send_json(&json!({"type":"response.create","model":model,"input":"after","previous_response_id":"resp_steer"})).unwrap();
        assert_eq!(
            event(&mut client, "error")["error"]["code"],
            "previous_response_not_found"
        );
        client.send_json(&interrupt).unwrap(); // late control, then prefetched next frame.
        client
            .send_json(&json!({"type":"response.create","model":model,"input":"after"}))
            .unwrap();
        assert_eq!(
            event(&mut client, "response.completed")["response"]["id"],
            if native { "resp_after" } else { "" }
        );
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
        worker.join().unwrap();
    }
}
