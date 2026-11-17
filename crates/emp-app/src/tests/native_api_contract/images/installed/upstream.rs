//! The native image fixture models HTTP, WebSocket prefill and image replies.
use super::*;

pub(super) fn spawn(
    listener: TcpListener,
    image_args: Value,
    edit: bool,
    mcp: bool,
    aero: bool,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut image_turn = String::new();
        let discovery = usize::from(aero && mcp);
        let image_stage = if aero { 2 + discovery } else { 1 };
        let final_stage = if aero { 4 + discovery } else { 2 };
        for stage in 0..=final_stage {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut socket = loop {
                assert!(
                    Instant::now() < deadline,
                    "image fixture upstream deadline at stage {stage}"
                );
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let raw = read_request_head(&mut socket).unwrap();
            let head = parse_request(&raw.head).unwrap();
            let mut websocket_socket = socket.try_clone().unwrap();
            let mut websocket = if head.header("upgrade") == Some("websocket") {
                let accept = websocket_accept(head.header("sec-websocket-key").unwrap()).unwrap();
                write!(socket,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
                socket.flush().unwrap();
                Some(WebSocketConnection::new(&mut websocket_socket))
            } else {
                None
            };
            let request = if let Some(websocket) = &mut websocket {
                let mut body: Value =
                    serde_json::from_str(&websocket.receive_text().unwrap().unwrap()).unwrap();
                if body["generate"] == false {
                    websocket.send_json(&json!({"type":"response.completed","response":{"id":"image-prefill","status":"completed","output":[]}})).unwrap();
                    let next: Value =
                        serde_json::from_str(&websocket.receive_text().unwrap().unwrap()).unwrap();
                    body.as_object_mut().unwrap().remove("generate");
                    body.as_object_mut()
                        .unwrap()
                        .extend(next.as_object().unwrap().clone());
                }
                ObservedNativeRequest {
                    path: head.target.to_owned(),
                    headers: head
                        .headers
                        .lines()
                        .skip(1)
                        .filter_map(|line| line.split_once(':'))
                        .map(|(name, value)| {
                            (name.trim().to_ascii_lowercase(), value.trim().to_owned())
                        })
                        .collect(),
                    body,
                }
            } else {
                finish_native_request(&mut socket, raw)
            };
            assert_eq!(request.headers["authorization"], "Bearer selected-fixture");
            assert_eq!(request.headers["chatgpt-account-id"], "selected-owner");
            assert!(!request.headers.contains_key("x-openai-actor-authorization"));
            if stage == image_stage {
                assert_eq!(
                    request.path,
                    format!(
                        "/backend-api/codex/images/{}",
                        if edit { "edits" } else { "generations" }
                    ),
                    "unexpected image stage: input tail={:?}",
                    request.body["input"]
                        .as_array()
                        .map(|items| &items[items.len().saturating_sub(4)..])
                );
                assert!(
                    request.body["model"]
                        .as_str()
                        .unwrap()
                        .starts_with("gpt-image-")
                );
                let metadata: Value =
                    serde_json::from_str(&request.headers["x-codex-turn-metadata"]).unwrap();
                if mcp {
                    assert_eq!(metadata["turn_id"], image_turn)
                } else {
                    assert_eq!(request.headers["x-codex-image-turn-id"], image_turn)
                };
                if edit {
                    assert!(request.body["images"].is_array());
                }
                let body = json!({"created":1,"data":[{"b64_json":PNG,"generation_id":"generated-fixture"}]}).to_string();
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-codex-imagegen-request-id: image-fixture\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                continue;
            }
            assert_eq!(request.path, "/backend-api/codex/responses");
            if stage == 0 {
                let metadata = request.body["client_metadata"]["x-codex-turn-metadata"]
                    .as_str()
                    .or_else(|| {
                        request
                            .headers
                            .get("x-codex-turn-metadata")
                            .map(String::as_str)
                    })
                    .unwrap();
                image_turn = serde_json::from_str::<Value>(metadata).unwrap()["turn_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
            } else if stage == final_stage {
                let serialized = request.body.to_string();
                assert!(serialized.contains("image-fixture-call"));
                assert_eq!(
                    serialized.contains(PNG),
                    !mcp,
                    "MCP returns a path; the native extension retains its image result"
                );
                if mcp {
                    assert!(
                        serialized.contains("generated_images"),
                        "saved image path reaches the next inference"
                    );
                }
            }
            let item = if aero && (stage == 0 || stage == 3 + discovery) {
                let (name, arguments) = if stage == 0 {
                    (
                        "exit_plan_mode",
                        json!({"plan":"Generate or edit the requested image, then return for review"}),
                    )
                } else {
                    (
                        "enter_plan_mode",
                        json!({"reason":"Image generation finished and the result was saved"}),
                    )
                };
                json!({"id":format!("aero-handoff-{stage}"),"type":"function_call","name":name,"call_id":format!("aero-handoff-{stage}"),"arguments":arguments.to_string()})
            } else if aero && mcp && stage == 1 {
                json!({"id":"search-image","type":"tool_search_call","execution":"client","call_id":"search-image","arguments":{"query":"imagegen generate and edit images","limit":1}})
            } else if stage == if aero { 1 + discovery } else { 0 } {
                assert!(
                    request.body.to_string().contains("imagegen"),
                    "image extension missing"
                );
                json!({"id":"image-fixture-call","type":"function_call","namespace":if mcp {"mcp__emp_images"} else {"image_gen"},
                "name":"imagegen","call_id":"image-fixture-call","arguments":image_args.to_string()})
            } else {
                json!({"id":"message-fixture","type":"message","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"Image fixture completed","annotations":[]}]})
            };
            let id = format!("response_fixture_{stage}");
            let events = [
                json!({"type":"response.created","response":{"id":id,"status":"in_progress","output":[]}}),
                json!({"type":"response.output_item.added","output_index":0,"item":item}),
                json!({"type":"response.output_item.done","output_index":0,"item":item}),
                json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[item],"end_turn":stage==final_stage}}),
            ];
            if let Some(websocket) = &mut websocket {
                for event in events {
                    websocket.send_json(&event).unwrap();
                }
            } else {
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
                for event in events {
                    writeln!(socket, "data: {event}\n").unwrap();
                }
            }
        }
    })
}
