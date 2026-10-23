//! Real Codex changes a tool description on model switch, then resumes it.
use super::installed_codex::Codex;
use super::installed_tool_search::response_events;
use super::support::receive_native_request;
use super::*;

#[test]
#[ignore = "requires an explicitly selected Codex 0.164+ and isolated loopback access"]
fn installed_codex_switches_and_resumes_incremental_tool_revisions() {
    for protocol in ["chat_completions", "anthropic_messages", "responses"] {
        switch_and_resume(protocol);
        eprintln!("Incremental revision and resume accepted: {protocol}");
    }
}

fn initialize(home: &Path) -> Codex {
    let mut codex = Codex::start(home);
    codex.rpc(1, "initialize", json!({"clientInfo":{"name":"emp_revision_fixture","version":"1"},"capabilities":{"experimentalApi":true}}));
    codex.send(json!({"method":"initialized"}));
    codex
}

fn completed(codex: &Codex) {
    let event = codex.wait(|value| value["method"] == "turn/completed");
    assert_eq!(event["params"]["turn"]["status"], "completed", "{event}");
}

fn switch_and_resume(protocol: &'static str) {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().canonicalize().unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = thread::spawn(move || {
        for (index, revision) in ["old", "updated", "direct", "updated", "updated"]
            .into_iter()
            .enumerate()
        {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut socket = loop {
                assert!(Instant::now() < deadline, "revision request deadline");
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            let body = receive_native_request(&mut socket).body;
            let definitions = body["tools"].as_array().expect("real Codex tools");
            let tools = definitions
                .iter()
                .map(|tool| tool.get("function").unwrap_or(tool))
                .collect::<Vec<_>>();
            if revision == "direct" {
                assert!(
                    !tools.iter().any(|tool| tool["description"]
                        .as_str()
                        .is_some_and(|description| description.starts_with("functions.exec\n"))),
                    "removed Code Mode tool is still callable: {body}"
                );
            } else {
                assert_eq!(
                    tools
                        .iter()
                        .filter(|tool| tool["description"]
                            .as_str()
                            .is_some_and(|description| description
                                .contains(&format!("{revision} fixture execution"))))
                        .count(),
                    1,
                    "missing current exec schema: {body}"
                );
            }
            if revision == "updated" {
                assert!(!tools.iter().any(|tool| {
                    tool["description"]
                        .as_str()
                        .is_some_and(|description| description.contains("old fixture execution"))
                }));
                assert!(
                    body.to_string().contains("First turn"),
                    "history lost on switch/resume"
                );
            }
            let item = json!({"type":"message","id":format!("msg-{index}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":"Fixture finished"}]});
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
            for event in response_events(protocol, &format!("revision-{index}"), &item) {
                writeln!(socket, "data: {event}\n").unwrap();
            }
            if protocol == "chat_completions" {
                socket.write_all(b"data: [DONE]\n\n").unwrap();
            }
            socket.flush().unwrap();
        }
    });
    let config = json!({"providers":[{"id":"demo","base_url":format!("http://{address}/v1"),"protocol":protocol,"auth_mode":"api_key","api_key":"fixture-only"}],
    "models":[
        {"id":"demo/old","provider":"demo","upstream_id":"old","enabled":true},
            {"id":"demo/updated","provider":"demo","upstream_id":"updated","enabled":true},
            {"id":"demo/direct","provider":"demo","upstream_id":"direct","enabled":true}
    ]});
    let mut catalog = emp_codex::merged_catalog::build_catalog(
        &config,
        &json!({"models":[]}),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    for entry in catalog["models"].as_array_mut().unwrap() {
        let revision = if entry["slug"] == "demo/old" {
            "old"
        } else {
            "updated"
        };
        entry["use_responses_lite"] = json!(true);
        entry["tool_mode"] = if entry["slug"] == "demo/direct" {
            json!("direct")
        } else {
            json!("code_mode")
        };
        entry["model_messages"]["tools"] =
            json!({"code_mode":{"exec":{"description":format!("{revision} fixture execution")}}});
    }
    std::fs::write(
        home.join("models.json"),
        serde_json::to_vec(&catalog).unwrap(),
    )
    .unwrap();
    std::fs::write(
        home.join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let server = ServerHandle::start_with_config(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &home.join("config.json"),
    )
    .unwrap();
    std::fs::write(home.join("config.toml"), format!(
        "model = \"demo/old\"\nmodel_provider = \"emp_fixture\"\nmodel_catalog_json = {}\napproval_policy = \"never\"\nsandbox_mode = \"read-only\"\nweb_search = \"disabled\"\n[features]\nplugins = false\n[model_providers.emp_fixture]\nname = \"EMP fixture\"\nbase_url = \"http://{}/v1\"\nwire_api = \"responses\"\nenv_key = \"EMP_TEST_KEY\"\nrequires_openai_auth = false\nsupports_websockets = false\nrequest_max_retries = 0\nhttp_headers = {{ \"X-EMP-Session\" = {} }}\n",
        serde_json::to_string(&home.join("models.json").to_string_lossy()).unwrap(), server.local_addr(), serde_json::to_string(&server.session_token()).unwrap()
    )).unwrap();
    let mut codex = initialize(&home);
    let started = codex.rpc(
        2,
        "thread/start",
        json!({"cwd":home,"approvalPolicy":"never","sandbox":"read-only"}),
    );
    let id = started["thread"]["id"].as_str().unwrap();
    codex.rpc(
        3,
        "turn/start",
        json!({"threadId":id,"input":[{"type":"text","text":"First turn"}]}),
    );
    completed(&codex);
    for (index, model) in ["demo/updated", "demo/direct", "demo/updated"]
        .into_iter()
        .enumerate()
    {
        codex.rpc(
            4 + index as u64,
            "turn/start",
            json!({"threadId":id,"model":model,"input":[{"type":"text","text":"Changed model"}]}),
        );
        completed(&codex);
    }
    drop(codex);
    let mut codex = initialize(&home);
    let resumed = codex.rpc(2, "thread/resume", json!({"threadId":id}));
    assert_eq!(
        resumed["model"], "demo/updated",
        "resume restores the selected model"
    );
    codex.rpc(
        3,
        "turn/start",
        json!({"threadId":id,"input":[{"type":"text","text":"Resumed turn"}]}),
    );
    completed(&codex);
    drop(codex);
    let history = std::fs::read_to_string(resumed["thread"]["path"].as_str().unwrap()).unwrap();
    assert!(
        history.matches("\"additional_tools\"").count() >= 3,
        "Codex must generate incremental catalog updates"
    );
    assert!(
        history.contains("tools.removed_definition"),
        "Codex must generate a typed removal notice"
    );
    upstream.join().unwrap();
    server.shutdown().unwrap();
}
