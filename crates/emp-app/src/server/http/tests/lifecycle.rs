use super::*;

#[cfg(unix)]
#[test]
fn healthz_uses_the_last_available_descriptor() {
    const CHILD: &str = "EMP_TEST_HEALTHZ_LAST_DESCRIPTOR";
    if std::env::var_os(CHILD).is_none() {
        let child = emp_test_support::OwnedChild::output(
            std::process::Command::new("sh")
                .args(["-c", "ulimit -n 128 && exec \"$@\"", "emp-fd-limit"])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "server::http::tests::lifecycle::healthz_uses_the_last_available_descriptor",
                    "--nocapture",
                ])
                .env(CHILD, "1"),
            Deadline::after(test_timeout(Duration::from_secs(10))),
        )
        .unwrap();
        assert!(
            child.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr),
        );
        assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed"));
        return;
    }

    use std::io::Read;
    let fixture = Fixture::new();
    let client = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    client
        .set_read_timeout(Some(crate::server::raw_tcp::wire_timeout().duration()))
        .unwrap();
    let mut fillers = Vec::new();
    loop {
        match std::fs::File::open("/dev/null") {
            Ok(file) => fillers.push(file),
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                break;
            }
        }
    }
    drop(fillers.pop().expect("one descriptor for accept"));
    client.connect(&fixture.server.local_addr().into()).unwrap();
    let mut client: std::net::TcpStream = client.into();
    client
        .write_all(&request(fixture.port(), "GET", "/healthz", &[], b""))
        .unwrap();
    let mut reply = Vec::new();
    let result = client.read_to_end(&mut reply);
    drop(fillers);
    result.unwrap();
    wire_contract::assert_json_reply(
        "/healthz with one available fd",
        &reply,
        "200 OK",
        &json!({"status":"ok"}),
    );
    fixture.server.shutdown().unwrap();
}

#[test]
fn raw_management_paths_follow_route_matching_without_writes() {
    Fixture::new().run_wire(async |fixture, endpoint| {
        let port = fixture.port();
        let session = fixture.session();
        for path in [
            "/api/quit#x",
            "/api/integration/enable#x",
            "/api/updates/check#x",
            "/api/language#x",
            "/api/session#x",
            "/api/config#x",
            "/api/catalog/refresh#x",
            "/api/modules#x",
            "/api/gateway/clients#x",
            "/api/migration/export#x",
            "/api%2Fquit",
            "/api//quit",
            "/api/./quit",
            "/api/a/../quit",
            "/api/quit/",
        ] {
            assert_entry_error(endpoint, &json_request(port, path, &[&session], b"{}"), 404).await;
        }
        let unknown_operation = json!({"error":{"code":"login_unknown_operation","message":"login_unknown_operation","origin":"emp"}});
        for suffix in [
            "x",
            "x<y",
            "x>y",
            "x`y",
            "x%20y",
            "x\"y",
            "x{y}",
            "x\\y",
            "x#y",
            "x%23y",
            "x%ZZ",
            "x/y",
            "x\x7fy",
            "x?probe=\"y\"",
            "é",
        ] {
            for route in ["/api/native/login/start", "/api/accounts/login/finish"] {
                let path = format!("{route}#{suffix}");
                let reply = exchange_wire(
                    endpoint,
                    &json_request(port, &path, &[&session], b"{}"),
                    false,
                )
                .await;
                wire_contract::assert_json_reply(
                    &path, &reply, "409 Conflict", &unknown_operation,
                );
            }
            for path in [
                format!("/api/quit#{suffix}"),
                format!("/api/integration/enable#{suffix}"),
            ] {
                assert_entry_error(
                    endpoint,
                    &json_request(port, &path, &[&session], b"{}"),
                    404,
                )
                .await;
            }
            for (route, headers) in [
                ("/healthz", &[][..]),
                ("/assets/i18n.js", &[][..]),
                ("/api/integration", &[session.as_str()][..]),
                ("/api/config", &[session.as_str()][..]),
            ] {
                for query in [format!("?probe={suffix}"), format!("?probe=ok#{suffix}")] {
                    let path = format!("{route}{query}");
                    let reply = exchange_wire(
                        endpoint,
                        &request(port, "GET", &path, headers, b""),
                        false,
                    )
                    .await;
                    match route {
                        "/healthz" => wire_contract::assert_json_reply(
                            &path, &reply, "200 OK", &json!({"status":"ok"}),
                        ),
                        "/assets/i18n.js" => {
                            let script = include_bytes!("../../../../web/i18n.js");
                            wire_contract::assert_reply(
                                &path,
                                &reply,
                                &wire_contract::asset_head!("text/javascript; charset=utf-8", script.len()),
                                script,
                            );
                        }
                        _ => {
                            wire_contract::assert_settings_read(route, &reply, fixture);
                        }
                    }
                }
            }
        }
        for path in [
            "/api/native/login/start#x y",
            "/api/native/login/start?probe=x y",
        ] {
            let reply = exchange_wire(
                endpoint,
                &json_request(port, path, &[&session], b"{}"),
                false,
            )
            .await;
            wire_contract::assert_json_reply(
                path, &reply, "400 Bad Request",
                &json!({"error":{"message":"invalid HTTP request"}}),
            );
        }
        for (headers, status) in [
            (vec![], 401),
            (vec![session.as_str(), "Origin: https://other.invalid"], 403),
        ] {
            for path in ["/api/quit#x", "/api/native/login/start#x<y"] {
                assert_entry_error(endpoint, &json_request(port, path, &headers, b"{}"), status)
                    .await;
            }
        }
        let reply = exchange_wire(
            endpoint,
            &request(port, "GET", "/healthz", &[], b""),
            false,
        )
        .await;
        wire_contract::assert_json_reply("/healthz", &reply, "200 OK", &json!({"status":"ok"}));
    });
}

#[test]
fn unknown_provider_read_returns_not_found() {
    Fixture::new().run_wire(async |fixture, endpoint| {
        assert_entry_error(
            endpoint,
            &request(
                fixture.port(),
                "GET",
                "/api/providers/example/unknown",
                &[&fixture.session()],
                b"",
            ),
            404,
        )
        .await;
    });
}

#[cfg(unix)]
#[test]
fn wrong_methods_keep_status_before_and_during_update_drain() {
    Fixture::new().run_wire(async |fixture, endpoint| {
        let port = fixture.port();
        let session = fixture.session();
        for (method, path) in [
            ("GET", "/v1/responses/compact"),
            ("GET", "/v1/responses"),
            ("GET", "/v1/images/generations"),
            ("GET", "/v1/images/edits"),
            ("GET", "/v1/alpha/search"),
            ("GET", "/v1/decisions"),
            ("GET", "/v1/systemone"),
            ("GET", "/v1/live"),
            ("POST", "/v1/live/call-fixture"),
            ("POST", "/v1/models"),
            ("POST", "/assets/i18n.js"),
            ("POST", "/guide/index.html"),
            ("POST", "/api/integration"),
            ("GET", "/api/integration/enable"),
            ("GET", "/api/quit"),
            ("GET", "/api/session"),
            ("POST", "/api/accounts"),
            ("GET", "/api/native/login/start"),
            ("GET", "/api/language"),
            ("POST", "/api/usage"),
            ("POST", "/api/request-limits"),
            ("GET", "/api/catalog/refresh"),
            ("DELETE", "/api/providers/example/models"),
            ("POST", "/api/updates"),
            ("DELETE", "/api/modules"),
            ("POST", "/api/gateway"),
            ("POST", "/api/diagnostics"),
            ("GET", "/api/migration/export"),
        ] {
            assert_entry_error(
                endpoint,
                &request(port, method, path, &[&session], b""),
                404,
            )
            .await;
        }
        let team = request(port, "GET", "/api/team/publish_source", &[&session], b"");
        let reply = exchange_wire(endpoint, &team, false).await;
        wire_contract::assert_json_reply(
            "/api/team/publish_source", &reply, "404 Not Found",
            &json!({"error":{"code":"module_disabled","message":"This EMP module is not active","origin":"emp"}}),
        );
        let state = fixture.server.state.clone();
        assert!(
            tokio::task::spawn_blocking(move || {
                state
                    .lifecycle
                    .updates()
                    .drain_for_test(test_timeout(Duration::from_secs(5)).duration())
            })
            .await
            .unwrap()
        );
        for path in [
            "/v1/responses/compact",
            "/v1/responses",
            "/v1/alpha/search",
            "/api/quit",
        ] {
            let reply = exchange_wire(
                endpoint,
                &request(port, "DELETE", path, &[&session], b""),
                false,
            )
            .await;
            wire_contract::assert_updating_reply(path, &reply);
        }
    });
}

#[cfg(unix)]
#[test]
fn idle_delegated_websocket_does_not_hold_parent_update_lease() {
    use crate::{
        server::raw_tcp::RawClient,
        services::modules::aero::{CallLease, CallParent},
    };

    {
        Fixture::new().run_wire(async |fixture, address| {
            let state = &fixture.server.state;
            let calls = &state.backend.aero_callers;
            let parent = state.lifecycle.admission.acquire_request().unwrap();
            let delegation = calls
                .delegate(
                    Caller {
                        id: "local".into(),
                        name: String::new(),
                    },
                    None,
                    Some(CallLease {
                        connection: parent.slot().clone(),
                        work: Arc::new(state.lifecycle.updates().enter().unwrap()),
                    }),
                    || CallParent::Connection(parent.slot().clone()),
                )
                .unwrap();
            let auth = format!("Authorization: Bearer {}", delegation.key);
            let mut ws = RawClient::request(
                address,
                &request(
                    fixture.port(),
                    "GET",
                    "/v1/responses",
                    &[
                        &auth,
                        "Upgrade: websocket",
                        "Connection: Upgrade",
                        "Sec-WebSocket-Version: 13",
                        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
                    ],
                    b"",
                ),
            )
            .await;
            assert!(ws.head.starts_with("HTTP/1.1 101"), "{}", ws.head);
            drop(delegation);
            assert!(
                state.lifecycle.updates().drain_for_test(Duration::ZERO),
                "idle WebSocket kept the parent work active"
            );
            ws.io.shutdown().await.unwrap();
        });
    }
}

#[test]
fn lifecycle_and_unmatched_requests_keep_method_and_auth_policy() {
    Fixture::new().run_wire(async |fixture, endpoint| {
        let port = fixture.port();
        let session = fixture.session();
        for path in [
            "/api/integration/enable",
            "/api/integration/restore",
            "/api/integration/reload",
            "/api/integration/verify",
            "/api/updates/check",
            "/api/updates/install",
            "/api/quit",
            "/api/providers/balance",
        ] {
            let invalid = |headers: &[&str]| json_request(port, path, headers, b"{");
            assert_entry_error(endpoint, &invalid(&[]), 401).await;
            assert_entry_error(
                endpoint,
                &invalid(&[&session, "Origin: https://other.invalid"]),
                403,
            )
            .await;
            let reply = exchange_wire(endpoint, &invalid(&[&session]), false).await;
            wire_contract::assert_json_reply(
                path, &reply, "400 Bad Request",
                &json!({"error":{"message":"request body must be valid JSON: EOF while parsing an object at line 1 column 1","origin":"emp"}}),
            );
        }
        let bad_force = exchange_wire(
            endpoint,
            &json_request(
                port,
                "/api/providers/balance",
                &[&session],
                br#"{"force":"yes"}"#,
            ),
            false,
        )
        .await;
        wire_contract::assert_json_reply(
            "invalid balance force", &bad_force, "400 Bad Request",
            &json!({"error":{"message":"force must be boolean","origin":"emp"}}),
        );
        let missing_balance = json_request(port, "/api/providers/balance", &[&session], b"{}");
        {
            let address = endpoint;
            assert!(
                exchange_wire(address, &missing_balance, false)
                    .await
                    .is_empty()
            );
        }

        let unavailable = exchange_wire(
            endpoint,
            &json_request(port, "/api/updates/install", &[&session], b"{}"),
            false,
        )
        .await;
        wire_contract::assert_json_reply(
            "unavailable update", &unavailable, "409 Error",
            &json!({"error":{"code":"update_unavailable","message":"update_unavailable","origin":"emp"}}),
        );
        for path in ["/", "/index.html", "/healthz"] {
            for method in ["POST", "DELETE"] {
                let reply = exchange_wire(endpoint, &request(port, method, path, &[], b""), false).await;
                if path == "/healthz" {
                    wire_contract::assert_json_reply(path, &reply, "200 OK", &json!({"status":"ok"}));
                } else {
                    assert_page_reply(path, &reply, "text/html; charset=utf-8", crate::WEB_INDEX_BYTES);
                }
                let denied = exchange_wire(
                    endpoint,
                    &request(port, method, path, &["Origin: https://other.invalid"], b""),
                    false,
                )
                .await;
                let message = if path == "/healthz" {
                    "cross-origin health request rejected"
                } else {
                    "cross-origin Web UI request rejected"
                };
                wire_contract::assert_json_reply(
                    path, &denied, "403 Forbidden", &json!({"error":{"message":message}}),
                );
            }
        }
        for (method, path, status) in [
            ("GET", "/api/providers//quota-history?start=2&end=1", 400),
            (
                "GET",
                "/api/providers/raw/id/quota-history?start=2&end=1",
                400,
            ),
            ("GET", "/api/providers/raw/id/quota-history", 404),
            ("GET", "/api/providers//quota-history", 404),
            ("GET", "/api/updates/check", 404),
            ("HEAD", "/api/updates", 404),
            ("HEAD", "/v1/models", 404),
            ("POST", "/api/not-present", 404),
            ("DELETE", "/api/providers/raw/id/quota-history", 404),
            ("DELETE", "/api/quit", 404),
            ("GET", "/not-present", 404),
            ("GET", "/v1/responses", 404),
        ] {
            let wire = request(port, method, path, &[&session], b"");
            if status == 404 {
                assert_entry_error(endpoint, &wire, status).await;
            } else {
                let reply = exchange_wire(endpoint, &wire, false).await;
                wire_contract::assert_json_reply(
                    path, &reply, "400 Bad Request",
                    &json!({"error":{"message":"invalid quota history period","origin":"emp"}}),
                );
            }
            if path.starts_with("/api/") {
                assert_entry_error(endpoint, &request(port, method, path, &[], b""), 401).await;
                assert_entry_error(
                    endpoint,
                    &request(
                        port,
                        method,
                        path,
                        &[&session, "Origin: https://other.invalid"],
                        b"",
                    ),
                    403,
                )
                .await;
            }
        }
    });
}

#[cfg(unix)]
fn integration_fixture(observed_at: Option<&str>) -> Fixture {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path();
    std::fs::write(
        root.join("config.json"),
        br#"{"providers":[{"id":"external","base_url":"https://example.invalid/v1","protocol":"responses"}],"models":[{"id":"external/model-a","provider":"external","upstream_id":"model-a","enabled":true}]}"#,
    ).unwrap();
    std::fs::create_dir(root.join("codex")).unwrap();
    std::fs::write(
        root.join("codex/config.toml"),
        b"# native fixture\nopenai_base_url = \"native\"\n",
    )
    .unwrap();
    if let Some(observed_at) = observed_at {
        let recovery = root.join("codex/easy-multi-provider/integration");
        std::fs::create_dir_all(&recovery).unwrap();
        std::fs::write(
            recovery.join("runtime.json"),
            json!({
                "schema": "easy-multi-provider.runtime-recovery",
                "version": 1,
                "state": "native_loaded",
                "target": "native",
                "configuration_relation": "unleased",
                "expected_models": [],
                "verified": true,
                "detail": "fixture observation",
                "updated_at": observed_at,
            })
            .to_string(),
        )
        .unwrap();
    }
    Fixture::with_directory(directory)
}

#[cfg(unix)]
#[test]
fn integration_recovery_record_returns_last_observation() {
    const OBSERVED_AT: &str = "2026-01-02T03:04:05Z";

    integration_fixture(Some(OBSERVED_AT)).run_wire(async |fixture, address| {
        let wire = request(
            fixture.port(),
            "GET",
            "/api/integration",
            &[&fixture.session()],
            b"",
        );
        let reply = assert_wire(address, &wire, false, 200).await;
        assert_eq!(
            json_body(&reply)["runtime"]["last_known"]["observed_at"],
            OBSERVED_AT
        );
    });
}

#[cfg(unix)]
#[test]
fn integration_commands_restore_native_files() {
    integration_fixture(None).run_wire(async |fixture, address| {
        for (operation, body, status, state) in [
            ("enable", b"{}".as_slice(), 409, "native"),
            ("restore", b"{}".as_slice(), 409, "native"),
            ("verify", b"{}".as_slice(), 409, "native"),
            (
                "enable",
                br#"{"confirm_reload":true}"#.as_slice(),
                200,
                "emp_applied",
            ),
            ("reload", b"{}".as_slice(), 200, "emp_applied"),
            ("verify", b"{}".as_slice(), 200, "emp_applied"),
            (
                "restore",
                br#"{"confirm_reload":true}"#.as_slice(),
                200,
                "native",
            ),
        ] {
            let path = format!("/api/integration/{operation}");
            let wire = json_request(fixture.port(), &path, &[&fixture.session()], body);
            let reply = assert_wire(address, &wire, false, status).await;
            let body = json_body(&reply);
            assert_eq!(body["configuration"]["state"], state);
            if status == 409 {
                assert_eq!(body["runtime"].get("last_known"), Some(&Value::Null));
            }
        }
        assert_eq!(
            std::fs::read(fixture.directory.path().join("codex/config.toml")).unwrap(),
            b"# native fixture\nopenai_base_url = \"native\"\n"
        );
    });
}

#[cfg(unix)]
#[test]
fn quit_acknowledges_after_admitted_requests_and_closes_without_reopening() {
    use super::wire_contract::{assert_reply, json_head};
    use crate::server::ShutdownReason;
    use crate::server::raw_tcp::{RawClient, read_websocket_frame, wire_timeout};

    {
        Fixture::new().run_wire(async |fixture, addr| {
            let port = fixture.port();
            let session = fixture.session();
            let websocket = request(
                port,
                "GET",
                "/v1/responses",
                &[
                    &session,
                    "Upgrade: websocket",
                    "Connection: Upgrade",
                    "Sec-WebSocket-Version: 13",
                    "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
                ],
                b"",
            );
            let mut ws = RawClient::request(addr, &websocket).await;
            assert!(ws.head.starts_with("HTTP/1.1 101"));
            let mut sse = RawClient::request(
                addr,
                &request(port, "GET", "/api/accounts/events", &[&session], b""),
            )
            .await;
            assert!(sse.head.starts_with("HTTP/1.1 200"));
            let mut admitted = TcpStream::connect(addr).await.unwrap();
            admitted
                .write_all(&request(
                    port,
                    "POST",
                    "/api/integration/verify",
                    &[
                        &session,
                        "Content-Length: 2",
                        "Content-Type: application/json",
                    ],
                    b"",
                ))
                .await
                .unwrap();
            let state = &fixture.server.state;
            crate::tests::wait_for(
                &Deadline::after(test_timeout(Duration::from_secs(5))),
                || state.lifecycle.updates().active_for_test() == 1,
            );
            let quit = json_request(port, "/api/quit", &[&session], b"{}");
            let mut stopping = TcpStream::connect(addr).await.unwrap();
            stopping.write_all(&quit).await.unwrap();
            assert_eq!(
                read_websocket_frame(&mut ws.io, false, &Deadline::after(wire_timeout()))
                    .await
                    .0,
                0x88
            );
            let mut denied = TcpStream::connect(addr).await.unwrap();
            let _ = denied
                .write_all(&request(port, "GET", "/healthz", &[], b""))
                .await;
            let mut rejected = [0];
            let rejected = timeout(
                test_timeout(Duration::from_secs(3)).duration(),
                denied.read(&mut rejected),
            )
            .await
            .unwrap();
            assert!(matches!(rejected, Ok(0) | Err(_)));
            assert!(!state.lifecycle.shutdown.load(Ordering::Acquire));
            admitted.write_all(b"{}").await.unwrap();
            let mut response = Vec::new();
            admitted.read_to_end(&mut response).await.unwrap();
            assert_status(&response, 409);
            let mut response = Vec::new();
            stopping.read_to_end(&mut response).await.unwrap();
            assert_reply(
                "POST /api/quit",
                &response,
                json_head!("200 OK", "21"),
                br#"{"status":"stopping"}"#,
            );
            let sse_end = sse.rest(&Deadline::after(wire_timeout())).await;
            assert!(String::from_utf8_lossy(&sse_end).contains("event: emp-exiting"));
            let signal = state.lifecycle.signal().wait().await;
            assert_eq!(signal, ShutdownReason::ResponseWritten);
            assert!(state.lifecycle.admission.acquire_request().is_none());
            crate::tests::wait_for(
                &Deadline::after(crate::server::raw_tcp::wire_timeout()),
                || fixture.server.listener_finished(),
            );
            assert!(std::net::TcpStream::connect(addr).is_err());
        });
    }
}
