//! Actual Codex image generation/editing through native and conditional MCP tools.
use super::super::installed_codex::Codex;
use super::*;
mod upstream;

#[derive(Clone, Copy)]
enum SignIn {
    Paid,
    Free,
    LocalKey,
}

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

#[test]
#[ignore = "requires an explicitly selected Codex CLI and isolated loopback access"]
fn installed_codex_image_tools_use_the_selected_subscription_for_generate_and_edit() {
    for edit in [false, true] {
        image_turn(edit, SignIn::Paid);
    }
}

#[test]
#[ignore = "requires an explicitly selected Codex CLI and isolated loopback access"]
fn installed_codex_free_and_local_key_mcp_generate_and_edit_images() {
    for sign_in in [SignIn::Free, SignIn::LocalKey] {
        for edit in [false, true] {
            image_turn(edit, sign_in);
        }
    }
}

fn image_turn(edit: bool, sign_in: SignIn) {
    let mcp = !matches!(sign_in, SignIn::Paid);
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
        "https://api.openai.com/auth":{"chatgpt_plan_type":if matches!(sign_in,SignIn::Free) {"free"} else {"plus"},"chatgpt_user_id":"fixture-user","chatgpt_account_id":"native-owner"}});
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
        "model = \"selected/gpt-6.1-sol\"\nmodel_provider = \"emp_fixture\"\nmodel_catalog_json = {}\napproval_policy = \"never\"\nsandbox_mode = \"read-only\"\nweb_search = \"disabled\"\n[features]\nplugins = false\nresponses_websockets = false\nresponses_websockets_v2 = false\ncode_mode_only = false\ntool_call_mcp_elicitation = false\n[model_providers.emp_fixture]\nname = \"OpenAI\"\nbase_url = \"http://{}/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\nsupports_websockets = false\nhttp_headers = {{ \"X-EMP-Session\" = {} }}\n",
        serde_json::to_string(&catalog_path.to_string_lossy()).unwrap(),
        server.local_addr(),
        serde_json::to_string(&server.session_token()).unwrap()
    );
    std::fs::write(home.join("config.toml"), &settings).unwrap();
    if matches!(sign_in, SignIn::LocalKey) {
        std::fs::write(
            home.join("auth.json"),
            serde_json::to_vec(&json!({
                "auth_mode":"apikey", "OPENAI_API_KEY":server.state.backend.gateway_key,
                "emp_local_connection":true
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let auth: Value =
        serde_json::from_slice(&std::fs::read(home.join("auth.json")).unwrap()).unwrap();
    assert_eq!(
        crate::services::integration::enable::image_mcp_required(&auth),
        mcp
    );
    if mcp {
        std::fs::write(
            home.join("config.toml"),
            settings.replace("model_provider = \"emp_fixture\"\n", ""),
        )
        .unwrap();
        assert!(
            server
                .state
                .backend
                .integration
                .manager
                .enable_with_options(
                    &server.state.base_url,
                    Some(catalog_path.to_str().unwrap()),
                    true,
                    None,
                    Some(&emp_integration::ImageMcpConfig {
                        command: std::env::var("EMP_TEST_EMP_BIN")
                            .expect("select the built EMP binary"),
                        key_file: server
                            .state
                            .backend
                            .configuration
                            .config_path
                            .parent()
                            .unwrap()
                            .join("state/gateway-key")
                            .to_string_lossy()
                            .into_owned(),
                    })
                )
                .unwrap()
                .ok()
        );
    }
    let image_args = if edit {
        json!({"prompt":"synthetic image edit","referenced_image_paths":[input_path]})
    } else {
        json!({"prompt":"synthetic image generation"})
    };
    let upstream = upstream::spawn(listener, image_args, edit, mcp);
    let mut codex = Codex::start(home);
    codex.rpc(1,"initialize",json!({"clientInfo":{"name":"emp_image_fixture","version":"1"},"capabilities":{"experimentalApi":true}}));
    codex.send(json!({"method":"initialized"}));
    let started = codex.rpc(
        2,
        "thread/start",
        json!({"model":"selected/gpt-6.1-sol","modelProvider":if mcp { "openai" } else { "emp_fixture" },
        "cwd":home,"approvalPolicy":if mcp {"on-request"} else {"never"},"sandbox":"read-only","ephemeral":true}),
    );
    codex.rpc(3,"turn/start",json!({"threadId":started["thread"]["id"],"input":[{"type":"text","text":"Generate the synthetic image fixture"}]}));
    if mcp {
        let approval = codex.wait(|value| value["method"] == "item/tool/requestUserInput");
        let question = &approval["params"]["questions"][0];
        assert!(
            question["id"]
                .as_str()
                .unwrap()
                .starts_with("mcp_tool_call_approval_")
        );
        let answers = serde_json::Map::from_iter([(
            question["id"].as_str().unwrap().to_owned(),
            json!({"answers":[question["options"][0]["label"]]}),
        )]);
        codex.send(json!({"id":approval["id"],"result":{"answers":answers}}));
    }
    let completed = codex.wait(|value| {
        value["method"] == "item/completed"
            && value["params"]["item"]["type"]
                == if mcp {
                    "mcpToolCall"
                } else {
                    "imageGeneration"
                }
    });
    let item = &completed["params"]["item"];
    let path = if mcp {
        assert_eq!(item["status"], "completed", "{item}");
        let result: Value = serde_json::from_str(
            item["result"]["content"][0]["text"]
                .as_str()
                .expect("short MCP result"),
        )
        .unwrap();
        assert!(!result.to_string().contains(PNG));
        result["path"].as_str().unwrap().to_owned()
    } else {
        assert_eq!(item["status"], "completed", "{item}");
        assert_eq!(item["result"], PNG);
        item["savedPath"]
            .as_str()
            .expect("image saved path")
            .to_owned()
    };
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
