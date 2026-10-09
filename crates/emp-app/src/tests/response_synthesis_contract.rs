//! JSON upstreams must remain correct when downstream delivery is incremental.
use super::*;

#[test]
fn responses_sse_stops_at_terminal_without_eof_or_parsing_the_tail() {
    for auth_mode in ["forward", "api_key"] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (release, hold) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = receive_upstream_request(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
            let data = concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_fixture\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"completed\",\"output\":[],\"end_turn\":false,\"future\":true}}\n\n",
                "data: broken-json-after-terminal\n\n"
            );
            stream.write_all(data.as_bytes()).unwrap();
            stream.flush().unwrap();
            // Completion must reach downstream EOF before the upstream closes.
            let _ = hold.recv_timeout(Duration::from_secs(5));
        });
        let (_root, server) = configured_protocol_server(&base_url, "responses", auth_mode);
        let mut downstream = open_post_stream(
            &server,
            "/v1/responses",
            br#"{"model":"demo/model","input":"hello","stream":true}"#,
            &[&session_header(&server), "Authorization: Bearer fixture"],
        );
        downstream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut wire = String::new();
        downstream.read_to_string(&mut wire).unwrap();
        let events: Vec<Value> = wire
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        assert_eq!(
            events.last().unwrap()["type"],
            "response.completed",
            "{wire}"
        );
        assert_eq!(events.last().unwrap()["response"]["end_turn"], false);
        assert_eq!(events.last().unwrap()["response"]["future"], true);
        assert_eq!(
            events
                .iter()
                .filter(|e| e["type"] == "response.completed")
                .count(),
            1
        );
        release.send(()).unwrap();
        worker.join().unwrap();
        server.shutdown().unwrap();
    }
}

#[test]
fn native_and_external_json_replies_stream_to_completion_beyond_64_mib() {
    for auth_mode in ["forward", "api_key"] {
        for amplified in [false, true] {
            let response = json!({"id":"response_fixture","status":"completed","model":"upstream-model",
                "output":[{"id":if amplified {"x".repeat(64 * 1024)} else {"message_fixture".into()},
                    "type":"message","role":"assistant","content":vec![json!({"type":"output_text","text":"answer"}); if amplified {256} else {1}]}],
                "usage":{"input_tokens":3,"output_tokens":2},"future_metadata":{"kept":true}});
            let upstream = OneShotUpstream::start(response);
            let (root, server) =
                configured_protocol_server(&upstream.base_url(), "responses", auth_mode);
            let stream = open_post_stream(
                &server,
                "/v1/responses",
                br#"{"model":"demo/model","input":"hello","stream":true}"#,
                &[&session_header(&server), "Authorization: Bearer fixture"],
            );
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with("HTTP/1.1 200"), "{auth_mode}: {line}");
            let mut last = Value::Null;
            let mut terminals = 0;
            let mut saw_delta = false;
            let mut delivered_bytes = 0;
            // Consume and discard each event; the acceptance test must not
            // recreate the all-events allocation that production eliminated.
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                if let Some(data) = line.strip_prefix("data: ") {
                    delivered_bytes += data.len();
                    let event: Value = serde_json::from_str(data.trim_end()).unwrap();
                    saw_delta |= event["type"] == "response.output_text.delta";
                    terminals +=
                        usize::from(crate::services::events::terminal_stream_event(&event));
                    last = event;
                }
            }
            let (path, headers, body) = upstream.observed();
            assert_eq!(path, "/v1/responses");
            assert_eq!(body["model"], "upstream-model");
            if auth_mode == "forward" {
                assert_eq!(headers["content-encoding"], "zstd");
            }
            assert!(saw_delta);
            assert_eq!(terminals, 1, "only one terminal, {auth_mode}");
            if amplified {
                assert!(delivered_bytes > 64 * 1024 * 1024);
            }
            assert_eq!(last["type"], "response.completed", "{last}");
            assert_eq!(last["response"]["model"], "upstream-model");
            assert_eq!(last["response"]["usage"]["input_tokens"], 3);
            assert_eq!(last["response"]["future_metadata"]["kept"], true);
            let records = super::request_observation_contract::finished(root.path(), 1);
            let done = records
                .iter()
                .find(|record| record["event"] == "request_finished")
                .unwrap();
            assert_eq!(done["fields"]["downstream_terminal"], "completed");
            assert_eq!(
                records
                    .iter()
                    .filter(|record| record["event"] == "model_attempt_started")
                    .count(),
                1,
                "streamed delivery must not repeat inference"
            );
            server.shutdown().unwrap();
        }
    }
}

#[test]
fn disconnect_during_synthesis_stops_delivery_without_retry_or_false_completion() {
    for auth_mode in ["forward", "api_key"] {
        let upstream = OneShotUpstream::start(json!({
            "status":"completed","output":[{"id":"x".repeat(64 * 1024),
            "type":"message","role":"assistant","content":
                vec![json!({"type":"output_text","text":"answer"}); 512]}]
        }));
        let (root, server) =
            configured_protocol_server(&upstream.base_url(), "responses", auth_mode);
        let stream = open_post_stream(
            &server,
            "/v1/responses",
            br#"{"model":"demo/model","input":"hello","stream":true}"#,
            &[&session_header(&server), "Authorization: Bearer fixture"],
        );
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line.starts_with("data: ") && line.contains("response.output_text.delta") {
                break;
            }
        }
        reader.get_ref().shutdown(Shutdown::Both).unwrap();
        drop(reader);
        let _ = upstream.observed();
        let records = super::request_observation_contract::finished(root.path(), 1);
        let done = records
            .iter()
            .find(|record| record["event"] == "request_finished")
            .unwrap();
        assert_eq!(done["fields"]["terminal_written"], false, "{done}");
        assert_ne!(done["fields"]["downstream_terminal"], "completed", "{done}");
        assert_eq!(
            records
                .iter()
                .filter(|record| record["event"] == "model_attempt_started")
                .count(),
            1
        );
        server.shutdown().unwrap();
    }
}
