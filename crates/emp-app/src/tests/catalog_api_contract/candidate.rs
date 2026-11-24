use super::*;
use crate::server::http::tests::{
    assert_sse_wire_parity, assert_status, assert_wire, assert_wire_parity,
    assert_wire_response_parity, exchange_wire, json_body, json_request, request,
};
use crate::tests::internal_events_contract::final_journal;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::{TcpListener as AsyncListener, TcpStream as AsyncStream},
    time::timeout,
};

fn with_candidate(server: ServerHandle, exercise: impl AsyncFnOnce(&ServerHandle, SocketAddr)) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let listener = AsyncListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let address = listener.local_addr().unwrap();
            let candidate = tokio::spawn(crate::server::serve_candidate(
                listener,
                server.state.clone(),
            ));
            exercise(&server, address).await;
            server.state.request_shutdown();
            candidate.await.unwrap().unwrap();
        });
    server.shutdown().unwrap();
}

#[test]
fn catalog_reads_match_legacy_wire_on_the_candidate_listener() {
    let upstream = CatalogUpstream::start(200);
    let (_directory, server) = catalog_server(&upstream);
    with_candidate(server, async |server, candidate| {
        let old = server.local_addr();
        let port = old.port();
        let session = session_header(server);
        let discover = json_request(
            port,
            "/api/providers/discover",
            &[&session],
            br#"{"provider":"demo"}"#,
        );
        let reply = exchange_wire(candidate, &discover, false).await;
        assert_status(&reply, 200);
        upstream.observed();

        let no_store = "Cache-Control: no-store";
        let rich_path = "/v1/models?client_version=&client_version=0.130.0";
        #[cfg(unix)]
        {
            let refresh = &server.state.catalog_refresh;
            assert!(refresh.wait_until_idle(Duration::ZERO));
            let rich = request(port, "GET", rich_path, &[], b"");
            assert_status(&exchange_wire(candidate, &rich, false).await, 200);
            assert!(!refresh.wait_until_idle(Duration::ZERO));
        }
        for (method, path, headers, status) in [
            (
                "GET",
                "/v1/models",
                ["Accept: text/event-stream"].as_slice(),
                200,
            ),
            ("GET", "/v1/models/demo%2Fold", [].as_slice(), 200),
            ("GET", "/v1/models/demo/old", [].as_slice(), 200),
            ("GET", "/v1/models/absent", [].as_slice(), 404),
            ("GET", "/v1/models/", [].as_slice(), 404),
            (
                "GET",
                "/v1/models",
                ["Host: outside.invalid"].as_slice(),
                403,
            ),
            (
                "GET",
                "/v1/models/demo%2Fold",
                ["Origin: https://outside.invalid"].as_slice(),
                403,
            ),
            (
                "GET",
                "/api/providers/%64emo/models",
                [&*session].as_slice(),
                200,
            ),
            ("GET", "/api/providers/demo/models", [].as_slice(), 401),
            (
                "GET",
                "/api/providers/demo/models",
                [&*session, "Origin: https://outside.invalid"].as_slice(),
                403,
            ),
            (
                "GET",
                "/api/providers/absent/models",
                [&*session].as_slice(),
                400,
            ),
            ("GET", "/api/providers//models", [&*session].as_slice(), 400),
            ("GET", "/api/providers/models", [&*session].as_slice(), 400),
            ("HEAD", "/v1/models", [].as_slice(), 404),
            ("HEAD", "/v1/models/demo%2Fold", [].as_slice(), 404),
            (
                "HEAD",
                "/api/providers/demo/models",
                [&*session].as_slice(),
                404,
            ),
        ] {
            let wire = request(port, method, path, headers, b"");
            let reply = assert_wire_parity(old, candidate, &wire, false, status).await;
            assert!(String::from_utf8_lossy(&reply).contains(no_store));
            if status == 200 {
                let body = json_body(&reply);
                if path == "/v1/models" {
                    assert_eq!(body["object"], "list");
                    assert!(
                        body["data"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|model| model["id"] == "demo/old")
                    );
                } else if path.starts_with("/v1/") {
                    assert_eq!(body["id"], "demo/old");
                } else {
                    assert_eq!(body["cached"], true);
                    assert_eq!(body["models"].as_array().unwrap().len(), 2);
                }
            }
        }
        let rich = request(port, "GET", rich_path, &[], b"");
        let reply = assert_wire_parity(old, candidate, &rich, true, 200).await;
        assert!(json_body(&reply)["models"].is_array());
        let etag = String::from_utf8_lossy(&reply)
            .lines()
            .find_map(|line| line.strip_prefix("ETag: "))
            .unwrap()
            .to_owned();
        let conditional = format!("If-None-Match: {etag}");
        let wire = request(port, "GET", rich_path, &[&conditional], b"");
        assert_eq!(
            assert_wire_parity(old, candidate, &wire, false, 200).await,
            reply
        );

        let bearer = format!("Authorization: Bearer {}", server.state.backend.gateway_key);
        let gateway = request(
            port,
            "GET",
            rich_path,
            &["Host: outside.invalid", &bearer],
            b"",
        );
        assert_eq!(
            assert_wire_parity(old, candidate, &gateway, false, 200).await,
            reply
        );
        server
            .state
            .backend
            .modules
            .mark_unavailable(emp_modules::Module::Gateway, "test");
        assert_wire_parity(old, candidate, &gateway, false, 403).await;
        assert_eq!(
            assert_wire_parity(old, candidate, &rich, false, 200).await,
            reply
        );
        assert!(upstream.requests.try_recv().is_err());
    });
}

#[test]
fn metadata_matches_legacy_wire_on_the_candidate_listener() {
    let upstream = CatalogUpstream::start_with_payload(
        200,
        json!({"inputTokenLimit":1024,"outputTokenLimit":128}),
    );
    let (directory, server) = metadata_server(&upstream);
    with_candidate(server, async |server, candidate| {
        let old = server.local_addr();
        let port = old.port();
        let session = session_header(server);
        let path = "/api/models/metadata";
        for (headers, body, status) in [
            ([].as_slice(), b"invalid".as_slice(), 401),
            (
                [&*session, &*session].as_slice(),
                b"invalid".as_slice(),
                401,
            ),
            (
                [&*session, "Origin: https://outside.invalid"].as_slice(),
                b"invalid".as_slice(),
                403,
            ),
            ([&*session].as_slice(), b"[]".as_slice(), 400),
            (
                [&*session].as_slice(),
                br#"{"provider":"absent","model":"x"}"#.as_slice(),
                400,
            ),
            ([&*session].as_slice(), b"{}".as_slice(), 400),
        ] {
            let wire = json_request(port, path, headers, body);
            assert_wire_parity(old, candidate, &wire, false, status).await;
        }
        let head = request(port, "HEAD", path, &[&session], b"");
        assert_wire_parity(old, candidate, &head, false, 404).await;
        let partial = request(
            port,
            "POST",
            path,
            &[
                &session,
                "Content-Type: application/json",
                "Content-Length: 20",
            ],
            b"{",
        );
        let reply = assert_wire_parity(old, candidate, &partial, true, 400).await;
        assert_eq!(
            json_body(&reply)["error"]["message"],
            "request body is incomplete"
        );
        assert!(upstream.requests.try_recv().is_err());

        let wire = json_request(
            port,
            path,
            &[&session],
            br#"{"provider":"gemini","model":"gemini/alpha beta/preview"}"#,
        );
        for end_input in [true, false] {
            let (legacy_reply, candidate_reply) = (
                checked_metadata_reply(
                    old,
                    &wire,
                    end_input,
                    200,
                    &upstream,
                    "/v1beta/models/alpha%20beta%2Fpreview",
                )
                .await,
                checked_metadata_reply(
                    candidate,
                    &wire,
                    end_input,
                    200,
                    &upstream,
                    "/v1beta/models/alpha%20beta%2Fpreview",
                )
                .await,
            );
            assert_wire_response_parity(&wire, &legacy_reply, &candidate_reply);
        }
        assert!(!directory.path().join("provider-model-lists").exists());
    });
}

#[test]
fn metadata_upstream_rejection_matches_legacy_wire_on_the_candidate_listener() {
    let upstream = CatalogUpstream::start(429);
    let (_directory, server) = metadata_server(&upstream);
    with_candidate(server, async |server, candidate| {
        let old = server.local_addr();
        let wire = json_request(
            old.port(),
            "/api/models/metadata",
            &[&session_header(server)],
            br#"{"provider":"gemini","model":"rate-limited"}"#,
        );
        let legacy_reply = checked_metadata_reply(
            old,
            &wire,
            false,
            429,
            &upstream,
            "/v1beta/models/rate-limited",
        )
        .await;
        let candidate_reply = checked_metadata_reply(
            candidate,
            &wire,
            false,
            429,
            &upstream,
            "/v1beta/models/rate-limited",
        )
        .await;
        assert_wire_response_parity(&wire, &legacy_reply, &candidate_reply);
    });
}

async fn checked_metadata_reply(
    address: SocketAddr,
    wire: &[u8],
    end_input: bool,
    status: u16,
    upstream: &CatalogUpstream,
    upstream_path: &str,
) -> Vec<u8> {
    let reply = assert_wire(address, wire, end_input, status).await;
    upstream.observed_metadata(&[upstream_path]);
    assert!(
        matches!(
            upstream.requests.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ),
        "expected exactly one upstream request for {upstream_path}"
    );
    let body = json_body(&reply);
    if status == 200 {
        assert!(String::from_utf8_lossy(&reply).contains("Cache-Control: no-store"));
        assert_eq!(body["context_window"], 1024);
    } else {
        assert_eq!(body["error"]["code"], "rate_limit_exceeded");
    }
    reply
}

#[test]
fn metadata_finishes_admitted_upstream_work_after_the_client_disconnects() {
    let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    upstream.set_nonblocking(true).unwrap();
    let (directory, server) =
        metadata_server_at(upstream.local_addr().unwrap(), HttpClientPolicy::default());
    with_candidate(server, async |server, candidate| {
        let upstream = AsyncListener::from_std(upstream).unwrap();
        let old = server.local_addr();
        let request = json_request(
            old.port(),
            "/api/models/metadata",
            &[&session_header(server)],
            br#"{"provider":"gemini","model":"disconnect"}"#,
        );
        for address in [old, candidate] {
            timeout(Duration::from_secs(5), async {
                let mut client = AsyncStream::connect(address).await.unwrap();
                client.write_all(&request).await.unwrap();
                let (peer, _) = upstream.accept().await.unwrap();
                let mut peer = tokio::io::BufReader::new(peer);
                let mut line = String::new();
                peer.read_line(&mut line).await.unwrap();
                assert_eq!(line, "GET /v1beta/models/disconnect HTTP/1.1\r\n");
                loop {
                    line.clear();
                    assert_ne!(peer.read_line(&mut line).await.unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                client.shutdown().await.unwrap();
                drop(client);
                let payload = br#"{"inputTokenLimit":1024,"outputTokenLimit":128}"#;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                peer.get_mut().write_all(head.as_bytes()).await.unwrap();
                peer.get_mut().write_all(payload).await.unwrap();
            })
            .await
            .unwrap();
            let state = server.state.clone();
            tokio::task::spawn_blocking(move || {
                assert!(
                    state
                        .lifecycle
                        .admission
                        .quiesce(0, Duration::from_secs(5))
                        .is_some()
                );
            })
            .await
            .unwrap();
        }
    });
    let records = final_journal(directory.path());
    let completed: Vec<_> = records
        .iter()
        .filter(|record| {
            record["event"] == "operation_finished"
                && record["fields"]["operation"] == "model_metadata"
        })
        .collect();
    assert_eq!(completed.len(), 2);
    for record in completed {
        let fields = &record["fields"];
        assert_eq!(fields["outcome"], "completed");
        assert!(
            fields["request_id"]
                .as_str()
                .is_some_and(|id| records.iter().any(|record| {
                    record["event"] == "http_request_completed"
                        && record["fields"]["request_id"] == id
                }))
        );
    }
}

#[test]
fn calls_sse_matches_legacy_after_http_framing() {
    let (_directory, server) = test_server();
    with_candidate(server, async |server, candidate| {
        let old = server.local_addr();
        let wire = request(
            old.port(),
            "GET",
            "/api/calls?start=0&end=4102444800",
            &[&session_header(server), "Accept: text/event-stream"],
            b"",
        );
        let reply = assert_sse_wire_parity(old, candidate, &wire).await;
        let reply = std::str::from_utf8(&reply).unwrap();
        assert!(reply.contains("Content-Type: text/event-stream\r\n"));
        assert!(reply.contains("event: result\n"));
    });
}
