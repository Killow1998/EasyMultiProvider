//! Real server contract tests.
use super::*;

#[test]
fn complete_chat_request_crosses_the_real_server_boundary() {
    let upstream = OneShotUpstream::start(json!({
        "id": "chat_upstream", "model": "upstream-model",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "answer"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
    }));
    let (_directory, server) = configured_server(&upstream.base_url());
    let request_body = serde_json::to_vec(&json!({
        "model": "demo/model",
        "input": [{
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        }],
        "stream": false
    }))
    .expect("request JSON");
    let response = post(
        &server,
        "/v1/responses",
        &request_body,
        &[&session_cookie_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let response_body: Value = serde_json::from_str(
        response
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("response JSON");
    assert_eq!(response_body["model"], "demo/model");
    assert_eq!(response_body["status"], "completed");
    assert_eq!(response_body["output"][0]["content"][0]["text"], "answer");

    let (path, headers, upstream_body) = upstream.observed();
    assert_eq!(path, "/v1/chat/completions");
    assert_eq!(headers["authorization"], "Bearer upstream-secret");
    assert_eq!(headers["x-emp-request-id"].len(), 16);
    assert_eq!(upstream_body["model"], "upstream-model");
    assert_eq!(upstream_body["stream"], false);
    server.shutdown().expect("shutdown");
}

fn chat_summary_upstream(summary: &str) -> OneShotUpstream {
    OneShotUpstream::start(json!({
        "id": "chat_summary", "model": "upstream-model",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": summary},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
    }))
}

#[test]
fn external_compact_endpoint_uses_the_selected_model_and_returns_a_portable_checkpoint() {
    let upstream = chat_summary_upstream("portable checkpoint");
    let (_directory, server) = configured_server(&upstream.base_url());
    let request_body = serde_json::to_vec(&json!({
        "model": "demo/model",
        "input": [{
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "history"}]
        }],
        "reasoning":{"effort":"high"}
    }))
    .expect("request JSON");
    let response = post(
        &server,
        "/v1/responses/compact",
        &request_body,
        &[&session_cookie_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let response_body: Value = serde_json::from_str(
        response
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("response JSON");
    assert_eq!(response_body["model"], "demo/model");
    assert_eq!(response_body["status"], "completed");
    assert_eq!(response_body["usage"], Value::Null);
    let encoded = response_body["output"][0]["encrypted_content"]
        .as_str()
        .expect("checkpoint")
        .strip_prefix("emp1:")
        .expect("portable prefix");
    assert_eq!(
        URL_SAFE.decode(encoded).expect("checkpoint base64"),
        b"portable checkpoint"
    );

    let (path, _, upstream_body) = upstream.observed();
    assert_eq!(path, "/v1/chat/completions");
    assert_eq!(upstream_body["model"], "upstream-model");
    assert_eq!(upstream_body["stream"], false);
    assert!(
        upstream_body["messages"]
            .as_array()
            .expect("summary messages")
            .last()
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .is_some_and(|text| text == COMPACTION_PROMPT)
    );
    assert!(upstream_body.get("reasoning_effort").is_none());
    server.shutdown().expect("shutdown");
}

#[test]
fn external_compaction_trigger_streams_one_emp_owned_checkpoint() {
    let upstream = chat_summary_upstream("stream checkpoint");
    let (_directory, server) = configured_server(&upstream.base_url());
    let request_body = serde_json::to_vec(&json!({
        "model": "demo/model",
        "stream": true,
        "input": [{
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "history"}]
        }, {"type":"compaction_trigger"}]
    }))
    .expect("request JSON");
    let response = post_stream(
        &server,
        "/v1/responses",
        &request_body,
        &[&session_cookie_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("Content-Type: text/event-stream\r\n"));
    assert!(!response.contains("Content-Length:"));
    assert!(response.contains("event: response.output_item.done\n"));
    assert!(response.contains("event: response.completed\n"));
    assert_eq!(response.matches("\"type\": \"compaction\"").count(), 3);

    let (_, _, upstream_body) = upstream.observed();
    assert!(!upstream_body.to_string().contains("compaction_trigger"));
    server.shutdown().expect("shutdown");
}

#[test]
fn native_checkpoint_switch_to_external_rebuilds_visible_codex_history() {
    let upstream = OneShotUpstream::start(json!({
        "id": "chat_upstream", "model": "upstream-model",
        "object": "chat.completion",
        "choices": [{"index":0,"message":{"role":"assistant","content":"continued"},"finish_reason":"stop"}]
    }));
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = canonical_root(&directory);
    let config = root.join("config.json");
    std::fs::write(
            &config,
            serde_json::to_vec_pretty(&json!({
                "providers":[{"id":"demo","name":"Demo","base_url":upstream.base_url(),"protocol":"chat_completions","auth_mode":"api_key","api_key":"upstream-secret"}],
                "models":[{"id":"demo/model","provider":"demo","upstream_id":"upstream-model","enabled":true}]
            }))
            .unwrap(),
        )
        .unwrap();
    let thread_id = "01a00000-0000-7000-8000-000000000001";
    let turn_id = "01a00000-0000-7000-8000-000000000002";
    let rollout = root.join("rollout.jsonl");
    let records = [
        json!({"type":"session_meta","payload":{"id":thread_id,"history_mode":"legacy"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":"keep this constraint"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":"completed old work"}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"old"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"compact"}}),
        json!({"type":"compacted","payload":{"message":""}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"compact"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn_id}}),
    ];
    std::fs::write(
        &rollout,
        records
            .iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let database = rusqlite::Connection::open(root.join("state_5.sqlite")).unwrap();
    database.execute("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, model TEXT)", []).unwrap();
    database
        .execute(
            "INSERT INTO threads VALUES (?1, ?2, 'legacy', 'gpt-native')",
            rusqlite::params![thread_id, rollout.to_str().unwrap()],
        )
        .unwrap();
    drop(database);
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "codex",
        root.join("auth.json"),
    )
    .expect("start history server");
    let body = serde_json::to_vec(&json!({
        "model":"demo/model",
        "stream":false,
        "input":[
            {"type":"compaction","encrypted_content":"native-opaque"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"continue now"}]}
        ]
    }))
    .unwrap();
    let metadata = format!("{{\"thread_id\":\"{thread_id}\",\"turn_id\":\"{turn_id}\"}}");
    let response = post(
        &server,
        "/v1/responses",
        &body,
        &[
            &session_cookie_header(&server),
            &format!("thread-id: {thread_id}"),
            &format!("x-codex-turn-metadata: {metadata}"),
        ],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let (_, _, upstream_body) = upstream.observed();
    let projected = upstream_body.to_string();
    assert!(projected.contains("keep this constraint"));
    assert!(projected.contains("completed old work"));
    assert!(projected.contains("continue now"));
    assert!(!projected.contains("native-opaque"));
    server.shutdown().expect("shutdown");
}

#[test]
fn long_to_short_external_switch_compacts_before_the_destination_request() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind compaction upstream");
    let address = listener.local_addr().unwrap();
    let (sender, observed) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let mut requests = Vec::new();
        loop {
            let (mut stream, _) = listener.accept().expect("accept compaction request");
            let (_, _, body) = receive_upstream_request(&mut stream);
            let wire = body.to_string();
            let summary = wire.contains("structured portable checkpoint")
                || wire.contains("Merge the visible portable checkpoints");
            requests.push(body);
            let answer = if summary {
                "checkpoint"
            } else {
                "final answer"
            };
            let response_body = serde_json::to_vec(&json!({
                    "id":"chat","object":"chat.completion","model":"short-model",
                    "choices":[{"index":0,"message":{"role":"assistant","content":answer},"finish_reason":"stop"}]
                })).unwrap();
            stream.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_body.len()
                ).as_bytes()).unwrap();
            stream.write_all(&response_body).unwrap();
            if !summary {
                sender.send(requests).unwrap();
                break;
            }
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let config = root.join("config.json");
    std::fs::write(&config, serde_json::to_vec_pretty(&json!({
            "providers":[{"id":"short","name":"Short","base_url":format!("http://{address}/v1"),"protocol":"chat_completions","auth_mode":"api_key","api_key":"key"}],
            "models":[{"id":"short/model","provider":"short","upstream_id":"short-model","enabled":true,
                "context_window":1200,"output_limit":64,
                "capability_sources":{"context_window":{"source":"manual","confidence":1.0}}}]
        })).unwrap()).unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "codex",
        root.join("auth.json"),
    )
    .unwrap();
    let body = serde_json::to_vec(&json!({
            "model":"short/model","stream":false,"max_output_tokens":64,
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"x".repeat(500)}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"y".repeat(500)}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"z".repeat(500)}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"w".repeat(500)}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"active request"}]}
            ]
        })).unwrap();
    let response = post(
        &server,
        "/v1/responses",
        &body,
        &[&session_cookie_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let requests = observed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        requests.len() >= 2,
        "summary request plus destination request"
    );
    let final_request = requests.last().unwrap().to_string();
    assert!(final_request.contains("checkpoint"));
    assert!(final_request.contains("active request"));
    assert!(!final_request.contains(&"x".repeat(500)));
    for summary in &requests[..requests.len() - 1] {
        assert_eq!(summary["stream"], false);
        assert!(!summary.to_string().contains("active request"));
    }
    server.shutdown().unwrap();
    worker.join().unwrap();
}
