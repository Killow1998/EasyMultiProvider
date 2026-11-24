use super::*;

#[test]
fn raw_management_paths_match_legacy_without_executing_writes() {
    Fixture::new().run_candidate(async |fixture, candidate| {
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
            fixture
                .parity(
                    candidate,
                    &json_request(port, path, &[&session], b"{}"),
                    404,
                )
                .await;
        }
        for path in ["/api/native/login/start#x", "/api/accounts/login/finish#x"] {
            fixture
                .parity(
                    candidate,
                    &json_request(port, path, &[&session], b"{}"),
                    409,
                )
                .await;
        }
        for (headers, status) in [
            (vec![], 401),
            (vec![session.as_str(), "Origin: https://other.invalid"], 403),
        ] {
            fixture
                .parity(
                    candidate,
                    &json_request(port, "/api/quit#x", &headers, b"{}"),
                    status,
                )
                .await;
        }
        fixture
            .parity(candidate, &request(port, "GET", "/healthz", &[], b""), 200)
            .await;
    });
}

#[test]
fn unknown_provider_read_matches_legacy_not_found() {
    Fixture::new().run_candidate(async |fixture, candidate| {
        fixture
            .parity(
                candidate,
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
fn migrated_wrong_methods_match_legacy_before_and_during_update_drain() {
    Fixture::new().run_candidate(async |fixture, candidate| {
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
            fixture
                .parity(
                    candidate,
                    &request(port, method, path, &[&session], b""),
                    404,
                )
                .await;
        }
        let state = fixture.legacy.state.clone();
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
            let reply = fixture
                .parity(
                    candidate,
                    &request(port, "DELETE", path, &[&session], b""),
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

    for use_candidate in [false, true] {
        Fixture::new().run_candidate(async |fixture, candidate| {
            let state = &fixture.legacy.state;
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
            let address = if use_candidate {
                candidate
            } else {
                fixture.legacy.local_addr()
            };
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
                "idle WebSocket kept the parent work active (candidate={use_candidate})"
            );
            ws.io.shutdown().await.unwrap();
        });
    }
}

#[test]
fn lifecycle_and_unmatched_requests_match_legacy_wire() {
    Fixture::new().run_candidate(async |fixture, candidate| {
        let port = fixture.port();
        let session = fixture.session();
        let old = fixture.legacy.local_addr();
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
                fixture
                    .parity(candidate, &json_request(port, path, &headers, b"{"), status)
                    .await;
            }
        }
        fixture
            .parity(
                candidate,
                &json_request(
                    port,
                    "/api/providers/balance",
                    &[&session],
                    br#"{"force":"yes"}"#,
                ),
                400,
            )
            .await;
        let missing_balance = json_request(port, "/api/providers/balance", &[&session], b"{}");
        assert!(
            assert_wire_parity(old, candidate, &missing_balance, false)
                .await
                .is_empty()
        );

        fixture
            .parity(
                candidate,
                &json_request(port, "/api/updates/install", &[&session], b"{}"),
                409,
            )
            .await;
        for path in ["/", "/index.html", "/healthz"] {
            for method in ["POST", "DELETE"] {
                fixture
                    .parity(candidate, &request(port, method, path, &[], b""), 200)
                    .await;
                fixture
                    .parity(
                        candidate,
                        &request(port, method, path, &["Origin: https://other.invalid"], b""),
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
            fixture
                .parity(
                    candidate,
                    &request(port, method, path, &[&session], b""),
                    status,
                )
                .await;
            if path.starts_with("/api/") {
                fixture
                    .parity(candidate, &request(port, method, path, &[], b""), 401)
                    .await;
                fixture
                    .parity(
                        candidate,
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
#[test]
fn integration_commands_match_legacy_wire_and_restore_native_files() {
    use super::account_writes::normalize_body;

    fn fixture() -> Fixture {
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
        Fixture::with_directory(directory)
    }

    let old = fixture();
    fixture().run_candidate(async |new, candidate| {
        let old_addr = old.legacy.local_addr();
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
            let old_request = json_request(old.port(), &path, &[&old.session()], body);
            let new_request = json_request(new.port(), &path, &[&new.session()], body);
            let (old_reply, new_reply) = tokio::join!(
                exchange_wire(old_addr, &old_request, false),
                exchange_wire(candidate, &new_request, false),
            );
            let normalize = |fixture: &Fixture, reply: Vec<u8>| {
                let value = json_body(&reply);
                normalize_body(reply, |body| {
                    let mut body = body
                        .replace(&fixture.directory.path().display().to_string(), "/fixture")
                        .replace(&format!("127.0.0.1:{}", fixture.port()), "127.0.0.1:PORT");
                    for runtime in [&value["runtime"], &value["runtime"]["last_known"]] {
                        for field in ["checked_at", "observed_at", "updated_at"] {
                            let time = &runtime[field];
                            if time.is_number() {
                                for delimiter in [',', '}'] {
                                    body = body.replace(
                                        &format!("\"{field}\":{time}{delimiter}"),
                                        &format!("\"{field}\":0{delimiter}"),
                                    );
                                }
                            }
                        }
                    }
                    body
                })
            };
            assert_status(&old_reply, status);
            assert_eq!(json_body(&old_reply)["configuration"]["state"], state);
            assert_wire_response_parity(
                &old_request,
                &normalize(&old, old_reply),
                &normalize(new, new_reply),
            );
        }
        for fixture in [&old, new] {
            assert_eq!(
                std::fs::read(fixture.directory.path().join("codex/config.toml")).unwrap(),
                b"# native fixture\nopenai_base_url = \"native\"\n"
            );
        }
    });
    old.legacy.shutdown().unwrap();
}

#[cfg(unix)]
#[test]
fn quit_acknowledges_after_admitted_requests_and_closes_without_reopening() {
    use crate::server::ShutdownReason;
    use crate::server::raw_tcp::{RawClient, read_websocket_frame};

    let old = Fixture::new();
    let new = Fixture::new();
    let (candidate, candidate_task) = spawn_candidate(&new.legacy);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut replies = Vec::new();
            for (fixture, addr) in [(&old, old.legacy.local_addr()), (&new, candidate)] {
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
                let state = &fixture.legacy.state;
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
                assert_status(&response, 200);
                assert_eq!(json_body(&response), json!({"status":"stopping"}));
                let sse_end = sse.rest().await;
                assert!(String::from_utf8_lossy(&sse_end).contains("event: emp-exiting"));
                let signal = state.lifecycle.signal().wait().await;
                assert_eq!(
                    signal,
                    if addr == candidate {
                        ShutdownReason::ResponseWritten
                    } else {
                        ShutdownReason::Requested
                    }
                );
                assert!(state.lifecycle.admission.acquire_request().is_none());
                replies.push(response);
            }
            assert_eq!(replies[0], replies[1]);
            candidate_task.await.unwrap().unwrap();
            assert!(TcpStream::connect(candidate).await.is_err());
        });
    old.legacy.shutdown().unwrap();
    new.legacy.shutdown().unwrap();
}
