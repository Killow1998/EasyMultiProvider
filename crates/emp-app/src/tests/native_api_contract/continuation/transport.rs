//! A failing large upload must not trap client retries on the same transport.
use super::*;

#[test]
fn native_failed_upload_recovers_over_http_without_replaying_or_disrupting_healthy_sockets() {
    for imported in [false, true] {
        let upstream = Upstream::with_fault(10 * 1024 * 1024, Fault::DisconnectFull);
        let (directory, server, model) = server(&upstream, imported);
        let mut healthy = connect(&server, "healthy-transport-fixture");
        let healthy_id = completed(turn(&mut healthy, &small_request(&model, None, "window")));
        let mut input = initial_input();
        input.truncate(17);
        let full = json!({"type":"response.create","model":model,"input":input});
        let mut client = connect(&server, "failed-upload-fixture");
        let failed = turn(&mut client, &full);
        assert_eq!(failed["response"]["error"]["status"], 502, "{failed}");
        assert_eq!(
            upstream.traffic.lock().unwrap().http_requests,
            0,
            "an uncertain failed send must not automatically repeat inference"
        );
        completed(turn(
            &mut healthy,
            &small_request(&model, Some(&healthy_id), "window"),
        ));
        client.close();
        client = connect(&server, "failed-upload-fixture");
        let recovered_id = completed(turn(&mut client, &full));
        let delta = json!({"role":"user","content":"continue after HTTP recovery"});
        let recovery = turn(
            &mut client,
            &json!({"type":"response.create","model":model,"previous_response_id":recovered_id,"input":[delta]}),
        );
        assert_eq!(recovery["error"]["code"], "previous_response_not_found");
        input.push(delta);
        completed(turn(
            &mut client,
            &json!({"type":"response.create","model":model,"input":input}),
        ));
        client.close();
        healthy.close();
        server.shutdown().unwrap();
        let stats = upstream.traffic.lock().unwrap();
        assert_eq!(
            stats.ws_connections, 2,
            "only the healthy and failed sockets"
        );
        assert_eq!(stats.http_requests, 2, "explicit full retries use HTTP");
        assert_eq!(
            stats.generated_hashes.len(),
            4,
            "one execution per successful request"
        );
        let mut hash = Sha256::new();
        append_input(&mut hash, &full);
        assert_eq!(stats.generated_hashes[2], format!("{:x}", hash.finalize()));
        let mut hash = Sha256::new();
        append_input(&mut hash, &json!({"input":input}));
        assert_eq!(stats.generated_hashes[3], format!("{:x}", hash.finalize()));
        let journal = crate::tests::internal_events_contract::journal(directory.path());
        let receipts = receipts(&journal);
        assert!(
            receipts
                .iter()
                .any(|receipt| receipt["peer_close_code"] == 1011)
        );
        assert_eq!(
            receipts
                .iter()
                .filter(|receipt| receipt["http_fallback"] == true)
                .count(),
            2
        );
        assert_private(&journal);
    }
}

#[test]
fn native_write_and_prefill_disconnects_use_http_only_after_the_client_retries() {
    for prefill in [false, true] {
        let upstream = Upstream::with_fault(10 * 1024 * 1024, Fault::ResetConnection);
        let (directory, server, model) = server(&upstream, true);
        let mut input = initial_input();
        if !prefill {
            input.truncate(4);
        }
        let full = json!({"type":"response.create","model":model,"input":input});
        let mut client = connect(&server, "reset-upload-fixture");
        let failed = turn(&mut client, &full);
        assert_eq!(failed["type"], "response.failed", "{failed}");
        assert_eq!(upstream.traffic.lock().unwrap().http_requests, 0);
        client.close();
        client = connect(&server, "reset-upload-fixture");
        completed(turn(&mut client, &full));
        client.close();
        server.shutdown().unwrap();
        let stats = upstream.traffic.lock().unwrap();
        assert_eq!(stats.ws_connections, 1, "prefill={prefill}");
        assert_eq!(stats.http_requests, 1, "prefill={prefill}");
        let mut hash = Sha256::new();
        append_input(&mut hash, &full);
        assert_eq!(stats.generated_hashes, [format!("{:x}", hash.finalize())]);
        let journal = crate::tests::internal_events_contract::journal(directory.path());
        assert!(
            receipts(&journal)
                .iter()
                .any(|receipt| receipt["transport_error"].is_string())
        );
        assert_private(&journal);
    }
}

#[test]
fn native_http_fallback_recovers_on_the_same_connection_after_cooldown() {
    let mut cases = Vec::new();
    for (imported, fault) in [
        (false, Fault::DisconnectFullOnce),
        (true, Fault::DisconnectFullOnce),
        (true, Fault::DisconnectFull),
        (false, Fault::None),
    ] {
        let limit = if fault == Fault::None {
            4096
        } else {
            10 * 1024 * 1024
        };
        let upstream = Upstream::with_fault(limit, fault);
        let (directory, server, model) = server(&upstream, imported);
        let mut client = connect(&server, "transient-transport-fixture");
        let mut full = small_request(&model, None, "window");
        full["input"] = json!([{"role":"user","content":"x".repeat(600 * 1024)}]);
        let first = turn(&mut client, &full);
        if fault == Fault::None {
            completed(first);
        } else {
            assert_eq!(first["response"]["error"]["status"], 502);
            assert_eq!(upstream.traffic.lock().unwrap().http_requests, 0);
            completed(turn(&mut client, &full));
        }
        let http_id = completed(turn(&mut client, &full));
        assert_eq!(upstream.traffic.lock().unwrap().ws_connections, 1);
        assert_eq!(upstream.traffic.lock().unwrap().http_requests, 2);
        cases.push((
            upstream, directory, server, model, client, full, http_id, fault,
        ));
    }
    // Exercise the real cooldown without a test clock or production test hooks.
    thread::sleep(Duration::from_secs(30));
    for (upstream, directory, server, model, mut client, mut full, http_id, fault) in cases {
        let stale = turn(
            &mut client,
            &small_request(&model, Some(&http_id), "window"),
        );
        assert_eq!(stale["error"]["code"], "previous_response_not_found");
        assert_eq!(
            upstream.traffic.lock().unwrap().ws_connections,
            1,
            "an HTTP response ID cannot be sent to a fresh WebSocket"
        );
        if fault == Fault::None {
            full["input"] = json!([{"role":"user","content":"synthetic compacted history"}]);
        }
        let result = turn(&mut client, &full);
        assert_eq!(
            upstream.traffic.lock().unwrap().ws_connections,
            2,
            "full-history requests must regain WebSocket after cooldown"
        );
        if fault == Fault::DisconnectFull {
            assert_eq!(result["response"]["error"]["status"], 502);
            assert_eq!(
                upstream.traffic.lock().unwrap().http_requests,
                2,
                "a failed probe must not automatically repeat inference"
            );
            completed(turn(&mut client, &full));
            completed(turn(&mut client, &full));
        } else {
            let prior = completed(result);
            let mut hash = Sha256::new();
            append_input(&mut hash, &full);
            assert_eq!(
                upstream
                    .traffic
                    .lock()
                    .unwrap()
                    .generated_hashes
                    .last()
                    .unwrap(),
                &format!("{:x}", hash.finalize()),
                "recovery must resend complete history"
            );
            completed(turn(
                &mut client,
                &small_request(&model, Some(&prior), "window"),
            ));
        }
        client.close();
        server.shutdown().unwrap();
        let stats = upstream.traffic.lock().unwrap();
        assert_eq!(
            stats.ws_connections, 2,
            "one new attempt after the cooldown"
        );
        assert_eq!(
            stats.http_requests,
            if fault == Fault::DisconnectFull { 4 } else { 2 }
        );
        assert_eq!(
            stats.generated_hashes.len(),
            4,
            "one execution per successful request"
        );
        assert_private(&crate::tests::internal_events_contract::journal(
            directory.path(),
        ));
    }
}

#[test]
fn native_connection_expiry_discards_the_old_socket_and_keeps_new_connections_native() {
    let upstream = Upstream::with_fault(10 * 1024 * 1024, Fault::ExpireIncremental);
    let (_directory, server, model) = server(&upstream, false);
    let mut client = connect(&server, "expired-connection-fixture");
    let original = completed(turn(&mut client, &small_request(&model, None, "window")));
    let expired = turn(
        &mut client,
        &small_request(&model, Some(&original), "window"),
    );
    assert_eq!(
        expired["error"]["code"],
        "websocket_connection_limit_reached"
    );
    let stale = turn(
        &mut client,
        &small_request(&model, Some(&original), "window"),
    );
    assert_eq!(stale["error"]["code"], "previous_response_not_found");
    let recovered = completed(turn(&mut client, &small_request(&model, None, "window")));
    completed(turn(
        &mut client,
        &small_request(&model, Some(&recovered), "window"),
    ));
    client.close();
    server.shutdown().unwrap();
    let stats = upstream.traffic.lock().unwrap();
    assert_eq!(stats.ws_connections, 2);
    assert_eq!(stats.http_requests, 0);
    assert_eq!(stats.generated_hashes.len(), 3);
}

#[test]
#[ignore = "requires an explicitly selected installed Codex and isolated loopback access"]
fn installed_codex_recovers_a_failed_native_upload_then_resumes_websocket() {
    use super::super::installed_codex::Codex;
    assert!(std::env::var_os("EMP_TEST_CODEX_CLI").is_some());
    let upstream = Upstream::with_fault(10 * 1024 * 1024, Fault::DisconnectFullOnce);
    let (directory, server, model) = server(&upstream, true);
    let home = directory.path().canonicalize().unwrap().join("runtime");
    std::fs::create_dir(&home).unwrap();
    let config = json!({"providers":[{"id":"fixture","base_url":"http://127.0.0.1:1/v1",
        "protocol":"responses","api_key":"fixture-only"}],
        "models":[{"id":"fixture/model","provider":"fixture","upstream_id":"upstream","enabled":true}]});
    let mut catalog = emp_codex::merged_catalog::build_catalog(
        &config,
        &json!({"models":[]}),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    catalog["models"][0]["slug"] = json!(model);
    catalog["models"][0]["context_window"] = json!(272000);
    let path = home.join("catalog.json");
    std::fs::write(&path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let settings = format!(
        "model = {}\nmodel_provider = \"fixture\"\nmodel_catalog_json = {}\napproval_policy = \"never\"\nsandbox_mode = \"read-only\"\nweb_search = \"disabled\"\n[features]\nplugins = false\n[model_providers.fixture]\nname = \"Fixture\"\nbase_url = \"http://{}/v1\"\nwire_api = \"responses\"\nenv_key = \"EMP_TEST_KEY\"\nrequires_openai_auth = false\nsupports_websockets = true\nhttp_headers = {{ \"X-EMP-Session\" = {} }}\n",
        serde_json::to_string(&model).unwrap(),
        serde_json::to_string(&path).unwrap(),
        server.local_addr(),
        serde_json::to_string(&server.session_token()).unwrap()
    );
    std::fs::write(home.join("config.toml"), settings).unwrap();
    let mut codex = Codex::start(&home);
    let info = codex.rpc(
        1,
        "initialize",
        json!({"clientInfo":{"name":"transport_recovery_fixture","version":"1"},
        "capabilities":{"experimentalApi":true}}),
    );
    eprintln!("transport recovery installed engine: {}", info["userAgent"]);
    codex.send(json!({"method":"initialized"}));
    let thread = codex.rpc(2, "thread/start", json!({"model":model,"cwd":home,"ephemeral":true,
        "approvalPolicy":"never","sandbox":"read-only","baseInstructions":"Reply only OK. Do not call tools."}));
    let id = &thread["thread"]["id"];
    for (number, text) in [
        (3, format!("{}Reply only OK.", "fixture ".repeat(100_000))),
        (4, "Reply only OK.".into()),
        (5, "Reply only OK.".into()),
        (6, "Reply only OK.".into()),
    ] {
        if number == 5 {
            thread::sleep(Duration::from_secs(30));
        }
        codex.rpc(
            number,
            "turn/start",
            json!({"threadId":id,"input":[{"type":"text","text":text}]}),
        );
        let saw_reply = std::cell::Cell::new(false);
        let done = codex.wait(|event| {
            if event["method"] == "item/completed"
                && event["params"]["item"]["type"] == "agentMessage"
                && event["params"]["item"]["text"] == "OK"
            {
                saw_reply.set(true);
            }
            event["method"] == "turn/completed" && event["params"]["threadId"] == *id
        });
        assert_eq!(done["params"]["turn"]["status"], "completed", "{done}");
        assert!(
            saw_reply.get(),
            "the recovered reply must reach the real client"
        );
    }
    drop(codex);
    server.shutdown().unwrap();
    let stats = upstream.traffic.lock().unwrap();
    assert_eq!(
        stats.ws_connections, 2,
        "one failed and one recovered native connection"
    );
    assert_eq!(
        stats.http_requests, 2,
        "only the two cooldown turns use HTTP"
    );
    assert_eq!(
        stats.generated_hashes.len(),
        4,
        "one successful inference per turn"
    );
    let journal = crate::tests::internal_events_contract::journal(directory.path());
    let receipts = receipts(&journal);
    let restored = receipts[receipts.len() - 2];
    let incremental = receipts[receipts.len() - 1];
    assert_eq!(incremental["connection_reused"], true);
    assert!(
        incremental["upstream_decoded_bytes_sent"].as_u64().unwrap() * 2
            < restored["upstream_decoded_bytes_sent"].as_u64().unwrap(),
        "restored WebSocket turns must upload deltas instead of complete history"
    );
    assert_private(&journal);
}
