use super::*;
use crate::api::realtime::MAX_REALTIME_REQUEST_BYTES;
use crate::server::raw_tcp::{RawClient, head, read_websocket_frame, websocket_frame};
use crate::tests::realtime_contract::{
    RealtimeUpstream, UpstreamResponse, app_server_for, multipart_offer,
};
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
    "Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
);

fn upgrade(port: u16, path: &str, session: &str, other: &[&str], prefix: &[u8]) -> Vec<u8> {
    let mut headers = UPGRADE.to_vec();
    headers.push(session);
    headers.extend_from_slice(other);
    request(port, "GET", path, &headers, prefix)
}

fn assert_upgrade(client: &RawClient) {
    assert_eq!(
        client
            .head
            .replace("Sec-Websocket-Accept:", "Sec-WebSocket-Accept:"),
        SWITCHING_PROTOCOLS
    );
}

fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    [code.to_be_bytes().as_slice(), reason.as_bytes()].concat()
}

#[test]
fn websocket_handshake_errors_match_legacy_bytes() {
    let fixture = Fixture::new();
    fixture.run_candidate(async |fixture, new| {
        let port = fixture.port();
        let session = fixture.session();
        for path in ["/v1/responses", "/v1/live/rtc_valid"] {
            for (headers, status) in [
                (vec!["Authorization: Bearer invalid"], 401),
                (vec!["Origin: https://other.example"], 403),
                (vec![session.as_str(), "Sec-WebSocket-Version: 12"], 400),
                (vec![session.as_str(), "Connection: keep-alive"], 400),
                (vec![session.as_str(), "Sec-WebSocket-Key: invalid"], 400),
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
                fixture
                    .parity(new, &request(port, "GET", path, &all, &[]), status)
                    .await;
            }
        }
        for path in ["/v1/live/", "/v1/live/rtc_bad%2Fid", "/v1/live/rtc_bad/id"] {
            fixture
                .parity(new, &upgrade(port, path, &session, &[], &[]), 400)
                .await;
        }
    });
}

#[test]
fn websocket_upgrade_carries_prefix_and_keeps_legacy_capacity_rejections() {
    let fixture = Fixture::new();
    fixture.run_candidate(async |fixture, new| {
        let port = fixture.port();
        let session = fixture.session();
        let old = fixture.legacy.local_addr();
        let state = &fixture.legacy.state;
        let slots = std::iter::from_fn(|| state.connection_admission.acquire_websocket())
            .collect::<Vec<_>>();
        let sideband = upgrade(port, "/v1/live/rtc_busy", &session, &[], &[]);
        let reply = fixture.parity(new, &sideband, 503).await;
        assert!(
            reply
                .windows(b"Retry-After: 2".len())
                .any(|window| window == b"Retry-After: 2")
        );
        assert_eq!(
            json_body(&reply)["error"]["code"],
            "realtime_capacity_unavailable"
        );
        let responses = upgrade(port, "/v1/responses", &session, &[], &[]);
        for address in [old, new] {
            let mut client = RawClient::request(address, &responses).await;
            assert_upgrade(&client);
            assert_eq!(
                read_websocket_frame(&mut client.io, false).await,
                (0x88, close_payload(1013, "too many websocket connections"))
            );
            assert!(client.rest().await.is_empty());
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
        for address in [old, new] {
            let mut client = RawClient::request(address, &wire).await;
            assert_upgrade(&client);
            assert_eq!(
                read_websocket_frame(&mut client.io, false).await,
                (0x8a, b"coalesced".to_vec())
            );
            assert_eq!(state.connection_admission.active_websockets(), 1);
            assert_eq!(
                state.connection_admission.active_requests(),
                usize::from(address == old)
            );
            client
                .io
                .write_all(&websocket_frame(0x88, MASK, &1000u16.to_be_bytes()))
                .await
                .unwrap();
            assert_eq!(
                read_websocket_frame(&mut client.io, false).await,
                (0x88, 1000u16.to_be_bytes().to_vec())
            );
            assert!(client.rest().await.is_empty());
        }
        assert_eq!(state.connection_admission.active_websockets(), 0);
    });
}

#[test]
fn responses_drain_closes_sessions_and_rejects_new_tcp_handshakes() {
    let fixture = Fixture::new();
    fixture.run_candidate(async |fixture, new| {
        let port = fixture.port();
        let session = fixture.session();
        for address in [fixture.legacy.local_addr(), new] {
            let wire = upgrade(
                port,
                "/v1/responses",
                &session,
                &[],
                &websocket_frame(0x89, MASK, b"ready"),
            );
            let mut client = RawClient::request(address, &wire).await;
            assert_eq!(
                read_websocket_frame(&mut client.io, false).await,
                (0x8a, b"ready".to_vec())
            );
            let state = fixture.legacy.state.clone();
            let exempt = usize::from(address != new);
            let (ready, drained) = tokio::sync::oneshot::channel();
            let (reopen, wait) = tokio::sync::oneshot::channel();
            let worker = tokio::task::spawn_blocking(move || {
                let guard = state
                    .connection_admission
                    .quiesce(exempt, Duration::from_secs(5))
                    .unwrap();
                ready.send(()).unwrap();
                wait.blocking_recv().unwrap();
                drop(guard);
            });
            assert_eq!(
                read_websocket_frame(&mut client.io, false).await,
                (0x88, close_payload(1001, "EMP connection is draining"))
            );
            assert!(client.rest().await.is_empty());
            drained.await.unwrap();
            let mut denied = AsyncStream::connect(address).await.unwrap();
            let _ = denied.write_all(&wire).await;
            let result = timeout(Duration::from_secs(5), denied.read_u8())
                .await
                .unwrap();
            assert!(result.is_err(), "drain must not send an HTTP response");
            assert_eq!(
                fixture
                    .legacy
                    .state
                    .connection_admission
                    .active_websockets(),
                0
            );
            reopen.send(()).unwrap();
            worker.await.unwrap();
            let mut reopened = RawClient::request(address, &wire).await;
            assert_upgrade(&reopened);
            assert_eq!(
                read_websocket_frame(&mut reopened.io, false).await,
                (0x8a, b"ready".to_vec())
            );
            reopened.io.shutdown().await.unwrap();
            assert_eq!(reopened.rest().await, b"\x88\x00");
        }
    });
}

fn realtime_fixture(upstream: &RealtimeUpstream) -> (tempfile::TempDir, Fixture) {
    let native = tempfile::tempdir().unwrap();
    let auth = native.path().join("auth.json");
    std::fs::write(
        &auth,
        br#"{"tokens":{"access_token":"native-secret","account_id":"acct-native"}}"#,
    )
    .unwrap();
    let (directory, legacy) = app_server_for(upstream, &auth);
    (
        native,
        Fixture {
            directory,
            legacy,
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
        let upstream = RealtimeUpstream::start(response);
        let candidate_upstream = RealtimeUpstream::start(response);
        let (_old_auth, old) = realtime_fixture(&upstream);
        let (_new_auth, new) = realtime_fixture(&candidate_upstream);
        let old_address = old.legacy.local_addr();
        new.run_candidate(async |new, address| {
            let boundary = "Content-Type: multipart/form-data; boundary=VoiceFixtureBoundary";
            let body = multipart_offer();
            let valid_length = format!("Content-Length: {}", body.len());
            let big_length = format!("Content-Length: {}", MAX_REALTIME_REQUEST_BYTES + 1);
            for (status, length, body) in [
                (413, &big_length, &[][..]),
                (response.status, &valid_length, body.as_slice()),
            ] {
                let old_wire = request(
                    old.port(),
                    "POST",
                    "/v1/live",
                    &[&old.session(), boundary, length],
                    body,
                );
                let new_wire = request(
                    new.port(),
                    "POST",
                    "/v1/live",
                    &[&new.session(), boundary, length],
                    body,
                );
                let reply = assert_wire_parity_with(
                    [(old_address, &old_wire), (address, &new_wire)],
                    false,
                    |reply| reply,
                )
                .await;
                assert_status(&reply, status);
                if status == 201 {
                    assert!(
                        String::from_utf8(reply)
                            .unwrap()
                            .contains("Location: /v1/live/rtc_h06b\r\n")
                    );
                } else if status == 403 {
                    assert_eq!(
                        json_body(&reply)["error"]["code"],
                        "native_subscription_auth_failed"
                    );
                }
            }
        });
        old.legacy.shutdown().unwrap();
    }
}

async fn accept_upstream(listener: &AsyncListener, extensions: &str) -> (AsyncStream, String) {
    let (mut io, _) = timeout(Duration::from_secs(5), listener.accept())
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
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n{extensions}\r\n"
    );
    io.write_all(wire.as_bytes()).await.unwrap();
    (io, headers)
}

#[test]
fn sideband_upgrade_preserves_raw_relay_and_tcp_half_close() {
    let listener = TcpListener::bind(LOOPBACK).unwrap();
    listener.set_nonblocking(true).unwrap();
    let fixture = native_fixture(listener.local_addr().unwrap());
    fixture.run_candidate(async |fixture, new| {
        let listener = AsyncListener::from_std(listener).unwrap();
        let target = format!("ws://{}/v1/live/rtc_relay", listener.local_addr().unwrap());
        fixture.legacy.state.test_sideband_url.set(target).unwrap();
        let session = fixture.session();
        let first = br#"{ "type": "client.first" }"#;
        let event = br#"{ "type": "server.event" }"#;
        for address in [fixture.legacy.local_addr(), new] {
            let peer = async {
                let (mut io, headers) = accept_upstream(&listener, "").await;
                let request = parse_request(&headers).unwrap();
                assert_eq!(request.raw_path(), "/v1/live/rtc_relay");
                assert_eq!(
                    request.header("Authorization"),
                    Some("Bearer native-secret")
                );
                assert_eq!(request.header("Session-Id"), Some("voice-fixture"));
                assert_eq!(
                    read_websocket_frame(&mut io, true).await,
                    (0x81, first.to_vec())
                );
                io.write_all(&websocket_frame(0x81, None, event))
                    .await
                    .unwrap();
                assert_eq!(
                    read_websocket_frame(&mut io, true).await,
                    (0x88, 1000u16.to_be_bytes().to_vec())
                );
            };
            let client = async {
                let wire = upgrade(
                    fixture.port(),
                    "/v1/live/rtc_relay?fixture=1",
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
                    read_websocket_frame(&mut client.io, false).await,
                    (0x81, event.to_vec())
                );
                assert_eq!(
                    fixture
                        .legacy
                        .state
                        .connection_admission
                        .active_websockets(),
                    1
                );
                client.io.shutdown().await.unwrap();
                assert_eq!(client.rest().await, b"\x88\x00");
            };
            tokio::join!(peer, client);
        }
    });
}

fn native_fixture(address: SocketAddr) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
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
    let legacy = LegacyServer::start_with_config_options(
        LOOPBACK.ip(),
        0,
        &config,
        "missing-test-codex",
        auth,
    )
    .unwrap();
    Fixture {
        directory,
        legacy,
        upstream: Arc::default(),
    }
}

async fn receive_event(client: &mut RawClient, kind: &str) -> Value {
    loop {
        let (flags, payload) = read_websocket_frame(&mut client.io, false).await;
        assert_eq!(flags, 0x81);
        let value: Value = serde_json::from_slice(&payload).unwrap();
        if value["type"] == kind {
            return value;
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
    fixture.run_candidate(async |fixture, new| {
        let listener = AsyncListener::from_std(listener).unwrap();
        let session = fixture.session();
        let shared_text: String = (0..1800).scan(13_u32, |state, _| {
            *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            Some(char::from(b'!' + (*state >> 24) as u8 % 90))
        }).collect();
        for address in [fixture.legacy.local_addr(), new] {
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
                    let (flags, mut compressed) = read_websocket_frame(&mut io, true).await;
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
                        assert_eq!(read_websocket_frame(&mut io, true).await, (0x88, 1000u16.to_be_bytes().to_vec()));
                        return;
                    }
                    if turn == 0 {
                        let (flags, mut compressed) = read_websocket_frame(&mut io, true).await;
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
                assert_eq!(client.rest().await, b"\x88\x00");
            };
            tokio::join!(peer, downstream);
        }
    });
}
