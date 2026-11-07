//! The actual image extension generates/edits and persists the returned PNG.
use super::super::installed_codex::Codex;
use super::*;
mod upstream;

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

#[test]
#[ignore = "requires an explicitly selected Codex CLI and isolated loopback access"]
fn installed_codex_image_tools_use_the_selected_subscription_for_generate_and_edit() {
    for edit in [false, true] {
        image_turn(edit, false);
    }
}

#[test]
#[ignore = "requires an explicitly selected Codex CLI and isolated loopback access"]
fn installed_codex_local_key_gateway_generates_and_edits_images() {
    for edit in [false, true] {
        image_turn(edit, true);
    }
}

fn image_turn(edit: bool, local_key: bool) {
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
    std::fs::write(home.join("config.toml"), &settings).unwrap();
    if local_key {
        std::fs::write(
            home.join("auth.json"),
            serde_json::to_vec(&json!({
                "auth_mode":"apikey", "OPENAI_API_KEY":server.state.backend.gateway_key,
                "emp_local_connection":true
            }))
            .unwrap(),
        )
        .unwrap();
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
                    Some(&server.state.backend.gateway_key)
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
    let upstream = upstream::spawn(listener, image_args, edit);
    let mut codex = Codex::start(home);
    codex.rpc(1,"initialize",json!({"clientInfo":{"name":"emp_image_fixture","version":"1"},"capabilities":{"experimentalApi":true}}));
    codex.send(json!({"method":"initialized"}));
    let started = codex.rpc(
        2,
        "thread/start",
        json!({"model":"selected/gpt-6.1-sol","modelProvider":if local_key { "emp" } else { "emp_fixture" },
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
