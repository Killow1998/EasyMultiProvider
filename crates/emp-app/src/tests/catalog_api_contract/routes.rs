use super::*;
use crate::server::http::tests::{
    assert_entry_error, assert_sse_wire, assert_status, assert_wire, exchange_wire, json_body,
    json_request, request, wire_contract::assert_json_reply,
};
use crate::tests::internal_events_contract::final_journal;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::{TcpListener as AsyncListener, TcpStream as AsyncStream},
    time::timeout,
};

fn with_server(server: ServerHandle, exercise: impl AsyncFnOnce(&ServerHandle, SocketAddr)) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(exercise(&server, server.local_addr()));
    server.shutdown().unwrap();
}

#[test]
fn catalog_reads_preserve_visibility_etag_and_auth() {
    let upstream = CatalogUpstream::start(200);
    let (_directory, server) = catalog_server(&upstream);
    with_server(server, async |server, endpoint| {
        let port = endpoint.port();
        let session = session_header(server);
        let discover = json_request(
            port,
            "/api/providers/discover",
            &[&session],
            br#"{"provider":"demo"}"#,
        );
        let reply = exchange_wire(endpoint, &discover, false).await;
        assert_status(&reply, 200);
        upstream.observed();

        let no_store = "Cache-Control: no-store";
        let rich_path = "/v1/models?client_version=&client_version=0.130.0";
        #[cfg(unix)]
        {
            let refresh = &server.state.catalog_refresh;
            assert!(refresh.wait_until_idle(Duration::ZERO));
            let rich = request(port, "GET", rich_path, &[], b"");
            assert_status(&exchange_wire(endpoint, &rich, false).await, 200);
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
            if status == 200 {
                let reply = assert_wire(endpoint, &wire, false, status).await;
                assert!(String::from_utf8_lossy(&reply).contains(no_store));
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
            } else if method == "HEAD" || path == "/api/providers/demo/models" {
                assert_entry_error(endpoint, &wire, status).await;
            } else {
                let (status, error) = match (path, status) {
                    ("/v1/models/absent", 404) => (
                        "404 Not Found",
                        json!({"message":"unknown model: absent","origin":"emp"}),
                    ),
                    ("/v1/models/", 404) => (
                        "404 Not Found",
                        json!({"message":"unknown model: ","origin":"emp"}),
                    ),
                    ("/api/providers/absent/models", 400) => (
                        "400 Bad Request",
                        json!({"message":"provider is missing or disabled: absent","origin":"emp"}),
                    ),
                    ("/api/providers//models", 400) => (
                        "400 Bad Request",
                        json!({"message":"provider is missing or disabled: ","origin":"emp"}),
                    ),
                    ("/api/providers/models", 400) => (
                        "400 Bad Request",
                        json!({"message":"Subscription account is unavailable","origin":"emp"}),
                    ),
                    (_, 403) => (
                        "403 Forbidden",
                        json!({"message":"cross-origin model catalog request rejected"}),
                    ),
                    _ => unreachable!("{method} {path}: {status}"),
                };
                let reply = exchange_wire(endpoint, &wire, false).await;
                assert_json_reply(path, &reply, status, &json!({"error":error}));
            }
        }
        let rich = request(port, "GET", rich_path, &[], b"");
        let reply = assert_wire(endpoint, &rich, true, 200).await;
        assert!(json_body(&reply)["models"].is_array());
        let etag = String::from_utf8_lossy(&reply)
            .lines()
            .find_map(|line| line.strip_prefix("ETag: "))
            .unwrap()
            .to_owned();
        let conditional = format!("If-None-Match: {etag}");
        let wire = request(port, "GET", rich_path, &[&conditional], b"");
        assert_eq!(assert_wire(endpoint, &wire, false, 200).await, reply);

        let bearer = format!("Authorization: Bearer {}", server.state.backend.gateway_key);
        let gateway = request(
            port,
            "GET",
            rich_path,
            &["Host: outside.invalid", &bearer],
            b"",
        );
        assert_eq!(assert_wire(endpoint, &gateway, false, 200).await, reply);
        server
            .state
            .backend
            .modules
            .mark_unavailable(emp_modules::Module::Gateway, "test");
        let rejected = exchange_wire(endpoint, &gateway, false).await;
        assert_json_reply(
            rich_path,
            &rejected,
            "403 Forbidden",
            &json!({"error":{"message":"cross-origin model catalog request rejected"}}),
        );
        assert_eq!(assert_wire(endpoint, &rich, false, 200).await, reply);
        assert!(upstream.requests.try_recv().is_err());
    });
}

#[test]
fn metadata_preserves_read_write_and_body_contracts() {
    let upstream = CatalogUpstream::start_with_payload(
        200,
        json!({"inputTokenLimit":1024,"outputTokenLimit":128}),
    );
    let (directory, server) = metadata_server_at(upstream.address, HttpClientPolicy::default());
    with_server(server, async |server, endpoint| {
        let port = endpoint.port();
        let session = session_header(server);
        let path = "/api/models/metadata";
        for (headers, status) in [
            ([].as_slice(), 401),
            ([&*session, &*session].as_slice(), 401),
            (
                [&*session, "Origin: https://outside.invalid"].as_slice(),
                403,
            ),
        ] {
            let wire = json_request(port, path, headers, b"invalid");
            assert_entry_error(endpoint, &wire, status).await;
        }
        for (body, message) in [
            (b"[]".as_slice(), "request body must be a JSON object"),
            (
                br#"{"provider":"absent","model":"x"}"#.as_slice(),
                "provider is missing or disabled: absent",
            ),
            (b"{}".as_slice(), "provider and model are required"),
        ] {
            let wire = json_request(port, path, &[&session], body);
            let reply = exchange_wire(endpoint, &wire, false).await;
            assert_json_reply(
                path,
                &reply,
                "400 Bad Request",
                &json!({"error":{"message":message,"origin":"emp"}}),
            );
        }
        let head = request(port, "HEAD", path, &[&session], b"");
        assert_entry_error(endpoint, &head, 404).await;
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
        let reply = exchange_wire(endpoint, &partial, true).await;
        assert_json_reply(
            path,
            &reply,
            "400 Bad Request",
            &json!({"error":{"message":"request body is incomplete","origin":"emp"}}),
        );
        assert!(upstream.requests.try_recv().is_err());

        let wire = json_request(
            port,
            path,
            &[&session],
            br#"{"provider":"gemini","model":"gemini/alpha beta/preview"}"#,
        );
        for end_input in [true, false] {
            {
                let address = endpoint;
                checked_metadata_reply(
                    address,
                    &wire,
                    end_input,
                    200,
                    &upstream,
                    "/v1beta/models/alpha%20beta%2Fpreview",
                )
                .await;
            }
        }
        assert!(!directory.path().join("provider-model-lists").exists());
    });
}

#[test]
fn metadata_preserves_upstream_rejections() {
    let upstream = CatalogUpstream::start(429);
    let (_directory, server) = metadata_server_at(upstream.address, HttpClientPolicy::default());
    with_server(server, async |server, endpoint| {
        let wire = json_request(
            endpoint.port(),
            "/api/models/metadata",
            &[&session_header(server)],
            br#"{"provider":"gemini","model":"rate-limited"}"#,
        );

        {
            let address = endpoint;
            checked_metadata_reply(
                address,
                &wire,
                false,
                429,
                &upstream,
                "/v1beta/models/rate-limited",
            )
            .await;
        }
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
    {
        let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        upstream.set_nonblocking(true).unwrap();
        let (directory, server) =
            metadata_server_at(upstream.local_addr().unwrap(), HttpClientPolicy::default());
        let address = server.local_addr();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let upstream = AsyncListener::from_std(upstream).unwrap();
                let request = json_request(
                    address.port(),
                    "/api/models/metadata",
                    &[&session_header(&server)],
                    br#"{"provider":"gemini","model":"disconnect"}"#,
                );
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
            });
        server.shutdown().unwrap();
        let records = final_journal(directory.path());
        let completed: Vec<_> = records
            .iter()
            .filter(|record| {
                record["event"] == "operation_finished"
                    && record["fields"]["operation"] == "model_metadata"
            })
            .collect();
        assert_eq!(completed.len(), 1);
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
}

#[test]
fn calls_sse_preserves_events_after_http_framing() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = canonical_root(&directory);
    let server = ServerHandle::start_for_test(
        Ipv4Addr::LOCALHOST.into(),
        &root.join("config.json"),
        root.join("codex/auth.json"),
    )
    .unwrap();
    with_server(server, async |server, endpoint| {
        let wire = request(
            endpoint.port(),
            "GET",
            "/api/calls?start=0&end=4102444800",
            &[&session_header(server), "Accept: text/event-stream"],
            b"",
        );
        let reply = assert_sse_wire(endpoint, &wire).await;
        let reply = std::str::from_utf8(&reply).unwrap();
        assert!(reply.contains("Content-Type: text/event-stream\r\n"));
        assert!(reply.contains("event: result\n"));
    });
}
