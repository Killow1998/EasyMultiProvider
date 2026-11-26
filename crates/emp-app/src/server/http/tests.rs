use super::{
    body::{JsonBody, RealtimeBody},
    error::{EntryError, JsonReply},
    management::{
        ManagementSession, PageOrigin, RecoveryAccess, RecoveryMutation, RecoveryOrigin,
        RecoverySession, SESSION_PATH, consume_export_confirmation, issue_export_confirmation,
        session_route,
    },
    proxy::{ModelCatalog, ProxyCaller, RequestAccess},
    with_entry_state,
};
#[cfg(unix)]
use crate::server::raw_tcp::wire_timeout;
use crate::{
    lifecycle::ServerHandle as RunningServer,
    server::{
        ServerHandle,
        tests::{LOOPBACK, admission},
    },
    services::gateway_clients::Caller,
    util::system_now,
};
use axum::{
    Extension, Router,
    extract::{Request, State},
    routing::{any, get, post},
};
use emp_test_support::{
    Deadline,
    http::{Body, ChunkDecoder},
    test_timeout,
};
use emp_transport::{MemoryStatus, RequestLimits, RequestLimitsConfig};
use flate2::{Compression, GzBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(unix)]
use std::net::TcpListener;
use std::{
    io::Write,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

pub(crate) async fn exchange_wire(address: SocketAddr, request: &[u8], end_input: bool) -> Vec<u8> {
    exchange_before(
        address,
        request,
        end_input,
        &Deadline::after(test_timeout(Duration::from_secs(15))),
    )
    .await
}

async fn exchange_before(
    address: SocketAddr,
    request: &[u8],
    end_input: bool,
    deadline: &Deadline,
) -> Vec<u8> {
    let reply = timeout(deadline.remaining(), async {
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
    .expect("complete HTTP response");
    deadline.accept(reply, "complete HTTP response").unwrap()
}

#[cfg(unix)]
async fn connect_submitted_write(fixture: &Fixture) -> (TcpStream, tokio::task::JoinHandle<()>) {
    connect_wire_from(&fixture.server.state, None).await
}

#[cfg(unix)]
pub(crate) async fn connect_wire_from(
    state: &Arc<crate::app::ServerState>,
    peer_ip: Option<std::net::IpAddr>,
) -> (TcpStream, tokio::task::JoinHandle<()>) {
    use crate::server::{connection::drive_http, http_router, transport::ConnectionIo};

    let listener = tokio::net::TcpListener::bind(LOOPBACK).await.unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (stream, mut peer) = listener.accept().await.unwrap();
    if let Some(ip) = peer_ip {
        peer.set_ip(ip);
    }
    let admission = &state.lifecycle.admission;
    let task = tokio::spawn(drive_http(
        ConnectionIo::new(stream),
        peer,
        tokio::time::Instant::now(),
        http_router(state.clone()),
        state.clone(),
        super::proxy::ConnectionIntake::Public(admission.acquire_request().unwrap()),
    ));
    (client, task)
}

#[cfg(unix)]
async fn disconnect_submitted_write(
    fixture: &Fixture,
    client: TcpStream,
    task: tokio::task::JoinHandle<()>,
) {
    client.set_zero_linger().unwrap();
    drop(client);
    {
        task.abort();
        if let Err(error) = task.await {
            assert!(error.is_cancelled(), "{error}");
        }
        let state = &fixture.server.state;
        let blocked = state.lifecycle.updates().drain_blocked_for_test();
        let quiet = state
            .lifecycle
            .admission
            .quiesce(0, Duration::ZERO)
            .is_some();
        assert_eq!(
            (blocked, quiet),
            (true, false),
            "write still blocked after disconnect: (update drain blocked, connections quiet)"
        );
    }
}

#[cfg(unix)]
mod account_writes;
#[cfg(unix)]
mod catalog_writes;
mod drain;
#[cfg(unix)]
mod inference;
mod lifecycle;
#[cfg(target_os = "linux")]
mod local_writes;
#[cfg(unix)]
mod login_writes;
#[cfg(unix)]
mod management_streams;
#[cfg(unix)]
mod migration_writes;
#[cfg(unix)]
mod module_writes;
mod report_fixture;
#[cfg(unix)]
mod team_writes;
#[cfg(unix)]
mod websocket;
pub(crate) mod wire_contract;

pub(crate) async fn assert_wire(
    address: SocketAddr,
    request: &[u8],
    end_input: bool,
    status: u16,
) -> Vec<u8> {
    let reply = exchange_wire(address, request, end_input).await;
    assert_status(&reply, status);
    reply
}

pub(crate) async fn assert_entry_error(address: SocketAddr, wire: &[u8], status: u16) {
    let (status_line, message) = match status {
        401 => ("401 Unauthorized", "management session is required"),
        403 => ("403 Forbidden", "management session is required"),
        404 => ("404 Not Found", "not found"),
        _ => panic!("unexpected entry rejection: {status}"),
    };
    let reply = exchange_wire(address, wire, false).await;
    wire_contract::assert_json_reply(
        str::from_utf8(wire).unwrap().lines().next().unwrap(),
        &reply,
        status_line,
        &json!({"error":{"message":message}}),
    );
}

pub(crate) async fn assert_sse_wire(address: SocketAddr, request: &[u8]) -> Vec<u8> {
    decode_report_wire(assert_wire(address, request, false, 200).await)
}

fn decode_report_wire(wire: Vec<u8>) -> Vec<u8> {
    let split = wire
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap();
    let head = std::str::from_utf8(&wire[..split]).unwrap();
    let (status, headers) = head.split_once("\r\n").unwrap();
    let mut headers: Vec<_> = headers.split("\r\n").collect();
    let chunked = headers
        .iter()
        .position(|header| header.eq_ignore_ascii_case("Transfer-Encoding: chunked"));
    if let Some(index) = chunked {
        headers.remove(index);
    }
    headers.sort_unstable();
    let mut decoded = format!("{status}\r\n{}\r\n\r\n", headers.join("\r\n")).into_bytes();
    assert!(chunked.is_some(), "missing chunked transfer encoding");
    let mut body = Body::new(&wire[split + 4..], true);
    std::io::Read::read_to_end(&mut body, &mut decoded).unwrap();
    assert!(body.get_mut().is_empty());
    decoded
}

pub(crate) async fn read_chunk_data(
    io: &mut (impl AsyncBufRead + Unpin),
    decoder: &mut ChunkDecoder,
    output: &mut Vec<u8>,
) -> std::io::Result<()> {
    while !decoder.is_finished() {
        let mut input = io.fill_buf().await?;
        if input.is_empty() {
            return decoder.finish();
        }
        let length = input.len();
        let body = decoder.decode(&mut input)?;
        let delivered = !body.is_empty();
        output.extend_from_slice(body);
        let consumed = length - input.len();
        io.consume(consumed);
        if delivered {
            break;
        }
    }
    Ok(())
}

async fn stop_at_chunk_boundary(
    io: &mut (impl AsyncBufRead + Unpin),
    decoder: &mut ChunkDecoder,
) -> std::io::Result<()> {
    while decoder.at_chunk_ending() {
        let mut input = io.fill_buf().await?;
        if input.is_empty() {
            return decoder.finish();
        }
        let length = input.len();
        decoder.stop_at_chunk_boundary(&mut input)?;
        let consumed = length - input.len();
        io.consume(consumed);
    }
    Ok(())
}

#[cfg(unix)]
async fn assert_wire_with<T>(
    address: SocketAddr,
    request: &[u8],
    end_input: bool,
    status: u16,
    check_reply: impl FnOnce(Vec<u8>) -> T,
) -> T {
    check_reply(assert_wire(address, request, end_input, status).await)
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

#[cfg(unix)]
fn shutdown_with_stalled_response(
    server: RunningServer,
    slow: crate::server::raw_tcp::RawClient,
    clock: &crate::server::tests::TestDrainClock,
) -> Result<(), crate::error::AppError> {
    let control = server.state.lifecycle.clone();
    let (complete, finished) = std::sync::mpsc::channel();
    let shutdown = std::thread::spawn(move || complete.send(server.shutdown()).unwrap());
    let deadline = Deadline::after(crate::server::raw_tcp::wire_timeout());
    let ready = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            timeout(deadline.remaining(), async {
                control.signal().wait().await;
                control.admission.wait_for_business_idle().await;
            })
            .await
        });
    let pending_at_cutoff = control.admission.active_requests() > 0
        && matches!(
            finished.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );
    clock.expire();
    let result = finished.recv_timeout(deadline.remaining());
    drop(slow);
    shutdown.join().unwrap();
    ready.expect("shutdown reached the sending stage");
    assert!(
        pending_at_cutoff,
        "the slow response must still be pending before cutoff"
    );
    result.expect("shutdown completed with the slow client still connected")
}

struct Fixture {
    directory: tempfile::TempDir,
    server: RunningServer,
    upstream: Arc<AtomicUsize>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_directory(emp_test_support::fixture_dir().unwrap())
    }

    fn with_directory(directory: tempfile::TempDir) -> Self {
        let root = emp_state::config::resolve_user_path(directory.path());
        let server = RunningServer::start_for_test(
            LOOPBACK.ip(),
            &root.join("config.json"),
            root.join("codex/auth.json"),
        )
        .unwrap();
        Self {
            directory,
            server,
            upstream: Arc::default(),
        }
    }

    fn run_wire<T>(self, test: impl AsyncFnOnce(&Self, SocketAddr) -> T) -> T {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(test(&self, self.server.local_addr()));
        self.server.shutdown().unwrap();
        result
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
        self.server.shutdown().unwrap();
    }

    #[cfg(unix)]
    async fn flushed_journal(&self) -> Vec<Value> {
        let root = self.directory.path().to_owned();
        let state = self.server.state.clone();
        tokio::task::spawn_blocking(move || crate::tests::flushed_journal(&root, &state))
            .await
            .unwrap()
    }

    fn router(&self) -> Router {
        let management_calls = self.upstream.clone();
        let proxy_calls = self.upstream.clone();
        let router = Router::new()
            .route(SESSION_PATH, session_route())
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
        with_entry_state(router, self.server.state.clone())
    }

    fn port(&self) -> u16 {
        self.server.local_addr().port()
    }

    fn session(&self) -> String {
        format!("X-EMP-Session: {}", self.server.session_token())
    }
}

#[test]
fn update_module_and_gateway_reads_keep_json_and_auth() {
    let fixture = Fixture::new();
    let backend = &fixture.server.state.backend;
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
        ("/api/updates", 1),
        ("/api/modules", 2),
        ("/api/gateway", 2),
        ("/api/gateway/clients", 2),
        ("/api/gateway/usage?group=device&start=0&end=10", 2),
        ("/api/decisions/connection", 2),
    ];
    let bearer = format!("Authorization: Bearer {}", backend.gateway_key);
    fixture.run_wire(async |fixture, address| {
        let session = fixture.session();
        let port = fixture.port();
        for (path, cache_headers) in routes {
            let wire = request(
                port,
                "GET",
                path,
                &[&session, "Accept: text/event-stream"],
                b"",
            );
            let reply = exchange_wire(address, &wire, true).await;
            let extra = (cache_headers == 2).then_some("Cache-Control: no-store");
            let body = wire_contract::assert_json_read(path, &reply, extra);
            match path {
                "/api/updates" => {
                    assert_eq!(body["current_version"], env!("CARGO_PKG_VERSION"));
                    assert_eq!(body["state"], "idle");
                    assert_eq!(body.get("latest_version"), Some(&Value::Null));
                }
                "/api/modules" => {
                    let modules = body["modules"].as_array().unwrap();
                    let ids = modules
                        .iter()
                        .map(|module| module["id"].as_str().unwrap())
                        .collect::<Vec<_>>();
                    assert_eq!(ids, ["decision", "gateway", "aggregate", "team", "aero"]);
                    assert!(
                        modules
                            .iter()
                            .all(|module| module["enabled"] == false && module["active"] == false)
                    );
                    assert_eq!(body["migration_allowed"], true);
                }
                "/api/gateway/clients" => {
                    let clients = body["clients"].as_array().unwrap();
                    assert_eq!(clients.len(), 1);
                    assert_eq!(clients[0]["id"], device["id"]);
                    assert_eq!(clients[0]["name"], "Parity device");
                    assert_eq!(clients[0]["api_key"], device["api_key"]);
                    assert!(clients[0]["created_at"].as_f64().unwrap() > 0.0);
                }
                "/api/gateway/usage?group=device&start=0&end=10" => assert_eq!(
                    body["rows"],
                    json!([{
                        "caller_id":device["id"], "caller_name":"Parity device", "calls":0,
                        "input_tokens":0, "output_tokens":0, "cost_nanos":0,
                        "reported_calls":0, "priced_calls":0, "models":[]
                    }])
                ),
                _ => assert_eq!(
                    body,
                    json!({"api_key": fixture.server.state.backend.gateway_key})
                ),
            }
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
                assert_entry_error(address, &request(port, method, path, headers, b""), status)
                    .await;
            }
        }
        let reply = assert_wire(
            address,
            &request(
                port,
                "GET",
                "/api/gateway/usage?start=invalid",
                &[&session],
                b"",
            ),
            false,
            400,
        )
        .await;
        wire_contract::assert_json_reply(
            "invalid gateway usage period",
            &reply,
            "400 Bad Request",
            &json!({"error":{"message":"Invalid report period","origin":"emp"}}),
        );
    });
}

#[test]
fn gateway_and_decision_module_gates_precede_management_authentication() {
    let fixture = Fixture::new();
    let modules = &fixture.server.state.backend.modules;
    modules.mark_unavailable(emp_modules::Module::Gateway, "test");
    modules.mark_unavailable(emp_modules::Module::Decision, "test");
    fixture.run_wire(async |fixture, address| {
        for path in [
            "/api/gateway",
            "/api/gateway/clients",
            "/api/gateway/usage?start=invalid",
            "/api/decisions/connection",
        ] {
            let reply = assert_wire(
                address,
                &request(
                    fixture.port(),
                    "GET",
                    path,
                    &["Origin: https://foreign.invalid"],
                    b"",
                ),
                false,
                404,
            )
            .await;
            wire_contract::assert_json_reply(
                path, &reply, "404 Not Found",
                &json!({"error":{"code":"module_disabled","message":"This EMP module is not active","origin":"emp"}}),
            );
        }
    });
}

#[cfg(unix)]
fn install_panel_fixture(
    directory: &std::path::Path,
    module: emp_modules::Module,
    panel: &str,
) -> String {
    install_worker_fixture(directory, module, panel, "while read -r line; do :; done")
}

#[cfg(unix)]
fn install_worker_fixture(
    directory: &std::path::Path,
    module: emp_modules::Module,
    panel: &str,
    handler: &str,
) -> String {
    use emp_modules::{HOST_API, Hello, PROTOCOL, Reply};
    use sha2::{Digest, Sha256};
    use std::{fs, os::unix::fs::PermissionsExt};

    let modules = directory.join("state/modules");
    fs::create_dir_all(&modules).unwrap();
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
        "#!/bin/sh\nprintf '%s\\n' '{hello}'\nread -r line\nprintf '%s\\n' '{reply}'\n{handler}\n"
    );
    let revision = format!("{:x}", Sha256::digest(&worker));
    let executable = modules.join(format!("{}-{revision}", module.id()));
    fs::write(&executable, worker).unwrap();
    fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(modules.join("modules.json"), json!({
        (module.id()):{"enabled":true,"versions":{},"artifact":{"digest":revision,"version":module.version()}}
    }).to_string()).unwrap();
    revision
}

#[test]
fn pages_and_health_keep_origin_and_method_policy() {
    Fixture::new().run_wire(async |fixture, address| {
        let port = fixture.port();
        let foreign = ["Origin: https://foreign.invalid"];
        for (path, content_type, body, error) in [
            (
                "/",
                "text/html; charset=utf-8",
                crate::WEB_INDEX_BYTES,
                "cross-origin Web UI request rejected",
            ),
            (
                "/index.html?bootstrap=fixture",
                "text/html; charset=utf-8",
                crate::WEB_INDEX_BYTES,
                "cross-origin Web UI request rejected",
            ),
            (
                "/healthz",
                "application/json",
                b"{\"status\":\"ok\"}".as_slice(),
                "cross-origin health request rejected",
            ),
        ] {
            for method in ["GET", "HEAD"] {
                let reply = assert_wire(
                    address,
                    &request(port, method, path, &["Accept: text/event-stream"], b""),
                    false,
                    200,
                )
                .await;
                assert_page_reply(path, &reply, content_type, body);
                let denied = assert_wire(
                    address,
                    &request(port, method, path, &foreign, b""),
                    false,
                    403,
                )
                .await;
                wire_contract::assert_json_reply(
                    path,
                    &denied,
                    "403 Forbidden",
                    &json!({"error":{"message":error}}),
                );
            }
        }
    });
}

fn assert_page_reply(path: &str, reply: &[u8], content_type: &str, body: &[u8]) {
    let head = if content_type == "application/json" {
        format!(wire_contract::json_head!("200 OK", "{}"), body.len())
    } else {
        wire_contract::INDEX.replace("{}", &body.len().to_string())
    };
    wire_contract::assert_reply(path, reply, &head, body);
}

fn page_reply_parts<'a>(reply: &'a [u8], content_type: &str, cache: &str) -> (&'a str, &'a [u8]) {
    let offset = reply
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap()
        + 4;
    let (head, body) = reply.split_at(offset);
    let head = str::from_utf8(head).unwrap();
    assert!(head.contains(&format!("\r\nContent-Type: {content_type}\r\n")));
    assert!(head.contains(&format!("\r\nContent-Length: {}\r\n", body.len())));
    assert!(head.contains(&format!("\r\nCache-Control: {cache}\r\n")));
    assert!(head.contains("\r\nX-Frame-Options: DENY\r\n"));
    assert!(head.contains("\r\nContent-Security-Policy: frame-ancestors 'none'\r\n"));
    assert!(head.contains("\r\nX-Content-Type-Options: nosniff\r\n"));
    (head, body)
}

#[test]
fn guides_and_assets_keep_wire_contracts() {
    use base64::{Engine, engine::general_purpose::STANDARD};

    let directory = emp_test_support::fixture_dir().unwrap();
    let state = directory.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let icon = crate::services::app_icon::DEFAULT_PNG;
    let source_icon =
        include_bytes!("../../../../../assets/branding/native/easy-multi-provider-tray.png")
            .as_slice();
    std::fs::write(
        state.join("appearance.json"),
        json!({"preset":"custom","monochrome":false,
            "png":STANDARD.encode(icon),"original_png":STANDARD.encode(source_icon)})
        .to_string(),
    )
    .unwrap();
    Fixture::with_directory(directory).run_wire(async |fixture, address| {
        let port = fixture.port();
        let same_origin = format!("Origin: http://localhost:{port}");
        let foreign = ["Host: foreign.invalid"];
        let gif =
            include_bytes!("../../../../../docs/assets/tutorials/00-native-context.gif").as_slice();
        for (path, content_type, cache, body, denied_message) in [
            (
                "/guide/README.zh-CN.md",
                "text/html; charset=utf-8",
                "no-store",
                None,
                "cross-origin guide request rejected",
            ),
            (
                "/guide/docs/assets/tutorials/00-native-context.gif",
                "image/gif",
                "no-store",
                Some(gif),
                "cross-origin guide request rejected",
            ),
            (
                "/assets/tutorial-00-native-context.gif",
                "image/gif",
                "no-store",
                Some(gif),
                "cross-origin guide request rejected",
            ),
            (
                "/assets/guide.css",
                "text/css; charset=utf-8",
                "no-store",
                Some(include_bytes!("../../../web/guide.css").as_slice()),
                "cross-origin Web UI request rejected",
            ),
            (
                "/assets/i18n.js?v=1",
                "text/javascript; charset=utf-8",
                "no-store",
                Some(include_bytes!("../../../web/i18n.js").as_slice()),
                "cross-origin Web UI request rejected",
            ),
            (
                "/assets/app-icon.png?v=1",
                "image/png",
                "no-cache",
                Some(icon),
                "cross-origin Web UI request rejected",
            ),
            (
                "/assets/app-icon-source.png?v=1",
                "image/png",
                "no-cache",
                Some(source_icon),
                "cross-origin Web UI request rejected",
            ),
        ] {
            let reply = assert_wire(
                address,
                &request(port, "GET", path, &[&same_origin], b""),
                false,
                200,
            )
            .await;
            if let Some(body) = body {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: frame-ancestors 'none'\r\nX-Content-Type-Options: nosniff\r\nCache-Control: {cache}\r\nConnection: close\r\n\r\n",
                    body.len(),
                );
                wire_contract::assert_reply(path, &reply, &head, body);
            } else {
                let (head, received) = page_reply_parts(&reply, content_type, cache);
                assert_eq!(head, wire_contract::asset_head!("text/html; charset=utf-8", received.len()), "{path}");
                let html = str::from_utf8(received).unwrap();
                assert!(html.starts_with("<!doctype html><html lang=\"zh-CN\">"));
                assert!(html.contains("href=\"/assets/guide.css\""));
                assert!(html.contains("命令行与配置恢复"));
            }
            let denied = assert_wire(
                address,
                &request(port, "GET", path, &foreign, b""),
                false,
                403,
            )
            .await;
            wire_contract::assert_json_reply(
                path, &denied, "403 Forbidden",
                &json!({"error":{"message":denied_message}}),
            );
            assert_entry_error(address, &request(port, "HEAD", path, &[], b""), 404).await;
        }
    });
}

#[test]
fn page_and_asset_misses_keep_status_and_origin_order() {
    Fixture::new().run_wire(async |fixture, address| {
        let port = fixture.port();
        let foreign = ["Origin: https://foreign.invalid"];
        for (path, foreign_status) in [
            ("/index.htm", 404),
            ("/healthz/", 404),
            ("/guide", 404),
            ("/guide/", 403),
            ("/guide/README%2Emd", 403),
            ("/guide/missing.md", 403),
            ("/assets", 404),
            ("/assets/", 404),
            ("/assets/unknown/missing.js", 404),
            ("/assets/app-icon-missing.png", 404),
            ("/assets/tutorial-missing.gif", 403),
            ("/assets/module-unknown.js", 404),
        ] {
            assert_entry_error(address, &request(port, "GET", path, &[], b""), 404).await;
            let wire = request(port, "GET", path, &foreign, b"");
            if foreign_status == 403 {
                let reply = exchange_wire(address, &wire, false).await;
                wire_contract::assert_json_reply(
                    path,
                    &reply,
                    "403 Forbidden",
                    &json!({"error":{"message":"cross-origin guide request rejected"}}),
                );
            } else {
                assert_entry_error(address, &wire, 404).await;
            }
        }
        for method in ["POST", "DELETE"] {
            for (path, content_type, content, denied_message) in [
                (
                    "/",
                    "text/html; charset=utf-8",
                    crate::web::WEB_INDEX_BYTES,
                    "cross-origin Web UI request rejected",
                ),
                (
                    "/index.html",
                    "text/html; charset=utf-8",
                    crate::web::WEB_INDEX_BYTES,
                    "cross-origin Web UI request rejected",
                ),
                (
                    "/healthz",
                    "application/json",
                    crate::http::response::HEALTH_JSON_BYTES,
                    "cross-origin health request rejected",
                ),
            ] {
                let reply =
                    assert_wire(address, &request(port, method, path, &[], b""), false, 200).await;
                assert_page_reply(path, &reply, content_type, content);
                let reply =
                    exchange_wire(address, &request(port, method, path, &foreign, b""), false)
                        .await;
                wire_contract::assert_json_reply(
                    path,
                    &reply,
                    "403 Forbidden",
                    &json!({"error":{"message":denied_message}}),
                );
            }
            for path in ["/unregistered", "/guide/README.md", "/assets/app-icon.png"] {
                assert_entry_error(address, &request(port, method, path, &[], b""), 404).await;
            }
        }
        for method in ["PUT", "OPTIONS"] {
            for path in ["/", "/unregistered"] {
                let reply =
                    exchange_wire(address, &request(port, method, path, &[], b""), false).await;
                wire_contract::assert_json_reply(
                    path,
                    &reply,
                    "400 Bad Request",
                    &json!({"error":{"message":"invalid HTTP request"}}),
                );
            }
        }
        assert_entry_error(
            address,
            &request(port, "HEAD", "/unregistered", &[], b""),
            404,
        )
        .await;
        assert_entry_error(
            address,
            &request(port, "GET", "/api/unregistered", &[], b""),
            401,
        )
        .await;
        assert_entry_error(
            address,
            &request(port, "GET", "/api/unregistered", &[&fixture.session()], b""),
            404,
        )
        .await;
    });
}

#[cfg(unix)]
#[test]
fn module_panel_checks_origin_and_revision() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let panel = "export const panel = \"fixture\";\n";
    let revision = install_panel_fixture(directory.path(), emp_modules::Module::Gateway, panel);
    Fixture::with_directory(directory).run_wire(async |fixture, address| {
        let port = fixture.port();
        let same_origin = format!("Origin: http://localhost:{port}");
        let path = format!("/assets/module-gateway.js?revision={revision}");
        let reply = assert_wire(
            address,
            &request(port, "GET", &path, &[&same_origin], b""),
            false,
            200,
        )
        .await;
        wire_contract::assert_reply(
            &path, &reply,
            &wire_contract::asset_head!("text/javascript; charset=utf-8", panel.len()),
            panel.as_bytes(),
        );
        let stale = assert_wire(
            address,
            &request(
                port,
                "GET",
                "/assets/module-gateway.js?revision=old",
                &[],
                b"",
            ),
            false,
            404,
        )
        .await;
        wire_contract::assert_reply(
            "stale panel revision", &stale,
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 0\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: frame-ancestors 'none'\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
            b"",
        );
        for (method, path, headers) in [
            (
                "GET",
                &*path,
                ["Origin: https://foreign.invalid"].as_slice(),
            ),
            ("GET", "/assets/module-unknown.js", [].as_slice()),
            ("HEAD", &*path, [].as_slice()),
        ] {
            assert_entry_error(address, &request(port, method, path, headers, b""), 404).await;
        }
        let reply = assert_wire(
            address,
            &request(port, "GET", "/assets/module-settings.js", &[], b""),
            true,
            200,
        )
        .await;
        let body = include_bytes!("../../../web/module-settings.js");
        wire_contract::assert_reply(
            "/assets/module-settings.js", &reply,
            &wire_contract::asset_head!("text/javascript; charset=utf-8", body.len()), body,
        );
    });
}

#[cfg(unix)]
#[test]
fn team_reads_preserve_identity_and_auth() {
    use emp_modules::Module;

    let directory = emp_test_support::fixture_dir().unwrap();
    install_panel_fixture(directory.path(), Module::Team, "export const panel = {};");
    let state_root = directory.path().join("state");
    let workspace_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let member_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let vault = emp_state::VaultStore::from_sources(None, &state_root.join("master.key")).unwrap();
    vault
        .write_encrypted_json(
            &state_root.join("team-profile.json"),
            &json!({
                "session": {
                    "base": "https://team.invalid",
                    "token": "fixture-team-session",
                    "identity": {
                "workspace": {"id":workspace_id},
                "member": {"id":member_id,"publish":true},
                        "native_usage": {"private":true},
                    },
                },
                "imported": {},
            }),
        )
        .unwrap();
    std::fs::write(state_root.join("team-budget-notices.json"), json!([
        {"workspace":workspace_id,"member":member_id,"day":1,"at":2,"reached":true,"used":10,"limit":10},
        {"workspace":workspace_id,"member":"cccccccccccccccccccccccccccccccc","day":1,"at":2,"reached":true,"used":20,"limit":20},
    ]).to_string()).unwrap();

    Fixture::with_directory(directory).run_wire(async |fixture, address| {
        let port = fixture.port();
        let session = fixture.session();
        let foreign_origin = "Origin: https://foreign.invalid";
        for (path, cache_headers) in [
            ("/api/team/status", 2),
            ("/api/team/notices", 2),
            ("/api/team/publish_source", 2),
        ] {
            let post = path.ends_with("/publish_source");
            let signed = [&*session, "Accept: text/event-stream"];
            let valid = |target: &str, headers: &[&str]| {
                if post {
                    json_request(port, target, headers, b"{}")
                } else {
                    request(port, "GET", target, headers, b"")
                }
            };
            let reply = exchange_wire(address, &valid(path, &signed), !post).await;
            let extra = (cache_headers == 2).then_some("Cache-Control: no-store");
            let body = wire_contract::assert_json_read(path, &reply, extra);
            match path {
                "/api/team/status" => {
                    assert_eq!(body["joined"], true);
                    assert_eq!(body["identity"]["member"]["id"], member_id);
                    assert!(body["identity"].get("native_usage").is_none());
                    assert!(!body.to_string().contains("fixture-team-session"));
                }
                "/api/team/notices" => {
                    assert_eq!(body.as_array().unwrap().len(), 1);
                    assert_eq!(body[0]["used"], 10);
                }
                _ => assert_eq!(body, json!({})),
            }
            for (headers, status) in [
                ([].as_slice(), 401),
                ([&*session, &*session].as_slice(), 401),
                (
                    [&*session, foreign_origin].as_slice(),
                    if post { 401 } else { 403 },
                ),
            ] {
                assert_entry_error(address, &valid(path, headers), status).await;
            }
            let missing = format!("{path}/missing");
            let wire = valid(&missing, &[&session]);
            if post {
                let reply = exchange_wire(address, &wire, false).await;
                let body = br#"{"error":{"code":"invalid_workspace_request","message":"Workspace request could not be completed","origin":"emp"}}"#;
                let head = format!(
                    wire_contract::json_head!("400 Bad Request", "{}", "Cache-Control: no-store"),
                    body.len()
                );
                wire_contract::assert_reply(&missing, &reply, &head, body);
            } else {
                assert_entry_error(address, &wire, 404).await;
            }
            assert_entry_error(address, &request(port, "HEAD", path, &[&session], b""), 404).await;
        }
        let path = "/api/team/publish_source";
        let wire = json_request(port, path, &[&session], b"[]");
        let reply = exchange_wire(address, &wire, false).await;
        wire_contract::assert_json_reply(
            path,
            &reply,
            "400 Bad Request",
            &json!({"error":{"message":"request body must be a JSON object","origin":"emp"}}),
        );
        assert_entry_error(address, &json_request(port, path, &[], b"[]"), 401).await;

        fixture
            .server
            .state
            .backend
            .modules
            .mark_unavailable(Module::Team, "fixture-disabled");
        for request in [
            request(port, "GET", "/api/team/status", &[&session], b""),
            request(port, "GET", "/api/team/notices", &[&session], b""),
            json_request(port, path, &[], b"[]"),
        ] {
            let reply = assert_wire(address, &request, false, 404).await;
            wire_contract::assert_json_reply(
                "disabled team module", &reply, "404 Not Found",
                &json!({"error":{"code":"module_disabled","message":"This EMP module is not active","origin":"emp"}}),
            );
        }
    });
}

#[test]
fn inspection_reads_preserve_auth_and_hide_credentials() {
    let fixture = Fixture::new();
    let auth = &fixture.server.state.backend.codex.auth_path;
    std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
    std::fs::write(auth, br#"{"tokens":{"access_token":"parity-private-token","account_id":"parity-private-account"}}"#).unwrap();
    fixture
        .server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["image_routing"]["credit"] = json!({
        "provider":"parity-provider","cycle_started_at":1,"expires_at":4_102_444_800_i64,
        "budget_cents":250,"percent":80,
    });
    fixture.run_wire(async |fixture, address| {
        let port = fixture.port();
        let session = fixture.session();
        for (path, field, cache_headers) in [
            ("/api/diagnostics", "capacity", 1),
            ("/api/support-report", "schema_version", 1),
            ("/api/capabilities", "capabilities", 1),
            ("/api/models/vision-test-image", "data_url", 2),
            ("/api/models/audio-test-sound", "data", 2),
            ("/api/image-credit", "limit_nanos", 2),
        ] {
            let wire = request(
                port,
                "GET",
                path,
                &[&session, "Accept: text/event-stream"],
                b"",
            );
            let reply = exchange_wire(address, &wire, true).await;
            let extra = match path {
                "/api/support-report" => {
                    Some("Content-Disposition: attachment; filename=\"EMP-support-report.json\"")
                }
                _ if cache_headers == 2 => Some("Cache-Control: no-store"),
                _ => None,
            };
            let body = wire_contract::assert_json_read(path, &reply, extra);
            let text = str::from_utf8(&reply).unwrap();
            match path {
                "/api/support-report" => {
                    assert_eq!(body["schema_version"], 1);
                    assert_eq!(body["emp_version"], env!("CARGO_PKG_VERSION"));
                    assert_eq!(body["accounts"]["imported_count"], 0);
                    assert_eq!(body["accounts"]["native"]["credential_present"], true);
                    time::OffsetDateTime::parse(
                        body["generated_at"].as_str().unwrap(),
                        &time::format_description::well_known::Rfc3339,
                    )
                    .unwrap();
                    for private in [
                        "parity-private-token",
                        "parity-private-account",
                        fixture.directory.path().to_str().unwrap(),
                    ] {
                        assert!(!text.contains(private), "support report exposed {private}");
                    }
                }
                "/api/models/vision-test-image" => {
                    assert!(
                        body[field]
                            .as_str()
                            .unwrap()
                            .starts_with("data:image/png;base64,iVBOR")
                    );
                }
                "/api/models/audio-test-sound" => {
                    assert_eq!(body["format"], "wav");
                    assert!(body[field].as_str().unwrap().starts_with("UklGR"));
                }
                "/api/image-credit" => assert_eq!(body[field], 2_000_000_000_i64),
                "/api/diagnostics" => {
                    assert_eq!(body["capacity"], 512);
                    assert_eq!(body["sample_count"], 0);
                    assert_eq!(body["records"], json!([]));
                }
                "/api/capabilities" => assert_eq!(body["capabilities"], json!([])),
                _ => unreachable!("{path}"),
            }
            for (headers, status) in [
                ([].as_slice(), 401),
                ([&*session, &*session].as_slice(), 401),
                (
                    [&*session, "Origin: https://foreign.invalid"].as_slice(),
                    403,
                ),
            ] {
                assert_entry_error(address, &request(port, "GET", path, headers, b""), status)
                    .await;
            }
            assert_entry_error(
                address,
                &request(port, "GET", &format!("{path}/missing"), &[&session], b""),
                404,
            )
            .await;
            assert_entry_error(address, &request(port, "HEAD", path, &[&session], b""), 404).await;
        }
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
fn account_reads_preserve_json_and_auth() {
    let fixture = Fixture::new();
    let state = &fixture.server.state;
    let auth_path = &state.backend.codex.auth_path;
    std::fs::create_dir_all(auth_path.parent().unwrap()).unwrap();
    std::fs::write(
        auth_path,
        br#"{"tokens":{"access_token":"fixture-token","account_id":"fixture-owner"}}"#,
    )
    .unwrap();

    fixture.run_wire(async |fixture, address| {
        let session = fixture.session();
        let port = fixture.port();
        for (path, field) in [
            ("/api/accounts", "accounts"),
            ("/api/accounts/%40native/models", "models"),
            ("/api/accounts/%40native/quota-history?range=all", "series"),
        ] {
            for end_input in [false, true] {
                let wire = request(port, "GET", path, &[&session], b"");
                let reply = exchange_wire(address, &wire, end_input).await;
                let body = wire_contract::assert_json_read(path, &reply, None);
                assert!(body[field].is_array(), "{path}");
                match field {
                    "accounts" => {
                        assert_eq!(body["accounts"], json!([]));
                        assert_eq!(body["native_account"]["id"], "@native");
                        assert_eq!(body["native_account"]["credential_set"], true);
                        assert!(!str::from_utf8(&reply).unwrap().contains("fixture-token"));
                    }
                    "series" => assert_eq!(body["series"], json!([])),
                    _ => {}
                }
            }
            for (method, headers, status) in [
                ("GET", [].as_slice(), 401),
                ("GET", [&*session, &*session].as_slice(), 401),
                (
                    "GET",
                    [&*session, "Origin: https://other.invalid"].as_slice(),
                    403,
                ),
                ("HEAD", [&*session].as_slice(), 404),
            ] {
                assert_entry_error(address, &request(port, method, path, headers, b""), status)
                    .await;
            }
        }
        for path in ["/api/accounts/absent", "/api/accounts//quota-history"] {
            assert_entry_error(address, &request(port, "GET", path, &[&session], b""), 404).await;
        }
        for (path, status, message) in [
            (
                "/api/accounts/models",
                "400 Bad Request",
                "Subscription account is unavailable",
            ),
            (
                "/api/accounts/absent/models",
                "400 Bad Request",
                "Subscription account is unavailable",
            ),
            (
                "/api/accounts/absent/quota-history",
                "404 Not Found",
                "unknown account: absent",
            ),
            (
                "/api/accounts/%40native/quota-history?range=invalid",
                "400 Bad Request",
                "unsupported quota history range",
            ),
        ] {
            let reply =
                exchange_wire(address, &request(port, "GET", path, &[&session], b""), true).await;
            wire_contract::assert_json_reply(
                path,
                &reply,
                status,
                &json!({"error":{"message":message,"origin":"emp"}}),
            );
        }
    });
}

#[test]
fn usage_and_settings_reads_preserve_json_and_auth() {
    let fixture = Fixture::new();
    fixture
        .server
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
    fixture.run_wire(async |fixture, address| {
        let port = fixture.port();
        let session = fixture.session();
        for (path, field, invalid) in [
            (
                "/api/usage?start=1&end=10",
                "totals",
                "/api/usage?start=invalid",
            ),
            ("/api/calls?start=1&end=10", "records", "/api/calls?limit=0"),
            (
                "/api/providers/fixture%2Dcpa/quota-history?start=1&end=10",
                "account_ids",
                "/api/providers/absent/quota-history?start=1&end=10",
            ),
            ("/api/config", "models", "/api/config/absent"),
            (
                "/api/integration",
                "configuration",
                "/api/integration/absent",
            ),
            (
                "/api/runtime/claude-cli",
                "available",
                "/api/runtime/claude-cli/absent",
            ),
            (
                "/api/request-limits",
                "baseline_bytes",
                "/api/request-limits/absent",
            ),
        ] {
            let wire = request(port, "GET", path, &[&session], b"");
            let reply = exchange_wire(
                address,
                &wire,
                !path.starts_with("/api/usage?") && !path.starts_with("/api/calls?"),
            )
            .await;
            let body = if matches!(path, "/api/config" | "/api/integration") {
                wire_contract::assert_settings_read(path, &reply, fixture)
            } else {
                wire_contract::assert_json_read(path, &reply, None)
            };
            match field {
                "totals" => {
                    assert_eq!(body["start"], json!(1.0));
                    assert_eq!(body["end"], json!(10.0));
                    assert_eq!(body["totals"]["requests"], 0);
                    assert_eq!(body["groups"], json!([]));
                }
                "records" => assert_eq!(body["records"], json!([])),
                "account_ids" => assert_eq!(body["account_ids"], json!([])),
                "models" => assert_eq!(body["providers"][0]["id"], "fixture-cpa"),
                "available" => {
                    let guidance = if body["available"].as_bool().unwrap() {
                        "Claude Code CLI is available to EMP."
                    } else {
                        "Claude Code CLI was not found as an available local installation. Install it or check that EMP can access it, then reopen this form."
                    };
                    assert_eq!(body["guidance"], guidance);
                }
                "baseline_bytes" => assert_eq!(body["baseline_bytes"], 67_108_864),
                _ => {}
            }
            for (method, path, headers, status) in [
                ("GET", path, [].as_slice(), 401),
                ("GET", path, [&*session, &*session].as_slice(), 401),
                (
                    "GET",
                    path,
                    [&*session, "Origin: https://other.invalid"].as_slice(),
                    403,
                ),
                ("HEAD", path, [&*session].as_slice(), 404),
            ] {
                assert_entry_error(address, &request(port, method, path, headers, b""), status)
                    .await;
            }
            if !invalid.starts_with("/api/usage?") && !invalid.starts_with("/api/calls?") {
                assert_entry_error(
                    address,
                    &request(port, "GET", invalid, &[&session], b""),
                    404,
                )
                .await;
            }
        }
        assert_entry_error(
            address,
            &request(
                port,
                "GET",
                "/api/providers//quota-history?start=1&end=10",
                &[&session],
                b"",
            ),
            404,
        )
        .await;
        for (path, status, message) in [
            (
                "/api/usage?start=invalid",
                "400 Bad Request",
                "Invalid usage period or category",
            ),
            (
                "/api/calls?limit=0",
                "400 Bad Request",
                "Invalid call filter",
            ),
            (
                "/api/usage?account_id=absent",
                "404 Not Found",
                "Account usage is unavailable",
            ),
            (
                "/api/providers/fixture-cpa/quota-history?start=10&end=1",
                "400 Bad Request",
                "invalid quota history period",
            ),
            (
                "/api/providers/fixture/cpa/quota-history?start=10&end=1",
                "400 Bad Request",
                "invalid quota history period",
            ),
        ] {
            let reply = exchange_wire(
                address,
                &request(port, "GET", path, &[&session], b""),
                false,
            )
            .await;
            wire_contract::assert_json_reply(
                path,
                &reply,
                status,
                &json!({"error":{"message":message,"origin":"emp"}}),
            );
        }
    });
}

#[test]
fn report_stream_selection_obeys_accept_header() {
    Fixture::new().run_wire(async |fixture, address| {
        let session = fixture.session();
        let accept = "Accept: text/event-stream";
        for path in [
            "/api/calls?start=1&end=10",
            "/api/usage?lifetime=%74rue&owner=fixture",
        ] {
            let wire = request(fixture.port(), "GET", path, &[&session, accept], b"");
            let reply = assert_sse_wire(address, &wire).await;
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
            let wire = request(fixture.port(), "GET", path, &[&session, accept], b"");
            let reply = exchange_wire(address, &wire, false).await;
            let body = wire_contract::assert_json_read(path, &reply, None);
            if field == "totals" {
                assert_eq!(body["totals"]["requests"], 0);
            } else {
                assert_eq!(body[field], json!([]));
            }
        }
    });
}

#[test]
fn call_reports_link_recoveries_in_either_commit_order_and_read_legacy_links() {
    let fixture = Fixture::new();
    let ledger = &fixture.server.state.backend.usage.ledger;
    let rejection = |id: &str| json!({"request_id":id,"route":"responses","error_code":"previous_response_not_found"});
    for (prior, retry, retry_first) in [
        ("rejection-first", "retry-second", false),
        ("rejection-second", "retry-first", true),
    ] {
        let reject = || {
            ledger
                .record_completion(&rejection(prior), 1.5, 1.0, 1.5)
                .unwrap();
        };
        let complete = || {
            let event = json!({
                "request_id":retry,"route":"responses","success":true,
                "continuation_recovery_from":prior
            });
            ledger.record_completion(&event, 3.5, 3.0, 3.5).unwrap();
        };
        if retry_first {
            complete();
            reject();
        } else {
            reject();
            complete();
        }
    }
    ledger
        .record_call(&rejection("legacy-rejection"), 1.0, 1.5)
        .unwrap();
    let database =
        rusqlite::Connection::open(fixture.directory.path().join("state/usage.sqlite3")).unwrap();
    database
        .execute_batch(
            "UPDATE call_records SET facts=json_set(facts,'$.recovery_request_id','legacy-retry')
             WHERE request_id IN ('legacy-rejection','rejection-first')",
        )
        .unwrap();
    drop(database);
    fixture.run_wire(async |fixture, address| {
        let session = fixture.session();
        for (prior, retry) in [
            ("rejection-first", "retry-second"),
            ("rejection-second", "retry-first"),
            ("legacy-rejection", "legacy-retry"),
        ] {
            let path = format!("/api/calls?start=1&end=2&request={prior}");
            let wire = request(fixture.port(), "GET", &path, &[&session], b"");
            let reply = exchange_wire(address, &wire, false).await;
            let body = wire_contract::assert_json_read(&path, &reply, None);
            assert_eq!(body["total"], 1, "{path}");
            assert_eq!(body["records"][0]["request_id"], prior);
            assert_eq!(body["records"][0]["state"], "recovery_required");
            assert_eq!(body["records"][0]["recovery_request_id"], retry, "{path}");
        }
    });
}

#[test]
fn closing_report_input_cancels_json_queries() {
    let fixture = Fixture::new();
    let ledger = &fixture.server.state.backend.usage.ledger;
    ledger.query(1.0, 10.0, "all", system_now(), None).unwrap();
    let database =
        rusqlite::Connection::open(fixture.directory.path().join("state/usage.sqlite3")).unwrap();
    database.execute_batch(
        "WITH RECURSIVE records(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM records WHERE n<20000)
         INSERT INTO call_records(request_id,operation,started_at,finished_at,provider_id,account_id,model_id,session_id,state,facts)
         SELECT n,'responses',2,3,'fixture','','model','','completed',
                json_object('request_id','fixture-' || n) FROM records;
         WITH RECURSIVE records(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM records WHERE n<20000)
         INSERT INTO usage_events(id,observed_at,category,owner,model,service_tier,operation,rates,origin,usage_turn,route_model,match_key)
         SELECT n,2,'external','fixture','model','','responses','{}','realtime','','model','' FROM records;",
    ).unwrap();
    drop(database);
    fixture.run_wire(async |fixture, address| {
        let session = fixture.session();
        for (path, field, message) in [
            (
                "/api/usage?start=1&end=10",
                "totals",
                "Usage history is unavailable",
            ),
            (
                "/api/calls?start=1&end=10",
                "records",
                "Call history is unavailable",
            ),
        ] {
            let wire = request(fixture.port(), "GET", path, &[&session], b"");
            let reply = exchange_wire(address, &wire, true).await;
            wire_contract::assert_json_reply(
                path,
                &reply,
                "503 Service Unavailable",
                &json!({"error":{"message":message,"origin":"emp"}}),
            );
            let reply = exchange_wire(address, &wire, false).await;
            let body = wire_contract::assert_json_read(path, &reply, None);
            if field == "totals" {
                assert_eq!(body["totals"]["requests"], 20_000);
            } else {
                let records = body["records"].as_array().unwrap();
                assert!(!records.is_empty());
                assert_eq!(body["total"], 20_000);
                assert_eq!(records[0]["request_id"], "fixture-9999");
            }
        }
    });
}

#[test]
fn identity_rejections_stop_before_fake_upstream_and_body() {
    Fixture::new().run(async |fixture, address| {
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
            assert_entry_error(
                address,
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
            timeout(
                test_timeout(Duration::from_secs(2)).duration(),
                assert_entry_error(address, &wire, 401),
            )
            .await
            .expect("auth rejection precedes the body idle deadline");
        }
        let expired = emp_state::load_or_create_web_session(
            &fixture.directory.path().join("expired-session.json"),
            system_now() - 2.0 * emp_state::WEB_SESSION_LIFETIME_SECONDS,
        )
        .unwrap();
        let expired_header = format!("X-EMP-Session: {}", expired.token());
        *fixture.server.state.sessions.session.lock().unwrap() = expired;
        assert_entry_error(
            address,
            &json_request(port, "/api/language", &[&expired_header], b"{}"),
            401,
        )
        .await;
        assert_eq!(fixture.upstream.load(Ordering::Relaxed), 0);

        let current = emp_state::load_or_create_web_session(
            &fixture.server.state.sessions.path,
            system_now(),
        )
        .unwrap();
        *fixture.server.state.sessions.session.lock().unwrap() = current;
        let reply = exchange_wire(
            address,
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
    Fixture::new().run(async |fixture, address| {
        let port = fixture.port();
        let spoof = [
            "X-EMP-Caller-Id: forged",
            "x-emp-caller-name: forged",
            "X-EMP-Peer-IP: 192.0.2.1",
            "X-EMP-Caller-Extra: forged",
        ];
        let ws = [spoof.as_slice(), &["Upgrade: websocket"]].concat();
        let unauthorized = |reply: &[u8], presented| {
            if presented {
                wire_contract::assert_presented_entry_error(
                    "presented proxy caller", reply, "401 Unauthorized",
                    &json!({"error":{
                        "message":"EMP 请求处理出错\nproxy caller authentication is required",
                        "origin":"emp","summary":"EMP 请求处理出错",
                    }}),
                );
            } else {
                wire_contract::assert_json_reply(
                    "proxy caller", reply, "401 Unauthorized",
                    &json!({"error":{"message":"proxy caller authentication is required","origin":"emp"}}),
                );
            }
        };
        for (wire, presented) in [
            (json_request(port, "/v1/responses", &spoof, b"{}"), true),
            (json_request(port, "/v1/images/generations", &spoof, b"{}"), false),
            (json_request(port, "/v1/alpha/search", &spoof, b"{}"), false),
            (request(port, "GET", "/v1/responses", &ws, b""), false),
        ] {
            unauthorized(&exchange_wire(address, &wire, false).await, presented);
        }
        assert_eq!(fixture.upstream.load(Ordering::Relaxed), 0);

        let admission = &fixture.server.state.lifecycle.admission;
        let parent = admission.acquire_request().unwrap();
        let delegation = fixture
            .server
            .state
            .backend
            .aero_callers
            .delegate(
                Caller {
                    id: "trusted".into(),
                    name: "Trusted Caller".into(),
                },
                Some("Bearer restored".into()),
                None,
                || crate::services::modules::aero::CallParent::Connection(parent.slot().clone()),
            )
            .unwrap();
        drop(parent);
        let auth = format!("Authorization: Bearer {}", delegation.key);
        let headers = [spoof.as_slice(), &[auth.as_str()]].concat();
        let wire = json_request(port, "/v1/responses", &headers, br#"{"new_field":1}"#);
        let reply = exchange_wire(address, &wire, false).await;
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
        unauthorized(&exchange_wire(address, &wire, false).await, true);
        assert_eq!(fixture.upstream.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn public_origins_and_models_keep_their_route_policies() {
    Fixture::new().run(async |fixture, address| {
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
            let received = assert_wire(
                address,
                &request(
                    fixture.port(),
                    method,
                    path,
                    &["Host: outside.invalid"],
                    b"",
                ),
                false,
                403,
            )
            .await;
            wire_contract::assert_json_reply(
                path,
                &received,
                "403 Forbidden",
                &json!({"error":{"message":message}}),
            );
        }
        let local = request(fixture.port(), "POST", "/healthz", &[], b"");
        let reply = exchange_wire(address, &local, false).await;
        wire_contract::assert_json_reply("/healthz", &reply, "200 OK", &json!({"status":"ok"}));
        let auth = format!(
            "Authorization: Bearer {}",
            fixture.server.state.backend.gateway_key
        );
        for headers in [&[][..], &["Host: outside.invalid", auth.as_str()][..]] {
            let wire = request(fixture.port(), "GET", "/v1/models", headers, b"");
            let reply = assert_wire(address, &wire, false, 200).await;
            let (head, body) = str::from_utf8(&reply)
                .unwrap()
                .split_once("\r\n\r\n")
                .unwrap();
            assert!(
                head.to_ascii_lowercase()
                    .contains("content-type: text/plain; charset=utf-8")
            );
            assert_eq!(body, "catalog upstream");
        }
    });
}

#[test]
fn route_access_precedes_identity_and_preserves_management_exceptions() {
    Fixture::new().run(async |fixture, address| {
        let decisions = request(
            fixture.port(),
            "POST",
            "/v1/decisions",
            &["Content-Length: -1"],
            b"",
        );
        let reply = exchange_wire(address, &decisions, false).await;
        wire_contract::assert_json_reply(
            "/v1/decisions", &reply, "401 Unauthorized",
            &json!({"error":{"message":"EMP gateway authentication is required","origin":"emp"}}),
        );
        fixture
            .server
            .state
            .backend
            .modules
            .mark_unavailable(emp_modules::Module::Decision, "test");
        let reply = exchange_wire(address, &decisions, false).await;
        wire_contract::assert_json_reply(
            "/v1/decisions", &reply, "404 Not Found",
            &json!({"error":{"code":"module_disabled","message":"This EMP module is not active","origin":"emp"}}),
        );
        assert_eq!(fixture.upstream.load(Ordering::Relaxed), 0);

        let modules = request(
            fixture.port(),
            "POST",
            "/api/modules",
            &["Origin: https://outside.invalid", &fixture.session()],
            b"",
        );
        assert_entry_error(address, &modules, 401).await;
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
    Fixture::new().run(async |fixture, address| {
        const MAX: usize = 5 * 1024 * 1024;
        let port = fixture.port();
        let session = fixture.session();
        let headers = [session.as_str(), "Content-Type: application/json"];
        let hard_limit = |limit: usize, decoded: bool| {
            let prefix = if decoded { "decoded " } else { "" };
            json!({"error":{
                "code":"request_too_large",
                "reason":"hard_limit",
                "message":format!("{prefix}request body is too large (EMP limit: {limit} bytes)"),
                "limit_bytes":limit
            }})
        };
        let bad_request = |message: &str| json!({"error":{"message":message,"origin":"emp"}});
        for (extra, status, expected) in [
            (vec!["Content-Length: 5242881"], "413 Content Too Large", hard_limit(MAX, false)),
            (vec!["Content-Length: -1"], "400 Bad Request", bad_request("Content-Length cannot be negative")),
            (
                vec!["Content-Length: invalid"],
                "400 Bad Request",
                bad_request("invalid Content-Length"),
            ),
            (
                vec!["Content-Length: 2", "content-length: 2"],
                "400 Bad Request",
                bad_request("duplicate Content-Length or unsupported Transfer-Encoding"),
            ),
            (
                vec!["Transfer-Encoding: chunked"],
                "400 Bad Request",
                bad_request("duplicate Content-Length or unsupported Transfer-Encoding"),
            ),
        ] {
            let reply = exchange_wire(
                address,
                &request(
                    port,
                    "POST",
                    "/api/language",
                    &[headers.as_slice(), &extra].concat(),
                    b"",
                ),
                false,
            )
            .await;
            wire_contract::assert_json_reply("language framing", &reply, status, &expected);
        }
        let declared = format!(
            "Content-Length: {}",
            RequestLimitsConfig::default().maximum + 1
        );
        let reply = exchange_wire(
            address,
            &request(
                port,
                "POST",
                "/v1/responses",
                &[headers.as_slice(), &[&declared]].concat(),
                b"",
            ),
            false,
        )
        .await;
        let mut expected = hard_limit(RequestLimitsConfig::default().maximum, false);
        let error = &mut expected["error"];
        error["message"] = json!(format!("EMP 请求处理出错\n{}", error["message"].as_str().unwrap()));
        error["summary"] = json!("EMP 请求处理出错");
        wire_contract::assert_presented_entry_error(
            "oversized inference body", &reply, "413 Content Too Large",
            &expected,
        );

        let gzip_json = gzip(b"{}", Vec::new());
        let mut truncated = gzip_json.clone();
        truncated.truncate(truncated.len() - 8);
        let encoded = |inner: &[u8]| emp_transport::zstd_encode(inner).unwrap();
        let intermediate_large = gzip(b"{}", vec![b'a'; MAX]);
        let final_large = gzip(&vec![b' '; MAX + 1], Vec::new());
        for (encoding, body, status, expected) in [
            ("br", b"{}".to_vec(), "400 Bad Request", bad_request("unsupported Content-Encoding")),
            ("gzip", truncated, "400 Bad Request", bad_request("invalid compressed request body")),
            (
                "gzip, zstd",
                encoded(&intermediate_large),
                "413 Content Too Large",
                hard_limit(MAX, true),
            ),
            (
                "gzip, zstd",
                encoded(&final_large),
                "413 Content Too Large",
                hard_limit(MAX, true),
            ),
            (
                "gzip, zstd",
                encoded(&gzip_json),
                "400 Bad Request",
                bad_request("language is required"),
            ),
            (
                "identity",
                b"[]".to_vec(),
                "400 Bad Request",
                bad_request("request body must be a JSON object"),
            ),
        ] {
            let content_encoding = format!("Content-Encoding: {encoding}");
            let reply = exchange_wire(
                address,
                &json_request(port, "/api/language", &[&session, &content_encoding], &body),
                false,
            )
            .await;
            wire_contract::assert_json_reply(encoding, &reply, status, &expected);
        }
        let padded = request(
            port,
            "POST",
            "/api/language",
            &[headers.as_slice(), &["Content-Length: +2"]].concat(),
            b"{}ignored",
        );
        let reply = exchange_wire(address, &padded, false).await;
        wire_contract::assert_json_reply(
            "signed Content-Length", &reply, "400 Bad Request",
            &json!({"error":{"message":"language is required","origin":"emp"}}),
        );
        let incomplete = request(
            port,
            "POST",
            "/api/language",
            &[headers.as_slice(), &["Content-Length: 2"]].concat(),
            b"{",
        );
        let reply = exchange_wire(address, &incomplete, true).await;
        wire_contract::assert_json_reply(
            "incomplete language request",
            &reply,
            "400 Bad Request",
            &json!({"error":{"message":"request body is incomplete","origin":"emp"}}),
        );

        let timed_out = request(
            port,
            "POST",
            "/api/language",
            &[headers.as_slice(), &["Content-Length: 2"]].concat(),
            b"{",
        );
        let reply = exchange_wire(address, &timed_out, false).await;
        wire_contract::assert_json_reply(
            "language body timeout", &reply, "408 Request Timeout",
            &json!({"error":{"code":"request_timeout","message":"request body timed out","origin":"emp"}}),
        );
    });
}

#[test]
fn insufficient_body_budget_rejects_before_read() {
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
        let router = with_entry_state(
            Router::new().route(
                "/v1/responses",
                post(move |_: ProxyCaller, request: Request| async move {
                    JsonBody::read(request, &limits)
                        .await
                        .map(|_| "unexpected upstream")
                }),
            ),
            fixture.server.state.clone(),
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
        let address = runtime.local_addr();
        let reply = assert_wire(address, &wire, false, 503).await;
        assert!(String::from_utf8_lossy(&reply).contains("\r\nRetry-After: 2\r\n"));
        assert_eq!(json_body(&reply)["error"]["memory"]["available_bytes"], 0);
        runtime.shutdown().await.unwrap();
    });
}

#[test]
fn bootstrap_and_export_confirmations_are_consumed_once() {
    Fixture::new().run(async |fixture, address| {
        let port = fixture.port();
        let original_session = fixture.session();
        let header = format!("X-EMP-Bootstrap: {}", fixture.server.state.bootstrap.token);
        let duplicate = request(port, "POST", "/api/session", &[&header, &header], b"");
        assert_entry_error(address, &duplicate, 401).await;
        let bootstrap = request(port, "POST", "/api/session", &[&header], b"");
        let accepted = exchange_wire(address, &bootstrap, false).await;
        assert_status(&accepted, 200);
        assert!(
            String::from_utf8_lossy(&accepted)
                .contains(crate::http::auth::CLEAR_LEGACY_SESSION_COOKIE)
        );
        assert_entry_error(address, &bootstrap, 401).await;
        assert_ne!(fixture.session(), original_session);
        let invalidated = json_request(port, "/api/language", &[&original_session], b"{}");
        assert_entry_error(address, &invalidated, 401).await;

        let session = fixture.session();
        let issue = json_request(port, "/api/migration/export/confirm", &[&session], b"{}");
        for first_is_valid in [true, false] {
            let issued = exchange_wire(address, &issue, false).await;
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
            let assert_confirmation_required = |reply: &[u8]| {
                wire_contract::assert_json_reply(
                    "/api/migration/export", reply, "403 Forbidden",
                    &json!({"error":{"code":"export_confirmation_required","message":"export confirmation is missing or expired; confirm the export again","origin":"emp"}}),
                );
            };
            if first_is_valid {
                assert_status(&exchange_wire(address, &export(&good), false).await, 200);
            } else {
                let rejected = exchange_wire(address, &export("wrong"), false).await;
                assert_confirmation_required(&rejected);
            }
            let rejected = exchange_wire(address, &export(&good), false).await;
            assert_confirmation_required(&rejected);
        }
    });
}

#[test]
fn realtime_uses_its_own_auth_and_multipart_errors() {
    Fixture::new().run(async |fixture, address| {
        let port = fixture.port();
        let unauthenticated = request(
            port,
            "POST",
            "/v1/live",
            &["Content-Length: 9", "content-length: 8"],
            b"",
        );
        let reply = exchange_wire(address, &unauthenticated, false).await;
        wire_contract::assert_json_reply(
            "/v1/live without subscription", &reply, "401 Unauthorized",
            &json!({"error":{"code":"native_subscription_unavailable","message":"A current native ChatGPT subscription login is required for Codex Voice","origin":"emp"}}),
        );

        let sideband = request(port, "GET", "/v1/live/rtc_demo", &[], b"");
        let reply = exchange_wire(address, &sideband, false).await;
        wire_contract::assert_json_reply(
            "/v1/live/rtc_demo", &reply, "401 Unauthorized",
            &json!({"error":{"code":"realtime_caller_unauthorized","message":"Caller authentication is required for Codex Voice","origin":"emp"}}),
        );

        let session = fixture.session();
        let content_type = "Content-Type: multipart/form-data; boundary=EMP";
        for (headers, status, code, message) in [
            (
                vec!["Content-Length: 9", "content-length: 8"],
                "400 Bad Request",
                "realtime_invalid_request",
                "Realtime request contains duplicate framing or media-type headers",
            ),
            (
                vec!["Transfer-Encoding: chunked"],
                "400 Bad Request",
                "realtime_invalid_request",
                "Transfer-Encoding is not supported for realtime calls",
            ),
            (
                vec!["Content-Encoding: gzip"],
                "415 Unsupported Media Type",
                "realtime_unsupported_encoding",
                "Realtime calls require identity Content-Encoding",
            ),
            (vec![], "411 Error", "realtime_length_required", "Content-Length is required for realtime calls"),
            (
                vec!["Content-Length: 262145"],
                "413 Content Too Large",
                "realtime_request_too_large",
                "Realtime request exceeds the 262144 byte limit",
            ),
        ] {
            let wire = request(
                port,
                "POST",
                "/v1/live",
                &[&[session.as_str(), content_type], headers.as_slice()].concat(),
                b"",
            );
            let reply = exchange_wire(address, &wire, false).await;
            wire_contract::assert_json_reply(
                message, &reply, status,
                &json!({"error":{"code":code,"message":message,"origin":"emp"}}),
            );
        }
        let reply = exchange_wire(
            address,
            &request(port, "POST", "/v1/live", &[&session, content_type, "Content-Length: 6"], b"--EMP\n"),
            false,
        ).await;
        wire_contract::assert_json_reply(
            "malformed realtime multipart body", &reply, "400 Bad Request",
            &json!({"error":{"code":"realtime_invalid_multipart","message":"Realtime request must contain exactly two multipart fields","origin":"emp"}}),
        );
    });
}

#[test]
fn recovery_keeps_flat_errors_and_auth_precedes_empty_body_policy() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let listener = tokio::net::TcpListener::bind(LOOPBACK).await.unwrap();
            let address = listener.local_addr().unwrap();
            let port = address.port();
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
            let control = Arc::new(crate::server::ServerControl::recovery(admission()));
            let server = tokio::spawn(crate::server::serve_recovery(
                listener,
                router,
                control.clone(),
            ));
            for (path, headers, expected_body) in [
                ("/", [].as_slice(), "recovery"),
                (
                    "/api/recovery",
                    ["X-EMP-Recovery: recovery-fixture"].as_slice(),
                    "private recovery",
                ),
            ] {
                let reply = assert_wire(
                    address,
                    &request(port, "GET", path, headers, b""),
                    true,
                    200,
                )
                .await;
                let (head, body) = str::from_utf8(&reply)
                    .unwrap()
                    .split_once("\r\n\r\n")
                    .unwrap();
                assert!(
                    head.to_ascii_lowercase()
                        .contains("content-type: text/plain; charset=utf-8")
                );
                assert_eq!(body, expected_body, "{path}");
            }
            for (method, path, headers, status, code) in [
                (
                    "GET",
                    "/",
                    vec!["Host: outside.invalid"],
                    "403 Forbidden",
                    "origin_rejected",
                ),
                (
                    "GET",
                    "/api/recovery",
                    vec![],
                    "401 Unauthorized",
                    "recovery_session_required",
                ),
                (
                    "POST",
                    "/api/recovery/reset",
                    vec!["Content-Length: 999"],
                    "401 Unauthorized",
                    "recovery_session_required",
                ),
                (
                    "POST",
                    "/api/recovery/reset",
                    vec!["X-EMP-Recovery: recovery-fixture", "Content-Length: 999"],
                    "400 Bad Request",
                    "invalid_request",
                ),
                (
                    "POST",
                    "/api/recovery/reset",
                    vec!["X-EMP-Recovery: recovery-fixture", "Content-Length: 0"],
                    "409 Error",
                    "confirmation_required",
                ),
            ] {
                let wire = request(port, method, path, &headers, b"");
                let reply = exchange_wire(address, &wire, false).await;
                wire_contract::assert_json_reply(path, &reply, status, &json!({"error":code}));
            }
            assert!(
                exchange_wire(address, b"PUT / HTTP/1.1\r\n\r\n", false)
                    .await
                    .is_empty()
            );
            let module_route = request(port, "PUT", "/api/module-runtime/demo/", &[], b"");
            assert!(
                exchange_wire(address, &module_route, false)
                    .await
                    .is_empty()
            );
            let exit = request(
                port,
                "POST",
                "/api/recovery/exit",
                &["X-EMP-Recovery: recovery-fixture", "Content-Length: 0"],
                b"",
            );
            let reply = exchange_wire(address, &exit, false).await;
            wire_contract::assert_json_reply(
                "/api/recovery/exit",
                &reply,
                "200 OK",
                &json!({"exiting":true}),
            );
            control.stop(crate::server::ShutdownReason::Requested);
            server.await.unwrap().unwrap();
        });
}

#[test]
fn rejected_routing_uri_returns_not_found() {
    Fixture::new().run_wire(async |fixture, endpoint| {
        let target = "example.invalid[";
        assert!(axum::http::Uri::try_from(target).is_err());
        assert_entry_error(
            endpoint,
            &request(fixture.port(), "GET", target, &[], b""),
            404,
        )
        .await;
    });
}

#[test]
fn http_router_receives_original_query() {
    let fixture = Fixture::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let listener = tokio::net::TcpListener::bind(LOOPBACK).await.unwrap();
            let address = listener.local_addr().unwrap();
            let state = &fixture.server.state;
            let router = Router::new().fallback(|uri: axum::http::Uri| async move {
                JsonReply::new(
                    200,
                    json!({"path": uri.path(), "query": uri.query()}).to_string(),
                )
            });
            let endpoint = tokio::spawn(crate::server::serve(
                listener,
                router,
                state.lifecycle.clone(),
                Some(state.clone()),
            ));
            for (target, path, query) in [
                ("/healthz?k=a+b&k=%2f?", "/healthz", Some("k=a+b&k=%2f?")),
                ("/healthz?", "/healthz", Some("")),
                ("/healthz", "/healthz", None),
                ("/path#x?<y>=a#b\"", "/path%23x", Some("%3Cy%3E=a%23b%22")),
            ] {
                let wire = request(fixture.port(), "GET", target, &[], b"");
                let reply = exchange_wire(address, &wire, false).await;
                assert_status(&reply, 200);
                assert_eq!(
                    json_body(&reply),
                    json!({"path": path, "query": query}),
                    "{target}"
                );
            }
            state.request_shutdown();
            endpoint.await.unwrap().unwrap();
        });
    fixture.server.shutdown().unwrap();
}

#[test]
fn hyper_parse_rejection_uses_emp_response_before_dispatch() {
    Fixture::new().run(async |fixture, address| {
        let wire = request(
            fixture.port(),
            "GET",
            "/healthz",
            &["Invalid Header: x"],
            b"",
        );

        let reply = assert_wire(address, &wire, false, 400).await;
        wire_contract::assert_invalid_head(&reply);
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
