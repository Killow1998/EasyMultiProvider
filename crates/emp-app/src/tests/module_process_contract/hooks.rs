use super::{budget, fixture};
use crate::{
    lifecycle::ServerHandle,
    modules::{hooks::HookOperation, process::ModuleProcess},
    server::{
        http::tests::{assert_wire, json_body, json_request, request},
        raw_tcp::{RawClient, read_websocket_frame, websocket_frame, wire_timeout},
    },
};
use emp_modules::{
    manifest::{HookPhase, ModuleManifest},
    peer::{CallError, Handlers, Peer, Side},
};
use emp_test_support::{Deadline, fixture_dir, test_timeout};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{self, BufReader},
    net::TcpListener,
    path::Path,
    sync::{Mutex, mpsc},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, sync::mpsc as events, time::timeout};

const PHASES: [HookPhase; 6] = [
    HookPhase::Authenticate,
    HookPhase::ProjectAccounts,
    HookPhase::ResolveRoute,
    HookPhase::CheckAccess,
    HookPhase::ProjectCatalog,
    HookPhase::CallFinished,
];
const BODY: &[u8] = br#"{"model":"demo/model","input":"fixture"}"#;
const BEARER: &str = "Authorization: Bearer hook-device";

pub(super) fn child(output: File) {
    let manifest: Value =
        serde_json::from_str(&std::env::var("EMP_TEST_HOOK_MANIFEST").unwrap()).unwrap();
    let behaviors: Value =
        serde_json::from_str(&std::env::var("EMP_TEST_HOOK_BEHAVIORS").unwrap()).unwrap();
    let (release, gated) = mpsc::channel();
    let gated = std::sync::Arc::new(Mutex::new(gated));
    let mut handlers = Handlers::default();
    handlers.register("module.initialize", move |_, _, _| Ok(manifest.clone()));
    handlers.register("module.release", move |_, _, _| {
        release.send(()).unwrap();
        Ok(Value::Null)
    });
    for phase in PHASES {
        let behavior = behaviors[phase.method()].clone();
        let gated = gated.clone();
        handlers.register(phase.method(), move |params, token, peer| {
            peer.call(
                "host.hook",
                json!({"method":phase.method(),"params":params}),
                budget(),
            )
            .unwrap();
            if matches!(behavior.as_str(), Some("exit" | "block")) {
                gated
                    .lock()
                    .unwrap()
                    .recv_timeout(test_timeout(Duration::from_secs(25)).duration())
                    .unwrap();
                if behavior == "exit" {
                    std::process::exit(0);
                }
                peer.call(
                    "host.hook",
                    json!({"method":"cancelled","params":token.is_cancelled()}),
                    budget(),
                )
                .unwrap();
            }
            if behavior.is_object() {
                Ok(behavior.clone())
            } else {
                Ok(json!({"status":"declined"}))
            }
        });
    }
    Peer::connect(Side::Module, BufReader::new(io::stdin()), output, handlers)
        .unwrap()
        .wait()
        .unwrap();
}

fn with_server(check: impl AsyncFnOnce(&ServerHandle, &Path, TcpListener)) {
    let dir = fixture_dir().unwrap();
    let config = dir.path().join("config.json");
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    upstream.set_nonblocking(true).unwrap();
    std::fs::write(
        &config,
        json!({
            "providers":[{"id":"demo","base_url":format!("http://{}/v1",upstream.local_addr().unwrap()),
                "protocol":"chat_completions","auth_mode":"api_key","api_key":"fixture"}],
            "models":[{"id":"demo/model","provider":"demo","upstream_id":"upstream-model","enabled":true}]
        }).to_string(),
    ).unwrap();
    let server =
        ServerHandle::start_for_test([127, 0, 0, 1].into(), &config, dir.path().join("auth.json"))
            .unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(check(&server, dir.path(), upstream));
    server.shutdown().unwrap();
}

async fn install(
    server: &ServerHandle,
    root: &Path,
    id: &str,
    generation: u64,
    phases: &[HookPhase],
    behaviors: Value,
    seen: &events::UnboundedSender<Value>,
) -> ModuleProcess {
    let manifest = json!({
        "id":id, "module_version":"1.0.0", "host_api":{"major":3,"min_minor":0},
        "contributions": phases.iter().map(|phase| json!({"type":"hook","phase":phase})).collect::<Vec<_>>()
    });
    let mut command = fixture("hooks");
    command.env("EMP_TEST_HOOK_MANIFEST", manifest.to_string());
    command.env("EMP_TEST_HOOK_BEHAVIORS", behaviors.to_string());
    let state = server.state.clone();
    let module = id.to_owned();
    let seen = seen.clone();
    let mut handlers = Handlers::default();
    handlers.register("host.hook", move |mut call, _, _| {
        assert!(state.backend.modules.registry.get(&module).is_some());
        call["module_id"] = json!(module);
        seen.send(call).unwrap();
        Ok(Value::Null)
    });
    let state = server.state.clone();
    let module = id.to_owned();
    let (process, manifest): (_, ModuleManifest) = ModuleProcess::start_with_connector(
        command,
        root,
        handlers,
        budget(),
        Peer::connect,
        move || state.backend.modules.registry.revoke(&module, generation),
    )
    .await
    .unwrap();
    server
        .state
        .backend
        .modules
        .registry
        .publish(
            manifest,
            generation,
            process.connection(),
            server.local_addr().port(),
            server.state.backend.gateway_key.len(),
        )
        .unwrap();
    process
}

async fn event(seen: &mut events::UnboundedReceiver<Value>) -> Value {
    timeout(
        test_timeout(Duration::from_secs(10)).duration(),
        seen.recv(),
    )
    .await
    .unwrap()
    .unwrap()
}

async fn release(process: &ModuleProcess) {
    let connection = process.connection();
    tokio::task::spawn_blocking(move || connection.call("module.release", Value::Null, budget()))
        .await
        .unwrap()
        .unwrap();
}

fn assert_no_upstream(upstream: &TcpListener) {
    assert_eq!(
        upstream.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

fn calls(seen: &mut events::UnboundedReceiver<Value>, expected: &[(&str, HookPhase)]) -> String {
    let mut operation = None;
    for (module, phase) in expected {
        let call = seen.try_recv().unwrap();
        assert_eq!(
            (&call["module_id"], &call["method"]),
            (&json!(module), &json!(phase.method()))
        );
        let id = call["params"]["operation_id"]
            .as_str()
            .expect("operation ID")
            .to_owned();
        assert_eq!(operation.get_or_insert(id.clone()), &id);
    }
    assert!(seen.try_recv().is_err());
    operation.unwrap()
}

#[test]
fn hooks_count_actual_http_ws_and_catalog_phases_by_operation() {
    use HookPhase::*;
    with_server(async |server, root, upstream| {
        let (report, mut seen) = events::unbounded_channel();
        let port = server.local_addr().port();
        let address = server.local_addr();
        let session = format!("X-EMP-Session: {}", server.session_token());
        let later = install(
            server,
            root,
            "zeta",
            1,
            &[CheckAccess],
            json!({
                (CheckAccess.method()):{"status":"claimed","value":false}
            }),
            &report,
        )
        .await;
        let skipped = install(
            server,
            root,
            "zzzz",
            1,
            &[ResolveRoute, CheckAccess],
            Value::Null,
            &report,
        )
        .await;
        let first = install(
            server,
            root,
            "alpha",
            1,
            &[
                Authenticate,
                ProjectAccounts,
                ResolveRoute,
                CheckAccess,
                ProjectCatalog,
                CallFinished,
            ],
            json!({
                (ProjectAccounts.method()):{"status":"claimed","value":[{"id":"alias","source_id":"missing"}]},
                (ResolveRoute.method()):{"status":"claimed","value":"demo/model"},
                (CheckAccess.method()):{"status":"claimed","value":true}
            }),
            &report,
        )
        .await;
        let mut operation_ids = std::collections::BTreeSet::new();
        let plain = json_request(port, "/v1/responses", &[BEARER], BODY);
        assert_wire(address, &plain, false, 401).await;
        let denied_auth = seen.try_recv().unwrap();
        assert_eq!(denied_auth["method"], Authenticate.method());
        assert_eq!(
            denied_auth["params"]["input"]["bearer_sha256"],
            format!(
                "{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(b"hook-device")
            )
        );
        assert!(seen.try_recv().is_err());

        let wire = json_request(port, "/v1/responses", &[BEARER, &session], BODY);
        assert_wire(address, &wire, false, 403).await;
        let phases = [
            ("alpha", ProjectAccounts),
            ("alpha", ResolveRoute),
            ("alpha", CheckAccess),
            ("zeta", CheckAccess),
        ];
        let http = [&[("alpha", Authenticate)][..], &phases].concat();
        assert!(operation_ids.insert(calls(&mut seen, &http)));

        let device = install(server, root, "alpha", 2, &[
            Authenticate, ProjectAccounts, ResolveRoute, CheckAccess, ProjectCatalog, CallFinished,
        ], json!({
            (Authenticate.method()):{"status":"claimed","value":{"id":"device","name":"Fixture"}},
            (ResolveRoute.method()):{"status":"claimed","value":"demo/model"},
        }), &report).await;
        let wire = request(
            port,
            "GET",
            "/v1/responses",
            &[
                BEARER,
                "Upgrade: websocket",
                "Connection: Upgrade",
                "Sec-WebSocket-Version: 13",
                "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
            ],
            b"",
        );
        let mut websocket = RawClient::request(address, &wire).await;
        assert!(websocket.head.starts_with("HTTP/1.1 101 "));
        assert!(operation_ids.insert(calls(&mut seen, &[("alpha", Authenticate)])));
        for _ in 0..2 {
            websocket
                .io
                .write_all(&websocket_frame(
                    0x81,
                    Some([1, 2, 3, 4]),
                    br#"{"type":"response.create","model":"demo/model","input":"fixture"}"#,
                ))
                .await
                .unwrap();
            let deadline = Deadline::after(wire_timeout());
            loop {
                let (opcode, payload) =
                    read_websocket_frame(&mut websocket.io, false, &deadline).await;
                assert_eq!(opcode, 0x81);
                let frame: Value = serde_json::from_slice(&payload).unwrap();
                if frame["type"] == "error" {
                    assert_eq!(frame["status"], 403);
                    break;
                }
                assert_eq!(frame["type"], "codex.response.metadata");
            }
            assert!(operation_ids.insert(calls(&mut seen, &phases)));
        }
        drop(websocket);

        let read = request(port, "GET", "/v1/models", &[BEARER], b"");
        let listing = json_body(&assert_wire(address, &read, false, 200).await);
        assert!(
            listing["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "demo/model")
        );
        assert!(operation_ids.insert(calls(
            &mut seen,
            &[("alpha", Authenticate), ("alpha", ProjectCatalog)]
        )));
        let config = request(port, "GET", "/api/config", &[&session], b"");
        assert_wire(address, &config, false, 200).await;
        assert!(operation_ids.insert(calls(&mut seen, &[("alpha", ProjectCatalog)])));
        assert_no_upstream(&upstream);

        first.stop().await.unwrap();
        device.stop().await.unwrap();
        later.stop().await.unwrap();
        skipped.stop().await.unwrap();
    });
}

#[test]
fn hook_route_exit_fails_closed_and_preserves_new_generation() {
    use HookPhase::*;
    with_server(async |server, root, upstream| {
        let (report, mut seen) = events::unbounded_channel();
        let address = server.local_addr();
        let session = format!("X-EMP-Session: {}", server.session_token());
        let wire = json_request(address.port(), "/v1/responses", &[&session], BODY);
        let old = install(
            server,
            root,
            "router",
            1,
            &[ResolveRoute],
            json!({(ResolveRoute.method()):"exit"}),
            &report,
        )
        .await;
        let running = tokio::spawn(async move { assert_wire(address, &wire, false, 503).await });
        assert_eq!(event(&mut seen).await["method"], ResolveRoute.method());

        let new = install(
            server,
            root,
            "router",
            2,
            &[ResolveRoute, CheckAccess],
            json!({
                (ResolveRoute.method()):{"status":"claimed","value":"demo/model"},
                (CheckAccess.method()):{"status":"claimed","value":false}
            }),
            &report,
        )
        .await;
        let connection = old.connection();
        let result = tokio::task::spawn_blocking(move || {
            connection.call("module.release", Value::Null, budget())
        })
        .await
        .unwrap();
        assert!(matches!(result, Ok(_) | Err(CallError::Disconnected)));
        let reply = json_body(&running.await.unwrap());
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap()
                .contains("provider for model")
        );
        assert!(old.stop().await.unwrap().success());

        let wire = json_request(address.port(), "/v1/responses", &[&session], BODY);
        assert_wire(address, &wire, false, 403).await;
        calls(
            &mut seen,
            &[("router", ResolveRoute), ("router", CheckAccess)],
        );
        assert_no_upstream(&upstream);
        new.stop().await.unwrap();
    });
}

#[test]
fn hook_access_timeout_and_client_disconnect_send_cancel_without_upstream() {
    use HookPhase::*;
    with_server(async |server, root, upstream| {
        let (report, mut seen) = events::unbounded_channel();
        let address = server.local_addr();
        let session = format!("X-EMP-Session: {}", server.session_token());
        let wire = json_request(address.port(), "/v1/responses", &[&session], BODY);
        let process = install(
            server,
            root,
            "access",
            1,
            &[CheckAccess],
            json!({(CheckAccess.method()):"block"}),
            &report,
        )
        .await;
        let timeout_wire = wire.clone();
        let running =
            tokio::spawn(async move { assert_wire(address, &timeout_wire, false, 403).await });
        assert_eq!(event(&mut seen).await["method"], CheckAccess.method());
        running.await.unwrap();
        release(&process).await;
        let cancelled = event(&mut seen).await;
        assert_eq!(cancelled["method"], "cancelled");
        assert_eq!(cancelled["params"], true);

        let (failed, failure) = tokio::sync::oneshot::channel();
        server
            .state
            .backend
            .diagnostics
            .journal
            .gate_next_event("module_hook_failed", move || {
                let _ = failed.send(());
            });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client.write_all(&wire).await.unwrap();
        let entered = event(&mut seen).await;
        assert_eq!(entered["method"], CheckAccess.method());
        drop(client);
        timeout(test_timeout(Duration::from_secs(10)).duration(), failure)
            .await
            .unwrap()
            .unwrap();
        release(&process).await;
        let cancelled = event(&mut seen).await;
        assert_eq!(cancelled["method"], "cancelled");
        assert_eq!(cancelled["params"], true);
        let id = entered["params"]["operation_id"].clone();
        let deadline = Deadline::after(test_timeout(Duration::from_secs(10)));
        crate::tests::wait_for(&deadline, || {
            crate::tests::live_journal(root).iter().any(|record| {
                record["event"] == "module_hook_failed"
                    && record["fields"]["operation_id"] == id
                    && record["fields"]["reason"] == "Cancelled"
            })
        });
        assert_no_upstream(&upstream);
        process.stop().await.unwrap();
    });
}

#[test]
fn hook_optional_projection_failure_keeps_base_view_and_finish_is_best_effort() {
    use HookPhase::*;
    with_server(async |server, root, _| {
        let (report, mut seen) = events::unbounded_channel();
        let address = server.local_addr();
        let wire = request(address.port(), "GET", "/v1/models", &[], b"");
        let base = json_body(&assert_wire(address, &wire, false, 200).await);
        let bad = install(server, root, "alpha", 1, &[ProjectCatalog, CallFinished], json!({
            (ProjectCatalog.method()):{"status":"claimed","value":{"entries":[{"id":"demo/model","display_name":"rewrite"}],"hidden_ids":["demo/model"]}}
        }), &report).await;
        let live = install(
            server,
            root,
            "zeta",
            1,
            &[CallFinished],
            Value::Null,
            &report,
        )
        .await;
        let after = json_body(&assert_wire(address, &wire, false, 200).await);
        assert_eq!(after, base);
        calls(&mut seen, &[("alpha", ProjectCatalog)]);

        let operation =
            HookOperation::capture(&server.state, Some("finished-fixture"), None).unwrap();
        bad.stop().await.unwrap();
        operation.notify_finished(json!({"result":"success"}));
        let completed = event(&mut seen).await;
        assert_eq!(completed["module_id"], "zeta");
        assert_eq!(completed["method"], CallFinished.method());
        assert_eq!(
            completed["params"],
            json!({"operation_id":"finished-fixture","caller_id":null,"input":{"result":"success"}})
        );
        assert!(seen.try_recv().is_err());
        live.stop().await.unwrap();
    });
}
