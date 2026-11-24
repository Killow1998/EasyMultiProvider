use super::*;

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
            assert_wire(
                endpoint,
                &json_request(port, path, &[&session], b"{}"),
                false,
                404,
            )
            .await;
        }
        for path in ["/api/native/login/start#x", "/api/accounts/login/finish#x"] {
            assert_wire(
                endpoint,
                &json_request(port, path, &[&session], b"{}"),
                false,
                409,
            )
            .await;
        }
        for suffix in [
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
                let reply = assert_wire(
                    endpoint,
                    &json_request(port, &path, &[&session], b"{}"),
                    false,
                    409,
                )
                .await;
                assert_eq!(
                    json_body(&reply)["error"]["code"],
                    "login_unknown_operation",
                    "{path}"
                );
            }
            for path in [
                format!("/api/quit#{suffix}"),
                format!("/api/integration/enable#{suffix}"),
            ] {
                assert_wire(
                    endpoint,
                    &json_request(port, &path, &[&session], b"{}"),
                    false,
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
                    assert_wire(
                        endpoint,
                        &request(port, "GET", &path, headers, b""),
                        false,
                        200,
                    )
                    .await;
                }
            }
        }
        for path in [
            "/api/native/login/start#x y",
            "/api/native/login/start?probe=x y",
        ] {
            assert_wire(
                endpoint,
                &json_request(port, path, &[&session], b"{}"),
                false,
                400,
            )
            .await;
        }
        for (headers, status) in [
            (vec![], 401),
            (vec![session.as_str(), "Origin: https://other.invalid"], 403),
        ] {
            for path in ["/api/quit#x", "/api/native/login/start#x<y"] {
                assert_wire(
                    endpoint,
                    &json_request(port, path, &headers, b"{}"),
                    false,
                    status,
                )
                .await;
            }
        }
        assert_wire(
            endpoint,
            &request(port, "GET", "/healthz", &[], b""),
            false,
            200,
        )
        .await;
    });
}

#[test]
fn unknown_provider_read_returns_not_found() {
    Fixture::new().run_wire(async |fixture, endpoint| {
        assert_wire(
            endpoint,
            &request(
                fixture.port(),
                "GET",
                "/api/providers/example/unknown",
                &[&fixture.session()],
                b"",
            ),
            false,
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
            ("GET", "/api/team/publish_source"),
            ("GET", "/api/migration/export"),
        ] {
            assert_wire(
                endpoint,
                &request(port, method, path, &[&session], b""),
                false,
                404,
            )
            .await;
        }
        let state = fixture.server.state.clone();
        assert!(
            tokio::task::spawn_blocking(move || {
                state
                    .lifecycle
                    .updates()
                    .drain_for_test(Duration::from_secs(5))
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
            let reply = assert_wire(
                endpoint,
                &request(port, "DELETE", path, &[&session], b""),
                false,
                503,
            )
            .await;
            assert!(String::from_utf8_lossy(&reply).contains("\r\nRetry-After: "));
        }
    });
}

#[cfg(unix)]
#[test]
fn idle_delegated_websocket_does_not_hold_parent_update_lease() {
    use crate::{server::raw_tcp::RawClient, services::modules::aero::CallLease};

    {
        Fixture::new().run_wire(async |fixture, address| {
            let state = &fixture.server.state;
            let calls = &state.backend.aero_callers;
            let delegation = calls
                .delegate(
                    Caller {
                        id: "local".into(),
                        name: String::new(),
                    },
                    None,
                    Some(CallLease {
                        connection: Arc::new(state.lifecycle.admission.acquire_request().unwrap()),
                        work: Arc::new(state.lifecycle.updates().enter().unwrap()),
                    }),
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
            for (headers, status) in [
                (vec![], 401),
                (vec![&session[..], "Origin: https://other.invalid"], 403),
                (vec![&session[..]], 400),
            ] {
                assert_wire(
                    endpoint,
                    &json_request(port, path, &headers, b"{"),
                    false,
                    status,
                )
                .await;
            }
        }
        assert_wire(
            endpoint,
            &json_request(
                port,
                "/api/providers/balance",
                &[&session],
                br#"{"force":"yes"}"#,
            ),
            false,
            400,
        )
        .await;
        let missing_balance = json_request(port, "/api/providers/balance", &[&session], b"{}");
        {
            let address = endpoint;
            assert!(
                exchange_wire(address, &missing_balance, false)
                    .await
                    .is_empty()
            );
        }

        assert_wire(
            endpoint,
            &json_request(port, "/api/updates/install", &[&session], b"{}"),
            false,
            409,
        )
        .await;
        for path in ["/", "/index.html", "/healthz"] {
            for method in ["POST", "DELETE"] {
                assert_wire(endpoint, &request(port, method, path, &[], b""), false, 200).await;
                assert_wire(
                    endpoint,
                    &request(port, method, path, &["Origin: https://other.invalid"], b""),
                    false,
                    403,
                )
                .await;
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
            assert_wire(
                endpoint,
                &request(port, method, path, &[&session], b""),
                false,
                status,
            )
            .await;
            if path.starts_with("/api/") {
                assert_wire(endpoint, &request(port, method, path, &[], b""), false, 401).await;
                assert_wire(
                    endpoint,
                    &request(
                        port,
                        method,
                        path,
                        &[&session, "Origin: https://other.invalid"],
                        b"",
                    ),
                    false,
                    403,
                )
                .await;
            }
        }
    });
}

#[cfg(unix)]
fn integration_fixture(observed_at: Option<&str>) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
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
    use crate::server::raw_tcp::{RawClient, read_websocket_frame};

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
            crate::tests::wait_for(Duration::from_secs(5), || {
                state.lifecycle.updates().active_for_test() == 1
            });
            let quit = json_request(port, "/api/quit", &[&session], b"{}");
            let mut stopping = TcpStream::connect(addr).await.unwrap();
            stopping.write_all(&quit).await.unwrap();
            assert_eq!(read_websocket_frame(&mut ws.io, false).await.0, 0x88);
            let mut denied = TcpStream::connect(addr).await.unwrap();
            let _ = denied
                .write_all(&request(port, "GET", "/healthz", &[], b""))
                .await;
            let mut rejected = [0];
            let rejected = timeout(Duration::from_secs(3), denied.read(&mut rejected))
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
            let sse_end = sse.rest().await;
            assert!(String::from_utf8_lossy(&sse_end).contains("event: emp-exiting"));
            let signal = state.lifecycle.signal().wait().await;
            assert_eq!(signal, ShutdownReason::ResponseWritten);
            assert!(state.lifecycle.admission.acquire_request().is_none());
            crate::tests::wait_for(crate::server::raw_tcp::DEADLINE, || {
                fixture.server.listener_finished()
            });
            assert!(std::net::TcpStream::connect(addr).is_err());
        });
    }
}
