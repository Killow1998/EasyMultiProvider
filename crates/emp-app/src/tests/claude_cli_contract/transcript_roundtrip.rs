//! Source bytes, not a CLI re-encoding or a plausible resize, are the oracle.
use super::*;

#[test]
#[ignore = "requires trusted installed Claude Code CLI; synthetic local upstream only"]
fn installed_cli_preserves_provider_image_limit_errors() {
    // Above the direct API's 8000-pixel edge limit, but small enough for EMP's
    // byte budget. The configured provider, not the CLI codec, owns this limit.
    let image = STANDARD.encode(synthetic_png(8001, 1, true));
    for status in [400, 413] {
        let upstream = OneShotUpstream::start_wire(
            status,
            "application/json",
            None,
            vec![serde_json::to_vec(&json!({
                "error":{"type":"invalid_request_error","message":"private provider image limit detail"}
            })).unwrap()],
        );
        let (_directory, server) = claude_server(&upstream.base_url());
        let response = post(
            &server,
            "/v1/responses",
            &serde_json::to_vec(&json!({"model":"demo/model","input":[
                {"role":"user","content":[{"type":"input_image",
                    "image_url":format!("data:image/png;base64,{image}")}]}]
            }))
            .unwrap(),
            &[&session_header(&server)],
        );
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status} ")),
            "{response}"
        );
        assert!(!response.contains("claude_cli_content_mismatch"));
        assert!(!response.contains("private provider image limit detail"));
        let (path, _, forwarded) = upstream.observed();
        assert_eq!(path, "/v1/messages");
        let images: Vec<_> = forwarded["messages"][0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|part| part["type"] == "image")
            .collect();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0]["source"]["data"], image);
        server.shutdown().unwrap();
    }
}

struct RoundtripUpstream {
    address: SocketAddr,
    observed: mpsc::Receiver<Value>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl RoundtripUpstream {
    fn start(proposals: Vec<Value>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, observed) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let worker = thread::spawn(move || {
            for proposal in proposals {
                let deadline = Instant::now() + Duration::from_secs(30);
                let mut stream = loop {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    assert!(Instant::now() < deadline, "bounded fake CPA accept");
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(error) => panic!("fake CPA accept: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                let (path, _, body) = receive_upstream_request(&mut stream);
                assert_eq!(path, "/v1/messages");
                sender.send(body).unwrap();
                let sse = structured_messages_sse(&proposal);
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", sse.len()).unwrap();
                stream.write_all(&sse).unwrap();
            }
        });
        Self {
            address,
            observed,
            stopped,
            worker: Some(worker),
        }
    }

    fn next(&self) -> Value {
        self.observed.recv_timeout(Duration::from_secs(5)).unwrap()
    }
}

impl Drop for RoundtripUpstream {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}

fn ws_event(socket: &mut emp_transport::ClientWebSocket) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "bounded websocket response");
        match socket.poll_receive_text().unwrap() {
            emp_transport::WebSocketPoll::Text(text) => {
                return serde_json::from_str(&text).unwrap();
            }
            emp_transport::WebSocketPoll::Pending => {}
            other => panic!("websocket closed before terminal event: {other:?}"),
        }
    }
}

fn turn(
    server: &ServerHandle,
    socket: &mut Option<emp_transport::ClientWebSocket>,
    body: &Value,
) -> Value {
    let result = if let Some(socket) = socket {
        let mut request = body.clone();
        request["type"] = json!("response.create");
        socket.send_json(&request).unwrap();
        let mut created = None;
        loop {
            let event = ws_event(socket);
            assert!(
                !matches!(
                    event["type"].as_str(),
                    Some("error" | "response.failed" | "response.incomplete")
                ),
                "{event}"
            );
            match event["type"].as_str() {
                Some("response.created") => assert!(
                    created.replace(event["response"]["id"].clone()).is_none(),
                    "one response per turn"
                ),
                Some("response.completed") => {
                    assert_eq!(created, Some(event["response"]["id"].clone()));
                    break event["response"].clone();
                }
                _ => {}
            }
        }
    } else {
        let response = post(
            server,
            "/v1/responses",
            &serde_json::to_vec(body).unwrap(),
            &[&session_header(server)],
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
    };
    assert_eq!(result["status"], "completed");
    assert!(result.get("error").is_none_or(Value::is_null));
    result
}

fn assert_source_images(forwarded: &Value, expected: &[String]) {
    let content = forwarded["messages"][0]["content"].as_array().unwrap();
    let actual: Vec<_> = content
        .iter()
        .enumerate()
        .filter(|(_, part)| part["type"] == "image")
        .collect();
    assert_eq!(actual.len(), expected.len());
    for (index, (block_index, image)) in actual.iter().enumerate() {
        assert_eq!(image["source"]["media_type"], "image/png");
        assert_eq!(
            image["source"]["data"], expected[index],
            "original image {index}, never a CLI slot"
        );
        let marker: Value =
            serde_json::from_str(content[block_index - 1]["text"].as_str().unwrap()).unwrap();
        assert!(
            marker.get("content_part_index").is_some()
                || marker.get("tool_output_part_index").is_some()
        );
    }
    assert_eq!(forwarded["tools"].as_array().unwrap().len(), 1);
    assert_eq!(forwarded["tools"][0]["name"], "StructuredOutput");
}

#[test]
#[ignore = "requires trusted installed Claude Code CLI; synthetic local upstream only"]
fn installed_cli_multiturn_http_and_websocket_preserve_images_and_exact_tool_results() {
    let images: Vec<_> = [(1326, 336), (2048, 409), (1995, 1248)]
        .into_iter()
        .map(|(width, height)| STANDARD.encode(synthetic_png(width, height, true)))
        .collect();
    let long_output = "opaque 中文 tool output\n  whitespace\t\"quoted\"\\n".repeat(2048);
    for model in ["claude-opus-5-5", "claude-sonnet-5-5"] {
        for websocket in [false, true] {
            let upstream = RoundtripUpstream::start(vec![
                json!({"answer":"","tool_calls":[{"type":"function_call","name":"inspect","namespace":"","arguments":{"path":"synthetic.png"},"input":""}]}),
                json!({"answer":"follow-up preserved","tool_calls":[]}),
            ]);
            let (_directory, server) =
                claude_server_with_model(&format!("http://{}/v1", upstream.address), model);
            let mut socket = websocket.then(|| {
                let headers = BTreeMap::from([(
                    "x-emp-session".to_owned(),
                    session_header(&server)
                        .trim_start_matches("X-EMP-Session: ")
                        .to_owned(),
                )]);
                let client = emp_transport::ClientWebSocket::connect(
                    &format!("ws://{}/v1/responses", server.local_addr()),
                    &headers,
                    Duration::from_secs(5),
                )
                .unwrap();
                client
                    .readiness_stream()
                    .unwrap()
                    .set_read_timeout(Some(Duration::from_millis(20)))
                    .unwrap();
                client
            });
            let mut input = Vec::new();
            for index in 0..40 {
                input.extend([
                    json!({"type":"function_call","call_id":format!("history-{index}"),"name":"inspect","arguments":"{}"}),
                    json!({"type":"function_call_output","call_id":format!("history-{index}"),"output":if index==20 {long_output.as_str()} else {"prior exact result\n"}}),
                ]);
            }
            input.push(json!({"role":"user","content":[{"type":"input_image","image_url":format!("data:image/png;base64,{}",images[0])}]}));
            let mut body = json!({"model":"demo/model","reasoning":{"effort":"medium"},"input":input,
                "tools":[{"type":"function","name":"inspect","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}]});
            let first = turn(&server, &mut socket, &body);
            assert_eq!(first["output"][0]["type"], "function_call");
            assert_eq!(first["output"][0]["name"], "inspect");
            let call_id = first["output"][0]["call_id"].as_str().unwrap();
            assert_source_images(&upstream.next(), &images[..1]);
            if let Some(socket) = &mut socket {
                // Non-native continuation IDs are deliberately rejected; Codex
                // retries with full history. Rejecting the ID must not poison
                // this connection or accidentally call the provider.
                socket.send_json(&json!({"type":"response.create","model":"demo/model","previous_response_id":first["id"],"input":[]})).unwrap();
                let error = loop {
                    let event = ws_event(socket);
                    if event["type"] == "error" {
                        break event;
                    }
                    // A completed turn may be followed by EMP's accounting
                    // metadata. No second terminal or inference is allowed.
                    assert_eq!(event["type"], "codex.response.metadata");
                };
                assert_eq!(error["type"], "error");
                assert_eq!(error["error"]["code"], "previous_response_not_found");
            }
            let history = body["input"].as_array_mut().unwrap();
            history.push(first["output"][0].clone());
            history.push(json!({"type":"function_call_output","call_id":call_id,"output":[
                {"type":"input_text","text":long_output},
                {"type":"input_image","image_url":format!("data:image/png;base64,{}",images[1])},
                {"type":"input_image","image_url":format!("data:image/png;base64,{}",images[2])}
            ]}));
            history.push(json!({"role":"user","content":"Continue exactly.\n"}));
            let final_response = turn(&server, &mut socket, &body);
            assert_ne!(first["id"], final_response["id"]);
            assert_eq!(
                crate::services::compaction::response_output_text(&final_response).as_deref(),
                Some("follow-up preserved")
            );
            let forwarded = upstream.next();
            assert_source_images(&forwarded, &images);
            let markers: Vec<Value> = forwarded["messages"][0]["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|part| serde_json::from_str(part["text"].as_str()?).ok())
                .collect();
            assert!(
                markers
                    .iter()
                    .any(|marker| marker["item"]["call_id"] == call_id
                        && marker["tool_output_part"]["text"] == long_output)
            );
            assert!(
                markers
                    .iter()
                    .any(|marker| marker["item"]["call_id"] == "history-20"
                        && marker["item"]["output"] == long_output)
            );
            assert!(
                markers
                    .iter()
                    .any(|marker| marker["item"]["content"] == "Continue exactly.\n")
            );
            if let Some(socket) = &mut socket {
                socket.close();
            }
            server.shutdown().unwrap();
        }
    }
}

#[test]
#[ignore = "requires trusted installed Claude Code CLI; synthetic local upstream only"]
fn installed_cli_preserves_codex_preprocessed_images_and_tool_history() {
    let images: Vec<_> = [(1326, 336), (2048, 409), (1995, 1248)]
        .into_iter()
        .map(|(width, height)| STANDARD.encode(synthetic_png(width, height, true)))
        .collect();
    for model in ["claude-opus-5-5", "claude-sonnet-5-5"] {
        let upstream = OneShotUpstream::start_sse(vec![structured_messages_sse(&json!({
            "answer":"preprocessed screenshots preserved", "tool_calls":[]
        }))]);
        let (_directory, server) = claude_server_with_model(&upstream.base_url(), model);
        let body = json!({"model":"demo/model","reasoning":{"effort":"medium"},"input":[
            {"role":"user","content":[{"type":"input_image","image_url":format!("data:image/png;base64,{}",images[0])}]},
            {"type":"function_call","call_id":"image-inspection","name":"inspect","arguments":"{}"},
            {"type":"function_call_output","call_id":"image-inspection","output":[
                {"type":"input_text","text":"exact tool output\n  中文\n"},
                {"type":"input_image","image_url":format!("data:image/png;base64,{}",images[1])},
                {"type":"input_image","image_url":format!("data:image/png;base64,{}",images[2])}
            ]},
            {"role":"user","content":"Continue without losing images or tool results.\n"}
        ],"tools":[{"type":"function","name":"inspect","parameters":{"type":"object"}}]});
        let response = post(
            &server,
            "/v1/responses",
            &serde_json::to_vec(&body).unwrap(),
            &[&session_header(&server)],
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{model}: {response}");
        let (_, _, forwarded) = upstream.observed();
        let actual: Vec<_> = forwarded["messages"][0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|part| part["type"] == "image")
            .collect();
        assert_eq!(actual.len(), images.len());
        for (index, image) in actual.iter().enumerate() {
            assert_eq!(image["source"]["media_type"], "image/png");
            assert_eq!(
                image["source"]["data"], images[index],
                "source bytes must be preserved at image {index}"
            );
        }
        server.shutdown().unwrap();
    }
}
