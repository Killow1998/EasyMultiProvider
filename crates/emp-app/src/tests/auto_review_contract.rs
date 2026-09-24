use super::canonical_root;
#[cfg(unix)]
use super::{OneShotUpstream, post, session_cookie_header};
use crate::http::request::{parse_request, read_request_head};
use crate::lifecycle::ServerHandle;
use crate::services::accounts::account_catalog_headers;
use crate::services::auto_review::{automatic_review_candidates, resolve_auto_review_route};
use crate::services::observation::Observation;
use emp_codex::{account_auth_headers, native_catalog_owner, subscription_route_model};
use emp_core::{ResolvedRoute, RouteResolutionError};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const PYTHON_ORACLE: &str = r#"
import json, os, sys
root = os.environ["EMP_PYTHON_ORACLE_ROOT"]
sys.path.insert(0, root)
import easy_multi_provider
assert easy_multi_provider.__version__ == "0.11.10", easy_multi_provider.__version__
from easy_multi_provider.auto_review import automatic_review_candidates
from easy_multi_provider.route_plan import resolve_route
payload = json.load(sys.stdin)
results = []
for case in payload["cases"]:
    config = case["config"]
    order = automatic_review_candidates(
        config, case.get("native_quota"), case.get("native_available", False),
        case.get("cooldowns", {}), now=100.0
    )
    item = {"order": order}
    if case.get("requested_model"):
        config["_auto_review_candidates"] = order
        route = resolve_route(config, case["requested_model"])
        item["route"] = {
            "requested_model": route.requested_model,
            "upstream_model": route.upstream_model,
            "source": route.source,
            "provider_id": route.provider_id,
            "protocol": route.protocol,
            "dialect": route.dialect,
            "auth_mode": route.provider.get("auth_mode"),
            "model_id": route.model.get("id"),
            "model_upstream_id": route.model.get("upstream_id"),
        }
    results.append(item)
json.dump({"version": easy_multi_provider.__version__, "results": results}, sys.stdout)
"#;

fn run_python_oracle(cases: &[Value]) -> Value {
    let python = std::env::var("EMP_PYTHON_INTEROP")
        .expect("EMP_PYTHON_INTEROP must point at the official 0.11.10 venv");
    let root = std::env::var("EMP_PYTHON_ORACLE_ROOT")
        .expect("EMP_PYTHON_ORACLE_ROOT must point at the official oracle");
    assert!(
        std::path::Path::new(&python).is_file(),
        "Python oracle venv is missing"
    );
    assert!(
        std::path::Path::new(&root)
            .join("easy_multi_provider")
            .is_dir()
    );
    let mut child = Command::new(python)
        .args(["-c", PYTHON_ORACLE])
        .env("EMP_PYTHON_ORACLE_ROOT", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start official Python oracle");
    child
        .stdin
        .take()
        .expect("oracle stdin")
        .write_all(&serde_json::to_vec(&json!({"cases":cases})).expect("oracle input JSON"))
        .expect("write oracle input");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("oracle JSON output")
}

fn route_summary(route: &ResolvedRoute) -> Value {
    json!({
        "requested_model": route.requested_model,
        "upstream_model": route.upstream_model,
        "source": serde_json::to_value(route.source).expect("route source JSON"),
        "provider_id": route.provider_id,
        "protocol": route.protocol.as_config_str(),
        "dialect": serde_json::to_value(route.dialect).expect("route dialect JSON"),
        "auth_mode": route.provider.value().get("auth_mode"),
        "model_id": route.model.value().get("id"),
        "model_upstream_id": route.model.value().get("upstream_id"),
    })
}

fn resolve_shared_route(
    server: &ServerHandle,
    config: &mut Value,
    requested_model: &str,
) -> Result<ResolvedRoute, RouteResolutionError> {
    if let Some(route) = resolve_auto_review_route(&server.state, config, requested_model) {
        return route;
    }
    emp_core::resolve_route(config, requested_model, |config, slug, account| {
        subscription_route_model(config, slug, account, |account| {
            account_catalog_headers(account, &server.state.backend.configuration.vault)
        })
    })
}

fn make_review_server(
    base_url: &str,
    accounts: &[Value],
    native_auth: Option<&str>,
) -> (tempfile::TempDir, ServerHandle, Value) {
    let directory = tempfile::tempdir().expect("auto-review tempdir");
    let root = canonical_root(&directory);
    let catalog = root.join("models_cache.json");
    std::fs::write(
        &catalog,
        serde_json::to_vec(&json!({"models":[{
            "slug":"codex-auto-review", "supported_in_api":true,
            "context_window":128000
        }]}))
        .expect("native catalog JSON"),
    )
    .expect("write review catalog");
    let config_path = root.join("config.json");
    let native_auth_path = root.join("codex/auth.json");
    std::fs::create_dir_all(native_auth_path.parent().expect("native parent"))
        .expect("create native parent");
    match native_auth {
        Some("symlink") => {
            let target = root.join("real-auth.json");
            std::fs::write(&target, br#"{"tokens":{"access_token":"native-secret"}}"#)
                .expect("write symlink target");
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &native_auth_path).expect("native symlink");
            #[cfg(not(unix))]
            panic!("symlink contract test requires Unix");
        }
        Some("regular") => {
            std::fs::write(
                &native_auth_path,
                br#"{"tokens":{"access_token":"native-secret"}}"#,
            )
            .expect("write regular native auth");
        }
        _ => {}
    }
    let mut config = json!({
        "codex_base_url":base_url,
        "native_catalog_path":catalog,
        "providers":[], "models":[], "accounts":accounts
    });
    let account_ids = config["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .map(|account| account["id"].as_str().expect("account id").to_owned())
        .collect::<Vec<_>>();
    for (index, id) in account_ids.iter().enumerate() {
        let auth_path = emp_state::account_auth_path(&config, id, &config_path)
            .expect("managed account auth path");
        config["accounts"][index]["auth_file"] =
            Value::String(auth_path.to_string_lossy().into_owned());
    }
    std::fs::write(
        &config_path,
        serde_json::to_vec(&config).expect("config JSON"),
    )
    .expect("write config");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "missing-test-codex",
        native_auth_path,
    )
    .expect("start auto-review server");
    for account in config["accounts"].as_array().expect("accounts") {
        let auth_path = account["auth_file"].as_str().expect("account auth path");
        server
            .state
            .backend
            .configuration
            .vault
            .write_encrypted_json(
                std::path::Path::new(auth_path),
                &json!({"tokens":{
                    "access_token":format!("{}-token", account["id"].as_str().unwrap()),
                    "account_id":format!("{}-owner", account["id"].as_str().unwrap())
                }}),
            )
            .expect("write encrypted account auth");
    }
    (directory, server, config)
}

#[cfg_attr(windows, allow(dead_code))]
fn write_owned_account_catalog(
    server: &ServerHandle,
    account: &Value,
    base_url: &str,
    models: Vec<Value>,
) {
    let auth_path = std::path::Path::new(account["auth_file"].as_str().expect("auth path"));
    let auth = server
        .state
        .backend
        .configuration
        .vault
        .read_encrypted_json(auth_path)
        .expect("read account auth");
    let headers = account_auth_headers(&auth).expect("account catalog headers");
    let owner = native_catalog_owner(&headers);
    let cache_path = auth_path
        .parent()
        .expect("account directory")
        .join("models_cache.json");
    std::fs::write(
        cache_path,
        serde_json::to_vec(&json!({"account_owner":owner,"base_url":base_url,"models":models}))
            .expect("owned catalog JSON"),
    )
    .expect("write owned account catalog");
}

fn set_cooldowns(server: &ServerHandle, cooldowns: &Value) {
    let now = std::time::Instant::now();
    let next = cooldowns
        .as_object()
        .into_iter()
        .flat_map(|cooldowns| cooldowns.iter())
        .map(|(id, until)| {
            let active = until.as_f64().is_some_and(|until| until > 100.0);
            (
                id.clone(),
                if active {
                    now + Duration::from_secs(300)
                } else {
                    now - Duration::from_secs(1)
                },
            )
        })
        .collect();
    *server
        .state
        .auto_review_cooldowns
        .lock()
        .expect("cooldown lock") = next;
}

#[test]
fn selector_and_route_match_live_python_01110() {
    let base_url = "http://127.0.0.1:9/v1";
    let accounts = vec![
        json!({"id":"high","name":"High","prefix":"current","enabled":true,"quota":{"rate_limits":{"primary":{"usedPercent":10}}}}),
        json!({"id":"backup","name":"Backup","prefix":"backup","enabled":true,"quota":{"rate_limits":{"primary":{"usedPercent":30}}}}),
        json!({"id":"tie-first","name":"Tie First","prefix":"tie-first","enabled":true,"quota":{"rate_limits":{"primary":{"usedPercent":50}}}}),
        json!({"id":"tie-second","name":"Tie Second","prefix":"tie-second","enabled":true,"quota":{"rate_limits":{"primary":{"usedPercent":50}}}}),
    ];
    let (_directory, server, mut base_config) =
        make_review_server(base_url, &accounts, Some("regular"));
    let mut config = server
        .state
        .backend
        .configuration
        .config
        .lock()
        .expect("config lock")
        .clone();
    config["native_catalog_path"] = base_config["native_catalog_path"].clone();
    config["codex_base_url"] = base_config["codex_base_url"].clone();
    config["models"] = json!([{"id":"codex-auto-review","provider":"fallback","upstream_id":"ordinary-review","enabled":true}]);
    config["providers"] = json!([{"id":"fallback","name":"Fallback","base_url":base_url,"protocol":"responses","auth_mode":"api_key","api_key":"unused"}]);
    base_config = config.clone();

    let falsy_config = json!({"accounts":[
        {"id":"null","auth_file":null}, {"id":"false","auth_file":false},
        {"id":"empty","auth_file":""}, {"id":"invalid","auth_file":"invalid.enc","credential_status":"invalid"},
        {"id":"ok","auth_file":"ok.enc","quota":{"rate_limits":{"primary":{"usedPercent":20}}}}
    ]});
    let cases = vec![
        json!({"config":base_config,"native_quota":{"rate_limits":{"primary":{"usedPercent":40}}},"native_available":true,"cooldowns":{},"requested_model":"codex-auto-review"}),
        json!({"config":base_config,"native_quota":{"rate_limits":{"primary":{"usedPercent":100}}},"native_available":true,"cooldowns":{"high":200},"requested_model":"stale-prefix/codex-auto-review"}),
        json!({"config":base_config,"native_quota":{"rate_limits":{"primary":{"usedPercent":40}}},"native_available":true,"cooldowns":{"@native":200,"high":200,"backup":200},"requested_model":"codex-auto-review"}),
        json!({"config":falsy_config,"native_quota":null,"native_available":false,"cooldowns":{}}),
        json!({"config":{"native_catalog_path":base_config["native_catalog_path"],"providers":base_config["providers"],"models":[{"id":"codex-auto-review","provider":"fallback","upstream_id":"ordinary-review","enabled":true}],"accounts":[]},"native_available":false,"cooldowns":{},"requested_model":"codex-auto-review"}),
    ];
    let oracle = run_python_oracle(&cases);
    assert_eq!(oracle["version"], "0.11.10");

    for (index, case) in cases.iter().enumerate() {
        let case_config = case["config"].clone();
        let native_quota = case
            .get("native_quota")
            .filter(|value| !value.is_null())
            .cloned();
        let cooling = case["cooldowns"]
            .as_object()
            .into_iter()
            .flat_map(|items| items.iter())
            .filter(|(_, until)| until.as_f64().is_some_and(|until| until > 100.0))
            .map(|(id, _)| id.clone())
            .collect::<BTreeSet<_>>();
        let native_available = case["native_available"].as_bool().unwrap_or(false);
        let rust_order = automatic_review_candidates(
            case_config.as_object().expect("scenario config"),
            native_quota.as_ref(),
            native_available,
            &cooling,
        );
        assert_eq!(
            serde_json::to_value(rust_order).unwrap(),
            oracle["results"][index]["order"]
        );
        if index == 0 {
            assert_eq!(
                oracle["results"][index]["order"],
                json!(["@native", "high", "backup", "tie-first", "tie-second"])
            );
        }
        if let Some(requested) = case.get("requested_model").and_then(Value::as_str)
            && (index < 3 || index == 4)
        {
            if index == 4 {
                std::fs::remove_file(&server.state.backend.accounts.native_auth_path)
                    .expect("make native auth unavailable for ordinary fallback");
            }
            *server.state.backend.accounts.native_quota.lock().unwrap() = native_quota.clone();
            set_cooldowns(&server, &case["cooldowns"]);
            let mut runtime_config = case_config.clone();
            runtime_config["_native_auth_path"] = Value::String(
                server
                    .state
                    .backend
                    .accounts
                    .native_auth_path
                    .to_string_lossy()
                    .into_owned(),
            );
            let route = resolve_shared_route(&server, &mut runtime_config, requested)
                .expect("Rust route resolution");
            assert_eq!(route_summary(&route), oracle["results"][index]["route"]);
            if index == 1 {
                assert_eq!(route.requested_model, "stale-prefix/codex-auto-review");
                assert_eq!(route.upstream_model, "codex-auto-review");
                assert_eq!(route.provider_id, "backup");
            } else if index == 4 {
                assert_eq!(route.requested_model, "codex-auto-review");
                assert_eq!(route.upstream_model, "ordinary-review");
                assert_eq!(route.source, emp_core::RouteSource::ExplicitModel);
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn http_auto_review_skips_symlink_native_and_missing_account_catalog_without_leaking_auth() {
    let upstream = OneShotUpstream::start(json!({
        "id":"auto-review-http","object":"response","status":"completed",
        "model":"codex-auto-review","output":[]
    }));
    let accounts = vec![
        json!({"id":"missing-model","name":"Missing Model","prefix":"old","enabled":true,"quota":{"rate_limits":{"primary":{"usedPercent":0}}}}),
        json!({"id":"selected","name":"Selected","prefix":"new","enabled":true,"quota":{"rate_limits":{"primary":{"usedPercent":20}}}}),
    ];
    let (_directory, server, config) =
        make_review_server(&upstream.base_url(), &accounts, Some("symlink"));
    assert!(!crate::services::accounts::regular_file(
        &server.state.backend.accounts.native_auth_path
    ));
    write_owned_account_catalog(
        &server,
        &config["accounts"][0],
        &upstream.base_url(),
        vec![json!({"slug":"other-model","supported_in_api":true})],
    );
    let wire = post(
        &server,
        "/v1/responses",
        br#"{"model":"stale-prefix/codex-auto-review","input":"review"}"#,
        &[&session_cookie_header(&server)],
    );
    let (path, headers, body) = upstream.observed();
    assert_eq!(body["model"], "codex-auto-review");
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer selected-token")
    );
    assert_eq!(path, "/v1/responses");
    assert!(wire.starts_with("HTTP/1.1 200 OK\r\n"), "{wire}");
    assert!(
        !wire.contains("selected-token"),
        "credential leaked in client response"
    );
}

#[cfg_attr(windows, allow(dead_code))]
struct ReviewStreamUpstream {
    address: SocketAddr,
    observed: mpsc::Receiver<(String, BTreeMap<String, String>, Value)>,
    worker: Option<JoinHandle<()>>,
}

#[cfg_attr(windows, allow(dead_code))]
impl ReviewStreamUpstream {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind WS upstream");
        let address = listener.local_addr().expect("WS upstream address");
        let (sender, observed) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept WS upstream request");
            let raw = read_request_head(&mut stream).expect("WS upstream request head");
            let request = parse_request(&raw.head).expect("WS upstream request");
            if request
                .header("Upgrade")
                .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
            {
                stream
                    .write_all(b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .expect("reject native WebSocket and exercise HTTP fallback");
                drop(stream);
                let (mut fallback, _) = listener.accept().expect("accept HTTP fallback");
                let (path, headers, body) = super::receive_upstream_request(&mut fallback);
                sender.send((path, headers, body)).unwrap();
                stream = fallback;
            } else {
                let headers = request
                    .headers
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| {
                        (name.trim().to_ascii_lowercase(), value.trim().to_owned())
                    })
                    .collect::<BTreeMap<_, _>>();
                let length = headers["content-length"]
                    .parse::<usize>()
                    .expect("Content-Length");
                let mut bytes = raw.body_prefix;
                while bytes.len() < length {
                    let mut chunk = [0_u8; 4096];
                    let count = stream.read(&mut chunk).expect("read WS request body");
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                }
                bytes.truncate(length);
                let decoded = emp_transport::decode_content(
                    bytes,
                    headers
                        .get("content-encoding")
                        .map(String::as_str)
                        .unwrap_or(""),
                    4 * 1024 * 1024,
                    None,
                )
                .expect("decode upstream request");
                let body: Value = serde_json::from_slice(&decoded).expect("WS upstream body JSON");
                sender
                    .send((request.target.to_owned(), headers, body))
                    .unwrap();
            }
            let events = concat!(
                "event: response.created\n",
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"review-ws\",\"status\":\"in_progress\"}}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"review-ws\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"codex-auto-review\",\"output\":[]}}\n\n"
            );
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",events.len()).unwrap();
            stream.write_all(events.as_bytes()).unwrap();
        });
        Self {
            address,
            observed,
            worker: Some(worker),
        }
    }
    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
    fn observed(&self) -> (String, BTreeMap<String, String>, Value) {
        self.observed
            .recv_timeout(Duration::from_secs(5))
            .expect("WS upstream observation")
    }
}
impl Drop for ReviewStreamUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            if !worker.is_finished()
                && let Ok(mut stream) = TcpStream::connect(self.address)
            {
                let _ =
                    stream.write_all(b"POST /v1/responses HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}");
            }
            worker.join().expect("join WS upstream");
        }
    }
}

#[cfg(unix)]
#[test]
fn websocket_auto_review_uses_the_shared_imported_account_route() {
    let upstream = ReviewStreamUpstream::start();
    let accounts = vec![json!({
        "id":"ws-account","name":"WS Account","prefix":"not-the-request-prefix",
        "enabled":true,"quota":{"rate_limits":{"primary":{"usedPercent":20}}}
    })];
    let (_directory, server, _) =
        make_review_server(&upstream.base_url(), &accounts, Some("symlink"));
    let url = format!("ws://{}/v1/responses", server.local_addr());
    let headers = BTreeMap::from([(
        "cookie".to_owned(),
        session_cookie_header(&server)
            .trim_start_matches("Cookie: ")
            .to_owned(),
    )]);
    let mut socket =
        emp_transport::ClientWebSocket::connect(&url, &headers, Duration::from_secs(5))
            .expect("connect local Responses websocket");
    socket.send_json(&json!({"type":"response.create","model":"stale/ws-prefix/codex-auto-review","input":"review"})).expect("send auto-review WS request");
    let mut completed = false;
    for _ in 0..12 {
        let event = socket
            .receive_json()
            .expect("read local WS response")
            .expect("WS response closed");
        if event["type"] == "response.completed" {
            completed = true;
            break;
        }
        assert_ne!(event["type"], "error", "unexpected WS error: {event}");
    }
    assert!(completed, "WS response did not complete");
    let (path, headers, body) = upstream.observed();
    assert_eq!(path, "/v1/responses");
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer ws-account-token")
    );
    assert_eq!(body["model"], "codex-auto-review");
}

#[test]
fn observation_failure_and_success_update_the_next_request_candidates() {
    let base_url = "http://127.0.0.1:9/v1";
    let accounts = vec![json!({
        "id":"review-account","name":"Review Account","prefix":"review",
        "enabled":true,"quota":{"rate_limits":{"primary":{"usedPercent":20}}}
    })];
    let (_directory, server, config) = make_review_server(base_url, &accounts, Some("regular"));
    let model = json!({"model":"codex-auto-review","input":"review"});
    let route = {
        let mut config = config.clone();
        config["_native_auth_path"] = Value::String(
            server
                .state
                .backend
                .accounts
                .native_auth_path
                .to_string_lossy()
                .into_owned(),
        );
        resolve_shared_route(&server, &mut config, "codex-auto-review").expect("initial route")
    };
    assert_eq!(route.provider_id, "codex-native");
    let mut failed = Observation::new(
        &server.state,
        &route,
        &model,
        &BTreeMap::new(),
        None,
        "responses",
    );
    failed.http_status(429);
    failed.finish();
    let after_failure = {
        let mut config = config.clone();
        config["_native_auth_path"] = Value::String(
            server
                .state
                .backend
                .accounts
                .native_auth_path
                .to_string_lossy()
                .into_owned(),
        );
        resolve_shared_route(&server, &mut config, "codex-auto-review")
            .expect("route after failure")
    };
    assert_eq!(after_failure.provider_id, "review-account");

    let mut succeeded = Observation::new(
        &server.state,
        &after_failure,
        &model,
        &BTreeMap::new(),
        None,
        "responses",
    );
    server.state.auto_review_cooldowns.lock().unwrap().insert(
        "review-account".to_owned(),
        std::time::Instant::now() + Duration::from_secs(300),
    );
    succeeded.observe(&json!({"type":"response.completed","response":{"status":"completed"}}));
    succeeded.finish();
    assert!(
        !server
            .state
            .auto_review_cooldowns
            .lock()
            .unwrap()
            .contains_key("review-account")
    );
    let after_success = {
        let mut config = config.clone();
        config["_native_auth_path"] = Value::String(
            server
                .state
                .backend
                .accounts
                .native_auth_path
                .to_string_lossy()
                .into_owned(),
        );
        resolve_shared_route(&server, &mut config, "codex-auto-review")
            .expect("route after success")
    };
    assert_eq!(after_success.provider_id, "review-account");
}
