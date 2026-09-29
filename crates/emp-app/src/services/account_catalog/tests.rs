use super::{client_version_or_minimum, refresh};
use crate::lifecycle::ServerHandle;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

mod automatic;

struct CatalogFixture {
    address: String,
    requests: Receiver<String>,
    release: Option<Sender<()>>,
    worker: JoinHandle<()>,
}

fn fake_catalog(hold_response: bool) -> CatalogFixture {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("catalog listener");
    let address = listener.local_addr().expect("catalog address");
    let (request_sender, requests) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("catalog accept");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("catalog read timeout");
        let mut request = String::new();
        {
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read request line");
                assert!(!line.is_empty(), "request ended before header terminator");
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
        }
        request_sender.send(request).expect("send observed request");
        if hold_response {
            release_receiver.recv().expect("release catalog response");
        }
        let body = br#"{"models":[{"slug":"fixture-model","display_name":"Fixture model"}]}"#;
        write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("catalog response head");
        stream.write_all(body).expect("catalog response body");
    });
    CatalogFixture {
        address: format!("http://{address}/v1"),
        requests,
        release: hold_response.then_some(release_sender),
        worker,
    }
}

fn start_server(
    directory: &tempfile::TempDir,
    base_url: &str,
    runtime_version: &str,
) -> (ServerHandle, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let root = directory
        .path()
        .canonicalize()
        .expect("canonical test root");
    let config_path = root.join("config.json");
    let native_auth_path = root.join("codex/auth.json");
    std::fs::create_dir_all(native_auth_path.parent().unwrap()).expect("native auth directory");
    let generated_catalog = emp_state::generated_catalog_path(native_auth_path.parent());
    assert!(
        !generated_catalog.exists(),
        "isolated server starts without a generated catalog"
    );
    let mut config = json!({
        "codex_base_url":base_url,
        "providers":[],
        "models":[],
        "accounts":[{
            "id":"demo","name":"Demo","prefix":"demo","enabled":true,
            "hidden_models":[]
        }]
    });
    let auth_path = emp_state::account_auth_path(&config, "demo", &config_path)
        .expect("managed account auth path");
    config["accounts"][0]["auth_file"] = auth_path.to_string_lossy().into_owned().into();
    std::fs::create_dir_all(auth_path.parent().unwrap()).expect("account directory");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).expect("config file");

    let executable = root.join("fake-codex");
    std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'codex-cli {runtime_version}'; else exit 99; fi\n"
            ),
        )
        .expect("fake runtime");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("fake runtime permissions");
    let server = ServerHandle::start_with_catalog_refresh_for_test(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        executable.to_str().expect("UTF-8 runtime path"),
        native_auth_path,
    )
    .expect("start account catalog server");
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(5))
    );
    assert!(
        generated_catalog.is_file(),
        "startup refresh publishes a missing merged catalog"
    );
    server
        .state
        .backend
        .configuration
        .vault
        .write_encrypted_json(
            &auth_path,
            &json!({"tokens":{"access_token":"selected-secret","account_id":"selected-owner"}}),
        )
        .expect("write account credentials");
    (server, auth_path)
}

#[test]
fn model_refresh_uses_each_selected_runtime_version_for_query_and_user_agent() {
    for version in ["0.158.6", "0.159.2"] {
        let directory = tempfile::Builder::new()
            .prefix("emp-account-catalog-")
            .tempdir()
            .expect("temporary directory");
        let catalog = fake_catalog(false);
        let (server, auth_path) = start_server(&directory, &catalog.address, version);
        let inventory_snapshot = server.state.backend.integration.inventory.snapshot(false);
        let observed_version = server
            .state
            .backend
            .integration
            .inventory
            .selected_trusted_version();
        assert_eq!(inventory_snapshot["helper_source"], "configured");
        assert_eq!(observed_version.as_deref(), Some(version));

        let result = refresh(&server.state, "demo").expect("refresh subscription catalog");
        assert!(result["models"].is_array());
        let request = catalog.requests.recv().expect("catalog request");
        assert!(
            request.starts_with(&format!(
                "GET /v1/models?client_version={version} HTTP/1.1\r\n"
            )),
            "unexpected request: {request}"
        );
        let lowered = request.to_ascii_lowercase();
        assert!(lowered.contains(&format!("user-agent: codex_cli_rs/{version}\r\n")));
        assert!(lowered.contains("authorization: bearer selected-secret\r\n"));

        let owner = emp_codex::native_catalog_owner(&BTreeMap::from([
            (
                "Authorization".to_owned(),
                "Bearer selected-secret".to_owned(),
            ),
            ("ChatGPT-Account-Id".to_owned(), "selected-owner".to_owned()),
        ]));
        let persisted: serde_json::Value = serde_json::from_slice(
            &std::fs::read(auth_path.parent().unwrap().join("models_cache.json"))
                .expect("persisted subscription catalog"),
        )
        .expect("catalog JSON");
        assert_eq!(persisted["account_owner"], owner);
        assert_eq!(persisted["base_url"], catalog.address);
        catalog.worker.join().expect("catalog worker");
        server.shutdown().expect("shutdown account server");
    }
}

#[test]
fn model_refresh_rechecks_account_ownership_after_the_upstream_request() {
    let directory = tempfile::Builder::new()
        .prefix("emp-account-catalog-owner-")
        .tempdir()
        .expect("temporary directory");
    let catalog = fake_catalog(true);
    let (server, auth_path) = start_server(&directory, &catalog.address, "0.159.2");
    let inventory_snapshot = server.state.backend.integration.inventory.snapshot(false);
    assert_eq!(inventory_snapshot["helper_source"], "configured");
    assert_eq!(
        server
            .state
            .backend
            .integration
            .inventory
            .selected_trusted_version()
            .as_deref(),
        Some("0.159.2")
    );
    let state = std::sync::Arc::clone(&server.state);
    let refresh_worker = thread::spawn(move || refresh(&state, "demo"));
    let request = catalog
        .requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("catalog request");
    assert!(request.contains("client_version=0.159.2"));

    server
        .state
        .backend
        .configuration
        .vault
        .write_encrypted_json(
            &auth_path,
            &json!({"tokens":{"access_token":"rotated-secret","account_id":"selected-owner"}}),
        )
        .expect("rotate account credentials");
    catalog.release.as_ref().unwrap().send(()).unwrap();

    let response = refresh_worker.join().expect("join refresh worker");
    let response = String::from_utf8(response.expect_err("owner change must reject stale refresh"))
        .expect("error response JSON");
    assert!(response.contains("Subscription changed during model refresh; retry"));
    assert!(
        !auth_path
            .parent()
            .unwrap()
            .join("models_cache.json")
            .exists()
    );
    catalog.worker.join().expect("catalog worker");
    server.shutdown().expect("shutdown account server");
}

#[test]
fn minimum_version_is_used_only_when_inventory_has_no_observation() {
    assert_eq!(
        client_version_or_minimum(None),
        emp_codex::runtime_inventory::minimum_codex()
    );
    assert_eq!(
        client_version_or_minimum(Some("0.159.2".to_owned())),
        "0.159.2"
    );
}
