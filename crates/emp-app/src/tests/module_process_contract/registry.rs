use super::{budget, fixture};
use crate::{
    lifecycle::ServerHandle,
    modules::process::ModuleProcess,
    server::http::tests::{
        assert_status, assert_wire, connect_wire_from, json_body, json_request, request,
    },
};
use emp_modules::{
    manifest::{HookPhase, ModuleManifest},
    peer::{CallError, Handlers},
};
use emp_test_support::{fixture_dir, test_timeout};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PREFIX: &str = "/api/module-runtime/fixture-module";

async fn start(
    state_dir: &Path,
    id: &str,
    contributions: Value,
) -> (ModuleProcess, ModuleManifest) {
    let manifest = json!({
        "id": id, "module_version": "1.0.0",
        "host_api": {"major": 3, "min_minor": 0}, "contributions": contributions,
    });
    let mut command = fixture("registry");
    command.env("EMP_TEST_API3_REGISTRY_MANIFEST", manifest.to_string());
    ModuleProcess::start(command, state_dir, Handlers::default(), budget())
        .await
        .unwrap()
}

fn with_registry_server(check: impl AsyncFnOnce(&ServerHandle, &Path)) {
    let dir = fixture_dir().unwrap();
    let server = ServerHandle::start_for_test(
        [127, 0, 0, 1].into(),
        &dir.path().join("config.json"),
        dir.path().join("auth.json"),
    )
    .unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(check(&server, dir.path()));
    server.shutdown().unwrap();
}

#[test]
fn module_routes_match_literal_paths_and_authorize_before_handoff() {
    with_registry_server(check_routes);
}

async fn check_routes(server: &ServerHandle, state_dir: &Path) {
    let address = server.local_addr();
    let port = address.port();
    let send = async |method, path, headers: &[&str], status| {
        let path = format!("{PREFIX}{path}");
        assert_wire(
            address,
            &request(port, method, &path, headers, b""),
            false,
            status,
        )
        .await
    };
    let state = &server.state;
    let registry = &state.backend.modules.registry;
    let records = &state.lifecycle.test_hooks.module_routes;
    let session = format!("X-EMP-Session: {}", server.session_token());
    let key_len = state.backend.gateway_key.len();
    let local_key = format!("Authorization: Bearer {}", state.backend.gateway_key);
    let origin = format!("Origin: http://localhost:{port}");
    let (old, manifest) = start(state_dir, "fixture-module", json!([
        {"type":"http_route","id":"root","method":"GET","path":"/","auth":"management"},
        {"type":"http_route","id":"put","method":"PUT","path":"/items/%61","auth":"proxy"},
        {"type":"http_route","id":"patch","method":"PATCH","path":"/items/%61","auth":"local_proxy"},
        {"type":"http_route","id":"post","method":"POST","path":"/items","auth":"management"},
        {"type":"http_route","id":"delete","method":"DELETE","path":"/items","auth":"management"},
        {"type":"hook","phase":"check_access"}, {"type":"catalog"},
        {"type":"panel","id":"status","entry":"status.js","revision":"1"},
    ])).await;
    registry
        .publish(manifest, 1, old.connection(), port, key_len)
        .unwrap();
    let declarations = registry.get("fixture-module").unwrap();
    assert!(declarations.hooks.contains(&HookPhase::CheckAccess));
    assert!(declarations.catalog);
    let panel = &declarations.panels["status"];
    assert_eq!(
        (panel.entry.as_str(), panel.revision.as_str()),
        ("status.js", "1")
    );

    let device = json_request(
        port,
        "/api/gateway/clients",
        &[&session],
        br#"{"name":"Device"}"#,
    );
    let issued = json_body(&assert_wire(address, &device, false, 200).await);
    let device_key = format!(
        "Authorization: Bearer {}",
        issued["api_key"].as_str().unwrap()
    );
    let device_id = issued["id"].as_str().unwrap();
    for (method, path, headers, status) in [
        ("GET", "", Vec::<&str>::new(), 404),
        ("GET", "/undeclared", vec![], 404),
        ("PUT", "/items/a", vec![&device_key], 404),
        ("PATCH", "/", vec![], 405),
        ("PUT", "/", vec![], 405),
        ("HEAD", "/", vec![], 405),
        ("OPTIONS", "/", vec![], 405),
        ("GET", "/", vec![], 401),
        ("GET", "/", vec![&device_key], 401),
        (
            "GET",
            "/",
            vec![&session, "Origin: https://foreign.invalid"],
            403,
        ),
        ("PUT", "/items/%61", vec![], 401),
        ("PUT", "/items/%61", vec![&device_key, &device_key], 401),
        (
            "PUT",
            "/items/%61",
            vec![&device_key, "Origin: https://foreign.invalid"],
            403,
        ),
        ("PATCH", "/items/%61", vec![], 401),
        ("PATCH", "/items/%61", vec![&device_key], 401),
        (
            "PATCH",
            "/items/%61",
            vec!["Authorization: Bearer wrong"],
            401,
        ),
        ("PATCH", "/items/%61", vec![&local_key, &local_key], 401),
        ("PATCH", "/items/%61", vec![&local_key, &origin], 403),
        (
            "PATCH",
            "/items/%61",
            vec![&local_key, "Host: foreign.invalid"],
            403,
        ),
    ] {
        let reply = send(method, path, &headers, status).await;
        if status == 405 {
            assert!(String::from_utf8_lossy(&reply).contains("\r\nAllow: GET\r\n"));
        }
    }
    let wire = request(
        port,
        "PATCH",
        &format!("{PREFIX}/items/%61"),
        &[&local_key, "X-EMP-Peer-IP: 127.0.0.1"],
        b"",
    );
    let remote_reply =
        tokio::time::timeout(test_timeout(Duration::from_secs(15)).duration(), async {
            let (mut client, connection) =
                connect_wire_from(state, Some([192, 0, 2, 1].into())).await;
            client.write_all(&wire).await.unwrap();
            let mut reply = Vec::new();
            client.read_to_end(&mut reply).await.unwrap();
            connection.await.unwrap();
            reply
        })
        .await
        .unwrap();
    assert_status(&remote_reply, 403);
    assert!(records.lock().unwrap().is_empty());

    for (method, path, header, id, caller) in [
        ("GET", "/?x=/undeclared", &session, "root", "local"),
        ("POST", "/items", &session, "post", "local"),
        ("DELETE", "/items", &session, "delete", "local"),
        ("PUT", "/items/%61?q=1", &device_key, "put", device_id),
        ("PATCH", "/items/%61", &local_key, "patch", "local"),
    ] {
        send(method, path, &[header], 503).await;
        assert_eq!(
            std::mem::take(&mut *records.lock().unwrap()),
            [("fixture-module".into(), id.to_owned(), 1, caller.to_owned())]
        );
    }
    let reply = send("POST", "/items/%61", &[], 405).await;
    assert!(String::from_utf8_lossy(&reply).contains("\r\nAllow: PATCH, PUT\r\n"));

    let (invalid, manifest) = start(state_dir, "fixture-module", json!([
        {"type":"http_route","id":"invalid","method":"GET","path":"/cannot match","auth":"management"},
    ])).await;
    assert!(
        registry
            .publish(manifest, 2, invalid.connection(), port, key_len)
            .is_err()
    );
    invalid.stop().await.unwrap();
    send("GET", "/", &[&session], 503).await;
    assert_eq!(
        std::mem::take(&mut *records.lock().unwrap()),
        [("fixture-module".into(), "root".into(), 1, "local".into())]
    );

    let (new, manifest) = start(state_dir, "fixture-module", json!([
        {"type":"http_route","id":"new-put","method":"PUT","path":"/items/%61","auth":"proxy"},
        {"type":"http_route","id":"new-patch","method":"PATCH","path":"/items/%61","auth":"local_proxy"},
    ])).await;
    registry
        .publish(manifest, 3, new.connection(), port, key_len)
        .unwrap();
    assert!(matches!(
        old.connection().call("module.exit", Value::Null, budget()),
        Err(CallError::Disconnected)
    ));
    assert!(old.stop().await.unwrap().success());
    registry.revoke("fixture-module", 1);
    send("GET", "/", &[&session], 404).await;
    send("PUT", "/items/%61", &[&device_key], 503).await;
    assert_eq!(
        std::mem::take(&mut *records.lock().unwrap()),
        [(
            "fixture-module".into(),
            "new-put".into(),
            3,
            device_id.into()
        )]
    );

    state.lifecycle.updates().close();
    for (method, header) in [("PUT", &device_key), ("PATCH", &local_key)] {
        let reply = send(method, "/items/%61", &[header], 503).await;
        assert_eq!(json_body(&reply)["error"]["code"], "updating");
    }
    registry.revoke("fixture-module", 3);
    send("PUT", "/items/%61", &[&device_key], 404).await;
    assert!(records.lock().unwrap().is_empty());
    new.stop().await.unwrap();
}

#[test]
fn module_routes_fit_the_authenticated_request_head_before_publication() {
    with_registry_server(async |server, state_dir| {
        let address = server.local_addr();
        let port = address.port();
        let state = &server.state;
        let registry = &state.backend.modules.registry;
        let records = &state.lifecycle.test_hooks.module_routes;
        let key_len = state.backend.gateway_key.len();
        let host = format!("Host:127.0.0.1:{port}");
        let session = format!("X-EMP-Session:{}", server.session_token());
        let wire = |path: &str| {
            request(
                port,
                "GET",
                &format!("{PREFIX}{path}"),
                &[&host, &session],
                b"",
            )
        };
        let exact = format!("/{}", "x".repeat(8192 + 4 - wire("/").len()));
        let oversized = format!("{exact}x");
        let contribution = |id: &str, path: &str| {
            json!([
                {"type":"http_route","id":id,"method":"GET","path":path,"auth":"management"},
            ])
        };

        let (old, manifest) = start(state_dir, "fixture-module", contribution("old", "/")).await;
        registry
            .publish(manifest, 1, old.connection(), port, key_len)
            .unwrap();
        let (invalid, manifest) = start(
            state_dir,
            "fixture-module",
            contribution("oversized", &oversized),
        )
        .await;
        assert!(
            registry
                .publish(manifest, 2, invalid.connection(), port, key_len)
                .is_err()
        );
        invalid.stop().await.unwrap();
        assert_wire(address, &wire(&oversized), false, 400).await;
        assert_wire(address, &wire("/"), false, 503).await;
        assert_eq!(
            std::mem::take(&mut *records.lock().unwrap()),
            [("fixture-module".into(), "old".into(), 1, "local".into())],
        );

        let (new, manifest) =
            start(state_dir, "fixture-module", contribution("exact", &exact)).await;
        registry
            .publish(manifest, 3, new.connection(), port, key_len)
            .unwrap();
        assert_wire(address, &wire(&exact), false, 503).await;
        assert_eq!(
            std::mem::take(&mut *records.lock().unwrap()),
            [("fixture-module".into(), "exact".into(), 3, "local".into())],
        );
        old.stop().await.unwrap();
        new.stop().await.unwrap();
    });
}

#[test]
fn module_routes_dispatch_only_to_the_named_module() {
    with_registry_server(async |server, state_dir| {
        let address = server.local_addr();
        let port = address.port();
        let state = &server.state;
        let registry = &state.backend.modules.registry;
        let records = &state.lifecycle.test_hooks.module_routes;
        let key_len = state.backend.gateway_key.len();
        let session = format!("X-EMP-Session: {}", server.session_token());
        let mut processes = Vec::new();
        for (module, route) in [("a", "route-a"), ("b", "route-b")] {
            let (process, manifest) = start(
                state_dir,
                module,
                json!([
                    {"type":"http_route","id":route,"method":"GET","path":"/x","auth":"management"},
                ]),
            )
            .await;
            registry
                .publish(manifest, 1, process.connection(), port, key_len)
                .unwrap();
            processes.push(process);
        }

        for (module, route) in [("a", "route-a"), ("b", "route-b")] {
            let path = format!("/api/module-runtime/{module}/x");
            assert_wire(
                address,
                &request(port, "GET", &path, &[&session], b""),
                false,
                503,
            )
            .await;
            assert_eq!(
                std::mem::take(&mut *records.lock().unwrap()),
                [(module.into(), route.into(), 1, "local".into())],
            );
        }
        for (method, path, headers) in [
            ("PUT", "/api/module-runtime/demo/", [].as_slice()),
            (
                "GET",
                "/api/module-runtime/unknown/x",
                [&*session].as_slice(),
            ),
            (
                "GET",
                "/api/module-runtime/a/../b/x",
                [&*session].as_slice(),
            ),
            (
                "GET",
                "/api/module-runtime/a/%2E%2E/b/x",
                [&*session].as_slice(),
            ),
        ] {
            assert_wire(
                address,
                &request(port, method, path, headers, b""),
                false,
                404,
            )
            .await;
            assert!(records.lock().unwrap().is_empty());
        }
        for process in processes {
            process.stop().await.unwrap();
        }
    });
}
