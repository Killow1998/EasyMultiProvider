use super::*;

#[test]
fn native_inline_images_survive_zstd_rejection_without_changing_history_or_accounts() {
    for model in ["gpt-6.1-sol", "selected/gpt-6.1-sol"] {
        for (path, streaming) in [
            ("/v1/responses", false),
            ("/v1/responses", true),
            ("/v1/responses/compact", false),
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let (sender, observed) = mpsc::sync_channel(1);
            let worker = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let raw = read_request_head(&mut stream).unwrap();
                let head = parse_request(&raw.head).unwrap();
                let request = if head.header("Upgrade") == Some("websocket") {
                    stream.write_all(b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    stream.flush().unwrap();
                    drop(stream);
                    let (mut fallback, _) = listener.accept().unwrap();
                    let request = receive_native_request(&mut fallback);
                    stream = fallback;
                    request
                } else {
                    finish_native_request(&mut stream, raw)
                };
                let rejected =
                    request.headers.get("content-encoding").map(String::as_str) == Some("zstd");
                sender.send(request).unwrap();
                let (status, content_type, body) = if rejected {
                    (
                        400,
                        "application/json",
                        br#"{"detail":"Bad Request"}"#.to_vec(),
                    )
                } else if streaming {
                    let event = json!({"type":"response.completed","response":{
                        "id":"resp_image_fixture","object":"response","status":"completed",
                        "model":"gpt-6.1-sol","output":[]}});
                    (
                        200,
                        "text/event-stream",
                        format!("data: {event}\n\n").into_bytes(),
                    )
                } else {
                    (
                        200,
                        "application/json",
                        serde_json::to_vec(&json!({
                        "id":"resp_image_fixture","object":"response","status":"completed",
                        "model":"gpt-6.1-sol","output":[]}))
                        .unwrap(),
                    )
                };
                write!(stream,"HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",status_text(status),body.len()).unwrap();
                stream.write_all(&body).unwrap();
            });
            let (_directory, server) = image_server(&format!("http://{address}/backend-api/codex"));
            let image = json!({"type":"input_image","detail":"high","image_url":
                "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aO1sAAAAASUVORK5CYII="});
            let input = json!([
                {"type":"message","role":"user","content":[
                    {"type":"input_text","text":"Retain this text and both original images."},image]},
                {"type":"reasoning","encrypted_content":"native-opaque-fixture","summary":[]},
                {"type":"function_call","call_id":"image_fixture","name":"read","arguments":"{}"},
                {"type":"function_call_output","call_id":"image_fixture","output":[image]}
            ]);
            let wire = post_stream(
                &server,
                path,
                &serde_json::to_vec(&json!({
                    "model":model,"input":input,"stream":streaming,"tool_choice":"none",
                    "reasoning":{"effort":"high","context":"all_turns"}
                }))
                .unwrap(),
                &[&session_header(&server)],
            );
            let request = observed.recv_timeout(Duration::from_secs(5)).unwrap();
            worker.join().unwrap();
            server.shutdown().unwrap();
            assert!(wire.starts_with("HTTP/1.1 200"), "{model} {path}: {wire}");
            assert!(!request.headers.contains_key("content-encoding"));
            assert_eq!(request.body["input"], input);
            assert_eq!(request.body["tool_choice"], "none");
            assert_eq!(
                request.headers["authorization"],
                if model.starts_with("selected/") {
                    "Bearer selected-fixture"
                } else {
                    "Bearer native-fixture"
                }
            );
            assert_eq!(
                request.path,
                format!("/backend-api/codex{}", path.strip_prefix("/v1").unwrap())
            );
            if streaming {
                assert!(wire.contains("response.completed"), "{wire}");
            }
        }
    }
}
