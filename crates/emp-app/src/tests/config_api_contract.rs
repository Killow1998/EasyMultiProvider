use super::catalog_api_contract::{CatalogUpstream, catalog_server, parsed_body};
use super::*;

fn normalize_observed_times(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key == "observed_at" && value.as_str().is_some_and(|value| !value.is_empty()) {
                    *value = json!("<observed_at>");
                } else {
                    normalize_observed_times(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                normalize_observed_times(value);
            }
        }
        _ => {}
    }
}

#[test]
fn settings_save_matches_python_state_and_preserves_credentials_across_restart() {
    let upstream = CatalogUpstream::start(200);
    let (directory, server) = catalog_server(&upstream);
    let root = canonical_root(&directory);
    let initial = server
        .state
        .backend
        .configuration
        .config
        .lock()
        .expect("config")
        .clone();
    let cookie = session_cookie_header(&server);
    let mut valid = parsed_body(&request(&server, "/api/config", &[&cookie]));
    valid["native_model_context_windows"] = json!({"native":150000});
    valid["providers"][0]["name"] = json!("Edited provider");
    valid["models"][0]["input_modalities"] = json!(["text", "image"]);
    valid["models"][0]["context_window"] = json!(80000);
    valid["catalog_family_presentations"] =
        json!({"old":{"catalog_alias":"Daily","show_context":false}});
    valid["codex_runtime_sources"] = json!(["vscode"]);
    valid["native_catalog_path"] = json!("user-form-cannot-replace-managed-path");
    let mut excessive = valid.clone();
    excessive["native_model_context_windows"] = json!({"native":200001});
    let mut invalid_type = valid.clone();
    invalid_type["native_model_context_windows"] = json!({"native":true});
    let mut private_file = valid.clone();
    private_file["providers"][0]["api_key_file"] = json!("form-injected-secret.key");
    let mut restored = valid.clone();
    restored["native_model_context_windows"] = json!({});
    let cases = vec![valid, excessive, invalid_type, private_file, restored];
    let mut actual = Vec::new();
    for (index, incoming) in cases.iter().enumerate() {
        let before =
            std::fs::read(&server.state.backend.configuration.config_path).expect("before");
        let wire = post(
            &server,
            "/api/config",
            &serde_json::to_vec(incoming).expect("request JSON"),
            &[&cookie],
        );
        let status: u16 = wire
            .split_whitespace()
            .nth(1)
            .expect("status")
            .parse()
            .expect("status number");
        assert_eq!(
            status,
            if index == 0 || index == 4 { 200 } else { 400 },
            "{wire}"
        );
        if status == 400 {
            assert_eq!(
                std::fs::read(&server.state.backend.configuration.config_path).expect("after"),
                before
            );
        }
        assert!(!wire.contains("synthetic-test-key"));
        actual.push(json!({"status":status,"payload":parsed_body(&wire)}));
    }
    let saved = load_configuration(Some(&root.join("config.json"))).expect("saved config");
    assert_eq!(
        saved["codex_runtime_sources"],
        initial["codex_runtime_sources"]
    );
    assert_eq!(saved["native_catalog_path"], initial["native_catalog_path"]);
    assert_eq!(
        provider_api_key(
            &saved["providers"][0],
            &server.state.backend.configuration.vault
        ),
        "synthetic-test-key"
    );
    let key_path = server
        .state
        .backend
        .configuration
        .vault
        .ensure_master_key()
        .map(Path::to_path_buf);
    server.shutdown().expect("shutdown");
    let restarted = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &root.join("config.json"),
        "codex",
        root.join("codex/auth.json"),
    )
    .expect("restart");
    assert_eq!(
        parsed_body(&request(&restarted, "/api/config", &[&cookie])),
        actual[4]["payload"]
    );
    restarted.shutdown().expect("shutdown restarted");

    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let script = r#"
import json, os, sys, threading
from pathlib import Path
from types import SimpleNamespace
from easy_multi_provider import server
import easy_multi_provider
# The archived Python oracle keeps its release identity; the contract compares
# state handling, so pin its version to the Rust build under test.
easy_multi_provider.__version__ = "0.12.1"
import easy_multi_provider.management_views as management_views
management_views.__version__ = "0.12.1"
import easy_multi_provider.server as emp_server
emp_server.__version__ = "0.12.1"
fixture=json.load(sys.stdin)
state=server.AppState.__new__(server.AppState)
state.config=fixture['initial']
state.path=Path(fixture['path'])
state.codex_home=Path(os.environ['CODEX_HOME'])
state.lock=threading.RLock()
state.runtime_controller=object()
state._native_quota=None
state._catalog_cache=None
state._catalog_cache_revision=None
state.integration_status=lambda:SimpleNamespace(state='inactive')
results=[]
for incoming in fixture['cases']:
    handler=object.__new__(server.make_handler(state))
    handler.path='/api/config'
    handler._management_allowed=lambda:True
    handler._record_http_request_start_once=lambda:None
    handler._record_management_event=lambda *args,**kwargs:None
    handler._record_unexpected_exception=lambda exc: (_ for _ in ()).throw(exc)
    handler._body=lambda limit: incoming
    captured={}
    handler._send=lambda status,body,*args,**kwargs:captured.update(status=status,payload=json.loads(body))
    handler._do_POST()
    results.append(captured)
json.dump(results,sys.stdout,ensure_ascii=False)
"#;
    let mut command = Command::new(python);
    command
        .args(["-c", script])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .env("CODEX_HOME", root.join("codex"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(path) = key_path {
        command.env(emp_state::MASTER_KEY_FILE_ENV, path);
    }
    let mut child = command.spawn().expect("Python settings oracle");
    serde_json::to_writer(
        child.stdin.take().expect("stdin"),
        &json!({"initial":initial,"cases":cases,"path":root.join("config.json")}),
    )
    .expect("fixture");
    let output = child.wait_with_output().expect("oracle output");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut expected: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");
    let mut actual = json!(actual);
    normalize_observed_times(&mut expected);
    normalize_observed_times(&mut actual);
    assert_eq!(actual, expected);
}

#[test]
fn startup_and_save_move_duplicate_visibility_to_native_without_touching_auth() {
    let upstream = CatalogUpstream::start(200);
    let (directory, server) = catalog_server(&upstream);
    let root = canonical_root(&directory);
    let path = root.join("config.json");
    let native_path = root.join("codex/auth.json");
    let native_bytes =
        br#"{"tokens":{"access_token":"shared-fixture-token","account_id":"native-owner"}}"#;
    std::fs::write(&native_path, native_bytes).expect("native fixture");
    let mut config = server
        .state
        .backend
        .configuration
        .config
        .lock()
        .expect("config")
        .clone();
    let auth_path =
        emp_state::account_auth_path(&config, "duplicate", &path).expect("account path");
    server.state.backend.configuration.vault.write_encrypted_json(&auth_path,&json!({"tokens":{"access_token":"shared-fixture-token","account_id":"different-owner"}})).expect("encrypted auth");
    config["accounts"] = json!([{"id":"duplicate","prefix":"duplicate","auth_file":auth_path,"hidden_models":["native"]}]);
    save_configuration(
        &config,
        Some(&path),
        &server.state.backend.configuration.vault,
    )
    .expect("fixture config");
    server.shutdown().expect("shutdown fixture server");
    let restarted = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &path,
        "missing-test-codex",
        native_path.clone(),
    )
    .expect("restart");
    let cookie = session_cookie_header(&restarted);
    let mut public = parsed_body(&request(&restarted, "/api/config", &[&cookie]));
    assert_eq!(public["native_hidden_models"], json!(["native"]));
    assert_eq!(public["accounts"][0]["hidden_models"], json!([]));
    assert_eq!(public["accounts"][0]["duplicate"], true);
    assert_eq!(public["accounts"][0]["duplicate_of"], "当前 Codex 登录");
    let accounts = parsed_body(&request(&restarted, "/api/accounts", &[&cookie]));
    assert_eq!(accounts["accounts"][0]["duplicate"], true);
    let models = parsed_body(&request(&restarted, "/v1/models", &[]));
    assert_eq!(
        models["data"]
            .as_array()
            .expect("models")
            .iter()
            .map(|model| model["id"].clone())
            .collect::<Vec<_>>(),
        vec![json!("demo/old")]
    );
    public["accounts"][0]["hidden_models"] = json!(["native", "missing"]);
    let response = post(
        &restarted,
        "/api/config",
        &serde_json::to_vec(&public).expect("body"),
        &[&cookie],
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(
        parsed_body(&response)["native_hidden_models"],
        json!(["missing", "native"])
    );
    assert_eq!(
        parsed_body(&response)["accounts"][0]["hidden_models"],
        json!([])
    );
    assert_eq!(
        std::fs::read(&native_path).expect("native unchanged"),
        native_bytes
    );
    let saved = std::fs::read(&path).expect("persisted");
    restarted.shutdown().expect("shutdown");
    let second = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &path,
        "missing-test-codex",
        native_path,
    )
    .expect("second restart");
    assert_eq!(std::fs::read(&path).expect("config unchanged"), saved);
    second.shutdown().expect("shutdown second restart");
}

#[cfg(unix)]
#[test]
fn failed_config_commit_restores_provider_secret_and_keeps_memory_snapshot() {
    use std::os::unix::fs::symlink;
    let upstream = CatalogUpstream::start(200);
    let (directory, server) = catalog_server(&upstream);
    let root = canonical_root(&directory);
    let before = server
        .state
        .backend
        .configuration
        .config
        .lock()
        .expect("config")
        .clone();
    let key_path = Path::new(
        before["providers"][0]["api_key_file"]
            .as_str()
            .expect("encrypted key path"),
    );
    let encrypted = std::fs::read(key_path).expect("old encrypted key");
    let cookie = session_cookie_header(&server);
    let mut incoming = parsed_body(&request(&server, "/api/config", &[&cookie]));
    incoming["providers"][0]["api_key"] = json!("replacement-fixture-key");
    let protected = root.join("protected.txt");
    std::fs::write(&protected, b"protected fixture").expect("protected file");
    std::fs::rename(root.join("config.json"), root.join("config.original.json"))
        .expect("retain original");
    symlink(&protected, root.join("config.json")).expect("unsafe destination");
    let result = post(
        &server,
        "/api/config",
        &serde_json::to_vec(&incoming).expect("JSON"),
        &[&cookie],
    );
    assert!(result.starts_with("HTTP/1.1 500"), "{result}");
    assert!(!result.contains("replacement-fixture-key"));
    assert_eq!(
        *server
            .state
            .backend
            .configuration
            .config
            .lock()
            .expect("config"),
        before
    );
    assert_eq!(
        std::fs::read(key_path).expect("restored encrypted key"),
        encrypted
    );
    assert_eq!(
        std::fs::read(&protected).expect("protected"),
        b"protected fixture"
    );
    server.shutdown().expect("shutdown");
}
