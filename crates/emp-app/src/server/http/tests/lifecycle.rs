use super::*;

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
            let normalize = |fixture: &Fixture, reply| {
                normalize_body(reply, |body| {
                    let mut value: Value = serde_json::from_str(
                        &body
                            .replace(&fixture.directory.path().display().to_string(), "/fixture")
                            .replace(&format!("127.0.0.1:{}", fixture.port()), "127.0.0.1:PORT"),
                    )
                    .unwrap();
                    for runtime in ["checked_at", "observed_at", "updated_at"] {
                        if !value["runtime"][runtime].is_null() {
                            value["runtime"][runtime] = json!(0);
                        }
                        if !value["runtime"]["last_known"][runtime].is_null() {
                            value["runtime"]["last_known"][runtime] = json!(0);
                        }
                    }
                    value.to_string()
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
