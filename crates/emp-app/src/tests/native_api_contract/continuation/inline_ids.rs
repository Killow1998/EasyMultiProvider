//! Exercise real WS forwarding, continuation, accounting and replay limits.
use super::*;

fn input() -> Value {
    json!([
        {"type":"additional_tools","id":"at_client","role":"developer","tools":[]},
        {"type":"message","id":"msg_client","role":"user","content":[{"type":"input_text","text":"synthetic task"}]},
        {"type":"reasoning","id":"rs_native","encrypted_content":"synthetic-ciphertext"},
        {"type":"function_call","id":"fc_native","call_id":"fixture-call","name":"fixture","arguments":"{}"},
        {"type":"function_call_output","id":"fco_native","call_id":"fixture-call","output":"synthetic result"},
        {"type":"item_reference","id":"msg_reference"},
        {"type":"message","id":"msg_opaque","role":"assistant"}
    ])
}

#[test]
fn inline_ids_recover_once_and_preserve_native_continuation_and_accounting() {
    for (imported, incremental) in [(false, false), (true, false), (false, true), (true, true)] {
        let upstream = Upstream::with_fault(1024 * 1024, Fault::RejectInlineIds);
        let (directory, server, model) = server(&upstream, imported);
        let mut client = connect(&server, "inline-replay-fixture");
        let prior = incremental.then(|| {
            completed(turn(
                &mut client,
                &json!({"type":"response.create","model":model,"generate":false,"input":[]}),
            ))
        });
        let first = turn(
            &mut client,
            &json!({"type":"response.create","model":model,"previous_response_id":prior,"input":input()}),
        );
        let prior = completed(first);
        let delta = json!([{"type":"message","id":"msg_next","role":"user","content":[]}]);
        completed(turn(
            &mut client,
            &json!({"type":"response.create","model":model,
            "previous_response_id":prior,"input":delta}),
        ));
        let report = request(
            &server,
            "/api/calls?start=0&end=9999999999",
            &[&session_header(&server)],
        );
        let report: Value = serde_json::from_str(report.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(report["total"], 2, "one logical outcome per user request");
        let mut attempts = report["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                assert_eq!(r["state"], "completed");
                r["attempts"].as_u64().unwrap()
            })
            .collect::<Vec<_>>();
        attempts.sort();
        assert_eq!(attempts, [1, 2]);
        client.close();
        server.shutdown().unwrap();
        let mut projected = input();
        projected[0].as_object_mut().unwrap().remove("id");
        projected[1].as_object_mut().unwrap().remove("id");
        let mut hash = Sha256::new();
        append_input(&mut hash, &json!({"input":projected}));
        let expected_first = format!("{:x}", hash.clone().finalize());
        let mut delta = delta;
        delta[0].as_object_mut().unwrap().remove("id");
        append_input(&mut hash, &json!({"input":delta}));
        let stats = upstream.traffic.lock().unwrap();
        assert_eq!(
            stats.generated_hashes,
            [expected_first, format!("{:x}", hash.finalize())]
        );
        assert_eq!(
            stats.inline_rejections, 1,
            "remember the compatibility result on this socket"
        );
        assert_eq!(stats.ws_connections, 1);
        assert_eq!(stats.http_requests, 0);
        let journal = crate::tests::internal_events_contract::journal(directory.path());
        assert_eq!(
            journal
                .iter()
                .filter(|e| e["event"] == "native_retry_decision"
                    && e["fields"]["reason"] == "inline_input_replay")
                .count(),
            1
        );
        assert_private(&journal);
    }
}

#[test]
fn inline_ids_preserve_unrelated_errors_and_never_loop_or_replay_started_responses() {
    for (fault, expected_attempts, items) in [
        (Fault::RejectAllInputs, 2, input()),
        (Fault::RejectOtherInput, 1, input()),
        (Fault::RejectAfterCreated, 1, input()),
        (
            Fault::RejectAllInputs,
            1,
            json!([{"type":"item_reference","id":"msg_reference"}]),
        ),
    ] {
        let upstream = Upstream::with_fault(1024 * 1024, fault);
        let (_directory, server, model) = server(&upstream, false);
        let mut client = connect(&server, "inline-replay-limit-fixture");
        let terminal = turn(
            &mut client,
            &json!({"type":"response.create","model":model,"input":items}),
        );
        assert_eq!(terminal["error"]["code"], "unsupported_parameter");
        client.close();
        server.shutdown().unwrap();
        let stats = upstream.traffic.lock().unwrap();
        assert_eq!(stats.inline_rejections, expected_attempts);
        assert!(stats.generated_hashes.is_empty());
    }
}

#[test]
#[ignore = "requires an explicitly selected installed Codex and loopback access"]
fn installed_codex_recovers_inline_ids_and_completes_two_turns() {
    use super::super::installed_codex::Codex;
    assert!(std::env::var_os("EMP_TEST_CODEX_CLI").is_some());
    for imported in [false, true] {
        let upstream = Upstream::with_fault(1024 * 1024, Fault::RejectInlineIds);
        let (directory, server, model) = server(&upstream, imported);
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
        catalog["models"][0]["use_responses_lite"] = json!(true);
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
            json!({"clientInfo":{"name":"inline_id_fixture","version":"1"},
            "capabilities":{"experimentalApi":true}}),
        );
        eprintln!("inline ID installed engine: {}", info["userAgent"]);
        codex.send(json!({"method":"initialized"}));
        let thread = codex.rpc(2, "thread/start", json!({"model":model,"cwd":home,"ephemeral":true,
            "approvalPolicy":"never","sandbox":"read-only","baseInstructions":"Reply only OK. Do not call tools."}));
        let id = &thread["thread"]["id"];
        for number in [3, 4] {
            codex.rpc(
                number,
                "turn/start",
                json!({"threadId":id,
                "input":[{"type":"text","text":"Reply only OK."}]}),
            );
            let done = codex.wait(|event| {
                event["method"] == "turn/completed" && event["params"]["threadId"] == *id
            });
            assert_eq!(done["params"]["turn"]["status"], "completed", "{done}");
        }
        drop(codex);
        server.shutdown().unwrap();
        let stats = upstream.traffic.lock().unwrap();
        assert_eq!(stats.inline_rejections, 1, "{stats:?}");
        assert_eq!(stats.generated_hashes.len(), 2);
        assert_eq!(stats.ws_connections, 1);
        assert_eq!(stats.http_requests, 0);
    }
}
