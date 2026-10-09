//! Real Codex discovers and invokes deferred tools through isolated EMP routes.
use super::installed_codex::Codex;
use super::support::receive_native_request;
use super::*;

#[test]
#[ignore = "requires installed Codex 0.162 CLI and isolated loopback access; run explicitly"]
fn installed_codex_discovers_and_calls_same_named_tools_across_protocols() {
    for (protocol, native) in [
        ("chat_completions", false),
        ("anthropic_messages", false),
        ("responses", false),
        ("responses", true),
    ] {
        tool_search(protocol, native);
        eprintln!("Tool Search accepted: {protocol}, native={native}");
    }
}

fn tool_search(protocol: &'static str, native: bool) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let home = root.join("codex");
    std::fs::create_dir(&home).unwrap();
    for namespace in ["alpha", "beta"] {
        std::fs::write(
            home.join(format!("{namespace}.txt")),
            format!("{namespace} fixture result"),
        )
        .unwrap();
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = thread::spawn(move || {
        let mut calls = Vec::new();
        for stage in 0..5 {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut socket = loop {
                assert!(Instant::now() < deadline, "Tool Search upstream deadline");
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
            let serialized = body.to_string();
            if stage == 0 && native {
                assert!(
                    serialized.contains("additional_tools"),
                    "real incremental catalog missing"
                );
            } else if stage > 0 {
                assert!(
                    serialized.contains("search-alpha"),
                    "search history lost at {stage}"
                );
            }
            if stage >= 2 {
                assert!(
                    serialized.contains("alpha fixture result"),
                    "tool result lost"
                );
            }
            if stage == 4 {
                assert!(
                    serialized.contains("beta fixture result"),
                    "second tool result lost"
                );
            }
            let (name, namespace, search) = if stage == 4 {
                (String::new(), None, false)
            } else {
                let search = stage % 2 == 0;
                let namespace = if stage < 2 { "alpha" } else { "beta" };
                if native {
                    if search {
                        ("tool_search".into(), None, true)
                    } else {
                        ("read".into(), Some(namespace), false)
                    }
                } else {
                    let tools = body["tools"].as_array().unwrap();
                    let definition = tools
                        .iter()
                        .map(|tool| tool.get("function").unwrap_or(tool))
                        .find(|tool| {
                            if search {
                                tool["description"]
                                    .as_str()
                                    .unwrap_or("")
                                    .contains("Tool discovery")
                            } else {
                                tool["description"]
                                    .as_str()
                                    .unwrap_or("")
                                    .contains(&format!("{namespace}.read"))
                            }
                        })
                        .expect("discovered tool definition reaches upstream");
                    (
                        definition["name"].as_str().unwrap().to_owned(),
                        None,
                        search,
                    )
                }
            };
            if !search && stage != 4 {
                calls.push(name.clone());
            }
            let args = if search {
                json!({"query": if stage == 0 {"Read alpha fixture"} else {"Read beta fixture"}, "limit":1})
            } else {
                json!({"fixture":true})
            };
            let id = [
                "search-alpha",
                "read-alpha",
                "search-beta",
                "read-beta",
                "done",
            ][stage];
            let item = if stage == 4 {
                json!({"id":id,"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Both fixtures completed","annotations":[]}]})
            } else if native && search {
                json!({"id":id,"type":"tool_search_call","call_id":id,"execution":"client","arguments":args})
            } else {
                let mut item = json!({"id":id,"type":"function_call","call_id":id,"name":name,"arguments":args.to_string()});
                if let Some(namespace) = namespace {
                    item["namespace"] = json!(namespace);
                }
                item
            };
            let events = response_events(protocol, id, &item);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
            for event in events {
                writeln!(socket, "data: {event}\n").unwrap();
            }
            if protocol == "chat_completions" {
                socket.write_all(b"data: [DONE]\n\n").unwrap();
            }
            socket.flush().unwrap();
        }
        if !native {
            assert_ne!(calls[0], calls[1], "namespace aliases must remain distinct");
        }
    });
    let config = json!({"providers":[{"id":"demo","base_url":format!("http://{address}/v1"),"protocol":protocol,
        "auth_mode":if native {"forward"} else {"api_key"},"api_key":"fixture-only"}],
        "models":[{"id":"demo/model","provider":"demo","upstream_id":"upstream","enabled":true}]});
    let mut catalog = emp_codex::merged_catalog::build_catalog(
        &config,
        &json!({"models":[]}),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    catalog["models"][0]["use_responses_lite"] = json!(true);
    std::fs::write(
        home.join("models.json"),
        serde_json::to_vec(&catalog).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let server = ServerHandle::start_with_config(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &root.join("config.json"),
    )
    .unwrap();
    let settings = format!(
        "model = \"demo/model\"\nmodel_provider = \"emp_fixture\"\nmodel_catalog_json = {}\napproval_policy = \"never\"\nsandbox_mode = \"read-only\"\nweb_search = \"disabled\"\n[features]\nplugins = false\nincremental_tools = true\n[model_providers.emp_fixture]\nname = \"EMP fixture\"\nbase_url = \"http://{}/v1\"\nwire_api = \"responses\"\nenv_key = \"EMP_TEST_KEY\"\nrequires_openai_auth = false\nsupports_websockets = false\nrequest_max_retries = 0\nhttp_headers = {{ \"X-EMP-Session\" = {} }}\n",
        serde_json::to_string(&home.join("models.json").to_string_lossy()).unwrap(),
        server.local_addr(),
        serde_json::to_string(&server.session_token()).unwrap()
    );
    std::fs::write(home.join("config.toml"), settings).unwrap();
    let mut codex = Codex::start(&home);
    codex.rpc(1, "initialize", json!({"clientInfo":{"name":"emp_tool_search_fixture","version":"1"},"capabilities":{"experimentalApi":true}}));
    codex.send(json!({"method":"initialized"}));
    let tools = ["alpha", "beta"].map(|namespace| json!({"type":"namespace","name":namespace,"description":namespace,
        "tools":[{"type":"function","name":"read","description":format!("Read {namespace} fixture"),"deferLoading":true,
        "inputSchema":{"type":"object","properties":{"fixture":{"type":"boolean"}},"required":["fixture"],"additionalProperties":false}}]}));
    let started = codex.rpc(2, "thread/start", json!({"model":"demo/model","modelProvider":"emp_fixture","cwd":home,"approvalPolicy":"never","sandbox":"read-only","ephemeral":true,"dynamicTools":tools}));
    let id = started["thread"]["id"].as_str().unwrap();
    codex.rpc(
        3,
        "turn/start",
        json!({"threadId":id,"input":[{"type":"text","text":"Read both fixtures."}]}),
    );
    for namespace in ["alpha", "beta"] {
        let call = codex.wait(|value| {
            value["method"] == "item/tool/call" || value["method"] == "turn/completed"
        });
        assert_eq!(call["method"], "item/tool/call", "{call}");
        assert_eq!(call["params"]["namespace"], namespace);
        assert_eq!(call["params"]["tool"], "read");
        assert_eq!(call["params"]["arguments"], json!({"fixture":true}));
        codex.send(json!({"id":call["id"],"result":{"success":true,"contentItems":[{"type":"inputText","text":std::fs::read_to_string(home.join(format!("{namespace}.txt"))).unwrap()}]}}));
    }
    let completed = codex.wait(|value| value["method"] == "turn/completed");
    assert_eq!(
        completed["params"]["turn"]["status"], "completed",
        "{completed}"
    );
    drop(codex);
    upstream.join().unwrap();
    server.shutdown().unwrap();
    let journal = crate::tests::internal_events_contract::journal(&root);
    let last = journal
        .iter()
        .rev()
        .find(|event| event["event"] == "route_observation")
        .unwrap();
    for kind in ["additional_tools", "tool_search_call", "tool_search_output"] {
        assert!(
            last["fields"]["request_item_types"]
                .as_array()
                .unwrap()
                .contains(&json!(kind)),
            "real Codex item missing: {kind}"
        );
    }
    assert_eq!(last["fields"]["tool_pairing_status"], "paired");
}

fn response_events(protocol: &str, id: &str, item: &Value) -> Vec<Value> {
    let final_message = item["type"] == "message";
    let name = &item["name"];
    let args = item["arguments"]
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or(Value::Null);
    match protocol {
        "chat_completions" => vec![
            json!({"id":id,"model":"upstream","choices":[{"index":0,"delta":if final_message {
            json!({"content":"Both fixtures completed"})
        } else {json!({"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]})},
            "finish_reason":if final_message {"stop"} else {"tool_calls"}}]}),
        ],
        "anthropic_messages" => vec![
            json!({"type":"message_start","message":{"id":id,"model":"upstream","usage":{"input_tokens":10}}}),
            json!({"type":"content_block_start","index":0,"content_block":if final_message {
                json!({"type":"text","text":"Both fixtures completed"})
            } else {json!({"type":"tool_use","id":id,"name":name,"input":args})}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":if final_message {"end_turn"} else {"tool_use"}},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ],
        _ => {
            vec![
                json!({"type":"response.created","response":{"id":format!("resp-{id}"),"status":"in_progress","output":[]}}),
                json!({"type":"response.output_item.added","output_index":0,"item":item}),
                json!({"type":"response.output_item.done","output_index":0,"item":item}),
                json!({"type":"response.completed","response":{"id":format!("resp-{id}"),"status":"completed","output":[item],"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12},"end_turn":final_message}}),
            ]
        }
    }
}
