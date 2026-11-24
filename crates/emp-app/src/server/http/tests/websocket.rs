use super::*;
use crate::api::realtime::MAX_REALTIME_REQUEST_BYTES;
use crate::http::request::parse_request;
use crate::server::raw_tcp::{
    RawClient, head, read_websocket_frame, websocket_frame, wire_timeout,
};
use crate::tests::realtime_contract::{
    RealtimeUpstream, UpstreamResponse, app_server_for, multipart_offer,
};
use crate::tests::{live_journal, wait_for};
use emp_test_support::{Deadline, test_timeout};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as AsyncListener, TcpStream as AsyncStream};
use tokio::time::timeout;

const UPGRADE: &[&str] = &[
    "Upgrade: websocket",
    "Connection: keep-alive, UpGrAdE, close",
    "Sec-WebSocket-Version: 13",
    "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
];
const MASK: Option<[u8; 4]> = Some([11, 12, 13, 14]);
const SWITCHING_PROTOCOLS: &str = concat!(
    "HTTP/1.1 101 Switching Protocols\r\n",
    "Upgrade: websocket\r\n",
    "Connection: Upgrade\r\n",
    "Sec-Websocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
);

pub(super) fn upgrade(
    port: u16,
    path: &str,
    session: &str,
    other: &[&str],
    prefix: &[u8],
) -> Vec<u8> {
    let mut headers = UPGRADE.to_vec();
    headers.push(session);
    headers.extend_from_slice(other);
    request(port, "GET", path, &headers, prefix)
}

fn assert_upgrade(client: &RawClient) {
    assert_eq!(client.head, SWITCHING_PROTOCOLS);
}

fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    [code.to_be_bytes().as_slice(), reason.as_bytes()].concat()
}

fn begin_restore(
    fixture: &Fixture,
    address: SocketAddr,
    session: &str,
) -> tokio::task::JoinHandle<Vec<u8>> {
    let wire = json_request(
        fixture.port(),
        "/api/integration/restore",
        &[session],
        br#"{"confirm_reload":true}"#,
    );
    tokio::spawn(async move { exchange_wire(address, &wire, false).await })
}

#[test]
fn websocket_handshake_errors_preserve_responses() {
    let fixture = Fixture::new();
    fixture.run_wire(async |fixture, address| {
        let port = fixture.port();
        let session = fixture.session();
        for path in ["/v1/responses", "/v1/live/rtc_valid"] {
            let caller_error = if path == "/v1/responses" {
                json!({"error":{"message":"proxy caller authentication is required","origin":"emp"}})
            } else {
                json!({"error":{"code":"realtime_caller_unauthorized","message":"Caller authentication is required for Codex Voice","origin":"emp"}})
            };
            let upgrade_error = json!({"error":{"message":"invalid websocket upgrade","origin":"emp"}});
            let key_error = json!({"error":{"message":"invalid Sec-WebSocket-Key","origin":"emp"}});
            for (headers, status, error) in [
                (vec!["Authorization: Bearer invalid"], "401 Unauthorized", &caller_error),
                (vec!["Origin: https://other.example"], "403 Forbidden", &caller_error),
                (vec![session.as_str(), "Sec-WebSocket-Version: 12"], "400 Bad Request", &upgrade_error),
                (vec![session.as_str(), "Connection: keep-alive"], "400 Bad Request", &upgrade_error),
                (vec![session.as_str(), "Sec-WebSocket-Key: invalid"], "400 Bad Request", &key_error),
            ] {
                let mut all = headers.clone();
                for header in UPGRADE {
                    let (name, _) = header.split_once(':').unwrap();
                    if !headers
                        .iter()
                        .any(|original| original.starts_with(&format!("{name}:")))
                    {
                        all.push(header);
                    }
                }
                let reply = exchange_wire(address, &request(port, "GET", path, &all, &[]), false).await;
                wire_contract::assert_json_reply(path, &reply, status, error);
            }
        }
        for path in ["/v1/live/", "/v1/live/rtc_bad%2Fid", "/v1/live/rtc_bad/id"] {
            let reply = exchange_wire(address, &upgrade(port, path, &session, &[], &[]), false).await;
            wire_contract::assert_json_reply(path, &reply, "400 Bad Request",
                &json!({"error":{"code":"realtime_invalid_call_id","message":"Realtime sideband call ID is invalid","origin":"emp"}}));
        }
    });
}

#[test]
fn tab_separated_websocket_request_line_is_accepted() {
    Fixture::new().run_wire(async |fixture, address| {
        let session = fixture.session();
        let mut wire = upgrade(
            fixture.port(),
            "/v1/responses",
            &session,
            &[],
            &websocket_frame(0x89, MASK, b"ready"),
        );
        for byte in &mut wire[..b"GET /v1/responses HTTP/1.1".len()] {
            if *byte == b' ' {
                *byte = b'\t';
            }
        }
        {
            let peer = address;
            let mut client = RawClient::request(peer, &wire).await;
            assert_upgrade(&client);
            assert_eq!(
                read_websocket_frame(&mut client.io, false, &Deadline::after(wire_timeout())).await,
                (0x8a, b"ready".to_vec())
            );
            client.io.shutdown().await.unwrap();
            assert_eq!(
                client.rest(&Deadline::after(wire_timeout())).await,
                b"\x88\x00"
            );
        }
    });
}

#[test]
fn tab_separated_http_request_line_is_accepted() {
    Fixture::new().run_wire(async |fixture, address| {
        let mut wire = request(
            fixture.port(),
            "GET",
            "/api/config",
            &[&fixture.session()],
            &[],
        );
        for byte in &mut wire[..b"GET /api/config HTTP/1.1".len()] {
            if *byte == b' ' {
                *byte = b'\t';
            }
        }
        let reply = assert_wire(address, &wire, true, 200).await;
        assert!(json_body(&reply)["models"].is_array());
        wire_contract::assert_reference_reply(
            "tab-separated /api/config",
            &reply,
            &crate::api::catalog::read_configuration(&fixture.server.state).encode(),
        );
    });
}

#[test]
fn websocket_upgrade_carries_prefix_and_keeps_capacity_rejections() {
    let fixture = Fixture::new();
    fixture.run_wire(async |fixture, address| {
        let port = fixture.port();
        let session = fixture.session();
        let state = &fixture.server.state;
        let slots = std::iter::from_fn(|| state.lifecycle.admission.acquire_websocket())
            .collect::<Vec<_>>();
        let sideband = upgrade(port, "/v1/live/rtc_busy", &session, &[], &[]);
        let reply = exchange_wire(address, &sideband, false).await;
        let expected = br#"{"error":{"code":"realtime_capacity_unavailable","message":"Too many WebSocket connections","origin":"emp"}}"#;
        wire_contract::assert_reply(
            "sideband capacity", &reply,
            &format!(wire_contract::json_head!("503 Service Unavailable", "{}", "Retry-After: 2"), expected.len()),
            expected,
        );
        let responses = upgrade(port, "/v1/responses", &session, &[], &[]);
        {
            let mut client = RawClient::request(address, &responses).await;
            assert_upgrade(&client);
            assert_eq!(
                read_websocket_frame(&mut client.io, false, &Deadline::after(wire_timeout())).await,
                (0x88, close_payload(1013, "too many websocket connections"))
            );
            assert!(client.rest(&Deadline::after(wire_timeout())).await.is_empty());
        }
        drop(slots);
        let wire = upgrade(
            port,
            "/v1/responses",
            &session,
            &[
                "Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits",
                "Sec-WebSocket-Protocol: fixture",
                "Content-Length: 99",
            ],
            &websocket_frame(0x89, MASK, b"coalesced"),
        );
        {
            let mut client = RawClient::request(address, &wire).await;
            assert_upgrade(&client);
            assert_eq!(
                read_websocket_frame(&mut client.io, false, &Deadline::after(wire_timeout())).await,
                (0x8a, b"coalesced".to_vec())
            );
            assert_eq!(state.lifecycle.admission.active_websockets(), 1);
            assert_eq!(state.lifecycle.admission.active_requests(), 0);
            client
                .io
                .write_all(&websocket_frame(0x88, MASK, &1000u16.to_be_bytes()))
                .await
                .unwrap();
            assert_eq!(
                read_websocket_frame(&mut client.io, false, &Deadline::after(wire_timeout())).await,
                (0x88, 1000u16.to_be_bytes().to_vec())
            );
            assert!(client.rest(&Deadline::after(wire_timeout())).await.is_empty());
        }
        assert_eq!(state.lifecycle.admission.active_websockets(), 0);
    });
}

#[test]
fn responses_drain_closes_sessions_and_rejects_new_tcp_handshakes() {
    let fixture = Fixture::new();
    fixture.run_wire(async |fixture, address| {
        let port = fixture.port();
        let session = fixture.session();
        {
            let wire = upgrade(
                port,
                "/v1/responses",
                &session,
                &[],
                &websocket_frame(0x89, MASK, b"ready"),
            );
            let mut client = RawClient::request(address, &wire).await;
            assert_eq!(
                read_websocket_frame(&mut client.io, false, &Deadline::after(wire_timeout())).await,
                (0x8a, b"ready".to_vec())
            );
            let state = fixture.server.state.clone();
            let (ready, drained) = tokio::sync::oneshot::channel();
            let (reopen, wait) = tokio::sync::oneshot::channel();
            let worker = tokio::task::spawn_blocking(move || {
                let guard = state
                    .lifecycle
                    .admission
                    .quiesce(0, test_timeout(Duration::from_secs(5)))
                    .unwrap();
                ready.send(()).unwrap();
                wait.blocking_recv().unwrap();
                drop(guard);
            });
            assert_eq!(
                read_websocket_frame(&mut client.io, false, &Deadline::after(wire_timeout())).await,
                (0x88, close_payload(1001, "EMP connection is draining"))
            );
            assert!(
                client
                    .rest(&Deadline::after(wire_timeout()))
                    .await
                    .is_empty()
            );
            drained.await.unwrap();
            let mut denied = AsyncStream::connect(address).await.unwrap();
            let _ = denied.write_all(&wire).await;
            let result = timeout(test_timeout(Duration::from_secs(5)), denied.read_u8())
                .await
                .unwrap();
            assert!(result.is_err(), "drain must not send an HTTP response");
            assert_eq!(
                fixture.server.state.lifecycle.admission.active_websockets(),
                0
            );
            reopen.send(()).unwrap();
            worker.await.unwrap();
            let mut reopened = RawClient::request(address, &wire).await;
            assert_upgrade(&reopened);
            assert_eq!(
                read_websocket_frame(&mut reopened.io, false, &Deadline::after(wire_timeout()))
                    .await,
                (0x8a, b"ready".to_vec())
            );
            reopened.io.shutdown().await.unwrap();
            assert_eq!(
                reopened.rest(&Deadline::after(wire_timeout())).await,
                b"\x88\x00"
            );
        }
    });
}

#[test]
fn responses_upgrade_cannot_start_after_restore_reopens_admission() {
    Fixture::new().run_wire(async |fixture, address| {
        let state = &fixture.server.state;
        let admission = &state.lifecycle.admission;
        let (reached, handoff) = tokio::sync::oneshot::channel();
        let (resume, paused) = tokio::sync::oneshot::channel();
        *state.test_upgrade_handoff.lock().unwrap() = Some((reached, paused));
        let session = fixture.session();
        let wire = upgrade(
            fixture.port(),
            "/v1/responses",
            &session,
            &[],
            &websocket_frame(0x89, MASK, b"session started"),
        );
        let mut client = RawClient::request(address, &wire).await;
        assert_upgrade(&client);
        assert_eq!(
            timeout(test_timeout(Duration::from_secs(5)), handoff)
                .await
                .unwrap()
                .unwrap(),
            (0, 1),
            "the WebSocket permit must already be held when the HTTP permit is released"
        );
        let restore = begin_restore(fixture, address, &session);
        timeout(
            test_timeout(Duration::from_secs(5)),
            admission.wait_for_websocket_drain(),
        )
        .await
        .unwrap();
        resume.send(()).unwrap();
        assert_eq!(
            read_websocket_frame(&mut client.io, false, &Deadline::after(wire_timeout())).await,
            (0x88, close_payload(1001, "EMP connection is draining"))
        );
        let reply = restore.await.unwrap();
        assert_status(&reply, 200);
        assert!(!admission.is_quiescing());
    });
}

#[test]
fn slow_device_revalidation_leaves_other_websockets_responsive() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    Fixture::new().run_wire(async |fixture, _| {
        let state = &fixture.server.state;
        let gateway = &state.backend.gateway_clients;
        let issued = serde_json::to_value(
            gateway
                .issue(
                    "slow device",
                    None,
                    &state.backend.configuration.vault,
                    &state.backend.modules,
                )
                .unwrap(),
        )
        .unwrap();
        let key = format!(
            "Authorization: Bearer {}",
            issued["api_key"].as_str().unwrap()
        );
        let start = |header: String| {
            let state = state.clone();
            let (client, downstream) = tokio::io::duplex(4096);
            let task = runtime.spawn(async move {
                let wire = format!(
                    "GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{header}\r\n\r\n",
                    state.port
                );
                let mut request = parse_request(&wire).unwrap();
                let source = tokio::task::block_in_place(|| {
                    crate::server::http::proxy::request_source(
                        request,
                        &state,
                        system_now(),
                        None,
                        None,
                    )
                });
                assert!(source.caller.is_some());
                request.source = Some(&source);
                let permit = Arc::new(state.lifecycle.admission.acquire_websocket().unwrap());
                crate::api::websocket::run_responses(
                    &state,
                    request,
                    None,
                    downstream,
                    &[],
                    None,
                    Some(permit),
                )
                .await;
            });
            (client, task)
        };
        let (mut device, device_task) = start(key);
        let (mut other, other_task) = start(fixture.session());
        for client in [&mut device, &mut other] {
            client
                .write_all(&websocket_frame(0x89, MASK, b"ready"))
                .await
                .unwrap();
            assert_eq!(
                read_websocket_frame(client, false, &Deadline::after(wire_timeout())).await,
                (0x8a, b"ready".to_vec())
            );
        }
        let (rechecking, release) = gateway.gate_next_identify_for_test();
        device
            .write_all(&websocket_frame(0x81, MASK, b"{}"))
            .await
            .unwrap();
        timeout(wire_timeout(), rechecking).await.unwrap().unwrap();
        other
            .write_all(&websocket_frame(0x89, MASK, b"responsive"))
            .await
            .unwrap();
        assert_eq!(
            read_websocket_frame(&mut other, false, &Deadline::after(wire_timeout())).await,
            (0x8a, b"responsive".to_vec())
        );
        state.lifecycle.admission.close();
        assert_eq!(
            read_websocket_frame(&mut other, false, &Deadline::after(wire_timeout())).await,
            (0x88, close_payload(1001, "EMP connection is draining")),
        );
        release.send(()).unwrap();
        assert_eq!(
            read_websocket_frame(&mut device, false, &Deadline::after(wire_timeout())).await,
            (0x88, close_payload(1001, "EMP connection is draining")),
        );
        device_task.await.unwrap();
        other_task.await.unwrap();
    });
}

#[test]
fn websocket_completion_log_follows_session_close_and_includes_idle_time() {
    Fixture::new().run_wire(async |fixture, address| {
        let wire = upgrade(
            fixture.port(),
            "/v1/responses",
            &fixture.session(),
            &[],
            &websocket_frame(0x89, MASK, b"ready"),
        );
        let root = fixture.directory.path().to_owned();
        let completed = move || {
            live_journal(&root)
                .into_iter()
                .filter(|record| {
                    record["event"] == "http_request_completed"
                        && record["fields"]["transport"] == "websocket"
                        && record["fields"]["path"] == "/v1/responses"
                })
                .collect::<Vec<_>>()
        };
        {
            let previous = 0;
            let mut client = RawClient::request(address, &wire).await;
            assert_upgrade(&client);
            assert_eq!(
                read_websocket_frame(&mut client.io, false, &Deadline::after(wire_timeout())).await,
                (0x8a, b"ready".to_vec())
            );
            let idle = Duration::from_millis(50);
            assert!(timeout(idle, client.io.read_u8()).await.is_err());
            assert_eq!(completed().len(), previous, "WebSocket is still open");
            client.io.shutdown().await.unwrap();
            assert_eq!(
                client.rest(&Deadline::after(wire_timeout())).await,
                b"\x88\x00"
            );
            let completed = completed.clone();
            let records = tokio::task::spawn_blocking(move || {
                wait_for(&Deadline::after(wire_timeout()), || {
                    completed().len() == previous + 1
                });
                completed()
            })
            .await
            .unwrap();
            assert!(
                records[previous]["fields"]["duration_ms"].as_u64().unwrap()
                    >= idle.as_millis() as u64
            );
        }
    });
}

pub(super) fn realtime_fixture(upstream: &RealtimeUpstream) -> (tempfile::TempDir, Fixture) {
    let native = emp_test_support::fixture_dir().unwrap();
    let auth = native.path().join("auth.json");
    std::fs::write(
        &auth,
        br#"{"tokens":{"access_token":"native-secret","account_id":"acct-native"}}"#,
    )
    .unwrap();
    let (directory, server) = app_server_for(upstream, &auth);
    (
        native,
        Fixture {
            directory,
            server,
            upstream: Arc::default(),
        },
    )
}

#[test]
fn realtime_call_preserves_multipart_rejection_location_and_upstream_error() {
    for response in [
        UpstreamResponse {
            status: 201,
            reason: "Created",
            content_type: "application/sdp",
            location: Some("/v1/live/rtc_h06b"),
            body: b"v=0\r\no=answer\r\n",
        },
        UpstreamResponse {
            status: 403,
            reason: "Forbidden",
            content_type: "application/json",
            location: None,
            body: b"",
        },
    ] {
        {
            let upstream = RealtimeUpstream::start(response);
            let (_auth, fixture) = realtime_fixture(&upstream);
            fixture.run_wire(async |fixture, address| {
                let boundary = "Content-Type: multipart/form-data; boundary=VoiceFixtureBoundary";
                let body = multipart_offer();
                let valid_length = format!("Content-Length: {}", body.len());
                let big_length = format!("Content-Length: {}", MAX_REALTIME_REQUEST_BYTES + 1);
                for (status, length, body) in [
                    (413, &big_length, &[][..]),
                    (response.status, &valid_length, body.as_slice()),
                ] {
                    let wire = request(
                        address.port(),
                        "POST",
                        "/v1/live",
                        &[&fixture.session(), boundary, length],
                        body,
                    );
                    let reply = assert_wire(address, &wire, false, status).await;
                    if status == 413 {
                        assert!(upstream.no_request());
                    } else if status == 201 {
                        assert!(
                            std::str::from_utf8(&reply)
                                .unwrap()
                                .contains("Location: /v1/live/rtc_h06b\r\n")
                        );
                    } else {
                        assert_eq!(
                            json_body(&reply)["error"]["code"],
                            "native_subscription_auth_failed"
                        );
                    }
                }
            });
            upstream.assert_only_call();
        }
    }
}

#[test]
fn realtime_rejections_retain_update_access_until_response_write() {
    Fixture::new().run(async |fixture, _| {
        let state = &fixture.server.state;
        let updates = state.lifecycle.updates();
        let session = fixture.session();
        let boundary = "Content-Type: multipart/form-data; boundary=VoiceFixtureBoundary";
        let big_length = format!("Content-Length: {}", MAX_REALTIME_REQUEST_BYTES + 1);
        for (status, headers) in [
            (401, vec!["Authorization: Bearer invalid"]),
            (413, vec![session.as_str(), boundary, &big_length]),
        ] {
            let reached = Arc::new(tokio::sync::Notify::new());
            let write = Arc::new(tokio::sync::Notify::new());
            let router = crate::server::http::websocket::routes()
                .with_state(state.clone())
                .layer(axum::middleware::from_fn({
                    let reached = reached.clone();
                    let write = write.clone();
                    move |request: Request, next: axum::middleware::Next| {
                        let reached = reached.clone();
                        let write = write.clone();
                        async move {
                            let response = next.run(request).await;
                            reached.notify_one();
                            write.notified().await;
                            response
                        }
                    }
                }));
            let server = ServerHandle::bind(LOOPBACK, router, admission())
                .await
                .unwrap();
            let mut client = AsyncStream::connect(server.local_addr()).await.unwrap();
            client
                .write_all(&request(fixture.port(), "POST", "/v1/live", &headers, &[]))
                .await
                .unwrap();
            timeout(wire_timeout(), reached.notified()).await.unwrap();
            assert!(
                !updates.drain_for_test(Duration::ZERO),
                "update handoff may not pass before the {status} has been written"
            );
            write.notify_one();
            let reply = timeout(wire_timeout(), head(&mut client))
                .await
                .unwrap()
                .unwrap();
            assert_status(&reply, status);
            server.shutdown().await.unwrap();
            assert_eq!(updates.active_for_test(), 0);
        }
    });
}

pub(super) async fn accept_upstream(
    listener: &AsyncListener,
    extensions: &str,
) -> (AsyncStream, String) {
    let (mut io, _) = timeout(test_timeout(Duration::from_secs(5)), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let headers = String::from_utf8(head(&mut io).await.unwrap()).unwrap();
    let key = parse_request(&headers)
        .unwrap()
        .header("Sec-WebSocket-Key")
        .unwrap();
    let accept = emp_transport::websocket_accept(key).unwrap();
    let wire = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-Websocket-Accept: {accept}\r\n{extensions}\r\n"
    );
    io.write_all(wire.as_bytes()).await.unwrap();
    (io, headers)
}

#[test]
fn sideband_upgrade_preserves_raw_relay_and_tcp_half_close() {
    let listener = TcpListener::bind(LOOPBACK).unwrap();
    listener.set_nonblocking(true).unwrap();
    let fixture = native_fixture(listener.local_addr().unwrap());
    fixture.run_wire(async |fixture, address| {
        let listener = AsyncListener::from_std(listener).unwrap();
        fixture
            .server
            .state
            .test_sideband_address
            .set(listener.local_addr().unwrap())
            .unwrap();
        let session = fixture.session();
        let first = br#"{ "type": "client.first" }"#;
        let event = br#"{ "type": "server.event" }"#;
        {
            let call_id = "rtc_relay";
            let peer = async {
                let (mut io, headers) = accept_upstream(&listener, "").await;
                let request = parse_request(&headers).unwrap();
                assert_eq!(request.raw_path(), format!("/v1/live/{call_id}"));
                assert_eq!(
                    request.header("Authorization"),
                    Some("Bearer native-secret")
                );
                assert_eq!(request.header("Session-Id"), Some("voice-fixture"));
                assert_eq!(
                    read_websocket_frame(&mut io, true, &Deadline::after(wire_timeout())).await,
                    (0x81, first.to_vec())
                );
                io.write_all(&websocket_frame(0x81, None, event))
                    .await
                    .unwrap();
                assert_eq!(
                    read_websocket_frame(&mut io, true, &Deadline::after(wire_timeout())).await,
                    (0x88, 1000u16.to_be_bytes().to_vec())
                );
            };
            let client = async {
                let wire = upgrade(
                    fixture.port(),
                    &format!("/v1/live/{call_id}?fixture=1"),
                    &session,
                    &[
                        "Authorization: Bearer caller-secret",
                        "Session-Id: voice-fixture",
                    ],
                    &websocket_frame(0x81, MASK, first),
                );
                let mut client = RawClient::request(address, &wire).await;
                assert_upgrade(&client);
                assert_eq!(
                    read_websocket_frame(&mut client.io, false, &Deadline::after(wire_timeout()))
                        .await,
                    (0x81, event.to_vec())
                );
                assert_eq!(
                    fixture.server.state.lifecycle.admission.active_websockets(),
                    1
                );
                client.io.shutdown().await.unwrap();
                assert_eq!(
                    client.rest(&Deadline::after(wire_timeout())).await,
                    b"\x88\x00"
                );
            };
            tokio::join!(peer, client);
        }
    });
}

pub(super) fn native_fixture(address: SocketAddr) -> Fixture {
    let directory = emp_test_support::fixture_dir().unwrap();
    let config = directory.path().join("config.json");
    let catalog = directory.path().join("catalog.json");
    std::fs::write(
        &catalog,
        br#"{"models":[{"slug":"gpt-fixture","display_name":"Fixture","visibility":"list"}]}"#,
    )
    .unwrap();
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "codex_base_url": format!("http://{address}/v1"),
            "native_catalog_path": catalog,
            "providers": [], "models": []
        }))
        .unwrap(),
    )
    .unwrap();
    let auth = directory.path().join("auth.json");
    std::fs::write(
        &auth,
        br#"{"tokens":{"access_token":"native-secret","account_id":"acct-native"}}"#,
    )
    .unwrap();
    let server = RunningServer::start_for_test(LOOPBACK.ip(), &config, auth).unwrap();
    Fixture {
        directory,
        server,
        upstream: Arc::default(),
    }
}

const NATIVE_CREATE: &[u8] =
    br#"{"type":"response.create","model":"gpt-fixture","input":"drain fixture"}"#;

async fn reject_native_upgrade(
    listener: &AsyncListener,
    delay: Duration,
) -> (AsyncStream, std::time::Instant) {
    let (mut ws, _) = listener.accept().await.unwrap();
    let request = String::from_utf8(head(&mut ws).await.unwrap()).unwrap();
    assert_eq!(
        parse_request(&request).unwrap().header("Upgrade"),
        Some("websocket")
    );
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let rejected_at = std::time::Instant::now();
    ws.write_all(
        b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    ws.shutdown().await.unwrap();
    (ws, rejected_at)
}

#[test]
fn native_http_fallback_drain_cancels_open_and_allows_restore() {
    let listener = TcpListener::bind(LOOPBACK).unwrap();
    listener.set_nonblocking(true).unwrap();
    native_fixture(listener.local_addr().unwrap()).run_wire(async |fixture, address| {
        let listener = AsyncListener::from_std(listener).unwrap();
        let session = fixture.session();
        let wire = upgrade(
            fixture.port(),
            "/v1/responses",
            &session,
            &["originator: codex_cli_rs"],
            &[],
        );
        let mut client = RawClient::request(address, &wire).await;
        assert_upgrade(&client);
        client
            .io
            .write_all(&websocket_frame(0x81, MASK, NATIVE_CREATE))
            .await
            .unwrap();
        receive_event(&mut client, "codex.response.metadata").await;
        let deadline = Deadline::after(wire_timeout());
        let (_rejected, _) = timeout(
            deadline.remaining(),
            reject_native_upgrade(&listener, Duration::ZERO),
        )
        .await
        .expect("native upstream WebSocket upgrade request");
        deadline
            .accept((), "rejected native WebSocket upgrade")
            .unwrap();
        let mut upstream = timeout(deadline.remaining(), async {
            let (mut http, _) = listener.accept().await.unwrap();
            let request = String::from_utf8(head(&mut http).await.unwrap()).unwrap();
            let request = parse_request(&request).unwrap();
            assert_eq!(request.method, crate::http::request::RequestMethod::Post);
            let mut body = vec![0; request.header("Content-Length").unwrap().parse().unwrap()];
            http.read_exact(&mut body).await.unwrap();
            let body = emp_transport::decode_content(
                body,
                request.header("Content-Encoding").unwrap_or(""),
                4 * 1024 * 1024,
                None,
            )
            .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap()["model"],
                "gpt-fixture"
            );
            http
        })
        .await
        .expect("native HTTP fallback POST after 426");
        deadline.accept((), "native HTTP fallback POST").unwrap();
        let restore = begin_restore(fixture, address, &session);
        assert_eq!(
            read_websocket_frame(&mut client.io, false, &Deadline::after(wire_timeout())).await,
            (0x88, close_payload(1001, "EMP connection is draining")),
        );
        assert_eq!(
            timeout(wire_timeout(), upstream.read(&mut [0]))
                .await
                .expect("drain must cancel the request waiting for HTTP response headers")
                .unwrap(),
            0,
        );
        let reply = timeout(wire_timeout(), restore)
            .await
            .expect("restore must complete without waiting for the upstream")
            .unwrap();
        assert_status(&reply, 200);
        assert_eq!(
            fixture.server.state.lifecycle.admission.active_websockets(),
            0
        );
    });
}

#[test]
fn native_http_fallback_open_error_uses_sse_transport_and_fallback_clock() {
    {
        let listener = TcpListener::bind(LOOPBACK).unwrap();
        listener.set_nonblocking(true).unwrap();
        native_fixture(listener.local_addr().unwrap()).run_wire(async |fixture, address| {
            let listener = AsyncListener::from_std(listener).unwrap();
            let session = fixture.session();
            let wire = upgrade(
                fixture.port(),
                "/v1/responses",
                &session,
                &["originator: codex_cli_rs", "Session-Id: fallback-open-error"],
                &[],
            );
            let mut client = RawClient::request(address, &wire).await;
            assert_upgrade(&client);
            client.io.write_all(&websocket_frame(0x81, MASK, NATIVE_CREATE)).await.unwrap();

            let ws_handshake_delay = Duration::from_millis(400);
            let deadline = Deadline::after(wire_timeout());
            let (_rejected, fallback_started) = timeout(deadline.remaining(), reject_native_upgrade(&listener, ws_handshake_delay))
                .await.expect("native upstream WebSocket upgrade request");
        deadline.accept((), "rejected native WebSocket upgrade").unwrap();
            let http_wait_ms = timeout(deadline.remaining(), async {
                let (mut http, _) = listener.accept().await.unwrap();
                let request = String::from_utf8(head(&mut http).await.unwrap()).unwrap();
                let request = parse_request(&request).unwrap();
                assert_eq!(request.method, crate::http::request::RequestMethod::Post);
                let mut body = vec![0; request.header("Content-Length").unwrap().parse().unwrap()];
                http.read_exact(&mut body).await.unwrap();
                let http_wait_started = std::time::Instant::now();
                tokio::time::sleep(ws_handshake_delay / 4).await;
                let http_wait_ms = http_wait_started.elapsed().as_millis() as u64;
                http.write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                ).await.unwrap();
                http.shutdown().await.unwrap();
                http_wait_ms
            }).await.expect("native HTTP fallback POST after 426");
        deadline.accept((), "native HTTP fallback POST").unwrap();
            let error = receive_event(&mut client, "error").await;
            let fallback_elapsed_ms = fallback_started.elapsed().as_millis() as u64;
            assert_eq!(error["status"], 403);

            let calls = exchange_wire(address, &request(fixture.port(), "GET", "/api/calls?start=0&end=9999999999", &[&session], &[]), false).await;
            let calls = json_body(&calls);
            let record = calls["records"].as_array().unwrap().iter()
                .find(|record| record["session_id"] == "fallback-open-error").unwrap();
            assert_eq!(record["status"], 403);
            assert_eq!(record["error_origin"], "upstream");
            assert_eq!(record["state"], "failed");
            let duration_ms = record["duration_ms"].as_u64().unwrap();
            assert!(
                duration_ms >= http_wait_ms,
                "call duration {duration_ms} ms is below the HTTP upstream wait of {http_wait_ms} ms"
            );
            assert!(
                duration_ms <= fallback_elapsed_ms,
                "call duration {duration_ms} ms exceeds the HTTP fallback window of {fallback_elapsed_ms} ms"
            );
            assert_eq!(record["transport"], "sse");
        });
    }
}

#[test]
fn native_generation_records_server_drain_separately_from_client_disconnect() {
    let listener = TcpListener::bind(LOOPBACK).unwrap();
    listener.set_nonblocking(true).unwrap();
    native_fixture(listener.local_addr().unwrap()).run_wire(async |fixture, endpoint| {
        let listener = AsyncListener::from_std(listener).unwrap();
        let session = fixture.session();
        let mut outcomes = Vec::new();
        let mut drain_ids = Vec::new();
        { let address = endpoint;
            for drain in [true, false] {
                let label = format!("ws-{}-{}", address.port(), if drain {"drain"} else {"disconnect"});
                let session_id = format!("Session-Id: {label}");
                let wire = upgrade(fixture.port(), "/v1/responses", &session, &["originator: codex_cli_rs", &session_id], &[]);
                let mut client = RawClient::request(address, &wire).await;
                assert_upgrade(&client);
                client.io.write_all(&websocket_frame(0x81, MASK, NATIVE_CREATE)).await.unwrap();
                let (mut upstream, _) = accept_upstream(&listener, "").await;
                let (flags, _) = read_websocket_frame(&mut upstream, true, &Deadline::after(wire_timeout())).await;
                assert_eq!(flags, 0x81);
                upstream.write_all(&websocket_frame(
                    0x81,
                    None,
                    br#"{"type":"response.created","response":{"id":"resp_draining","status":"in_progress","model":"gpt-fixture"}}"#,
                )).await.unwrap();
                receive_event(&mut client, "response.created").await;
                if drain {
                    let restore = begin_restore(fixture, address, &session);
                    assert_eq!(
                        read_websocket_frame(&mut client.io, false, &Deadline::after(wire_timeout())).await,
                        (0x88, close_payload(1001, "EMP connection is draining")),
                    );
                    assert_status(&timeout(wire_timeout(), restore).await.unwrap().unwrap(), 200);
                } else {
                    client.io.shutdown().await.unwrap();
                    assert_eq!(client.rest(&Deadline::after(wire_timeout())).await, b"\x88\x00");
                }
                assert_eq!(read_websocket_frame(&mut upstream, true, &Deadline::after(wire_timeout())).await, (0x88, 1000u16.to_be_bytes().to_vec()));
                let calls = exchange_wire(endpoint, &request(fixture.port(), "GET", "/api/calls?start=0&end=9999999999", &[&session], &[]), false).await;
                let calls = json_body(&calls);
                let record = calls["records"].as_array().unwrap().iter().find(|record| record["session_id"] == label).unwrap_or_else(|| panic!("missing call for {label} in {calls}"));
                outcomes.push((address, drain, record["error_origin"].clone(), record["error_class"].clone()));
                if drain {
                    drain_ids.push(record["request_id"].clone());
                }
                assert_eq!(record["state"], "cancelled", "{label}");
                assert_eq!(calls["summary"]["cancelled"], outcomes.len());
                assert_eq!(calls["summary"]["failed"], 0);
                assert_eq!(calls["summary"]["success_samples"], 0);
            }
        }
        let expected = [endpoint].into_iter().flat_map(|address| [
            (address, true, json!("emp"), json!("server_draining")),
            (address, false, json!("client"), json!("client_disconnect")),
        ]).collect::<Vec<_>>();
        assert_eq!(outcomes, expected);
        let requests = expected.len();
        let diagnostics = exchange_wire(endpoint, &request(fixture.port(), "GET", "/api/diagnostics", &[&session], &[]), false).await;
        let diagnostics = json_body(&diagnostics);
        assert_eq!(diagnostics["health"]["cancelled_count"], requests);
        assert_eq!(diagnostics["health"]["failure_count"], 0);
        assert_eq!(diagnostics["health"]["sample_count"], 0);
        let root = fixture.directory.path().to_owned();
        let journal = tokio::task::spawn_blocking(move || {
            let observations = || live_journal(&root).into_iter()
                .filter(|row| row["event"] == "route_observation")
                .map(|row| row["fields"].clone()).collect::<Vec<_>>();
            wait_for(&Deadline::after(wire_timeout()), || observations().len() >= requests);
            observations()
        }).await.unwrap();
        for id in drain_ids {
            for records in [diagnostics["records"].as_array().unwrap(), &journal] {
                let observation = records.iter().find(|record| record["request_id"] == id).unwrap();
                assert_eq!(observation["error_origin"], "emp");
                assert_eq!(observation["error_class"], "server_draining");
            }
        }
    });
}

async fn receive_event(client: &mut RawClient, kind: &str) -> Value {
    let deadline = Deadline::after(wire_timeout());
    loop {
        let (flags, payload) = read_websocket_frame(&mut client.io, false, &deadline).await;
        assert_eq!(flags, 0x81);
        let value: Value = serde_json::from_slice(&payload).unwrap();
        if value["type"] == kind {
            return deadline.accept(value, kind).unwrap();
        }
        assert!(
            !matches!(
                value["type"].as_str(),
                Some("error" | "response.failed" | "response.completed")
            ),
            "{value}"
        );
    }
}

async fn send_compressed(
    io: &mut AsyncStream,
    deflate: &mut flate2::Compress,
    event: &Value,
) -> usize {
    let json = serde_json::to_vec(event).unwrap();
    let mut compressed = Vec::with_capacity(json.len() + 128);
    deflate
        .compress_vec(&json, &mut compressed, flate2::FlushCompress::Sync)
        .unwrap();
    assert!(compressed.ends_with(&[0, 0, 255, 255]));
    compressed.truncate(compressed.len() - 4);
    io.write_all(&websocket_frame(0xc1, None, &compressed))
        .await
        .unwrap();
    compressed.len()
}

#[test]
fn responses_native_prefetch_and_deflate_context_survive_turns_and_disconnect() {
    let listener = TcpListener::bind(LOOPBACK).unwrap();
    listener.set_nonblocking(true).unwrap();
    let fixture = native_fixture(listener.local_addr().unwrap());
    fixture.run_wire(async |fixture, address| {
        let listener = AsyncListener::from_std(listener).unwrap();
        let session = fixture.session();
        let shared_text: String = (0..1800).scan(13_u32, |state, _| {
            *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            Some(char::from(b'!' + (*state >> 24) as u8 % 90))
        }).collect();
        {
            let (queued, wait_for_next) = tokio::sync::oneshot::channel();
            let peer = async {
                let (mut io, headers) = accept_upstream(
                    &listener,
                    "Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits=12; server_max_window_bits=12\r\n",
                ).await;
                assert!(headers.contains("Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits"));
                let mut inflate = flate2::Decompress::new_with_window_bits(false, 12);
                let mut deflate = flate2::Compress::new_with_window_bits(Compression::fast(), false, 12);
                let mut wait_for_next = Some(wait_for_next);
                for turn in 0..3 {
                    let (flags, mut compressed) = read_websocket_frame(&mut io, true, &Deadline::after(wire_timeout())).await;
                    assert_eq!(flags, 0xc1);
                    if turn > 0 {
                        assert!(compressed.len() < 256, "the upstream client reused its dictionary");
                    }
                    compressed.extend_from_slice(&[0, 0, 255, 255]);
                    let mut decoded = Vec::with_capacity(8192);
                    inflate.decompress_vec(&compressed, &mut decoded, flate2::FlushDecompress::Sync).unwrap();
                    let body: Value = serde_json::from_slice(&decoded).unwrap();
                    assert!(body["input"].to_string().contains(&serde_json::to_string(&shared_text).unwrap()));
                    let previous = if turn == 1 { Some("resp_first") } else { None };
                    assert_eq!(body.get("previous_response_id").and_then(Value::as_str), previous);
                    let id = if turn == 0 { "resp_first" } else { "resp_later" };
                    send_compressed(&mut io, &mut deflate, &json!({"type":"response.created","response":{"id":id,"status":"in_progress"}})).await;
                    if turn == 2 {
                        assert_eq!(read_websocket_frame(&mut io, true, &Deadline::after(wire_timeout())).await, (0x88, 1000u16.to_be_bytes().to_vec()));
                        return;
                    }
                    if turn == 0 {
                        let (flags, mut compressed) = read_websocket_frame(&mut io, true, &Deadline::after(wire_timeout())).await;
                        assert_eq!(flags, 0xc1);
                        compressed.extend_from_slice(&[0, 0, 255, 255]);
                        let mut decoded = Vec::with_capacity(1024);
                        inflate.decompress_vec(&compressed, &mut decoded, flate2::FlushDecompress::Sync).unwrap();
                        assert_eq!(serde_json::from_slice::<Value>(&decoded).unwrap(), json!({
                            "type": "response.interrupt", "response_id": "resp_first",
                            "mode": "discard_partial_items"
                        }));
                    }
                    if let Some(queued) = wait_for_next.take() {
                        queued.await.unwrap();
                    }
                    let completion = if turn == 0 {
                        json!({"type":"response.incomplete","response":{"id":id,"status":"incomplete",
                            "incomplete_details":{"reason":"interrupted"},"output":[],"end_turn":false,
                            "model":"gpt-fixture","fixture_context":shared_text}})
                    } else {
                        json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[],
                            "model":"gpt-fixture","fixture_context":shared_text}})
                    };
                    let encoded = send_compressed(&mut io, &mut deflate, &completion).await;
                    if turn == 1 {
                        assert!(encoded < 128, "the upstream reused its dictionary");
                    }
                }
            };
            let downstream = async {
                let first = json!({"type":"response.create","model":"gpt-fixture",
                    "input":[{"role":"user","content":shared_text}]});
                let wire = upgrade(
                    fixture.port(), "/v1/responses", &session,
                    &["originator: codex_cli_rs"], &websocket_frame(0x81, MASK, &serde_json::to_vec(&first).unwrap())
                );
                let mut client = RawClient::request(address, &wire).await;
                assert_upgrade(&client);
                assert_eq!(receive_event(&mut client, "response.created").await["response"]["id"], "resp_first");
                let interrupt = json!({
                    "type": "response.interrupt", "response_id": "resp_first",
                    "mode": "discard_partial_items"
                });
                client.io.write_all(&websocket_frame(0x81, MASK, &serde_json::to_vec(&interrupt).unwrap())).await.unwrap();
                let mut next = first.clone();
                next["previous_response_id"] = json!("resp_first");
                client.io.write_all(&websocket_frame(0x81, MASK, &serde_json::to_vec(&next).unwrap())).await.unwrap();
                queued.send(()).unwrap();
                assert_eq!(receive_event(&mut client, "response.incomplete").await["response"]["id"], "resp_first");
                assert_eq!(receive_event(&mut client, "response.completed").await["response"]["id"], "resp_later");
                client.io.write_all(&websocket_frame(0x81, MASK, &serde_json::to_vec(&first).unwrap())).await.unwrap();
                assert_eq!(receive_event(&mut client, "response.created").await["response"]["id"], "resp_later");
                client.io.shutdown().await.unwrap();
                assert_eq!(client.rest(&Deadline::after(wire_timeout())).await, b"\x88\x00");
            };
            tokio::join!(peer, downstream);
        }
    });
}
