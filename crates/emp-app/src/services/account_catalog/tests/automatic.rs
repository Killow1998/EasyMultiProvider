use crate::lifecycle::ServerHandle;
use base64::Engine as _;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use super::start_server;

mod publish_retry;

#[derive(Clone)]
struct RefreshReply {
    status: u16,
}

struct RefreshCatalogFixture {
    address: String,
    requests: Receiver<String>,
    reply: Arc<Mutex<RefreshReply>>,
    release_first: Option<Sender<()>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl RefreshCatalogFixture {
    fn start(hold_first: bool) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("refresh listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking refresh listener");
        let address = listener.local_addr().expect("refresh address");
        let (request_sender, requests) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let reply = Arc::new(Mutex::new(RefreshReply { status: 200 }));
        let worker_reply = Arc::clone(&reply);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut request_number = 0;
            while !worker_stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(accepted) => accepted,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("refresh accept: {error}"),
                };
                stream
                    .set_nonblocking(false)
                    .expect("blocking refresh stream");
                request_number += 1;
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .expect("refresh request timeout");
                let mut request = String::new();
                let mut input = None;
                {
                    let mut reader = BufReader::new(stream.try_clone().expect("clone refresh"));
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).expect("read refresh request");
                        assert!(!line.is_empty(), "refresh request ended before headers");
                        request.push_str(&line);
                        if line == "\r\n" {
                            break;
                        }
                    }
                    if request.starts_with("POST ") {
                        let header = |name: &str| {
                            request.lines().find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case(name).then(|| value.trim())
                            })
                        };
                        let length: usize = header("content-length").unwrap().parse().unwrap();
                        let mut body = vec![0; length];
                        reader.read_exact(&mut body).expect("native request body");
                        let body = emp_transport::decode_content(
                            body,
                            header("content-encoding").unwrap_or(""),
                            4 * 1024 * 1024,
                            None,
                        )
                        .expect("decode native request");
                        input = Some(serde_json::from_slice::<Value>(&body).unwrap());
                        request.push_str(std::str::from_utf8(&body).unwrap());
                    }
                }
                request_sender
                    .send(request.clone())
                    .expect("send refresh request");
                let status = worker_reply.lock().expect("refresh reply lock").status;
                if hold_first && request_number == 1 {
                    release_receiver
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .expect("release first refresh response");
                }
                let model = if request.contains("Bearer replacement-secret") {
                    "replacement-model"
                } else if request.contains("Bearer second-secret") {
                    "second-account-model"
                } else if request.contains("Bearer selected-secret") {
                    "first-account-model"
                } else {
                    "unknown-account-model"
                };
                let response = input.map_or_else(
                    || {
                        json!({"models":[{
                            "slug":model,
                            "display_name":model,
                            "context_window":262144,
                            "supports_reasoning_summaries":true,
                            "capabilities":{"parallel_tools":true},
                            "schema_marker":{"origin":"authenticated-models-route"}
                        }]})
                    },
                    |input| {
                        json!({"id":"resp_fixture", "object":"response",
                    "status":"completed", "model":input["model"], "output":[]})
                    },
                );
                let body = serde_json::to_vec(&response).expect("refresh catalog JSON");
                write!(stream,"HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",body.len()).expect("refresh head");
                if status == 200 {
                    stream.write_all(&body).expect("refresh body");
                }
            }
        });
        Self {
            address: format!("http://{address}/v1"),
            requests,
            reply,
            release_first: hold_first.then_some(release_sender),
            stop,
            worker: Some(worker),
        }
    }

    fn set_status(&self, status: u16) {
        self.reply.lock().expect("refresh reply lock").status = status;
    }

    fn take_request(&self) -> String {
        self.requests
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("automatic model catalog request")
    }
}

impl Drop for RefreshCatalogFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(release) = &self.release_first {
            let _ = release.send(());
        }
        if let Some(worker) = self.worker.take()
            && let Err(payload) = worker.join()
        {
            if thread::panicking() {
                let message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("non-string panic payload");
                eprintln!("refresh fixture worker also panicked during test unwind: {message}");
            } else {
                std::panic::resume_unwind(payload);
            }
        }
    }
}

fn add_account(server: &ServerHandle, id: &str, prefix: &str, token: &str) -> PathBuf {
    let mut config = server
        .state
        .backend
        .configuration
        .config
        .lock()
        .expect("config lock");
    let auth_path =
        emp_state::account_auth_path(&config, id, &server.state.backend.configuration.config_path)
            .expect("managed auth path");
    let mut account = json!({
        "id":id,"name":id,"prefix":prefix,"enabled":true,"hidden_models":[],
        "auth_file":auth_path.to_string_lossy()
    });
    account["auth_file"] = json!(auth_path.to_string_lossy());
    config["accounts"].as_array_mut().unwrap().push(account);
    server
        .state
        .backend
        .configuration
        .vault
        .write_encrypted_json(
            &auth_path,
            &json!({"tokens":{"access_token":token,"account_id":id}}),
        )
        .expect("write account credentials");
    server.state.catalog_refresh.account_changed(id);
    auth_path
}

fn http_request(
    server: &ServerHandle,
    method: &str,
    target: &str,
    body: &[u8],
    session: bool,
) -> String {
    let mut stream =
        std::net::TcpStream::connect(server.local_addr()).expect("connect HTTP server");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("HTTP response timeout");
    let session = if session {
        format!("X-EMP-Session: {}\r\n", server.session_token())
    } else {
        String::new()
    };
    let length = if method == "POST" { body.len() } else { 0 };
    let content_type = if method == "POST" {
        "Content-Type: application/json\r\n"
    } else {
        ""
    };
    write!(stream,"{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{session}{content_type}Content-Length: {length}\r\nConnection: close\r\n\r\n",server.local_addr().port()).expect("HTTP request head");
    if method == "POST" {
        stream.write_all(body).expect("HTTP request body");
    }
    let mut response = Vec::new();
    std::io::Read::read_to_end(&mut stream, &mut response).expect("HTTP response");
    String::from_utf8(response).expect("HTTP response text")
}

fn http_json(wire: &str) -> Value {
    serde_json::from_str(wire.split_once("\r\n\r\n").expect("HTTP response body").1)
        .expect("HTTP JSON")
}

#[test]
fn partial_native_catalog_refresh_keeps_a_known_model_routable() {
    let upstream = RefreshCatalogFixture::start(false);
    let directory = tempfile::Builder::new()
        .prefix("emp-native-partial-catalog-")
        .tempdir()
        .unwrap();
    let (server, _) = start_server(&directory, &upstream.address, "0.159.2");
    let native_path = directory.path().join("native.json");
    std::fs::write(
        &native_path,
        serde_json::to_vec(&json!({"models":[{
            "slug":"future-native", "display_name":"Future native",
            "supported_in_api":true, "context_window":262144
        }]}))
        .unwrap(),
    )
    .unwrap();
    {
        let mut config = server.state.backend.configuration.config.lock().unwrap();
        config["accounts"] = json!([]);
        config["native_catalog_path"] = json!(native_path);
    }
    std::fs::write(
        &server.state.backend.accounts.native_auth_path,
        serde_json::to_vec(&json!({"tokens":{"access_token":"native-secret",
            "account_id":"native-owner"}}))
        .unwrap(),
    )
    .unwrap();
    for _ in 0..2 {
        crate::services::account_catalog::request_refresh(&server.state, true);
        let request = upstream.take_request();
        assert!(request.starts_with("GET /v1/models?client_version=0.159.2"));
        assert!(
            server
                .state
                .catalog_refresh
                .wait_until_idle(std::time::Duration::from_secs(10))
        );
        let catalog = http_json(&http_request(
            &server,
            "GET",
            "/v1/models?client_version=0.159.2",
            &[],
            false,
        ));
        assert!(
            catalog["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["slug"] == "future-native")
        );
        let input = json!({"model":"future-native", "stream":false,
            "input":[{"role":"user", "content":"Continue the existing conversation."}]});
        let response = http_request(
            &server,
            "POST",
            "/v1/responses",
            &serde_json::to_vec(&input).unwrap(),
            true,
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(http_json(&response)["status"], "completed");
        let forwarded = upstream.take_request();
        assert!(forwarded.starts_with("POST /v1/responses"));
        assert!(forwarded.contains("Continue the existing conversation."));
        assert!(forwarded.contains("future-native"));
        assert!(
            forwarded
                .to_ascii_lowercase()
                .contains("authorization: bearer native-secret")
        );
    }
    server.shutdown().expect("shutdown");
}

#[test]
fn automatic_http_refresh_publishes_capabilities_etag_and_isolated_account_catalogs() {
    use std::os::unix::fs::MetadataExt;

    let upstream = RefreshCatalogFixture::start(false);
    let directory = tempfile::Builder::new()
        .prefix("emp-auto-catalog-http-")
        .tempdir()
        .expect("temporary directory");
    let (server, first_auth) = start_server(&directory, &upstream.address, "0.159.2");
    let second_auth = add_account(&server, "second", "second", "second-secret");
    crate::services::account_catalog::request_refresh(&server.state, false);

    let mut requests = [upstream.take_request(), upstream.take_request()];
    requests.sort();
    assert!(requests.iter().any(|request| {
        request.starts_with("GET /v1/models?client_version=0.159.2 HTTP/1.1\r\n")
            && request
                .to_ascii_lowercase()
                .contains("authorization: bearer selected-secret\r\n")
            && request
                .to_ascii_lowercase()
                .contains("user-agent: codex_cli_rs/0.159.2\r\n")
    }));
    assert!(requests.iter().any(|request| {
        request.starts_with("GET /v1/models?client_version=0.159.2 HTTP/1.1\r\n")
            && request
                .to_ascii_lowercase()
                .contains("authorization: bearer second-secret\r\n")
    }));
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );

    let catalog_wire = http_request(
        &server,
        "GET",
        "/v1/models?client_version=0.159.2",
        &[],
        false,
    );
    assert!(catalog_wire.starts_with("HTTP/1.1 200"), "{catalog_wire}");
    let catalog = http_json(&catalog_wire);
    let demo = catalog["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == "demo/first-account-model")
        .expect("new subscription model appears in EMP catalog");
    assert_eq!(demo["context_window"], 262144);
    assert_eq!(demo["supports_reasoning_summaries"], true);
    assert_eq!(demo["capabilities"]["parallel_tools"], true);
    assert_eq!(
        demo["schema_marker"]["origin"],
        "authenticated-models-route"
    );
    let etag = catalog_wire
        .split("\r\n\r\n")
        .next()
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("ETag: "))
        .expect("catalog ETag");
    assert_eq!(etag, emp_state::catalog_etag(&catalog).unwrap());

    let mut config = http_json(&http_request(&server, "GET", "/api/config", &[], true));
    config["accounts"][0]["name"] = json!("Renamed demo");
    config["accounts"][0]["hidden_models"] = json!(["not-upstream"]);
    let saved = http_request(
        &server,
        "POST",
        "/api/config",
        &serde_json::to_vec(&config).unwrap(),
        true,
    );
    assert!(saved.starts_with("HTTP/1.1 200"), "{saved}");
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );
    assert!(
        upstream.requests.try_recv().is_err(),
        "name and visibility edits recompose locally without a source refetch"
    );

    let synced = http_request(&server, "POST", "/api/catalog/refresh", b"{}", true);
    assert!(synced.starts_with("HTTP/1.1 200"), "{synced}");
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );
    assert!(
        upstream.requests.try_recv().is_err(),
        "ordinary model reads and UI catalog sync respect cache freshness"
    );

    let merged_path = crate::services::catalog::generated_catalog_path(&server.state);
    let merged_inode = std::fs::metadata(&merged_path).unwrap().ino();
    crate::services::account_catalog::request_refresh(&server.state, true);
    let _ = upstream.take_request();
    let _ = upstream.take_request();
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );
    assert_eq!(
        std::fs::metadata(&merged_path).unwrap().ino(),
        merged_inode,
        "unchanged upstream polls do not rewrite the merged catalog"
    );

    let first = http_request(&server, "GET", "/api/accounts/demo/models", &[], true);
    let second = http_request(&server, "GET", "/api/accounts/second/models", &[], true);
    assert!(
        http_json(&first)["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "first-account-model")
    );
    assert!(
        http_json(&second)["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "second-account-model")
    );
    let first_cache: Value = serde_json::from_slice(
        &std::fs::read(first_auth.parent().unwrap().join("models_cache.json")).unwrap(),
    )
    .unwrap();
    let second_cache: Value = serde_json::from_slice(
        &std::fs::read(second_auth.parent().unwrap().join("models_cache.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(first_cache["models"][0]["slug"], "first-account-model");
    assert_eq!(second_cache["models"][0]["slug"], "second-account-model");
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );
    server.shutdown().expect("shutdown");
}

#[test]
fn credential_rotation_with_same_owner_invalidates_cached_refresh() {
    let upstream = RefreshCatalogFixture::start(false);
    let directory = tempfile::Builder::new()
        .prefix("emp-auto-catalog-credential-rotation-")
        .tempdir()
        .expect("temporary directory");
    let (server, auth_path) = start_server(&directory, &upstream.address, "0.159.2");
    let stable_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "https://api.openai.com/auth":{"chatgpt_user_id":"stable-user"}
        }))
        .unwrap(),
    );
    let first_token = format!("header.{stable_payload}.first-signature");
    server
        .state
        .backend
        .configuration
        .vault
        .write_encrypted_json(
            &auth_path,
            &json!({"tokens":{"access_token":first_token,"account_id":"selected-owner"}}),
        )
        .expect("write first test credential");

    crate::services::account_catalog::request_refresh(&server.state, false);
    let first_request = upstream.take_request();
    assert!(first_request.contains(&format!("Bearer {first_token}")));
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );
    let cache_path = auth_path.parent().unwrap().join("models_cache.json");
    let before: Value = serde_json::from_slice(&std::fs::read(&cache_path).unwrap()).unwrap();

    let rotated_token = format!("header.{stable_payload}.rotated-signature");
    server
        .state
        .backend
        .configuration
        .vault
        .write_encrypted_json(
            &auth_path,
            &json!({"tokens":{"access_token":rotated_token,"account_id":"selected-owner"}}),
        )
        .expect("rotate credential outside config save");
    crate::services::account_catalog::request_refresh(&server.state, false);
    let rotated_request = upstream.take_request();
    assert!(rotated_request.contains(&format!("Bearer {rotated_token}")));
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );
    let after: Value = serde_json::from_slice(&std::fs::read(&cache_path).unwrap()).unwrap();
    assert_eq!(before["account_owner"], after["account_owner"]);
    assert!(upstream.requests.try_recv().is_err());
    server.shutdown().expect("shutdown");
}

#[test]
fn automatic_refresh_uses_the_newly_observed_selected_runtime_version() {
    use std::os::unix::fs::PermissionsExt;

    let upstream = RefreshCatalogFixture::start(false);
    let directory = tempfile::Builder::new()
        .prefix("emp-auto-catalog-runtime-")
        .tempdir()
        .expect("temporary directory");
    let (server, _auth_path) = start_server(&directory, &upstream.address, "0.158.6");
    crate::services::account_catalog::request_refresh(&server.state, false);
    let first_request = upstream.take_request();
    assert!(first_request.starts_with("GET /v1/models?client_version=0.158.6 HTTP/1.1\r\n"));
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );

    std::fs::write(
        &server.state.backend.accounts.codex_binary,
        b"#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'codex-cli 0.159.2'; else exit 99; fi\n",
    )
    .expect("replace selected runtime version");
    std::fs::set_permissions(
        &server.state.backend.accounts.codex_binary,
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("selected runtime permissions");
    let inventory = server.state.backend.integration.inventory.snapshot(true);
    assert_eq!(
        server
            .state
            .backend
            .integration
            .inventory
            .selected_trusted_version()
            .as_deref(),
        Some("0.159.2"),
        "the actual selected runtime is observed before refreshing"
    );
    assert_eq!(inventory["helper_source"], "configured");

    crate::services::account_catalog::request_refresh(&server.state, false);
    let second_request = upstream.take_request();
    let lowered = second_request.to_ascii_lowercase();
    assert!(second_request.starts_with("GET /v1/models?client_version=0.159.2 HTTP/1.1\r\n"));
    assert!(lowered.contains("user-agent: codex_cli_rs/0.159.2\r\n"));
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );
    server.shutdown().expect("shutdown");
}

#[test]
fn overlapping_refresh_requests_coalesce_and_failed_refresh_retains_last_good_catalog() {
    let upstream = RefreshCatalogFixture::start(true);
    let directory = tempfile::Builder::new()
        .prefix("emp-auto-catalog-coalesce-")
        .tempdir()
        .expect("temporary directory");
    let (server, auth_path) = start_server(&directory, &upstream.address, "0.159.2");
    crate::services::account_catalog::request_refresh(&server.state, false);
    let first_request = upstream.take_request();
    assert!(first_request.contains("Bearer selected-secret"));
    crate::services::account_catalog::request_refresh(&server.state, true);
    crate::services::account_catalog::request_refresh(&server.state, true);
    upstream.release_first.as_ref().unwrap().send(()).unwrap();
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );
    assert!(
        upstream.requests.try_recv().is_err(),
        "coalesced poll made one request"
    );

    let cache_path = auth_path.parent().unwrap().join("models_cache.json");
    let last_good = std::fs::read(&cache_path).expect("last good account cache");
    upstream.set_status(503);
    crate::services::account_catalog::request_refresh(&server.state, true);
    let failed_request = upstream.take_request();
    assert!(failed_request.contains("Bearer selected-secret"));
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );
    assert_eq!(std::fs::read(&cache_path).unwrap(), last_good);
    let catalog = http_json(&http_request(
        &server,
        "GET",
        "/v1/models?client_version=0.159.2",
        &[],
        false,
    ));
    assert!(
        catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["slug"] == "demo/first-account-model")
    );
    server.shutdown().expect("shutdown");
}

#[test]
fn delete_and_reimport_during_refresh_rejects_the_stale_response() {
    let upstream = RefreshCatalogFixture::start(true);
    let directory = tempfile::Builder::new()
        .prefix("emp-auto-catalog-reimport-")
        .tempdir()
        .expect("temporary directory");
    let (server, auth_path) = start_server(&directory, &upstream.address, "0.159.2");
    crate::services::account_catalog::request_refresh(&server.state, false);
    let first_request = upstream.take_request();
    assert!(first_request.contains("Bearer selected-secret"));
    let deleted = http_request(&server, "DELETE", "/api/accounts/demo", &[], true);
    assert!(deleted.starts_with("HTTP/1.1 200"), "{deleted}");
    let imported = http_request(
        &server,
        "POST",
        "/api/accounts/import",
        br#"{"id":"demo","name":"Demo","prefix":"demo","auth_json":{"tokens":{"access_token":"replacement-secret","account_id":"demo"}}}"#,
        true,
    );
    assert!(imported.starts_with("HTTP/1.1 200"), "{imported}");
    upstream.release_first.as_ref().unwrap().send(()).unwrap();
    let replacement_request = upstream.take_request();
    assert!(replacement_request.contains("Bearer replacement-secret"));
    assert!(
        server
            .state
            .catalog_refresh
            .wait_until_idle(std::time::Duration::from_secs(10))
    );

    let cache_path = auth_path.parent().unwrap().join("models_cache.json");
    let cache: Value = serde_json::from_slice(&std::fs::read(cache_path).unwrap()).unwrap();
    assert_eq!(cache["models"][0]["slug"], "replacement-model");
    assert_ne!(cache["models"][0]["slug"], "first-account-model");
    let catalog = http_json(&http_request(
        &server,
        "GET",
        "/v1/models?client_version=0.159.2",
        &[],
        false,
    ));
    assert!(
        catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["slug"] == "demo/replacement-model")
    );
    assert!(
        !catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["slug"] == "demo/first-account-model")
    );
    server.shutdown().expect("shutdown");
}
