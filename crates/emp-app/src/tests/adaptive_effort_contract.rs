use super::auto_review_contract::make_review_server;
use super::*;

#[test]
fn adaptive_effort_changes_with_tool_evidence_and_replays_updates_without_new_user_turns() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (sender, requests) = mpsc::channel();
    let worker = thread::spawn(move || {
        // Three classifier calls, one full replay of a delta, and one exact retry.
        for index in 0..8 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                if let Ok((stream, _)) = listener.accept() {
                    break stream;
                }
                assert!(Instant::now() < deadline, "missing request {index}");
                thread::sleep(Duration::from_millis(5));
            };
            let observed = receive_upstream_request(&mut stream);
            let classifier = observed.2["model"] == "gpt-6-luna";
            let answer = if classifier {
                let context = observed.2["input"][0]["content"].as_str().unwrap();
                if context.contains("tests passed") {
                    "low"
                } else if context.contains("conflicting failure") {
                    "xhigh"
                } else {
                    "low"
                }
            } else {
                "OK"
            };
            let answer = if classifier {
                json!({"effort":answer}).to_string()
            } else {
                answer.into()
            };
            sender.send(observed).unwrap();
            let response = json!({"id":format!("resp_{index}"),"object":"response","status":"completed",
                "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":answer}]}],
                "usage":{"input_tokens":20,"output_tokens":2}});
            let bytes = serde_json::to_vec(&response).unwrap();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).unwrap();
            stream.write_all(&bytes).unwrap();
        }
    });
    let (_directory, server, config) = make_review_server(&base, &[], Some("regular"));
    let models = json!({"models":[{"slug":"gpt-6-astra","supported_in_api":true,"context_window":128000,
        "supports_reasoning_effort_updates":true,"default_reasoning_level":"high",
        "supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"},{"effort":"xhigh"}]}]});
    std::fs::write(
        config["native_catalog_path"].as_str().unwrap(),
        serde_json::to_vec(&models).unwrap(),
    )
    .unwrap();
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["adaptive_effort"]["enabled"] = json!(true);
    let auth = session_header(&server);
    let metadata = json!({"thread_id":"adaptive-thread"}).to_string();
    let mut body = json!({"model":"gpt-6-astra","stream":false,"reasoning":{"effort":"high"},
        "prompt_cache_key":"adaptive-thread","client_metadata":{"x-codex-turn-metadata":metadata},
        "input":[{"role":"user","content":"Fix the bug and verify it"}]});
    let mut main = Vec::new();
    let mut full_input = body["input"].as_array().unwrap().clone();
    for index in 0..5 {
        if (1..=2).contains(&index) {
            full_input.extend([
                json!({"type":"function_call","name":"test","call_id":format!("call_{index}"),"arguments":"{}"}),
                json!({"type":"function_call_output","call_id":format!("call_{index}"),
                    "output":if index == 1 { "conflicting failure" } else { "tests passed" }}),
            ]);
        }
        body["input"] = json!(full_input);
        if index == 2 {
            body["input"] = json!(&full_input[full_input.len() - 2..]);
            body["previous_response_id"] = json!("resp_3");
        } else {
            body.as_object_mut().unwrap().remove("previous_response_id");
        }
        let wire = post(
            &server,
            "/v1/responses",
            &serde_json::to_vec(&body).unwrap(),
            &[&auth],
        );
        assert!(wire.starts_with("HTTP/1.1 200"), "{wire}");
        if index < 3 {
            let (_, headers, classifier) = requests.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(classifier["model"], "gpt-6-luna");
            assert_eq!(headers["authorization"], "Bearer native-secret");
            assert!(!headers.contains_key("x-codex-turn-metadata"));
        }
        let (_, headers, sent) = requests.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!headers.contains_key("x-emp-effort-decision"));
        assert_eq!(
            sent["reasoning"]["effort"], "high",
            "root baseline must stay pinned"
        );
        let updates = sent["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "configuration_update")
            .map(|item| item["reasoning"]["effort"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            updates,
            match index {
                0 => vec!["low"],
                1 => vec!["low", "xhigh"],
                2 => vec!["low"],
                _ => vec!["low", "xhigh", "low"],
            }
        );
        main.push(sent);
    }
    assert_eq!(main[3], main[4], "exact retry keeps the same projection");
    assert_eq!(
        main[0]["input"],
        json!(&main[3]["input"].as_array().unwrap()[..2]),
        "full replay retains the original prefix"
    );
    worker.join().unwrap();
    let calls = request(&server, "/api/calls?start=0&end=9999999999", &[&auth]);
    let calls: Value = serde_json::from_str(calls.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let records = calls["records"].as_array().unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|row| row["route"] == "effort_classification")
            .count(),
        3
    );
    let selected: Vec<_> = records
        .iter()
        .filter(|row| row["route"] == "responses")
        .collect();
    assert_eq!(selected.len(), 5);
    assert!(
        selected
            .iter()
            .all(|row| row["configured_effort"] == "high")
    );
    assert!(
        selected
            .iter()
            .any(|row| row["requested_effort"] == "xhigh")
    );
    assert_eq!(
        selected
            .iter()
            .filter(|row| row["adaptive_effort"]["cached"] == true)
            .count(),
        2
    );
    assert!(
        records
            .iter()
            .all(|row| row["thread_id"] == "adaptive-thread"),
        "{records:?}"
    );
}

#[test]
fn adaptive_effort_keeps_manual_requests_when_disabled_or_unsupported_and_on_invalid_selection() {
    for (enabled, supported, subagent) in [
        (false, true, false),
        (true, false, false),
        (true, true, true),
        (true, true, false),
    ] {
        let classify = enabled && supported && !subagent;
        let response = json!({"id":"resp_manual","object":"response","status":"completed",
            "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"not an effort"}]}],
            "usage":{"input_tokens":20,"output_tokens":2}});
        let upstream = OneShotUpstream::start_repeated_wire(
            200,
            "application/json",
            None,
            if classify { 2 } else { 1 },
            vec![serde_json::to_vec(&response).unwrap()],
        );
        let (_directory, server, config) =
            make_review_server(&upstream.base_url(), &[], Some("regular"));
        let catalog = json!({"models":[{"slug":"gpt-6-astra","supported_in_api":true,
            "supports_reasoning_effort_updates":supported,"default_reasoning_level":"high",
            "supported_reasoning_levels":[{"effort":"low"},{"effort":"high"},{"effort":"xhigh"}]}]});
        std::fs::write(
            config["native_catalog_path"].as_str().unwrap(),
            serde_json::to_vec(&catalog).unwrap(),
        )
        .unwrap();
        server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap()["adaptive_effort"]["enabled"] = json!(enabled);
        let auth = session_header(&server);
        let mut headers = vec![
            auth.as_str(),
            "X-EMP-Effort-Decision: not valid internal JSON",
        ];
        if subagent {
            headers.push("x-openai-subagent: collab_spawn");
        }
        let body = json!({"model":"gpt-6-astra","reasoning":{"effort":"high"},"stream":false,
            "input":[{"role":"user","content":"Inspect this task"}]});
        let wire = post(
            &server,
            "/v1/responses",
            &serde_json::to_vec(&body).unwrap(),
            &headers,
        );
        assert!(wire.starts_with("HTTP/1.1 200"), "{wire}");
        if classify {
            assert_eq!(upstream.observed().2["model"], "gpt-6-luna");
        }
        let sent = upstream.observed().2;
        assert_eq!(sent["reasoning"]["effort"], "high");
        assert_eq!(sent["input"], body["input"]);
        let wire = request(&server, "/api/calls?start=0&end=9999999999", &[&auth]);
        let report: Value = serde_json::from_str(wire.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let main = report["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["route"] == "responses")
            .unwrap();
        if classify {
            assert_eq!(main["adaptive_effort"]["fallback"], "invalid_selection");
            let classification = report["records"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["route"] == "effort_classification")
                .unwrap();
            assert_eq!(classification["state"], "failed");
            assert_eq!(
                classification["error_class"],
                "effort_classification_invalid_selection"
            );
        } else {
            assert!(main.get("adaptive_effort").is_none());
        }
    }
}
