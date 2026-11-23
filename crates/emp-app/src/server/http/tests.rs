use super::{
    body::{JsonBody, RealtimeBody, read_json_body_with_limits},
    error::{EntryError, JsonReply},
    management::{
        BootstrapExchange, ManagementSession, PageOrigin, RecoveryAccess, RecoveryMutation,
        RecoveryOrigin, RecoverySession, consume_export_confirmation, issue_export_confirmation,
    },
    proxy::{ModelCatalog, ProxyCaller, RequestAccess},
    with_entry_state,
};
use crate::{
    http::{
        request::{parse_request, read_request_head},
        response::body_error_response,
    },
    lifecycle::ServerHandle as LegacyServer,
    server::{
        ServerHandle,
        tests::{LOOPBACK, admission},
    },
    services::error_presentation::ErrorPresentation,
    services::gateway_clients::Caller,
    util::system_now,
};
use axum::{
    Extension, Router,
    extract::{Request, State},
    routing::{any, get, post},
};
use emp_transport::{MemoryStatus, RequestLimits, RequestLimitsConfig};
use flate2::{Compression, GzBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::Write,
    net::{SocketAddr, TcpListener},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

pub(crate) async fn exchange_wire(address: SocketAddr, request: &[u8], end_input: bool) -> Vec<u8> {
    timeout(Duration::from_secs(15), async {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(request).await.unwrap();
        if end_input {
            stream.shutdown().await.unwrap();
        }
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await.unwrap();
        reply
    })
    .await
    .expect("complete HTTP response")
}

pub(crate) async fn assert_wire_parity(
    old: SocketAddr,
    new: SocketAddr,
    request: &[u8],
    end_input: bool,
) -> Vec<u8> {
    let (old, new) = tokio::join!(
        exchange_wire(old, request, end_input),
        exchange_wire(new, request, end_input),
    );
    assert!(
        new == old,
        "different responses for {}\nnew: {}\nold: {}",
        String::from_utf8_lossy(request.split(|&byte| byte == b'\r').next().unwrap()),
        String::from_utf8_lossy(&new),
        String::from_utf8_lossy(&old),
    );
    old
}

pub(crate) fn assert_status(reply: &[u8], status: u16) {
    assert!(
        reply.starts_with(format!("HTTP/1.1 {status} ").as_bytes()),
        "{}",
        String::from_utf8_lossy(reply),
    );
}

pub(crate) fn json_body(reply: &[u8]) -> Value {
    let offset = reply
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap()
        + 4;
    serde_json::from_slice(&reply[offset..]).unwrap()
}

pub(crate) fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[&str],
    body: &[u8],
) -> Vec<u8> {
    let mut wire = format!("{method} {path} HTTP/1.1\r\n");
    if !headers.iter().any(|line| line.starts_with("Host:")) {
        wire.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for header in headers {
        wire.push_str(header);
        wire.push_str("\r\n");
    }
    wire.push_str("\r\n");
    let mut wire = wire.into_bytes();
    wire.extend_from_slice(body);
    wire
}

pub(crate) fn json_request(port: u16, path: &str, headers: &[&str], body: &[u8]) -> Vec<u8> {
    let length = format!("Content-Length: {}", body.len());
    let mut headers = headers.to_vec();
    headers.extend(["Content-Type: application/json", length.as_str()]);
    request(port, "POST", path, &headers, body)
}

struct Fixture {
    directory: tempfile::TempDir,
    legacy: LegacyServer,
    upstream: Arc<AtomicUsize>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_directory(tempfile::tempdir().unwrap())
    }

    fn with_directory(directory: tempfile::TempDir) -> Self {
        let legacy = LegacyServer::start_with_config(
            LOOPBACK.ip(),
            0,
            &directory.path().join("config.json"),
        )
        .unwrap();
        Self {
            directory,
            legacy,
            upstream: Arc::default(),
        }
    }

    fn run(self, test: impl AsyncFnOnce(&Self, SocketAddr)) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let server = ServerHandle::bind(LOOPBACK, self.router(), admission())
                    .await
                    .unwrap();
                test(&self, server.local_addr()).await;
                server.shutdown().await.unwrap();
            });
        self.legacy.shutdown().unwrap();
    }

    fn run_candidate(self, test: impl AsyncFnOnce(&Self, SocketAddr)) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let listener = tokio::net::TcpListener::bind(LOOPBACK).await.unwrap();
                let address = listener.local_addr().unwrap();
                let state = &self.legacy.state;
                let candidate =
                    tokio::spawn(crate::server::serve_candidate(listener, state.clone()));
                test(&self, address).await;
                state.request_shutdown();
                candidate.await.unwrap().unwrap();
            });
        self.legacy.shutdown().unwrap();
    }

    fn router(&self) -> Router {
        let management_calls = self.upstream.clone();
        let proxy_calls = self.upstream.clone();
        let router = Router::new()
            .route("/api/session", post(|_access: RequestAccess, exchange: BootstrapExchange| async move { exchange }))
            .route("/api/language", post(move |_access: RequestAccess, _session: ManagementSession, body: JsonBody| async move {
                management_calls.fetch_add(1, Ordering::Relaxed);
                #[derive(Deserialize)]
                struct Language {
                    language: Option<String>,
                }
                let body = body.into_typed::<Language>()?;
                if body.language.is_none() {
                    return Err(EntryError::Json(400, "language is required", None));
                }
                Ok(JsonReply::new(200, json!({"language":body.language}).to_string().into_bytes()))
            }))
            .route("/api/migration/export/confirm", post(
                |_access: RequestAccess, _session: ManagementSession, State(state): State<Arc<crate::app::ServerState>>, _body: JsonBody| async move {
                    issue_export_confirmation(&state, system_now())
                }
            ))
            .route("/api/migration/export", post(
                |_access: RequestAccess, _session: ManagementSession, State(state): State<Arc<crate::app::ServerState>>, JsonBody(body): JsonBody| async move {
                    consume_export_confirmation(&state, body["confirmation"].as_str().unwrap_or_default(), "/api/migration/export", system_now())?;
                    Ok::<_, EntryError>(JsonReply::new(200, b"{}".to_vec()))
                }
            ))
            .route("/v1/responses", post(move |_access: RequestAccess, caller: ProxyCaller, JsonBody(body): JsonBody| async move {
                proxy_calls.fetch_add(1, Ordering::Relaxed);
                JsonReply::new(200, json!({"id": caller.caller.id, "headers": caller.incoming, "body":body}).to_string().into_bytes())
            }).get(proxy_probe))
            .route("/v1/decisions", post(proxy_probe))
            .route("/api/modules", post(|_access: RequestAccess, _session: ManagementSession, _: JsonBody| async { "management" }))
            .route("/v1/alpha/search", post(proxy_probe))
            .route("/v1/images/generations", post(proxy_probe))
            .route("/v1/live", post(|_access: RequestAccess, _caller: ProxyCaller, RealtimeBody(body): RealtimeBody| async move {
                JsonReply::new(200, body.session.to_string().into_bytes())
            }))
            .route("/v1/live/{id}", get(proxy_probe))
            .route("/v1/models", get(|_identity: ModelCatalog| async { "catalog upstream" }))
            .route("/", any(|_origin: PageOrigin| async { "page" }))
            .route("/assets/app-icon.png", get(|_origin: PageOrigin| async { "static" }))
            .route("/guide/guide.css", get(|_origin: PageOrigin| async { "guide" }))
            .route("/healthz", any(|_origin: PageOrigin| async {
                JsonReply::new(200, crate::http::response::HEALTH_JSON_BYTES.to_vec())
            }))
            .layer(Extension(self.upstream.clone()));
        with_entry_state(router, self.legacy.state.clone())
    }

    fn port(&self) -> u16 {
        self.legacy.local_addr().port()
    }

    fn session(&self) -> String {
        format!("X-EMP-Session: {}", self.legacy.session_token())
    }

    async fn parity(&self, new: SocketAddr, request: &[u8], status: u16) -> Vec<u8> {
        let reply = assert_wire_parity(self.legacy.local_addr(), new, request, false).await;
        assert_status(&reply, status);
        reply
    }
}

#[test]
fn update_module_and_gateway_reads_match_legacy_wire() {
    let fixture = Fixture::new();
    let backend = &fixture.legacy.state.backend;
    let device = backend
        .gateway_clients
        .issue(
            "Parity device",
            None,
            &backend.configuration.vault,
            &backend.modules,
        )
        .unwrap();
    let device = serde_json::to_value(device).unwrap();
    let routes = [
        ("/api/updates", "current_version", json!(crate::VERSION), 1),
        (
            "/api/modules",
            "modules",
            json!(backend.modules.status()),
            2,
        ),
        ("/api/gateway", "api_key", json!(backend.gateway_key), 2),
        (
            "/api/gateway/clients",
            "clients",
            json!(backend.gateway_clients.list()),
            2,
        ),
        (
            "/api/gateway/usage?group=device&start=0&end=10",
            "rows",
            json!([{
                "caller_id":device["id"], "caller_name":"Parity device", "calls":0,
                "input_tokens":0, "output_tokens":0, "cost_nanos":0,
                "reported_calls":0, "priced_calls":0, "models":[]
            }]),
            2,
        ),
        (
            "/api/decisions/connection",
            "api_key",
            json!(backend.gateway_key),
            2,
        ),
    ];
    let bearer = format!("Authorization: Bearer {}", backend.gateway_key);
    fixture.run_candidate(async |fixture, address| {
        let session = fixture.session();
        let port = fixture.port();
        for (path, field, expected, cache_headers) in routes {
            let wire = request(
                port,
                "GET",
                path,
                &[&session, "Accept: text/event-stream"],
                b"",
            );
            let reply = assert_wire_parity(fixture.legacy.local_addr(), address, &wire, true).await;
            assert_status(&reply, 200);
            assert_eq!(json_body(&reply)[field], expected);
            assert_eq!(
                str::from_utf8(&reply)
                    .unwrap()
                    .matches("\r\nCache-Control: no-store\r\n")
                    .count(),
                cache_headers,
            );
            let missing = format!("{}/missing", path.split('?').next().unwrap());
            for (method, path, headers, status) in [
                ("GET", path, [&*bearer].as_slice(), 401),
                ("GET", path, [&*session, &*session].as_slice(), 401),
                (
                    "GET",
                    path,
                    [&*session, "Origin: https://foreign.invalid"].as_slice(),
                    403,
                ),
                ("GET", &missing, [&*session].as_slice(), 404),
                ("HEAD", path, [&*session].as_slice(), 404),
            ] {
                fixture
                    .parity(address, &request(port, method, path, headers, b""), status)
                    .await;
            }
        }
        let reply = fixture
            .parity(
                address,
                &request(
                    port,
                    "GET",
                    "/api/gateway/usage?start=invalid",
                    &[&session],
                    b"",
                ),
                400,
            )
            .await;
        assert_eq!(
            json_body(&reply)["error"]["message"],
            "Invalid report period"
        );
    });
}

#[test]
fn gateway_and_decision_module_gates_precede_management_authentication() {
    let fixture = Fixture::new();
    let modules = &fixture.legacy.state.backend.modules;
    modules.mark_unavailable(emp_modules::Module::Gateway, "test");
    modules.mark_unavailable(emp_modules::Module::Decision, "test");
    fixture.run_candidate(async |fixture, address| {
        for path in [
            "/api/gateway",
            "/api/gateway/clients",
            "/api/gateway/usage?start=invalid",
            "/api/decisions/connection",
        ] {
            let reply = fixture
                .parity(
                    address,
                    &request(
                        fixture.port(),
                        "GET",
                        path,
                        &["Origin: https://foreign.invalid"],
                        b"",
                    ),
                    404,
                )
                .await;
            assert_eq!(json_body(&reply)["error"]["code"], "module_disabled");
        }
    });
}

#[cfg(unix)]
#[test]
fn module_panel_matches_legacy_wire_for_origin_and_revision() {
    use emp_modules::{HOST_API, Hello, Module, PROTOCOL, Reply};
    use sha2::{Digest, Sha256};
    use std::{fs, os::unix::fs::PermissionsExt};

    let directory = tempfile::tempdir().unwrap();
    let modules = directory.path().join("state/modules");
    fs::create_dir_all(&modules).unwrap();
    let module = Module::Gateway;
    let panel = "export const panel = \"fixture\";\n";
    let hello = serde_json::to_string(&Hello {
        module,
        version: crate::VERSION.into(),
        module_version: Some(module.version().into()),
        protocol: PROTOCOL,
        host_api: Some(HOST_API),
    })
    .unwrap();
    let reply = serde_json::to_string(&Reply {
        value: json!(panel),
        error: None,
    })
    .unwrap();
    let worker = format!(
        "#!/bin/sh\nprintf '%s\\n' '{hello}'\nread -r line\nprintf '%s\\n' '{reply}'\nwhile read -r line; do :; done\n"
    );
    let revision = format!("{:x}", Sha256::digest(&worker));
    let executable = modules.join(format!("gateway-{revision}"));
    fs::write(&executable, worker).unwrap();
    fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(modules.join("modules.json"), json!({
        "gateway":{"enabled":true,"versions":{},"artifact":{"digest":revision,"version":module.version()}}
    }).to_string()).unwrap();

    Fixture::with_directory(directory).run_candidate(async |fixture, address| {
        let port = fixture.port();
        let same_origin = format!("Origin: http://localhost:{port}");
        let path = format!("/assets/module-gateway.js?revision={revision}");
        let reply = fixture
            .parity(
                address,
                &request(port, "GET", &path, &[&same_origin], b""),
                200,
            )
            .await;
        let (head, body) = str::from_utf8(&reply)
            .unwrap()
            .split_once("\r\n\r\n")
            .unwrap();
        assert!(head.contains("\r\nContent-Type: text/javascript; charset=utf-8\r\n"));
        assert!(head.contains("\r\nCache-Control: no-store\r\n"));
        assert_eq!(body, panel);
        let stale = fixture
            .parity(
                address,
                &request(
                    port,
                    "GET",
                    "/assets/module-gateway.js?revision=old",
                    &[],
                    b"",
                ),
                404,
            )
            .await;
        let (head, body) = str::from_utf8(&stale)
            .unwrap()
            .split_once("\r\n\r\n")
            .unwrap();
        assert!(head.contains("\r\nContent-Type: text/plain\r\n"));
        assert!(body.is_empty());
        for (method, path, headers) in [
            (
                "GET",
                &*path,
                ["Origin: https://foreign.invalid"].as_slice(),
            ),
            ("GET", "/assets/module-unknown.js", [].as_slice()),
            ("HEAD", &*path, [].as_slice()),
        ] {
            let reply = fixture
                .parity(address, &request(port, method, path, headers, b""), 404)
                .await;
            assert_eq!(json_body(&reply)["error"]["message"], "not found");
        }
        fixture
            .parity(
                address,
                &request(port, "GET", "/assets/module-settings.js", &[], b""),
                200,
            )
            .await;
    });
}

async fn proxy_probe(
    _access: RequestAccess,
    _caller: ProxyCaller,
    Extension(upstream): Extension<Arc<AtomicUsize>>,
) -> &'static str {
    upstream.fetch_add(1, Ordering::Relaxed);
    "upstream"
}

#[test]
fn account_reads_match_legacy_wire_on_the_candidate_listener() {
    let fixture = Fixture::new();
    let state = &fixture.legacy.state;
    let auth_path = &state.backend.accounts.native_auth_path;
    std::fs::create_dir_all(auth_path.parent().unwrap()).unwrap();
    std::fs::write(
        auth_path,
        br#"{"tokens":{"access_token":"fixture-token","account_id":"fixture-owner"}}"#,
    )
    .unwrap();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let listener = tokio::net::TcpListener::bind(LOOPBACK).await.unwrap();
            let address = listener.local_addr().unwrap();
            let candidate = tokio::spawn(crate::server::serve_candidate(listener, state.clone()));
            let session = fixture.session();
            let port = fixture.port();
            for (path, field, missing, missing_status) in [
                ("/api/accounts", "accounts", "/api/accounts/absent", 404),
                (
                    "/api/accounts/%40native/models",
                    "models",
                    "/api/accounts/absent/models",
                    400,
                ),
                (
                    "/api/accounts/%40native/quota-history?range=all",
                    "series",
                    "/api/accounts/absent/quota-history",
                    404,
                ),
            ] {
                let reply = fixture
                    .parity(address, &request(port, "GET", path, &[&session], b""), 200)
                    .await;
                assert!(json_body(&reply)[field].is_array());
                for (method, path, headers, status) in [
                    ("GET", path, [].as_slice(), 401),
                    ("GET", path, [&*session, &*session].as_slice(), 401),
                    (
                        "GET",
                        path,
                        [&*session, "Origin: https://other.invalid"].as_slice(),
                        403,
                    ),
                    ("GET", missing, [&*session].as_slice(), missing_status),
                    ("HEAD", path, [&*session].as_slice(), 404),
                ] {
                    fixture
                        .parity(address, &request(port, method, path, headers, b""), status)
                        .await;
                }
            }
            for (path, status) in [
                ("/api/accounts/models", 400),
                ("/api/accounts//quota-history", 404),
                ("/api/accounts/%40native/quota-history?range=invalid", 400),
            ] {
                fixture
                    .parity(
                        address,
                        &request(port, "GET", path, &[&session], b""),
                        status,
                    )
                    .await;
            }
            state.request_shutdown();
            candidate.await.unwrap().unwrap();
        });
    fixture.legacy.shutdown().unwrap();
}

#[test]
fn usage_and_settings_reads_match_legacy_wire_on_candidate_listener() {
    let fixture = Fixture::new();
    fixture
        .legacy
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["providers"] = json!([{
        "id": "fixture-cpa",
        "name": "Fixture CPA",
        "base_url": "http://127.0.0.1:9",
        "execution_backend": "claude_cli",
        "enabled": false,
    }]);
    fixture.run_candidate(async |fixture, address| {
        let port = fixture.port();
        let session = fixture.session();
        for (path, field, invalid, invalid_status) in [
            (
                "/api/usage?start=1&end=10",
                "totals",
                "/api/usage?start=invalid",
                400,
            ),
            (
                "/api/calls?start=1&end=10",
                "records",
                "/api/calls?limit=0",
                400,
            ),
            (
                "/api/providers/fixture%2Dcpa/quota-history?start=1&end=10",
                "account_ids",
                "/api/providers/absent/quota-history?start=1&end=10",
                404,
            ),
            ("/api/config", "models", "/api/config/absent", 404),
            (
                "/api/integration",
                "configuration",
                "/api/integration/absent",
                404,
            ),
            (
                "/api/runtime/claude-cli",
                "available",
                "/api/runtime/claude-cli/absent",
                404,
            ),
            (
                "/api/request-limits",
                "baseline_bytes",
                "/api/request-limits/absent",
                404,
            ),
        ] {
            let reply = fixture
                .parity(address, &request(port, "GET", path, &[&session], b""), 200)
                .await;
            assert!(
                !json_body(&reply)[field].is_null(),
                "missing {field} for {path}"
            );
            assert!(
                reply
                    .windows(b"\r\nCache-Control: no-store\r\n".len())
                    .any(|line| line == b"\r\nCache-Control: no-store\r\n")
            );
            for (method, path, headers, status) in [
                ("GET", path, [].as_slice(), 401),
                ("GET", path, [&*session, &*session].as_slice(), 401),
                (
                    "GET",
                    path,
                    [&*session, "Origin: https://other.invalid"].as_slice(),
                    403,
                ),
                ("GET", invalid, [&*session].as_slice(), invalid_status),
                ("HEAD", path, [&*session].as_slice(), 404),
            ] {
                fixture
                    .parity(address, &request(port, method, path, headers, b""), status)
                    .await;
            }
        }
        for (path, status) in [
            ("/api/usage?account_id=absent", 404),
            (
                "/api/providers/fixture-cpa/quota-history?start=10&end=1",
                400,
            ),
            ("/api/providers//quota-history?start=1&end=10", 404),
            (
                "/api/providers/fixture/cpa/quota-history?start=10&end=1",
                400,
            ),
        ] {
            fixture
                .parity(
                    address,
                    &request(port, "GET", path, &[&session], b""),
                    status,
                )
                .await;
        }
    });
}

#[test]
fn report_event_streams_remain_on_the_legacy_listener() {
    Fixture::new().run_candidate(async |fixture, address| {
        let session = fixture.session();
        let accept = "Accept: text/event-stream";
        for path in [
            "/api/calls?start=1&end=10",
            "/api/usage?lifetime=%74rue&owner=fixture",
        ] {
            let reply = fixture
                .parity(
                    address,
                    &request(fixture.port(), "GET", path, &[&session, accept], b""),
                    200,
                )
                .await;
            let reply = str::from_utf8(&reply).unwrap();
            assert!(reply.contains("\r\nContent-Type: text/event-stream\r\n"));
            assert!(reply.contains("event: progress\n"));
            assert!(reply.contains("event: result\n"));
        }
        for (path, accept, field) in [
            ("/api/usage?start=1&end=10", accept, "totals"),
            (
                "/api/usage?lifetime=true&lifetime=true&owner=fixture",
                accept,
                "groups",
            ),
            (
                "/api/calls?start=1&end=10",
                "Accept: text/event-stream, application/json",
                "records",
            ),
        ] {
            let reply = fixture
                .parity(
                    address,
                    &request(fixture.port(), "GET", path, &[&session, accept], b""),
                    200,
                )
                .await;
            assert!(!json_body(&reply)[field].is_null());
        }
    });
}

#[test]
fn closing_report_input_cancels_legacy_and_candidate_json_queries() {
    let fixture = Fixture::new();
    let ledger = &fixture.legacy.state.backend.usage.ledger;
    ledger.query(1.0, 10.0, "all", system_now(), None).unwrap();
    let database =
        rusqlite::Connection::open(fixture.directory.path().join("state/usage.sqlite3")).unwrap();
    database.execute_batch(
        "WITH RECURSIVE records(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM records WHERE n<20000)
         INSERT INTO call_records(request_id,operation,started_at,finished_at,provider_id,account_id,model_id,session_id,state,facts)
         SELECT n,'responses',2,3,'fixture','','model','','completed','{}' FROM records;
         WITH RECURSIVE records(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM records WHERE n<20000)
         INSERT INTO usage_events(id,observed_at,category,owner,model,service_tier,operation,rates,origin,usage_turn,route_model,match_key)
         SELECT n,2,'external','fixture','model','','responses','{}','realtime','','model','' FROM records;",
    ).unwrap();
    drop(database);
    fixture.run_candidate(async |fixture, address| {
        let session = fixture.session();
        for (path, message) in [
            ("/api/usage?start=1&end=10", "Usage history is unavailable"),
            ("/api/calls?start=1&end=10", "Call history is unavailable"),
        ] {
            let wire = request(fixture.port(), "GET", path, &[&session], b"");
            let reply = assert_wire_parity(fixture.legacy.local_addr(), address, &wire, true).await;
            assert_status(&reply, 503);
            assert_eq!(json_body(&reply)["error"]["message"], message);
            fixture.parity(address, &wire, 200).await;
        }
    });
}

#[test]
fn identity_rejections_stop_before_fake_upstream_and_body() {
    Fixture::new().run(async |fixture, new| {
        let port = fixture.port();
        let session = fixture.session();
        let common = [
            "Content-Type: application/json",
            "Content-Length: 900000000",
            "Expect: 100-continue",
        ];
        for (extra, status) in [
            (vec![], 401),
            (vec![session.as_str(), session.as_str()], 401),
            (vec![session.as_str(), "Origin: https://other.invalid"], 403),
            (vec![session.as_str(), "Host: other.invalid"], 403),
            (
                vec![session.as_str(), "Host: localhost", "Host: localhost"],
                403,
            ),
            (vec!["Content-Length: -1"], 401),
            (vec!["Transfer-Encoding: chunked"], 401),
        ] {
            let headers = [common.as_slice(), &extra].concat();
            fixture
                .parity(
                    new,
                    &request(port, "POST", "/api/language", &headers, b""),
                    status,
                )
                .await;
        }
        for length in [
            "Content-Length: -1",
            "Content-Length: invalid",
            "Content-Length: 42",
        ] {
            let slow = ["Content-Type: application/json", length];
            let wire = request(port, "POST", "/api/language", &slow, b"{");
            timeout(Duration::from_secs(2), fixture.parity(new, &wire, 401))
                .await
                .expect("auth rejection precedes the body idle deadline");
        }
        let expired = emp_state::load_or_create_web_session(
            &fixture.directory.path().join("expired-session.json"),
            system_now() - 2.0 * emp_state::WEB_SESSION_LIFETIME_SECONDS,
        )
        .unwrap();
        let expired_header = format!("X-EMP-Session: {}", expired.token());
        *fixture.legacy.state.sessions.session.lock().unwrap() = expired;
        fixture
            .parity(
                new,
                &json_request(port, "/api/language", &[&expired_header], b"{}"),
                401,
            )
            .await;
        assert_eq!(fixture.upstream.load(Ordering::Relaxed), 0);

        let current = emp_state::load_or_create_web_session(
            &fixture.legacy.state.sessions.path,
            system_now(),
        )
        .unwrap();
        *fixture.legacy.state.sessions.session.lock().unwrap() = current;
        let reply = exchange_wire(
            new,
            &json_request(
                port,
                "/api/language",
                &[&session],
                br#"{"language":"zh-CN"}"#,
            ),
            false,
        )
        .await;
        assert_status(&reply, 200);
        assert_eq!(fixture.upstream.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn proxy_identity_rebuilds_headers_and_requires_live_delegation() {
    Fixture::new().run(async |fixture, new| {
        let port = fixture.port();
        let spoof = [
            "X-EMP-Caller-Id: forged",
            "x-emp-caller-name: forged",
            "X-EMP-Peer-IP: 192.0.2.1",
            "X-EMP-Caller-Extra: forged",
        ];
        for path in [
            "/v1/responses",
            "/v1/images/generations",
            "/v1/alpha/search",
        ] {
            fixture
                .parity(new, &json_request(port, path, &spoof, b"{}"), 401)
                .await;
        }
        let ws = [spoof.as_slice(), &["Upgrade: websocket"]].concat();
        fixture
            .parity(new, &request(port, "GET", "/v1/responses", &ws, b""), 401)
            .await;
        assert_eq!(fixture.upstream.load(Ordering::Relaxed), 0);

        let delegation = fixture
            .legacy
            .state
            .backend
            .aero_callers
            .delegate(
                Caller {
                    id: "trusted".into(),
                    name: "Trusted Caller".into(),
                },
                Some("Bearer restored".into()),
            )
            .unwrap();
        let auth = format!("Authorization: Bearer {}", delegation.key);
        let headers = [spoof.as_slice(), &[auth.as_str()]].concat();
        let wire = json_request(port, "/v1/responses", &headers, br#"{"new_field":1}"#);
        let reply = exchange_wire(new, &wire, false).await;
        assert_status(&reply, 200);
        let body = json_body(&reply);
        assert_eq!(body["id"], "trusted");
        assert_eq!(body["headers"]["x-emp-caller-id"], "trusted");
        assert_eq!(body["headers"]["x-emp-caller-name"], "Trusted Caller");
        assert_eq!(body["headers"]["x-emp-peer-ip"], "127.0.0.1");
        assert!(body["headers"].get("x-emp-caller-extra").is_none());
        assert_eq!(body["headers"]["authorization"], "Bearer restored");
        assert_eq!(body["body"]["new_field"], 1);

        drop(delegation);
        fixture.parity(new, &wire, 401).await;
        assert_eq!(fixture.upstream.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn public_origins_and_models_keep_their_route_policies() {
    Fixture::new().run(async |fixture, new| {
        for (method, path, message) in [
            ("HEAD", "/", "cross-origin Web UI request rejected"),
            (
                "GET",
                "/assets/app-icon.png",
                "cross-origin Web UI request rejected",
            ),
            (
                "GET",
                "/guide/guide.css",
                "cross-origin guide request rejected",
            ),
            ("DELETE", "/healthz", "cross-origin health request rejected"),
            (
                "GET",
                "/v1/models",
                "cross-origin model catalog request rejected",
            ),
        ] {
            let received = fixture
                .parity(
                    new,
                    &request(
                        fixture.port(),
                        method,
                        path,
                        &["Host: outside.invalid"],
                        b"",
                    ),
                    403,
                )
                .await;
            assert_eq!(json_body(&received)["error"]["message"], message);
        }
        let local = request(fixture.port(), "POST", "/healthz", &[], b"");
        fixture.parity(new, &local, 200).await;
        let local = request(fixture.port(), "GET", "/v1/models", &[], b"");
        assert_status(&exchange_wire(new, &local, false).await, 200);

        let auth = format!(
            "Authorization: Bearer {}",
            fixture.legacy.state.backend.gateway_key
        );
        let outside = request(
            fixture.port(),
            "GET",
            "/v1/models",
            &["Host: outside.invalid", &auth],
            b"",
        );
        assert_status(
            &exchange_wire(fixture.legacy.local_addr(), &outside, false).await,
            200,
        );
        assert_status(&exchange_wire(new, &outside, false).await, 200);
    });
}

#[test]
fn route_access_precedes_identity_and_preserves_management_exceptions() {
    Fixture::new().run(async |fixture, new| {
        let decisions = request(
            fixture.port(),
            "POST",
            "/v1/decisions",
            &["Content-Length: -1"],
            b"",
        );
        let reply = fixture.parity(new, &decisions, 401).await;
        assert_eq!(
            json_body(&reply)["error"]["message"],
            "EMP gateway authentication is required"
        );
        fixture
            .legacy
            .state
            .backend
            .modules
            .mark_unavailable(emp_modules::Module::Decision, "test");
        let reply = fixture.parity(new, &decisions, 404).await;
        assert_eq!(json_body(&reply)["error"]["code"], "module_disabled");
        assert_eq!(fixture.upstream.load(Ordering::Relaxed), 0);

        let modules = request(
            fixture.port(),
            "POST",
            "/api/modules",
            &["Origin: https://outside.invalid", &fixture.session()],
            b"",
        );
        let reply = fixture.parity(new, &modules, 401).await;
        assert_eq!(
            json_body(&reply)["error"]["message"],
            "management session is required"
        );
    });
}

fn gzip(payload: &[u8], comment: Vec<u8>) -> Vec<u8> {
    let mut encoder = GzBuilder::new()
        .comment(comment)
        .write(Vec::new(), Compression::fast());
    encoder.write_all(payload).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn json_errors_match_wire_for_limits_encodings_and_framing() {
    Fixture::new().run(async |fixture, new| {
        const MAX: usize = 5 * 1024 * 1024;
        let port = fixture.port();
        let session = fixture.session();
        let headers = [session.as_str(), "Content-Type: application/json"];
        for (extra, status, message) in [
            (vec!["Content-Length: 5242881"], 413, "too large"),
            (vec!["Content-Length: -1"], 400, "cannot be negative"),
            (
                vec!["Content-Length: invalid"],
                400,
                "invalid Content-Length",
            ),
            (
                vec!["Content-Length: 2", "content-length: 2"],
                400,
                "duplicate Content-Length",
            ),
            (
                vec!["Transfer-Encoding: chunked"],
                400,
                "unsupported Transfer-Encoding",
            ),
        ] {
            let reply = fixture
                .parity(
                    new,
                    &request(
                        port,
                        "POST",
                        "/api/language",
                        &[headers.as_slice(), &extra].concat(),
                        b"",
                    ),
                    status,
                )
                .await;
            assert!(
                json_body(&reply)["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(message)
            );
        }
        let declared = format!(
            "Content-Length: {}",
            RequestLimitsConfig::default().maximum + 1
        );
        let reply = fixture
            .parity(
                new,
                &request(
                    port,
                    "POST",
                    "/v1/responses",
                    &[headers.as_slice(), &[&declared]].concat(),
                    b"",
                ),
                413,
            )
            .await;
        assert_eq!(json_body(&reply)["error"]["code"], "request_too_large");

        let gzip_json = gzip(b"{}", Vec::new());
        let mut truncated = gzip_json.clone();
        truncated.truncate(truncated.len() - 8);
        let encoded = |inner: &[u8]| emp_transport::zstd_encode(inner).unwrap();
        let intermediate_large = gzip(b"{}", vec![b'a'; MAX]);
        let final_large = gzip(&vec![b' '; MAX + 1], Vec::new());
        for (encoding, body, status, message) in [
            ("br", b"{}".to_vec(), 400, "unsupported Content-Encoding"),
            ("gzip", truncated, 400, "invalid compressed request body"),
            (
                "gzip, zstd",
                encoded(&intermediate_large),
                413,
                "decoded request body is too large",
            ),
            (
                "gzip, zstd",
                encoded(&final_large),
                413,
                "decoded request body is too large",
            ),
            (
                "gzip, zstd",
                encoded(&gzip_json),
                400,
                "language is required",
            ),
            (
                "identity",
                b"[]".to_vec(),
                400,
                "request body must be a JSON object",
            ),
        ] {
            let content_encoding = format!("Content-Encoding: {encoding}");
            let reply = fixture
                .parity(
                    new,
                    &json_request(port, "/api/language", &[&session, &content_encoding], &body),
                    status,
                )
                .await;
            assert!(
                json_body(&reply)["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(message)
            );
        }
        let padded = request(
            port,
            "POST",
            "/api/language",
            &[headers.as_slice(), &["Content-Length: +2"]].concat(),
            b"{}ignored",
        );
        let reply = fixture.parity(new, &padded, 400).await;
        assert_eq!(
            json_body(&reply)["error"]["message"],
            "language is required"
        );
        let incomplete = request(
            port,
            "POST",
            "/api/language",
            &[headers.as_slice(), &["Content-Length: 2"]].concat(),
            b"{",
        );
        let reply = assert_wire_parity(fixture.legacy.local_addr(), new, &incomplete, true).await;
        assert_eq!(
            json_body(&reply)["error"]["message"],
            "request body is incomplete"
        );

        let timed_out = request(
            port,
            "POST",
            "/api/language",
            &[headers.as_slice(), &["Content-Length: 2"]].concat(),
            b"{",
        );
        let reply = fixture.parity(new, &timed_out, 408).await;
        assert_eq!(json_body(&reply)["error"]["code"], "request_timeout");
    });
}

#[test]
fn insufficient_body_budget_matches_legacy_before_read() {
    Fixture::new().run(async |fixture, _| {
        let limits = RequestLimits::new(
            RequestLimitsConfig {
                baseline: 64,
                maximum: 256,
                growth_quantum: 64,
                memory_headroom: 0,
            },
            || Some(MemoryStatus::available(0)),
            || 0,
            "H03-memory",
        )
        .unwrap();
        let legacy = TcpListener::bind(LOOPBACK).unwrap();
        let legacy_address = legacy.local_addr().unwrap();
        let legacy_limits = limits.clone();
        let presentation = ErrorPresentation::new(&fixture.legacy.state);
        let worker = tokio::task::spawn_blocking(move || {
            let (mut stream, _) = legacy.accept().unwrap();
            let raw = read_request_head(&mut stream).unwrap();
            let parsed = parse_request(&raw.head).unwrap();
            let error =
                read_json_body_with_limits(&mut stream, parsed, raw.body_prefix, &legacy_limits)
                    .expect_err("body budget rejects request");
            let wire = body_error_response(error);
            stream
                .write_all(&presentation.http(&wire).unwrap_or(wire))
                .unwrap();
        });
        let router = with_entry_state(
            Router::new().route(
                "/v1/responses",
                post(move |_: ProxyCaller, request: Request| async move {
                    JsonBody::read(request, &limits)
                        .await
                        .map(|_| "unexpected upstream")
                }),
            ),
            fixture.legacy.state.clone(),
        );
        let runtime = ServerHandle::bind(LOOPBACK, router, admission())
            .await
            .unwrap();
        let wire = request(
            fixture.port(),
            "POST",
            "/v1/responses",
            &[
                &fixture.session(),
                "Content-Type: application/json",
                "Content-Length: 65",
            ],
            b"",
        );
        let reply = assert_wire_parity(legacy_address, runtime.local_addr(), &wire, false).await;
        assert_status(&reply, 503);
        assert!(String::from_utf8_lossy(&reply).contains("\r\nRetry-After: 2\r\n"));
        assert_eq!(json_body(&reply)["error"]["memory"]["available_bytes"], 0);
        worker.await.unwrap();
        runtime.shutdown().await.unwrap();
    });
}

#[test]
fn bootstrap_and_export_confirmations_are_consumed_once() {
    Fixture::new().run(async |fixture, new| {
        let port = fixture.port();
        let original_session = fixture.session();
        let header = format!("X-EMP-Bootstrap: {}", fixture.legacy.state.bootstrap.token);
        let duplicate = request(port, "POST", "/api/session", &[&header, &header], b"");
        fixture.parity(new, &duplicate, 401).await;
        let bootstrap = request(port, "POST", "/api/session", &[&header], b"");
        let accepted = exchange_wire(new, &bootstrap, false).await;
        assert_status(&accepted, 200);
        assert!(
            String::from_utf8_lossy(&accepted)
                .contains(crate::http::auth::CLEAR_LEGACY_SESSION_COOKIE)
        );
        fixture.parity(new, &bootstrap, 401).await;
        assert_ne!(fixture.session(), original_session);
        let invalidated = json_request(port, "/api/language", &[&original_session], b"{}");
        fixture.parity(new, &invalidated, 401).await;

        let session = fixture.session();
        let issue = json_request(port, "/api/migration/export/confirm", &[&session], b"{}");
        for first_is_valid in [true, false] {
            let issued = exchange_wire(new, &issue, false).await;
            assert_status(&issued, 200);
            let good = json_body(&issued)["confirmation"]
                .as_str()
                .unwrap()
                .to_owned();
            let export = |value: &str| {
                json_request(
                    port,
                    "/api/migration/export",
                    &[&session],
                    json!({"confirmation":value}).to_string().as_bytes(),
                )
            };
            if first_is_valid {
                assert_status(&exchange_wire(new, &export(&good), false).await, 200);
            } else {
                let rejected = exchange_wire(new, &export("wrong"), false).await;
                assert_status(&rejected, 403);
            }
            let rejected = fixture.parity(new, &export(&good), 403).await;
            assert_eq!(
                json_body(&rejected)["error"]["code"],
                "export_confirmation_required"
            );
        }
    });
}

#[test]
fn realtime_uses_its_own_auth_and_multipart_errors() {
    Fixture::new().run(async |fixture, new| {
        let port = fixture.port();
        let unauthenticated = request(
            port,
            "POST",
            "/v1/live",
            &["Content-Length: 9", "content-length: 8"],
            b"",
        );
        let reply = fixture.parity(new, &unauthenticated, 401).await;
        assert_eq!(
            json_body(&reply)["error"]["code"],
            "native_subscription_unavailable"
        );

        let sideband = request(port, "GET", "/v1/live/rtc_demo", &[], b"");
        let reply = fixture.parity(new, &sideband, 401).await;
        assert_eq!(
            json_body(&reply)["error"]["code"],
            "realtime_caller_unauthorized"
        );

        let session = fixture.session();
        let content_type = "Content-Type: multipart/form-data; boundary=EMP";
        for (headers, status, code) in [
            (
                vec!["Content-Length: 9", "content-length: 8"],
                400,
                "realtime_invalid_request",
            ),
            (
                vec!["Transfer-Encoding: chunked"],
                400,
                "realtime_invalid_request",
            ),
            (
                vec!["Content-Encoding: gzip"],
                415,
                "realtime_unsupported_encoding",
            ),
            (vec![], 411, "realtime_length_required"),
            (
                vec!["Content-Length: 262145"],
                413,
                "realtime_request_too_large",
            ),
        ] {
            let wire = request(
                port,
                "POST",
                "/v1/live",
                &[&[session.as_str(), content_type], headers.as_slice()].concat(),
                b"",
            );
            let reply = fixture.parity(new, &wire, status).await;
            assert_eq!(json_body(&reply)["error"]["code"], code);
        }
    });
}

#[test]
fn recovery_keeps_flat_errors_and_auth_precedes_empty_body_policy() {
    Fixture::new().run(async |fixture, _| {
        let legacy = TcpListener::bind(LOOPBACK).unwrap();
        let old = legacy.local_addr().unwrap();
        let config = fixture.directory.path().join("config.json");
        let worker = std::thread::spawn(move || {
            crate::startup_recovery::serve_for_test(legacy, &config, "recovery-fixture").unwrap()
        });
        let port = old.port();
        let state = RecoveryAccess {
            port,
            token: Arc::from("recovery-fixture"),
        };
        let router = Router::new()
            .route("/", get(|_: RecoveryOrigin| async { "recovery" }))
            .route(
                "/api/recovery",
                get(|_: RecoverySession| async { "private recovery" }),
            )
            .route(
                "/api/recovery/reset",
                post(|_: RecoveryMutation| async { "confirmed" }),
            )
            .route(
                "/api/recovery/exit",
                post(|_: RecoveryMutation| async {
                    JsonReply::new(200, br#"{"exiting":true}"#.to_vec())
                }),
            )
            .with_state(state);
        let server = ServerHandle::bind_recovery(LOOPBACK, router, admission())
            .await
            .unwrap();
        for (method, path, headers, status, code) in [
            (
                "GET",
                "/",
                vec!["Host: outside.invalid"],
                403,
                "origin_rejected",
            ),
            (
                "GET",
                "/api/recovery",
                vec![],
                401,
                "recovery_session_required",
            ),
            (
                "POST",
                "/api/recovery/reset",
                vec!["Content-Length: 999"],
                401,
                "recovery_session_required",
            ),
            (
                "POST",
                "/api/recovery/reset",
                vec!["X-EMP-Recovery: recovery-fixture", "Content-Length: 999"],
                400,
                "invalid_request",
            ),
            (
                "POST",
                "/api/recovery/reset",
                vec!["X-EMP-Recovery: recovery-fixture", "Content-Length: 0"],
                409,
                "confirmation_required",
            ),
        ] {
            let wire = request(port, method, path, &headers, b"");
            let reply = assert_wire_parity(old, server.local_addr(), &wire, false).await;
            assert_status(&reply, status);
            assert_eq!(json_body(&reply), json!({"error":code}));
        }
        let malformed =
            assert_wire_parity(old, server.local_addr(), b"PUT / HTTP/1.1\r\n\r\n", false).await;
        assert!(malformed.is_empty());
        let exit = request(
            port,
            "POST",
            "/api/recovery/exit",
            &["X-EMP-Recovery: recovery-fixture", "Content-Length: 0"],
            b"",
        );
        assert_status(
            &assert_wire_parity(old, server.local_addr(), &exit, false).await,
            200,
        );
        assert!(!worker.join().unwrap());
        server.shutdown().await.unwrap();
    });
}

#[test]
fn hyper_parse_rejection_uses_emp_response_before_dispatch() {
    Fixture::new().run(async |fixture, new| {
        let legacy_accepts = request(
            fixture.port(),
            "GET",
            "/healthz",
            &["Invalid Header: x"],
            b"",
        );
        assert_status(
            &exchange_wire(fixture.legacy.local_addr(), &legacy_accepts, false).await,
            200,
        );
        let reply = exchange_wire(new, &legacy_accepts, false).await;
        assert_status(&reply, 400);
        assert_eq!(reply, crate::http::response::bad_request_response());
        assert_eq!(fixture.upstream.load(Ordering::Relaxed), 0);
    });
}

#[test]
fn body_total_timeout_includes_upload_grace_without_resetting_on_progress() {
    use crate::server::{ServerControl, connection::drive, tests::poll_once};
    use std::pin::pin;
    use tokio::{
        io::duplex,
        time::{Instant, advance, pause},
    };

    Fixture::new().run(async |fixture, _| {
        pause();
        let admission = admission();
        let control = Arc::new(ServerControl::new(admission.clone()));
        let permit = admission.acquire_request().unwrap();
        let (mut client, io) = duplex(4096);
        let started = Instant::now();
        let wire = request(
            fixture.port(),
            "POST",
            "/api/language",
            &[
                &fixture.session(),
                "Content-Type: application/json",
                "Content-Length: 1048576",
            ],
            b"{",
        );
        client.write_all(&wire).await.unwrap();
        let mut driver = pin!(drive(
            io,
            LOOPBACK,
            started,
            fixture.router(),
            control,
            permit
        ));
        assert!(poll_once(driver.as_mut()).await.is_pending());
        for _ in 0..30 {
            advance(Duration::from_secs(4)).await;
            client.write_all(b" ").await.unwrap();
            assert!(poll_once(driver.as_mut()).await.is_pending());
        }
        advance(Duration::from_millis(999)).await;
        assert!(poll_once(driver.as_mut()).await.is_pending());
        advance(Duration::from_millis(1)).await;
        driver.await;
        assert!(
            (Duration::from_secs(121)..=Duration::from_millis(121_001))
                .contains(&started.elapsed())
        );
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await.unwrap();
        assert_status(&reply, 408);
        assert_eq!(json_body(&reply)["error"]["code"], "request_timeout");
    });
}
