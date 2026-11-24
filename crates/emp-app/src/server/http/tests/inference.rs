use super::*;
use crate::{
    server::raw_tcp::{RawClient, wire_timeout},
    tests::{OneShotUpstream, live_journal, wait_for},
};
use emp_test_support::{Deadline, test_timeout};
use std::{collections::BTreeMap, io::Read, sync::mpsc};

const RESPONSE: &[u8] = br#"{ "id":"resp_fixture","object":"response","status":"completed","model":"upstream","output":[], "z":1.2300e+2, "a":null }"#;
const COMPACT: &[u8] =
    br#"{ "type":"compaction", "encrypted_content":"opaque", "z":1.2300e+2, "a":null }"#;
pub(super) const RESULT: &[u8] = br#"{ "data":[{"opaque":true}], "z":1.2300e+2, "a":null }"#;
pub(super) const STREAM_REQUEST: &[u8] =
    br#"{"model":"demo/model","stream":true,"input":"fixture"}"#;
const COMPLETE_REQUEST: &[u8] = br#"{"model":"demo/model","stream":false,"input":"fixture"}"#;
pub(super) const FIRST: &[u8] = b"data: {\"id\":\"chat_fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}\n\n";
pub(super) const LAST: &[u8] = b"data: {\"id\":\"chat_fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\ndata: [DONE]\n\n";
const CHAT_COMPLETE: &[u8] = br#"{"id":"chat_fixture","model":"upstream-model","choices":[{"index":0,"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#;

#[test]
fn running_aero_callbacks_leave_head_capacity_for_public_and_callback_requests() {
    use crate::services::modules::aero::CallLease;
    use tokio::io::{AsyncBufReadExt, BufReader};

    {
        let upstream = TcpListener::bind(LOOPBACK).unwrap();
        upstream.set_nonblocking(true).unwrap();
        let base = format!("http://{}/v1", upstream.local_addr().unwrap());
        fixture(&base).run_wire(async move |fixture, address| {
            let upstream = tokio::net::TcpListener::from_std(upstream).unwrap();
            let sender = tokio::spawn(async move {
                let mut held = Vec::new();
                for _ in 0..65 {
                    let (socket, _) = upstream.accept().await.unwrap();
                    let mut socket = BufReader::new(socket);
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':')
                            && name.eq_ignore_ascii_case("Content-Length")
                        {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    assert!(length > 0);
                    socket.read_exact(&mut vec![0; length]).await.unwrap();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        FIRST.len() + LAST.len()
                    );
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(FIRST).await.unwrap();
                    held.push(socket);
                }
                held
            });
            let state = &fixture.server.state;
            let parent = state.lifecycle.admission.acquire_request().unwrap();
            let delegation = state.backend.aero_callers.delegate(
                Caller { id: "local".into(), name: String::new() },
                None,
                Some(CallLease {
                    connection: parent.slot().clone(),
                    work: Arc::new(state.lifecycle.updates().enter().unwrap()),
                }),
            ).unwrap();
            let port = address.port();
            let auth = format!("Authorization: Bearer {}", delegation.key);
            let wire = json_request(port, "/v1/responses", &[&auth], STREAM_REQUEST);
            let mut callbacks = Vec::new();
            for _ in 0..64 {
                let stream = RawClient::request(address, &wire).await;
                assert!(stream.head.starts_with("HTTP/1.1 200"), "{}", stream.head);
                callbacks.push(stream);
            }
            for path in ["/healthz", "/"] {
                let reply = exchange_wire(address, &request(port, "GET", path, &[], b""), false).await;
                assert!(reply.starts_with(b"HTTP/1.1 200 "),
                    "{path} blocked by 64 active callbacks");
            }
            let state_for_wait = state.clone();
            tokio::task::spawn_blocking(move || {
                wait_for(&Deadline::after(wire_timeout()), || state_for_wait.lifecycle.admission.active_requests() == 1);
            }).await.unwrap();
            let admission = &state.lifecycle.admission;
            let public_slots = std::iter::from_fn(|| admission.acquire_request()).collect::<Vec<_>>();
            assert!(!public_slots.is_empty());
            assert!(admission.acquire_request().is_none());
            let stream = RawClient::request(address, &wire).await;
            assert!(stream.head.starts_with("HTTP/1.1 200"), "{}", stream.head);
            assert!(admission.acquire_request().is_none());
            drop(public_slots);
            callbacks.push(stream);
            for (mut socket, mut callback) in sender.await.unwrap().into_iter().zip(callbacks) {
                socket.write_all(LAST).await.unwrap();
                assert!(String::from_utf8_lossy(&callback.rest(&Deadline::after(wire_timeout())).await).contains("response.completed"));
            }
        });
    }
}

#[cfg(unix)]
#[test]
fn aero_callback_in_public_intake_inherits_parent_leases_during_update_drain() {
    use crate::services::modules::aero::CallLease;

    let (release, released) = mpsc::channel();
    let upstream = OneShotUpstream::serve(1, move |socket| {
        upstream_reply(
            socket,
            200,
            "text/event-stream",
            FIRST.len() + LAST.len(),
            "",
        );
        socket.write_all(FIRST).unwrap();
        released.recv_timeout(wire_timeout()).unwrap();
        socket.write_all(LAST).unwrap();
    });
    fixture(&upstream.base_url()).run_wire(async |fixture, address| {
            let state = &fixture.server.state;
            let admission = &state.lifecycle.admission;
            let updates = state.lifecycle.updates();
            let callers = &state.backend.aero_callers;
            let parent_permit = admission.acquire_request().unwrap();
            let parent = callers
                .delegate(
                    Caller {
                        id: "local".into(),
                        name: String::new(),
                    },
                    None,
                    Some(CallLease {
                        connection: parent_permit.slot().clone(),
                        work: Arc::new(updates.enter().unwrap()),
                    }),
                )
                .unwrap();
            let header_slots = (0..64)
                .map(|_| callers.acquire_header().unwrap())
                .collect::<Vec<_>>();
            assert!(callers.acquire_header().is_none());
            let state_for_drain = state.clone();
            let draining = tokio::task::spawn_blocking(move || {
                state_for_drain
                    .lifecycle
                    .updates()
                    .drain_for_test(test_timeout(Duration::from_secs(15)))
            });
            wait_for(&Deadline::after(wire_timeout()), || updates.enter().is_none());

            let authorization = format!("Authorization: Bearer {}", parent.key);
            let wire = json_request(
                fixture.port(),
                "/v1/responses",
                &[&authorization],
                STREAM_REQUEST,
            );
            let mut io = TcpStream::connect(address).await.unwrap();
            let state_for_intake = state.clone();
            tokio::task::spawn_blocking(move || {
                wait_for(&Deadline::after(wire_timeout()), || {
                    state_for_intake.lifecycle.admission.active_requests() == 2
                });
            })
            .await
            .unwrap();
            io.write_all(&wire).await.unwrap();
            let head = timeout(wire_timeout(), crate::server::raw_tcp::head(&mut io))
                .await
                .unwrap()
                .unwrap();
            let mut callback = RawClient {
                io,
                head: String::from_utf8(head).unwrap(),
            };
            assert!(
                callback.head.starts_with("HTTP/1.1 200 "),
                "callback during drain: {}",
                callback.head
            );
            assert_eq!(
                admission.active_requests(),
                1,
                "callback kept the temporary public slot"
            );
            assert_eq!(
                updates.active_for_test(),
                1,
                "callback used a separate work permit"
            );
            drop((parent, parent_permit));
            assert_eq!(
                admission.active_requests(),
                1,
                "callback lost the parent's connection permit"
            );
            assert_eq!(
                updates.active_for_test(),
                1,
                "callback lost the parent's work permit"
            );
            assert!(
                !draining.is_finished(),
                "drain finished while the callback was open"
            );
            release.send(()).unwrap();
            assert!(
                {
                    let mut received = Vec::new();
                    let outcome = timeout(wire_timeout(), callback.io.read_to_end(&mut received)).await;
                    assert!(
                        matches!(outcome, Ok(Ok(_))),
                        "Aero callback drain: waiting for downstream EOF after releasing the upstream, received {} bytes: {outcome:?}",
                        received.len()
                    );
                    String::from_utf8_lossy(&received).contains("response.completed")
                },
                "Aero callback drain: no response.completed before downstream EOF"
            );
            assert!(draining.await.unwrap());
            drop(header_slots);
        });
}

pub(super) fn fixture(base: &str) -> Fixture {
    Fixture::with_directory(fixture_directory(base))
}

pub(super) fn fixture_directory(base: &str) -> tempfile::TempDir {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path();
    let config = root.join("config.json");
    let catalog = crate::services::configuration::fixture_native_catalog_path(&config);
    std::fs::create_dir_all(catalog.parent().unwrap()).unwrap();
    std::fs::write(catalog, br#"{"models":[]}"#).unwrap();
    std::fs::write(
        &config,
        json!({
            "codex_base_url":base, "subscription_search":{"enabled":true},
            "providers":[
                {"id":"native","base_url":base,"protocol":"responses","auth_mode":"forward"},
                {"id":"demo","base_url":base,"protocol":"chat_completions","auth_mode":"api_key",
                    "api_key":"fixture-provider-secret","decision_api":{"protocol":"openai","model":"upstream-model"}}
            ],
            "models":[
                {"id":"demo/model","provider":"demo","upstream_id":"upstream-model","enabled":true},
                {"id":"native/alias","provider":"native","upstream_id":"upstream","enabled":true}
            ],
            "decision_provider":"demo"
        })
        .to_string(),
    )
    .unwrap();
    directory
}

#[derive(Debug, PartialEq, Eq)]
struct ResponseHead {
    status: String,
    headers: BTreeMap<String, Vec<String>>,
}

impl ResponseHead {
    fn parse(head: &str) -> Self {
        let mut lines = head.lines();
        let status = lines.next().unwrap().to_owned();
        let mut headers = BTreeMap::<_, Vec<_>>::new();
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':').unwrap();
            headers
                .entry(name.to_ascii_lowercase())
                .or_default()
                .push(value.trim().to_owned());
        }
        Self { status, headers }
    }

    fn assert_security(&self) {
        assert_eq!(self.headers["x-frame-options"], ["DENY"]);
        assert_eq!(
            self.headers["content-security-policy"],
            ["frame-ancestors 'none'"]
        );
        assert_eq!(self.headers["x-content-type-options"], ["nosniff"]);
        assert_eq!(self.headers["connection"], ["close"]);
    }
}

fn split_response(wire: &[u8]) -> (ResponseHead, &[u8]) {
    let split = wire
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap();
    let head = ResponseHead::parse(std::str::from_utf8(&wire[..split]).unwrap());
    (head, &wire[split + 4..])
}

async fn assert_buffered(address: SocketAddr, wire: &[u8], status: u16) -> Vec<u8> {
    let reply = assert_wire(address, wire, false, status).await;
    let (head, body) = split_response(&reply);
    head.assert_security();
    assert_eq!(head.headers["content-length"], [body.len().to_string()]);
    reply
}

pub(super) fn upstream_reply(
    socket: &mut std::net::TcpStream,
    status: u16,
    content_type: &str,
    length: usize,
    extra: &str,
) {
    let reason = crate::http::response::status_text(status);
    write!(socket, "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\n{extra}Connection: close\r\n\r\n").unwrap();
}

fn stalled_buffered_inference(quit: bool) {
    const BODY_BYTES: usize = 12 * 1024 * 1024;
    let body = json!({
        "id":"resp_fixture", "object":"response", "status":"completed", "model":"upstream",
        "output":[], "fixture_payload":"x".repeat(BODY_BYTES),
    })
    .to_string();
    let upstream = OneShotUpstream::serve(1, move |stream| {
        upstream_reply(stream, 200, "application/json", body.len(), "");
        stream.write_all(body.as_bytes()).unwrap();
    });
    let fixture = fixture(&upstream.base_url());
    let clock = crate::server::tests::TestDrainClock::install(&fixture.server.state.lifecycle);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let slow = runtime.block_on(async {
        let address = fixture.server.local_addr();
        let session = fixture.session();
        let wire = json_request(
            address.port(),
            "/v1/responses",
            &[&session, "Authorization: Bearer caller"],
            br#"{"model":"native/alias","stream":false,"input":"fixture"}"#,
        );
        let slow = RawClient::request(address, &wire).await;
        let head = ResponseHead::parse(&slow.head);
        assert_eq!(head.status, "HTTP/1.1 200 OK");
        assert!(head.headers["content-length"][0].parse::<usize>().unwrap() >= BODY_BYTES);
        if quit {
            let reply = timeout(crate::server::REQUEST_DRAIN_TIMEOUT, async {
                let mut socket = TcpStream::connect(address).await.unwrap();
                socket
                    .write_all(&json_request(
                        address.port(),
                        "/api/quit",
                        &[&session],
                        b"{}",
                    ))
                    .await
                    .unwrap();
                let mut reply = Vec::new();
                socket.read_to_end(&mut reply).await.unwrap();
                reply
            })
            .await
            .expect("quit returned a response");
            assert_status(&reply, 200);
            assert_eq!(json_body(&reply), json!({"status":"stopping"}));
            let signal = fixture.server.state.lifecycle.signal();
            assert_eq!(
                timeout(crate::server::raw_tcp::wire_timeout(), signal.wait())
                    .await
                    .unwrap(),
                crate::server::ShutdownReason::ResponseWritten,
            );
            assert!(fixture.server.state.lifecycle.admission.active_requests() > 0);
        }
        slow
    });
    let result = shutdown_with_stalled_response(fixture.server, slow, &clock);
    result.expect("shutdown completed successfully");
    let id = &upstream.observed().1["x-emp-request-id"];
    let journal = live_journal(fixture.directory.path());
    for (event, status, delivery) in [
        ("http_request_completed", "status", "result"),
        ("request_finished", "response_status", "delivery"),
    ] {
        let records: Vec<_> = journal
            .iter()
            .filter(|entry| entry["event"] == event && entry["fields"]["request_id"] == id.as_str())
            .map(|entry| (&entry["fields"][status], &entry["fields"][delivery]))
            .collect();
        assert_eq!(records, [(&json!(200), &json!("write_failed"))], "{event}");
    }
}

#[test]
fn quit_completes_with_a_stalled_buffered_inference_response() {
    stalled_buffered_inference(true);
}

#[test]
fn shutdown_records_write_failure_for_a_stalled_buffered_inference_response() {
    stalled_buffered_inference(false);
}

#[test]
fn inference_buffered_routes_preserve_wire_bytes_and_headers() {
    let native_request = br#"{"model":"native/alias","input":"fixture","stream":false}"#;
    let image_request = br#"{"model":"gpt-image-2","prompt":"fixture"}"#;
    let decision_request = br#"{"model":"upstream-model","input":"fixture"}"#;
    for (path, upstream_path, request_body, raw, extra) in [
        (
            "/v1/responses",
            "/v1/responses",
            native_request.as_slice(),
            RESPONSE,
            "OpenAI-Model: upstream\r\nX-Models-Etag: stale\r\n",
        ),
        (
            "/v1/responses/compact",
            "/v1/responses/compact",
            native_request,
            COMPACT,
            "OpenAI-Model: upstream\r\nX-Models-Etag: stale\r\n",
        ),
        (
            "/v1/decisions",
            "/v1/decisions",
            decision_request,
            RESULT,
            "Retry-After: 7\r\n",
        ),
        (
            "/v1/systemone",
            "/v1/decisions",
            decision_request,
            RESULT,
            "Retry-After: 7\r\n",
        ),
        (
            "/v1/images/generations",
            "/v1/images/generations",
            image_request,
            RESULT,
            "X-Codex-Imagegen-Request-Id: image-fixture\r\n",
        ),
        (
            "/v1/images/edits",
            "/v1/images/edits",
            image_request,
            RESULT,
            "X-Codex-Imagegen-Request-Id: image-fixture\r\n",
        ),
        (
            "/v1/alpha/search",
            "/v1/alpha/search",
            br#"{"query":"fixture"}"#,
            RESULT,
            "",
        ),
    ] {
        {
            let upstream = OneShotUpstream::serve(1, move |socket| {
                upstream_reply(socket, 200, "application/json", raw.len(), extra);
                socket.write_all(raw).unwrap();
            });
            fixture(&upstream.base_url()).run_wire(async |fixture, address| {
                let session = fixture.session();
                let mut headers = vec![session.as_str(), "Authorization: Bearer fixture-caller"];
                if path.contains("/images/") {
                    headers.push("X-EMP-Provider: demo");
                }
                let wire = json_request(address.port(), path, &headers, request_body);
                let reply = assert_buffered(address, &wire, 200).await;
                let (head, body) = split_response(&reply);
                assert_eq!(head.headers["content-type"], ["application/json"]);
                assert_eq!(head.headers["cache-control"], ["no-store"]);
                assert_eq!(body, raw);
                if path.starts_with("/v1/responses") {
                    assert_eq!(head.headers["openai-model"], ["native/alias"]);
                    assert_ne!(
                        head.headers.get("x-models-etag"),
                        Some(&vec!["stale".to_owned()])
                    );
                }
                if path.contains("/images/") {
                    assert_eq!(
                        head.headers["x-codex-imagegen-request-id"],
                        ["image-fixture"]
                    );
                }
                if matches!(path, "/v1/decisions" | "/v1/systemone") {
                    assert_eq!(head.headers["retry-after"], ["7"]);
                }
            });
            assert_eq!(upstream.observed().0, upstream_path);
        }
    }
}

#[test]
fn inference_gate_and_body_errors_match_before_upstream_dispatch() {
    Fixture::new().run_wire(async |fixture, address| {
        for path in [
            "/v1/responses",
            "/v1/responses/compact",
            "/v1/decisions",
            "/v1/systemone",
            "/v1/images/generations",
            "/v1/images/edits",
            "/v1/alpha/search",
        ] {
            let auth = fixture.session();
            let unauthenticated =
                request(fixture.port(), "POST", path, &["Content-Length: 100"], b"");
            assert_buffered(address, &unauthenticated, 401).await;
            let invalid = json_request(fixture.port(), path, &[&auth], b"{invalid");
            assert_buffered(address, &invalid, 400).await;
        }
        let too_large = format!(
            "Content-Length: {}",
            emp_transport::MAX_EXPANDED_REQUEST_BYTES + 1
        );
        let oversized = request(
            fixture.port(),
            "POST",
            "/v1/responses",
            &[
                &fixture.session(),
                "Content-Type: application/json",
                &too_large,
            ],
            b"",
        );
        let reply = assert_buffered(address, &oversized, 413).await;
        assert_eq!(json_body(&reply)["error"]["code"], "request_too_large");

        let multipart = request(
            fixture.port(),
            "POST",
            "/v1/images/edits",
            &[
                &fixture.session(),
                "Content-Type: multipart/form-data; boundary=fixture",
                "Content-Length: 100",
            ],
            b"",
        );
        let reply = assert_buffered(address, &multipart, 400).await;
        assert_eq!(
            json_body(&reply)["error"]["message"],
            "Content-Type must be application/json"
        );

        let duplicate = request(
            fixture.port(),
            "POST",
            "/v1/decisions",
            &[
                &fixture.session(),
                "X-EMP-Provider: demo",
                "X-EMP-Provider: duplicate",
                "Content-Length: 100",
            ],
            b"",
        );
        let reply = assert_buffered(address, &duplicate, 400).await;
        assert_eq!(
            json_body(&reply)["error"]["message"],
            "X-EMP-Provider must name one configured API service"
        );

        let state = fixture.server.state.clone();
        tokio::task::spawn_blocking(move || {
            wait_for(&Deadline::after(wire_timeout()), || {
                state.lifecycle.updates().active_for_test() == 0
            });
            assert!(state.lifecycle.updates().drain_for_test(Duration::ZERO));
        })
        .await
        .unwrap();
        let reply = assert_buffered(
            address,
            &request(
                fixture.port(),
                "POST",
                "/v1/responses",
                &["Content-Length: 100"],
                b"",
            ),
            503,
        )
        .await;
        assert_eq!(json_body(&reply)["error"]["code"], "updating");
    });
}

#[test]
fn inference_projects_upstream_4xx_5xx_and_connection_failure() {
    let raw = br#"{ "error":{"message":"fixture"}, "z":2.00 }"#;
    for status in [400, 502] {
        {
            let upstream = OneShotUpstream::start_repeated_wire(
                status,
                "application/json",
                Some(7),
                1,
                vec![raw.to_vec()],
            );
            fixture(&upstream.base_url()).run_wire(async |fixture, address| {
                let wire = json_request(
                    address.port(),
                    "/v1/decisions",
                    &[&fixture.session()],
                    br#"{"model":"upstream-model","input":"fixture"}"#,
                );
                let reply = assert_buffered(address, &wire, status).await;
                assert_eq!(split_response(&reply).1, raw);
            });
            assert_eq!(upstream.observed().0, "/v1/decisions");
        }
    }
    let listener = TcpListener::bind(LOOPBACK).unwrap();
    let unavailable = listener.local_addr().unwrap();
    drop(listener);
    fixture(&format!("http://{unavailable}/v1")).run_wire(async |fixture, address| {
        let wire = json_request(
            fixture.port(),
            "/v1/decisions",
            &[&fixture.session()],
            br#"{"model":"upstream-model","input":"fixture"}"#,
        );
        let reply = assert_buffered(address, &wire, 503).await;
        assert_eq!(json_body(&reply)["error"]["code"], "network_error");
    });
}

pub(super) struct SseClient {
    io: tokio::io::BufReader<TcpStream>,
    pending: Vec<u8>,
}

impl SseClient {
    pub(super) fn from_raw(raw: RawClient) -> Self {
        let RawClient { io, head } = raw;
        let mut head = ResponseHead::parse(&head);
        let chunked = head.headers.remove("transfer-encoding");
        assert_eq!(chunked, Some(vec!["chunked".to_owned()]));
        assert_eq!(head.status, "HTTP/1.1 200 OK");
        assert_eq!(head.headers["content-type"], ["text/event-stream"]);
        assert_eq!(head.headers["cache-control"], ["no-cache"]);
        assert!(!head.headers.contains_key("content-length"));
        head.assert_security();
        Self {
            io: tokio::io::BufReader::new(io),
            pending: Vec::new(),
        }
    }

    pub(super) async fn next(&mut self) -> Option<Value> {
        use tokio::io::AsyncBufReadExt as _;
        loop {
            let bytes = loop {
                if let Some(frame_end) =
                    self.pending.iter().enumerate().find_map(|(index, byte)| {
                        if *byte != b'\n' {
                            return None;
                        }
                        match self.pending[index + 1..] {
                            [b'\n', ..] => Some(index + 2),
                            [b'\r', b'\n', ..] => Some(index + 3),
                            _ => None,
                        }
                    })
                {
                    break self.pending.drain(..frame_end).collect::<Vec<_>>();
                }
                {
                    let mut line = String::new();
                    assert_ne!(
                        self.io.read_line(&mut line).await.unwrap(),
                        0,
                        "missing terminal HTTP chunk"
                    );
                    let len =
                        usize::from_str_radix(line.strip_suffix("\r\n").unwrap(), 16).unwrap();
                    let start = self.pending.len();
                    self.pending.resize(start + len, 0);
                    self.io
                        .read_exact(&mut self.pending[start..])
                        .await
                        .unwrap();
                    assert_eq!(
                        self.io.read_u16().await.unwrap(),
                        u16::from_be_bytes(*b"\r\n")
                    );
                    if len == 0 {
                        assert!(self.pending.is_empty(), "truncated SSE event");
                        return None;
                    }
                }
            };
            let text = std::str::from_utf8(&bytes).unwrap();
            let Some(event_type) = text.lines().find_map(|line| line.strip_prefix("event: "))
            else {
                assert!(text.starts_with(':'));
                continue;
            };
            let payload = sse::data(text).expect("SSE event data");
            let event: Value = serde_json::from_str(&payload).unwrap();
            assert_eq!(event["type"], event_type);
            return Some(event);
        }
    }

    async fn first_output(&mut self, deadline: &Deadline) -> Vec<Value> {
        let events = timeout(deadline.remaining(), async {
            let mut events = Vec::new();
            loop {
                let event = self.next().await.expect("first visible SSE event");
                let first = event["type"] == "response.output_text.delta";
                if first {
                    assert_eq!(event["delta"], "answer");
                }
                events.push(event);
                if first {
                    return events;
                }
            }
        })
        .await
        .expect("first SSE output arrived while the upstream tail was gated");
        deadline.accept(events, "first visible SSE output").unwrap()
    }
}

fn assert_upstream_cancelled(socket: &std::net::TcpStream, deadline: &Deadline) {
    let mut byte = [0];
    match deadline.io(socket).read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        outcome => panic!("upstream connection did not close: {outcome:?}"),
    }
    deadline.accept((), "upstream cancellation").unwrap();
}

async fn assert_settled(
    fixture: &Fixture,
    id: String,
    terminal_written: bool,
    token_reports: u64,
) -> Value {
    let root = fixture.directory.path().to_owned();
    let state = fixture.server.state.clone();
    let receipt = tokio::task::spawn_blocking(move || {
        let _drained = state
            .lifecycle
            .admission
            .quiesce(0, wire_timeout())
            .expect("inference connection settled");
        let records = live_journal(&root);
        let mut finished = records
            .iter()
            .filter(|row| row["event"] == "request_finished" && row["fields"]["request_id"] == id);
        let receipt = &finished
            .next()
            .unwrap_or_else(|| panic!("missing request_finished for {id} in {records:?}"))
            ["fields"];
        assert!(finished.next().is_none(), "duplicate request_finished for {id}");
        assert_eq!(receipt["terminal_written"], terminal_written, "{receipt}");
        receipt.clone()
    })
    .await
    .unwrap();

    let now = system_now();
    let ledger = &fixture.server.state.backend.usage.ledger;
    let usage = ledger.query(0.0, now, "external", now, None).unwrap();
    let totals = &usage["totals"];
    assert_eq!(totals["requests"], 1, "{totals}");
    assert_eq!(totals["reported_requests"], token_reports, "{totals}");
    assert_eq!(totals["input_tokens"], 3 * token_reports, "{totals}");
    assert_eq!(totals["output_tokens"], 2 * token_reports, "{totals}");
    receipt
}

#[test]
fn inference_sse_first_output_precedes_upstream_release_and_settles_once() {
    {
        let upstream = OneShotUpstream::serve_observed(move |socket, probe| {
            upstream_reply(
                socket,
                200,
                "text/event-stream",
                FIRST.len() + LAST.len(),
                "",
            );
            socket.write_all(FIRST).unwrap();
            probe.observe(|deadline| deadline.io(socket).write_all(LAST).unwrap());
        });
        fixture(&upstream.worker().base_url()).run_wire(async move |fixture, address| {
            let wire = json_request(
                fixture.port(),
                "/v1/responses",
                &[&fixture.session()],
                STREAM_REQUEST,
            );
            let deadline = Deadline::after(wire_timeout());
            let mut downstream =
                SseClient::from_raw(RawClient::request_before(address, &wire, &deadline).await);
            let mut events = downstream.first_output(&deadline).await;
            assert!(
                !fixture
                    .server
                    .state
                    .lifecycle
                    .updates()
                    .drain_for_test(Duration::ZERO),
                "SSE released the update permit before its body ended"
            );
            let deadline = Deadline::after(wire_timeout());
            upstream
                .begin(deadline, "release upstream SSE tail")
                .wait()
                .unwrap();
            timeout(deadline.remaining(), async {
                while let Some(event) = downstream.next().await {
                    events.push(event);
                }
            })
            .await
            .unwrap();
            deadline
                .accept((), "complete downstream SSE response")
                .unwrap();
            assert_eq!(events.last().unwrap()["type"], "response.completed");
            assert_eq!(events.last().unwrap()["response"]["status"], "completed");
            assert_eq!(
                events.last().unwrap()["response"]["usage"]["total_tokens"],
                5
            );
            let sequence = events
                .iter()
                .map(|event| event["type"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                sequence,
                [
                    "response.created",
                    "response.output_item.added",
                    "response.content_part.added",
                    "response.output_text.delta",
                    "response.output_text.done",
                    "response.content_part.done",
                    "response.output_item.done",
                    "response.completed",
                ],
            );
            assert_eq!(events[4]["text"], "answer");
            assert_eq!(events[5]["part"]["text"], "answer");
            assert_eq!(events[6]["item"]["content"][0]["text"], "answer");
            let id = upstream.worker().observed().1["x-emp-request-id"].clone();
            let receipt = assert_settled(fixture, id, true, 1).await;
            assert_eq!(receipt["delivery"], "terminal_written", "{receipt}");
            assert!(
                receipt["writes_completed"]
                    .as_u64()
                    .is_some_and(|count| count > 1),
                "{receipt}"
            );
            assert_eq!(
                receipt.get("response_status"),
                Some(&Value::Null),
                "{receipt}"
            );
            assert_eq!(receipt["downstream_terminal"], "completed", "{receipt}");
            assert_eq!(receipt["last_phase"], "execute_and_relay", "{receipt}");
        });
    }
}

#[test]
fn inference_stream_disconnect_closes_upstream_and_settles_once() {
    {
        let upstream = OneShotUpstream::serve_observed(move |socket, probe| {
            upstream_reply(
                socket,
                200,
                "text/event-stream",
                FIRST.len() + LAST.len(),
                "",
            );
            socket.write_all(FIRST).unwrap();
            probe.observe(|deadline| assert_upstream_cancelled(socket, deadline));
        });
        let receipt =
            fixture(&upstream.worker().base_url()).run_wire(async move |fixture, address| {
                let wire = json_request(
                    address.port(),
                    "/v1/responses",
                    &[&fixture.session()],
                    STREAM_REQUEST,
                );
                let deadline = Deadline::after(wire_timeout());
                let mut downstream =
                    SseClient::from_raw(RawClient::request_before(address, &wire, &deadline).await);
                downstream.first_output(&deadline).await;
                downstream.io.get_ref().set_zero_linger().unwrap();
                let deadline = Deadline::after(wire_timeout());
                let closure = upstream.begin(deadline, "upstream was closed");
                drop(downstream);
                closure.wait().unwrap();
                let id = upstream.worker().observed().1["x-emp-request-id"].clone();
                assert_settled(fixture, id, false, 0).await
            });
        assert_eq!(
            receipt.get("delivery"),
            Some(&json!("partial")),
            "{receipt}"
        );
        assert!(
            receipt
                .get("writes_completed")
                .and_then(Value::as_u64)
                .is_some_and(|count| count > 1),
            "{receipt}"
        );
        assert_eq!(
            receipt.get("response_status"),
            Some(&Value::Null),
            "{receipt}"
        );
        assert_eq!(
            receipt.get("downstream_terminal"),
            Some(&json!("unknown")),
            "{receipt}"
        );
        assert_eq!(
            receipt.get("last_phase"),
            Some(&json!("execute_and_relay")),
            "{receipt}"
        );
    }
}

#[test]
fn inference_buffered_disconnect_preserves_cancel_and_finish_policies() {
    for (path, cancel) in [("/v1/responses/compact", true), ("/v1/responses", false)] {
        {
            let upstream = OneShotUpstream::serve_observed(move |socket, probe| {
                if !probe.observe(|_| ()) {
                    return;
                }
                probe.observe(|deadline| {
                    if cancel {
                        assert_upstream_cancelled(socket, deadline);
                    } else {
                        upstream_reply(socket, 200, "application/json", CHAT_COMPLETE.len(), "");
                        deadline.io(socket).write_all(CHAT_COMPLETE).unwrap();
                    }
                });
            });
            let receipt =
                fixture(&upstream.worker().base_url()).run_wire(async move |fixture, address| {
                    let wire = json_request(
                        address.port(),
                        path,
                        &[&fixture.session()],
                        COMPLETE_REQUEST,
                    );
                    let deadline = Deadline::after(wire_timeout());
                    let dispatch = upstream.begin(deadline, "EMP dispatched upstream");
                    let mut downstream = timeout(deadline.remaining(), TcpStream::connect(address))
                        .await
                        .unwrap()
                        .unwrap();
                    timeout(deadline.remaining(), downstream.write_all(&wire))
                        .await
                        .unwrap()
                        .unwrap();
                    dispatch.wait().unwrap();
                    downstream.set_zero_linger().unwrap();
                    let deadline = Deadline::after(wire_timeout());
                    let closure = upstream.begin(deadline, "upstream finished");
                    drop(downstream);
                    closure.wait().unwrap();
                    let id = upstream.worker().observed().1["x-emp-request-id"].clone();
                    assert_settled(fixture, id, false, u64::from(!cancel)).await
                });
            assert_eq!(
                receipt.get("delivery"),
                Some(&json!(if cancel {
                    "not_observed"
                } else {
                    "write_failed"
                })),
                "{path}: {receipt}"
            );
            assert_eq!(
                receipt.get("writes_completed"),
                Some(&json!(0)),
                "{path}: {receipt}"
            );
            assert_eq!(
                receipt.get("response_status"),
                Some(&if cancel { Value::Null } else { json!(200) }),
                "{path}: {receipt}"
            );
            assert_eq!(
                receipt.get("downstream_terminal"),
                Some(&json!("unknown")),
                "{path}: {receipt}"
            );
            assert_eq!(
                receipt.get("last_phase"),
                Some(&json!("execute_and_relay")),
                "{path}: {receipt}"
            );
        }
    }
}
