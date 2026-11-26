use super::{
    SIDEBAND, accept_sideband, budget, fixture_with_sideband, read_control_request,
    registry::with_registry_server, write_response,
};
use crate::{
    lifecycle::ServerHandle,
    modules::process::ModuleProcess,
    server::http::tests::json_body,
    server::{
        http::tests::{assert_status, assert_wire, read_chunk_data, request},
        raw_tcp::head as read_head,
    },
};
use emp_modules::{
    handshake::{INITIALIZE, Initialize, Initialized, ROUTE_ID_HEADER},
    manifest::ModuleManifest,
    peer::Handlers,
};
use emp_test_support::{http::ChunkDecoder, test_timeout};
use std::{fs::File, future::pending, io, net::Ipv4Addr, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpSocket, TcpStream},
    sync::Notify,
    task::JoinSet,
    time::timeout,
};

const PREFIX: &str = "/api/module-runtime/data";
const MANIFEST: &str = r#"{
    "id":"data","module_version":"1.0.0","host_api":{"major":3,"min_minor":0},
    "contributions":[
        {"type":"http_route","id":"echo","method":"PUT","path":"/items/%61","auth":"management"},
        {"type":"http_route","id":"wait","method":"GET","path":"/wait","auth":"management"},
        {"type":"http_route","id":"upload","method":"POST","path":"/upload","auth":"management"},
        {"type":"http_route","id":"sse","method":"GET","path":"/sse","auth":"management"},
        {"type":"http_route","id":"probe","method":"GET","path":"/probe","auth":"management"},
        {"type":"http_route","id":"drop","method":"GET","path":"/drop","auth":"management"},
        {"type":"http_route","id":"redirect","method":"GET","path":"/redirect","auth":"management"},
        {"type":"http_route","id":"framing","method":"GET","path":"/framing","auth":"management"},
        {"type":"http_route","id":"truncated","method":"GET","path":"/truncated","auth":"management"}
    ]
}"#;
const FIRST_EVENT: &[u8] = b"8\r\ndata:a\n\n\r\n";
const LAST_EVENT: &[u8] = b"8\r\ndata:z\n\n\r\n0\r\nX-Final: hidden\r\n\r\n";
const LARGE_CHUNK: usize = 64 * 1024;
const SLOW_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn child(mut output: File) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let listener = TcpSocket::new_v4().unwrap();
            listener.set_recv_buffer_size(8 * 1024).unwrap();
            listener.bind((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
            let listener = listener.listen(16).unwrap();
            let data_port = listener.local_addr().unwrap().port().try_into().unwrap();
            let mut input = io::BufReader::new(io::stdin());
            let initialize = read_control_request(&mut input, INITIALIZE);
            let params: Initialize = serde_json::from_value(initialize["params"].clone()).unwrap();
            let authorization: Arc<str> = format!("Bearer {}", params.module_bearer).into();
            let initialized = Initialized {
                manifest: ModuleManifest::from_json(MANIFEST.as_bytes()).unwrap(),
                data_port: Some(data_port),
            };
            write_response(
                &mut output,
                &initialize["id"],
                &serde_json::to_string(&initialized).unwrap(),
            );
            let close_control = Arc::new(Notify::new());
            let mut output = Some(output);
            let mut requests = JoinSet::new();
            loop {
                tokio::select! {
                    _ = close_control.notified(), if output.is_some() => drop(output.take()),
                    client = listener.accept() => {
                        requests.spawn(serve(
                            client.unwrap().0,
                            authorization.clone(),
                            close_control.clone(),
                        ));
                    }
                    Some(result) = requests.join_next() => result.unwrap(),
                }
            }
        });
}

fn fields<'a>(head: &'a [u8], name: &str) -> Vec<&'a [u8]> {
    head.split(|&byte| byte == b'\n')
        .skip(1)
        .filter_map(|line| {
            let colon = line.iter().position(|&byte| byte == b':')?;
            line[..colon]
                .eq_ignore_ascii_case(name.as_bytes())
                .then(|| line[colon + 1..].trim_ascii())
        })
        .collect()
}

async fn serve(socket: TcpStream, authorization: Arc<str>, close_control: Arc<Notify>) {
    let mut socket = BufReader::new(socket);
    let head = read_head(&mut socket).await.unwrap();
    if fields(&head, "authorization") != [authorization.as_bytes()] {
        socket
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        return;
    }
    let target = head.split(|&byte| byte == b' ').nth(1).unwrap();
    let path = target.split(|&byte| byte == b'?').next().unwrap();
    match path {
        b"/probe" => {
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            return;
        }
        b"/drop" => return,
        b"/redirect" => {
            socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://example.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            return;
        }
        b"/framing" => {
            let response: &[u8] = match target {
                b"/framing?te-cl" => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 2\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
                b"/framing?te-chain" => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
                b"/framing?conflicting-cl" => b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 5\r\n\r\nhello",
                _ => panic!("unexpected framing fixture"),
            };
            socket.write_all(response).await.unwrap();
            return;
        }
        _ => {}
    }

    let mut side = TcpStream::connect(std::env::var(SIDEBAND).unwrap())
        .await
        .unwrap();
    side.write_all(&head).await.unwrap();
    match path {
        b"/items/%61" => {
            let mut bytes = [0; 6];
            socket.read_exact(&mut bytes[..2]).await.unwrap();
            side.write_all(&bytes[..2]).await.unwrap();
            socket.read_exact(&mut bytes[2..]).await.unwrap();
            socket.write_all(
                b"HTTP/1.1 201 Created\r\nContent-Length: 6\r\nContent-Type: application/octet-stream\r\n\
                  Content-Encoding: fixture-encoding\r\n\
                  X-Reply: first\r\nX-Reply: \x80\r\nConnection: close, X-Internal\r\nConnection: X-Another\r\n\
                  X-Internal: hidden\r\nX-Another: hidden\r\nProxy-Authenticate: hidden\r\n\
                  Set-Cookie: module=1\r\nAccess-Control-Allow-Origin: *\r\n\
                  Content-Security-Policy: frame-ancestors *\r\nX-Frame-Options: ALLOWALL\r\nX-Content-Type-Options: off\r\n\r\n",
            ).await.unwrap();
            socket.write_all(&bytes).await.unwrap();
            return;
        }
        b"/sse" => {
            socket.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nTrailer: X-Final\r\n\r\n").await.unwrap();
            socket.write_all(FIRST_EVENT).await.unwrap();
        }
        b"/truncated" => {
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nshort",
                )
                .await
                .unwrap();
        }
        b"/upload" => {
            let mut first = [0];
            socket.read_exact(&mut first).await.unwrap();
            side.write_all(&first).await.unwrap();
            if target == b"/upload?blocked" {
                pending::<()>().await;
            }
        }
        b"/wait" => {}
        _ => panic!("undeclared private request"),
    }

    tokio::select! {
        eof = socket.read_u8() => {
            assert_eq!(eof.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
            side.write_all(b"x").await.unwrap();
        }
        gate = side.read_u8(), if matches!(path, b"/sse" | b"/truncated") => {
            let gate = gate.unwrap();
            if target == b"/sse?old" {
                assert_eq!(gate, b'c');
                close_control.notify_one();
                pending::<()>().await;
            }
            assert_eq!(gate, b'r');
            if path == b"/sse" {
                if target == b"/sse?slow" {
                    let mut chunk = format!("{LARGE_CHUNK:x}\r\n").into_bytes();
                    chunk.resize(chunk.len() + LARGE_CHUNK, b'x');
                    chunk.extend_from_slice(b"\r\n");
                    for index in 0..SLOW_RESPONSE_BYTES / LARGE_CHUNK {
                        socket.write_all(&chunk).await.unwrap();
                        if index == 0 {
                            side.write_all(b"w").await.unwrap();
                        }
                    }
                    side.write_all(b"d").await.unwrap();
                }
                socket.write_all(LAST_EVENT).await.unwrap();
            }
        }
    }
}

async fn start(
    server: &ServerHandle,
    root: &Path,
    generation: u64,
) -> (ModuleProcess, TcpListener) {
    let (command, side) = fixture_with_sideband("data").await;
    let (process, manifest) = ModuleProcess::start(command, root, Handlers::default(), budget())
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
            &process,
            server.local_addr().port(),
            server.state.backend.gateway_key.len(),
        )
        .unwrap();
    (process, side)
}

fn public_request(
    server: &ServerHandle,
    method: &str,
    target: &str,
    headers: &[&str],
    body: &[u8],
) -> Vec<u8> {
    let session = format!("X-EMP-Session: {}", server.session_token());
    let mut headers = headers.to_vec();
    headers.push(&session);
    request(
        server.local_addr().port(),
        method,
        &format!("{PREFIX}{target}"),
        &headers,
        body,
    )
}

async fn send(server: &ServerHandle, wire: &[u8]) -> BufReader<TcpStream> {
    let mut socket = TcpStream::connect(server.local_addr()).await.unwrap();
    socket.write_all(wire).await.unwrap();
    BufReader::new(socket)
}

#[test]
fn data_channel_preserves_body_and_header_bytes_across_the_trust_boundary() {
    with_registry_server(async |server, root| {
        let (process, side) = start(server, root, 1).await;
        timeout(budget(), async {
            let target = "/items/%61?q=%2B+&q=%26";
            for (headers, status) in [
                (vec!["Transfer-Encoding: chunked"], 400),
                (vec!["Content-Length: 2", "Content-Length: 2"], 400),
                (vec!["Content-Length: 5242881"], 413),
            ] {
                let wire = public_request(server, "PUT", target, &headers, b"");
                assert_wire(server.local_addr(), &wire, false, status).await;
            }
            let origin = format!("Origin: http://localhost:{}", server.local_addr().port());
            let wire = public_request(
                server,
                "PUT",
                target,
                &[
                    "Content-Length: 6",
                    "X-Duplicate: one",
                    "X-Duplicate: café",
                    "Authorization: Bearer public",
                    "Cookie: client=1",
                    &origin,
                    "Connection: X-Upload-Hop",
                    "Connection: close, X-Extra-Hop",
                    "X-Upload-Hop: hidden",
                    "X-Extra-Hop: hidden",
                    "Proxy-Authorization: hidden",
                    "TE: trailers",
                    "Trailer: X-Client-Trailer",
                    "X-EMP-Route-ID: forged",
                    "X-EMP-Peer-IP: 192.0.2.1",
                    "X-EMP-Caller-ID: forged",
                    "X-EMP-Request-ID: forged",
                    "X-Forwarded-For: 192.0.2.2",
                    "Forwarded: for=192.0.2.3",
                    "X-Aero-Caller-Key: forged",
                    "X-Aero-Caller-Id: forged",
                ],
                b"\0\xff",
            );
            let mut client = send(server, &wire).await;
            let mut side = BufReader::new(accept_sideband(&side).await);
            let head = read_head(&mut side).await.unwrap();
            assert!(head.starts_with(b"PUT /items/%61?q=%2B+&q=%26 HTTP/1.1\r\n"));
            let endpoint = process.data_endpoint.as_ref().unwrap();
            let private_host = format!("127.0.0.1:{}", endpoint.port);
            assert_eq!(fields(&head, "host"), [private_host.as_bytes()]);
            assert!(fields(&head, "authorization") == [endpoint.authorization.as_bytes()]);
            assert_eq!(fields(&head, ROUTE_ID_HEADER), [b"echo".as_slice()]);
            assert_eq!(
                fields(&head, "x-duplicate"),
                [b"one".as_slice(), "café".as_bytes()]
            );
            for name in [
                "x-emp-session",
                "cookie",
                "origin",
                "x-upload-hop",
                "x-extra-hop",
                "te",
                "trailer",
                "proxy-authorization",
                "x-emp-peer-ip",
                "x-emp-caller-id",
                "x-forwarded-for",
                "forwarded",
                "x-aero-caller-key",
                "x-aero-caller-id",
            ] {
                assert!(fields(&head, name).is_empty(), "{name}");
            }
            let request_ids = fields(&head, "x-emp-request-id");
            assert_eq!(request_ids.len(), 1);
            assert_eq!(request_ids[0].len(), 16);
            assert!(request_ids[0].iter().all(u8::is_ascii_hexdigit));
            assert_eq!(fields(&head, "connection"), [b"close".as_slice()]);
            let mut received = [0; 2];
            side.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"\0\xff");
            client.write_all(b"\r\n\x80\x01").await.unwrap();
            let head = read_head(&mut client).await.unwrap();
            assert_status(&head, 201);
            assert_eq!(
                fields(&head, "content-encoding"),
                [b"fixture-encoding".as_slice()]
            );
            assert_eq!(
                fields(&head, "x-reply"),
                [b"first".as_slice(), b"\x80".as_slice()]
            );
            assert_eq!(
                fields(&head, "content-security-policy"),
                [b"frame-ancestors 'none'".as_slice()]
            );
            assert_eq!(fields(&head, "x-frame-options"), [b"DENY".as_slice()]);
            assert_eq!(
                fields(&head, "x-content-type-options"),
                [b"nosniff".as_slice()]
            );
            for name in [
                "x-internal",
                "x-another",
                "proxy-authenticate",
                "set-cookie",
                "access-control-allow-origin",
            ] {
                assert!(fields(&head, name).is_empty(), "{name}");
            }
            let mut reply = Vec::new();
            client.read_to_end(&mut reply).await.unwrap();
            assert_eq!(reply, b"\0\xff\r\n\x80\x01");
        })
        .await
        .unwrap();
        process.stop().await.unwrap();
    });
}

async fn start_sse(
    server: &ServerHandle,
    side: &TcpListener,
    target: &str,
) -> (BufReader<TcpStream>, BufReader<TcpStream>) {
    let mut client = send(server, &public_request(server, "GET", target, &[], b"")).await;
    let mut side = BufReader::new(accept_sideband(side).await);
    let request = read_head(&mut side).await.unwrap();
    assert!(request.starts_with(format!("GET {target} HTTP/1.1\r\n").as_bytes()));
    let reply = read_head(&mut client).await.unwrap();
    assert_status(&reply, 200);
    assert_eq!(
        fields(&reply, "content-type"),
        [b"text/event-stream".as_slice()]
    );
    assert!(fields(&reply, "trailer").is_empty());
    assert_stream_bytes(&mut client, b"data:a\n\n").await;
    (client, side)
}

async fn assert_stream_bytes(client: &mut BufReader<TcpStream>, expected: &[u8]) {
    let mut decoder = ChunkDecoder::default();
    let mut received = Vec::new();
    while received.len() < expected.len() {
        while !decoder.at_chunk_ending() {
            read_chunk_data(client, &mut decoder, &mut received)
                .await
                .unwrap();
            assert!(!decoder.is_finished(), "expected a non-empty HTTP chunk");
        }
        let mut ending = [0; 2];
        client.read_exact(&mut ending).await.unwrap();
        decoder.decode(&mut ending.as_slice()).unwrap();
    }
    assert_eq!(received, expected);
}

async fn finish_sse(mut client: BufReader<TcpStream>, mut side: BufReader<TcpStream>) {
    side.write_all(b"r").await.unwrap();
    assert_stream_bytes(&mut client, b"data:z\n\n").await;
    let mut tail = Vec::new();
    client.read_to_end(&mut tail).await.unwrap();
    assert_eq!(tail, b"0\r\n\r\n");
}

#[test]
fn a_backpressured_response_does_not_block_another_stream() {
    with_registry_server(async |server, root| {
        let (process, side) = start(server, root, 1).await;
        timeout(budget(), async {
            let (mut slow, mut hold) = start_sse(server, &side, "/sse?slow").await;
            hold.write_all(b"r").await.unwrap();
            assert_eq!(hold.read_u8().await.unwrap(), b'w');
            assert!(
                timeout(
                    test_timeout(Duration::from_secs(1)).duration(),
                    hold.read_u8()
                )
                .await
                .is_err(),
                "a paused public reader must backpressure the module producer"
            );
            let (fast, release) = start_sse(server, &side, "/sse?fast").await;
            finish_sse(fast, release).await;
            let bytes = tokio::io::copy(&mut slow, &mut tokio::io::sink())
                .await
                .unwrap();
            assert!(bytes >= SLOW_RESPONSE_BYTES as u64);
            assert_eq!(hold.read_u8().await.unwrap(), b'd');
        })
        .await
        .unwrap();
        process.stop().await.unwrap();
    });
}

#[test]
fn client_disconnect_closes_the_private_upload_and_both_response_phases() {
    with_registry_server(async |server, root| {
        let (process, side) = start(server, root, 1).await;
        timeout(budget(), async {
            for (method, target, headers, body) in [
                ("GET", "/wait", [].as_slice(), b"".as_slice()),
                (
                    "POST",
                    "/upload",
                    ["Content-Length: 2"].as_slice(),
                    b"u".as_slice(),
                ),
            ] {
                let client = send(
                    server,
                    &public_request(server, method, target, headers, body),
                )
                .await;
                let mut event = BufReader::new(accept_sideband(&side).await);
                read_head(&mut event).await.unwrap();
                if !body.is_empty() {
                    assert_eq!(event.read_u8().await.unwrap(), b'u');
                }
                drop(client);
                assert_eq!(event.read_u8().await.unwrap(), b'x');
            }
            let (client, mut event) = start_sse(server, &side, "/sse?disconnect").await;
            drop(client);
            assert_eq!(event.read_u8().await.unwrap(), b'x');
        })
        .await
        .unwrap();
        process.stop().await.unwrap();
    });
}

#[test]
fn losing_control_closes_the_old_listener_and_stream_before_replacement() {
    with_registry_server(async |server, root| {
        let (old, old_side) = start(server, root, 1).await;
        let new = timeout(budget(), async {
            let (mut old_response, mut old_event) = start_sse(server, &old_side, "/sse?old").await;
            let old_endpoint = old.data_endpoint.as_ref().unwrap();
            let old_port = old_endpoint.port.get();
            let old_address = (Ipv4Addr::LOCALHOST, old_port).into();
            let old_key = old_endpoint.authorization.to_str().unwrap();
            let bearer = format!("Authorization: {old_key}");
            let probe = request(old_port, "GET", "/probe", &[&bearer], b"");
            assert_wire(old_address, &probe, false, 200).await;
            old_event.write_all(b"c").await.unwrap();
            let mut tail = Vec::new();
            old_response.read_to_end(&mut tail).await.unwrap();
            assert!(tail.is_empty());
            let mut replay_reply = Vec::new();
            let replay_result = async {
                let mut socket = TcpStream::connect(old_address).await?;
                socket.write_all(&probe).await?;
                socket.read_to_end(&mut replay_reply).await
            }
            .await;
            if let Err(error) = replay_result {
                assert!(
                    matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::BrokenPipe
                    ),
                    "{error}"
                );
            }
            assert!(
                replay_reply.is_empty() || replay_reply.starts_with(b"HTTP/1.1 401 "),
                "the old listener accepted the revoked bearer: {replay_reply:?}"
            );
            assert_eq!(old_event.read(&mut [0]).await.unwrap(), 0);
            let wire = public_request(server, "GET", "/probe", &[], b"");
            assert_wire(server.local_addr(), &wire, false, 503).await;

            let (new, new_side) = start(server, root, 2).await;
            let (new_response, new_event) = start_sse(server, &new_side, "/sse?new").await;
            let endpoint = new.data_endpoint.as_ref().unwrap();
            let address = (Ipv4Addr::LOCALHOST, endpoint.port.get()).into();
            let replay = request(endpoint.port.get(), "GET", "/probe", &[&bearer], b"");
            assert_wire(address, &replay, false, 401).await;
            finish_sse(new_response, new_event).await;
            assert_wire(server.local_addr(), &wire, false, 200).await;
            new
        })
        .await
        .unwrap();
        old.stop().await.unwrap();
        new.stop().await.unwrap();
        let wire = public_request(server, "GET", "/probe", &[], b"");
        assert_wire(server.local_addr(), &wire, false, 503).await;
    });
}

#[test]
fn a_disconnected_backpressured_upload_releases_its_admission_slot() {
    with_registry_server(async |server, root| {
        let (process, side) = start(server, root, 1).await;
        let length = 5 * 1024 * 1024;
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_send_buffer_size(16 * 1024).unwrap();
        let mut client = socket.connect(server.local_addr()).await.unwrap();
        client
            .write_all(&public_request(
                server,
                "POST",
                "/upload?blocked",
                &[&format!("Content-Length: {length}")],
                b"u",
            ))
            .await
            .unwrap();
        let mut module = BufReader::new(accept_sideband(&side).await);
        timeout(budget(), async {
            read_head(&mut module).await.unwrap();
            assert_eq!(module.read_u8().await.unwrap(), b'u');
        })
        .await
        .unwrap();

        let data = vec![b'u'; length - 2];
        let mut sent = 0;
        assert!(
            timeout(test_timeout(Duration::from_secs(1)).duration(), async {
                while sent < data.len() {
                    sent += client.write(&data[sent..]).await.unwrap();
                }
            })
            .await
            .is_err(),
            "the upload must reach backpressure while the module pauses"
        );
        assert!(sent > 0);
        let admission = &server.state.lifecycle.admission;
        assert_eq!(admission.active_requests(), 1);
        client.set_zero_linger().unwrap();
        drop(client);
        assert!(
            admission.quiesce(0, budget()).is_some(),
            "the disconnected upload still holds its admission slot"
        );
        process.stop().await.unwrap();
    });
}

#[test]
fn invalid_private_response_framing_is_rejected_before_public_success() {
    with_registry_server(async |server, root| {
        let (process, _side) = start(server, root, 1).await;
        for query in ["te-cl", "te-chain", "conflicting-cl"] {
            let wire = public_request(server, "GET", &format!("/framing?{query}"), &[], b"");
            let reply = assert_wire(server.local_addr(), &wire, false, 502).await;
            assert_eq!(
                json_body(&reply)["error"]["message"],
                "module data request failed"
            );
        }
        process.stop().await.unwrap();
    });
}

#[test]
fn data_channel_maps_failures_only_before_the_public_head() {
    with_registry_server(async |server, root| {
        let (process, side) = start(server, root, 1).await;
        timeout(budget(), async {
            for path in ["/drop", "/redirect"] {
                let wire = public_request(server, "GET", path, &[], b"");
                assert_wire(server.local_addr(), &wire, false, 502).await;
            }
            let wire = public_request(server, "GET", "/truncated", &[], b"");
            let mut client = send(server, &wire).await;
            let mut side = BufReader::new(accept_sideband(&side).await);
            read_head(&mut side).await.unwrap();
            let head = read_head(&mut client).await.unwrap();
            assert_status(&head, 200);
            assert_eq!(fields(&head, "content-length"), [b"10".as_slice()]);
            let mut body = [0; 5];
            client.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"short");
            side.write_all(b"r").await.unwrap();
            assert_eq!(client.read(&mut [0]).await.unwrap(), 0);
        })
        .await
        .unwrap();
        process.stop().await.unwrap();
    });
}
