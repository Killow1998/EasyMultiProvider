//! The actual image extension generates/edits and persists the returned PNG.
use super::super::installed_codex::Codex;
use super::*;

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

#[test]
#[ignore = "requires an explicitly selected Codex CLI and isolated loopback access"]
fn installed_codex_image_tools_use_the_selected_subscription_for_generate_and_edit() {
    for edit in [false, true] {
        image_turn(edit);
    }
}

fn image_turn(edit: bool) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (_directory, server) = image_server(&format!("http://{address}/backend-api/codex"));
    let home = server
        .state
        .backend
        .accounts
        .native_auth_path
        .parent()
        .unwrap();
    let input_path = home.join("reference.png");
    std::fs::write(&input_path, STANDARD.decode(PNG).unwrap()).unwrap();
    let claims = json!({"email":"image-fixture@example.test","exp":2100000000,
        "https://api.openai.com/auth":{"chatgpt_plan_type":"plus","chatgpt_user_id":"fixture-user","chatgpt_account_id":"native-owner"}});
    let id_token = format!(
        "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.{}.c2ln",
        URL_SAFE
            .encode(serde_json::to_vec(&claims).unwrap())
            .trim_end_matches('=')
    );
    std::fs::write(home.join("auth.json"),serde_json::to_vec(&json!({"auth_mode":"chatgpt",
        "tokens":{"access_token":"native-fixture","refresh_token":"","id_token":id_token,"account_id":"native-owner"},
        "last_refresh":"2026-10-16T00:00:00Z"})).unwrap()).unwrap();
    let config = server.state.backend.configuration.snapshot().unwrap();
    let seed = emp_codex::merged_catalog::build_catalog(
        &json!({"providers":[{"id":"fixture","protocol":"responses","auth_mode":"api_key","api_key":"fake","base_url":"http://127.0.0.1:1/v1"}],
            "models":[{"id":"fixture/model","provider":"fixture","upstream_id":"upstream","enabled":true}]}),
        &json!({"models":[]}),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let mut model = seed["models"][0].clone();
    model["slug"] = json!("gpt-6.1-sol");
    model["input_modalities"] = json!(["text", "image"]);
    let catalog = emp_codex::merged_catalog::build_catalog(
        &config,
        &json!({"models":[model]}),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let catalog_path = home.join("client-models.json");
    std::fs::write(&catalog_path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let settings = format!(
        "model = \"selected/gpt-6.1-sol\"\nmodel_provider = \"emp_fixture\"\nmodel_catalog_json = {}\napproval_policy = \"never\"\nsandbox_mode = \"read-only\"\nweb_search = \"disabled\"\n[features]\nplugins = false\nresponses_websockets = false\nresponses_websockets_v2 = false\ncode_mode_only = false\n[model_providers.emp_fixture]\nname = \"OpenAI\"\nbase_url = \"http://{}/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\nsupports_websockets = false\nhttp_headers = {{ \"X-EMP-Session\" = {} }}\n",
        serde_json::to_string(&catalog_path.to_string_lossy()).unwrap(),
        server.local_addr(),
        serde_json::to_string(&server.session_token()).unwrap()
    );
    std::fs::write(home.join("config.toml"), settings).unwrap();
    let image_args = if edit {
        json!({"prompt":"synthetic image edit","referenced_image_paths":[input_path]})
    } else {
        json!({"prompt":"synthetic image generation"})
    };
    let upstream = thread::spawn(move || {
        let mut image_turn = String::new();
        for stage in 0..3 {
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
            let request = receive_native_request(&mut socket);
            assert_eq!(request.headers["authorization"], "Bearer selected-fixture");
            assert_eq!(request.headers["chatgpt-account-id"], "selected-owner");
            if stage == 1 {
                assert_eq!(
                    request.path,
                    format!(
                        "/backend-api/codex/images/{}",
                        if edit { "edits" } else { "generations" }
                    )
                );
                assert!(
                    request.body["model"]
                        .as_str()
                        .unwrap()
                        .starts_with("gpt-image-")
                );
                assert_eq!(request.headers["x-codex-image-turn-id"], image_turn);
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
                assert!(
                    request.body.to_string().contains("imagegen"),
                    "image extension missing"
                );
            } else {
                let serialized = request.body.to_string();
                assert!(serialized.contains("image-fixture-call"));
                assert!(
                    serialized.contains(PNG),
                    "generated image missing from next inference"
                );
            }
            let item = if stage == 0 {
                json!({"id":"image-fixture-call","type":"function_call","namespace":"image_gen",
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
                json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[item],"end_turn":stage==2}}),
            ];
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            for event in events {
                writeln!(socket, "data: {event}\n").unwrap();
            }
        }
    });
    let mut codex = Codex::start(home);
    codex.rpc(1,"initialize",json!({"clientInfo":{"name":"emp_image_fixture","version":"1"},"capabilities":{"experimentalApi":true}}));
    codex.send(json!({"method":"initialized"}));
    let started = codex.rpc(
        2,
        "thread/start",
        json!({"model":"selected/gpt-6.1-sol","modelProvider":"emp_fixture",
        "cwd":home,"approvalPolicy":"never","sandbox":"read-only","ephemeral":true}),
    );
    codex.rpc(3,"turn/start",json!({"threadId":started["thread"]["id"],"input":[{"type":"text","text":"Generate the synthetic image fixture"}]}));
    let completed = codex.wait(|value| {
        value["method"] == "item/completed" && value["params"]["item"]["type"] == "imageGeneration"
    });
    let item = &completed["params"]["item"];
    assert_eq!(item["status"], "completed", "{item}");
    assert_eq!(item["result"], PNG);
    let path = item["savedPath"].as_str().expect("image saved path");
    assert_eq!(std::fs::read(path).unwrap(), STANDARD.decode(PNG).unwrap());
    let completed = codex.wait(|value| value["method"] == "turn/completed");
    assert_eq!(
        completed["params"]["turn"]["status"], "completed",
        "{completed}"
    );
    drop(codex);
    upstream.join().unwrap();
    server.shutdown().unwrap();
}
