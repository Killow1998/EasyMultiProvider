use super::*;

#[test]
fn agent_calls_keep_client_and_session_attribution_through_http_and_billing() {
    let cases = [
        (
            vec!["User-Agent: Pi/1.4.0", "x-session-id: pi-task"],
            json!({}),
            Some("pi-task"),
            Some("Pi/1.4.0"),
        ),
        (
            vec!["User-Agent: Pi/1.4.0", "x-session-id: pi-task"],
            json!({"stream":true}),
            Some("pi-task"),
            Some("Pi/1.4.0"),
        ),
        (
            vec!["originator: OMP", "session_id: omp-task"],
            json!({}),
            Some("omp-task"),
            Some("OMP"),
        ),
        (
            vec![
                "X-EMP-Client: Python script",
                "User-Agent: OpenAI/Python 1.0",
            ],
            json!({"metadata":{"session_id":"python-task"}}),
            Some("python-task"),
            Some("Python script"),
        ),
        (
            vec![
                "User-Agent: python-requests/2.32",
                "session-id: custom-task",
            ],
            json!({}),
            Some("custom-task"),
            Some("python-requests/2.32"),
        ),
        (
            vec![
                "originator: codex_cli_rs",
                "thread-id: codex-thread",
                "session-id: runtime-session",
            ],
            json!({}),
            Some("codex-thread"),
            Some("codex_cli_rs"),
        ),
        (
            vec!["User-Agent: python-requests/2.32"],
            json!({}),
            None,
            Some("python-requests/2.32"),
        ),
        (vec![], json!({}), None, None),
        (
            vec!["User-Agent: OpenAI/Python 1.0"],
            json!({"metadata":{"session_id":"x".repeat(257)}}),
            None,
            Some("OpenAI/Python"),
        ),
        (
            vec!["User-Agent: Pi/1.4.0", "x-client-request-id: request-only"],
            json!({}),
            None,
            Some("Pi/1.4.0"),
        ),
        (
            vec!["User-Agent: OpenAI/Python 1.0", "thread-id: python-thread"],
            json!({}),
            Some("python-thread"),
            Some("OpenAI/Python"),
        ),
    ];
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let count = cases.len();
    let worker = thread::spawn(move || {
        let mut ids = Vec::new();
        for index in 0..count {
            let (mut stream, _) = listener.accept().unwrap();
            let (_, headers, payload) = receive_upstream_request(&mut stream);
            assert_eq!(headers["authorization"], "Bearer upstream-secret");
            ids.push(headers["x-emp-request-id"].clone());
            let body = json!({"id":format!("agent-response-{index}"),"object":"chat.completion",
                "model":"upstream-model","choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}});
            let (content_type, wire) = if payload["stream"] == true {
                let mut chunk = body;
                chunk["choices"][0]["delta"] = chunk["choices"][0]["message"].take();
                (
                    "text/event-stream",
                    format!("data: {chunk}\n\ndata: [DONE]\n\n").into_bytes(),
                )
            } else {
                ("application/json", serde_json::to_vec(&body).unwrap())
            };
            stream
                .write_all(&crate::http::response::response(
                    "HTTP/1.1 200 OK",
                    content_type,
                    &wire,
                    &[],
                ))
                .unwrap();
        }
        ids
    });
    let (_directory, server) = configured_server(&endpoint);
    let management = session_header(&server);
    let device = server
        .state
        .backend
        .gateway_clients
        .issue(
            "Temporary tools",
            None,
            &server.state.backend.configuration.vault,
        )
        .unwrap();
    let device_auth = format!(
        "Authorization: Bearer {}",
        device["api_key"].as_str().unwrap()
    );
    let unused_device = server
        .state
        .backend
        .gateway_clients
        .issue(
            "Spare device",
            None,
            &server.state.backend.configuration.vault,
        )
        .unwrap();
    let shared_auth = format!("Authorization: Bearer {}", server.state.backend.gateway_key);
    for (index, (headers, fields, _, _)) in cases.iter().enumerate() {
        let mut headers = headers.clone();
        headers.push(if matches!(index, 6 | 7) {
            &device_auth
        } else {
            &shared_auth
        });
        let mut body = json!({"model":"demo/model","input":"public fixture","stream":false});
        body.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let send = if body["stream"] == true {
            post_stream
        } else {
            post
        };
        let response = send(
            &server,
            "/v1/responses",
            &serde_json::to_vec(&body).unwrap(),
            &headers,
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("done"), "{response}");
    }
    let ids = worker.join().unwrap();
    let read = |path: &str| -> Value {
        let response = request(&server, path, &[&management]);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
    };
    let calls = read("/api/calls?start=0");
    assert_eq!(calls["total"], count);
    for (index, (_, _, session, client)) in cases.iter().enumerate() {
        let row = calls["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["request_id"] == ids[index])
            .unwrap();
        assert_eq!(
            row["session_id"].as_str(),
            Some(session.unwrap_or("")),
            "{row}"
        );
        assert_eq!(row["client_kind"].as_str(), *client, "{row}");
        if index == 6 {
            assert_eq!(row["caller_name"], "Temporary tools");
        }
        assert!(!row.to_string().contains("public fixture"));
        assert!(!row.to_string().contains("upstream-secret"));
    }
    let bills = read("/api/usage?sessions=true&start=0");
    assert_eq!(bills["total"], 5, "{bills}");
    assert_eq!(bills["totals"]["requests"], count);
    assert_eq!(bills["unassigned"]["requests"], 5);
    assert_eq!(
        bills["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["session_id"] == "pi-task")
            .unwrap()["requests"],
        2
    );
    let callers = read("/api/gateway/usage?start=0");
    assert!(
        callers["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["client_kind"] == "OMP" && row["calls"] == 1)
    );
    assert!(
        callers["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["caller_name"] == "Temporary tools" && row["calls"] == 1)
    );
    let usage = read("/api/gateway/usage?group=device&start=0");
    let rows = usage["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["caller_id"], device["id"]);
    assert_eq!(rows[0]["caller_name"], "Temporary tools");
    assert_eq!(rows[0]["calls"], 2);
    assert_eq!(rows[0]["input_tokens"], 14);
    assert_eq!(rows[0]["output_tokens"], 6);
    assert_eq!(rows[0]["reported_calls"], 2);
    let models = rows[0]["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["model"], "upstream-model");
    assert_eq!(models[0]["calls"], 2);
    assert_eq!(models[0]["input_tokens"], 14);
    assert_eq!(models[0]["output_tokens"], 6);
    assert_eq!(models[0]["reported_calls"], 2);
    assert_eq!(rows[1]["caller_id"], unused_device["id"]);
    assert_eq!(rows[1]["calls"], 0);
    assert_eq!(rows[1]["models"], json!([]));
    assert!(rows.iter().all(|row| row.get("api_key").is_none()));
    server.shutdown().unwrap();
}
