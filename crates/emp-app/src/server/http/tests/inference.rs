use super::*;
use crate::{
    server::raw_tcp::{DEADLINE, RawClient},
    tests::{OneShotUpstream, live_journal, wait_for},
};
use std::{collections::BTreeMap, io::Read, sync::mpsc};
use tokio::sync::oneshot;

const RESPONSE: &[u8] = br#"{ "id":"resp_fixture","object":"response","status":"completed","model":"upstream","output":[], "z":1.2300e+2, "a":null }"#;
const COMPACT: &[u8] =
    br#"{ "type":"compaction", "encrypted_content":"opaque", "z":1.2300e+2, "a":null }"#;
const RESULT: &[u8] = br#"{ "data":[{"opaque":true}], "z":1.2300e+2, "a":null }"#;
const STREAM_REQUEST: &[u8] = br#"{"model":"demo/model","stream":true,"input":"fixture"}"#;
const COMPLETE_REQUEST: &[u8] = br#"{"model":"demo/model","stream":false,"input":"fixture"}"#;
const FIRST: &[u8] = b"data: {\"id\":\"chat_fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}\n\n";
const LAST: &[u8] = b"data: {\"id\":\"chat_fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\ndata: [DONE]\n\n";
const CHAT_COMPLETE: &[u8] = br#"{"id":"chat_fixture","model":"upstream-model","choices":[{"index":0,"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#;

#[test]
fn running_aero_callbacks_leave_head_capacity_for_public_and_callback_requests() {
    use crate::services::modules::aero::CallLease;
    use tokio::io::{AsyncBufReadExt, BufReader};

    for use_candidate in [false, true] {
        let upstream = TcpListener::bind(LOOPBACK).unwrap();
        upstream.set_nonblocking(true).unwrap();
        let base = format!("http://{}/v1", upstream.local_addr().unwrap());
        fixture(&base).run_candidate(async move |fixture, candidate| {
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
            let state = &fixture.legacy.state;
            let delegation = state.backend.aero_callers.delegate(
                Caller { id: "local".into(), name: String::new() },
                None,
                Some(CallLease {
                    connection: Arc::new(state.lifecycle.admission.acquire_request().unwrap()),
                    work: Arc::new(state.lifecycle.updates().enter().unwrap()),
                }),
            ).unwrap();
            let address = if use_candidate { candidate } else { fixture.legacy.local_addr() };
            let port = fixture.port();
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
                    "{path} blocked by 64 active callbacks (candidate={use_candidate})");
            }
            let state_for_wait = state.clone();
            tokio::task::spawn_blocking(move || {
                wait_for(DEADLINE, || state_for_wait.lifecycle.admission.active_requests() == 1);
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
                assert!(String::from_utf8_lossy(&callback.rest().await).contains("response.completed"));
            }
        });
    }
}

#[cfg(unix)]
#[test]
fn aero_callback_in_public_intake_inherits_parent_leases_during_update_drain() {
    use crate::services::modules::aero::CallLease;

    let (release, released) = mpsc::channel();
    let upstream = OneShotUpstream::serve(2, move |socket| {
        upstream_reply(
            socket,
            200,
            "text/event-stream",
            FIRST.len() + LAST.len(),
            "",
        );
        socket.write_all(FIRST).unwrap();
        released.recv_timeout(DEADLINE).unwrap();
        socket.write_all(LAST).unwrap();
    });
    let mut heads = Vec::new();
    for use_candidate in [false, true] {
        fixture(&upstream.base_url()).run_candidate(async |fixture, candidate| {
            let state = &fixture.legacy.state;
            let admission = &state.lifecycle.admission;
            let updates = state.lifecycle.updates();
            let callers = &state.backend.aero_callers;
            let parent = callers
                .delegate(
                    Caller {
                        id: "local".into(),
                        name: String::new(),
                    },
                    None,
                    Some(CallLease {
                        connection: Arc::new(admission.acquire_request().unwrap()),
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
                    .drain_for_test(Duration::from_secs(15))
            });
            wait_for(DEADLINE, || updates.enter().is_none());

            let address = if use_candidate {
                candidate
            } else {
                fixture.legacy.local_addr()
            };
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
                wait_for(DEADLINE, || {
                    state_for_intake.lifecycle.admission.active_requests() == 2
                });
            })
            .await
            .unwrap();
            io.write_all(&wire).await.unwrap();
            let head = timeout(DEADLINE, crate::server::raw_tcp::head(&mut io))
                .await
                .unwrap()
                .unwrap();
            let mut callback = RawClient {
                io,
                head: String::from_utf8(head).unwrap(),
            };
            if callback.head.starts_with("HTTP/1.1 200 ") {
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
                drop(parent);
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
                    String::from_utf8_lossy(&callback.rest().await).contains("response.completed")
                );
            } else {
                drop(parent);
            }
            assert!(draining.await.unwrap());
            drop(header_slots);
            heads.push(callback.head);
        });
    }
    assert!(
        heads.iter().all(|head| head.starts_with("HTTP/1.1 200 ")),
        "callback during drain: legacy={:?}, candidate={:?}",
        heads[0],
        heads[1]
    );
}

fn fixture(base: &str) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let config = root.join("config.json");
    let auth = root.join("codex/auth.json");
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

async fn buffered_parity(
    fixture: &Fixture,
    address: SocketAddr,
    wire: &[u8],
    status: u16,
) -> Vec<u8> {
    let legacy = exchange_wire(fixture.legacy.local_addr(), wire, false).await;
    let candidate = exchange_wire(address, wire, false).await;
    assert_status(&legacy, status);
    assert_status(&candidate, status);
    let (old_head, old_body) = split_response(&legacy);
    let (new_head, new_body) = split_response(&candidate);
    assert_eq!(old_head, new_head, "{}", String::from_utf8_lossy(wire));
    assert_eq!(old_body, new_body, "{}", String::from_utf8_lossy(wire));
    new_head.assert_security();
    assert_eq!(
        new_head.headers["content-length"],
        [new_body.len().to_string()]
    );
    candidate
}

fn upstream_reply(
    socket: &mut std::net::TcpStream,
    status: u16,
    content_type: &str,
    length: usize,
    extra: &str,
) {
    let reason = crate::http::response::status_text(status);
    write!(socket, "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\n{extra}Connection: close\r\n\r\n").unwrap();
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
        let upstream = OneShotUpstream::serve(2, move |socket| {
            upstream_reply(socket, 200, "application/json", raw.len(), extra);
            socket.write_all(raw).unwrap();
        });
        fixture(&upstream.base_url()).run_candidate(async |fixture, address| {
            let session = fixture.session();
            let mut headers = vec![session.as_str(), "Authorization: Bearer fixture-caller"];
            if path.contains("/images/") {
                headers.push("X-EMP-Provider: demo");
            }
            let wire = json_request(fixture.port(), path, &headers, request_body);
            let reply = buffered_parity(fixture, address, &wire, 200).await;
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
        for _ in 0..2 {
            assert_eq!(upstream.observed().0, upstream_path);
        }
    }
}

#[test]
fn inference_gate_and_body_errors_match_before_upstream_dispatch() {
    Fixture::new().run_candidate(async |fixture, address| {
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
            buffered_parity(fixture, address, &unauthenticated, 401).await;
            let invalid = json_request(fixture.port(), path, &[&auth], b"{invalid");
            buffered_parity(fixture, address, &invalid, 400).await;
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
        let reply = buffered_parity(fixture, address, &oversized, 413).await;
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
        let reply = buffered_parity(fixture, address, &multipart, 400).await;
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
        let reply = buffered_parity(fixture, address, &duplicate, 400).await;
        assert_eq!(
            json_body(&reply)["error"]["message"],
            "X-EMP-Provider must name one configured API service"
        );

        let state = fixture.legacy.state.clone();
        tokio::task::spawn_blocking(move || {
            wait_for(DEADLINE, || {
                state.lifecycle.updates().active_for_test() == 0
            });
            assert!(state.lifecycle.updates().drain_for_test(Duration::ZERO));
        })
        .await
        .unwrap();
        let reply = buffered_parity(
            fixture,
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
        let upstream = OneShotUpstream::start_repeated_wire(
            status,
            "application/json",
            Some(7),
            2,
            vec![raw.to_vec()],
        );
        fixture(&upstream.base_url()).run_candidate(async |fixture, address| {
            let wire = json_request(
                fixture.port(),
                "/v1/decisions",
                &[&fixture.session()],
                br#"{"model":"upstream-model","input":"fixture"}"#,
            );
            let reply = buffered_parity(fixture, address, &wire, status).await;
            assert_eq!(split_response(&reply).1, raw);
        });
        for _ in 0..2 {
            assert_eq!(upstream.observed().0, "/v1/decisions");
        }
    }
    let listener = TcpListener::bind(LOOPBACK).unwrap();
    let unavailable = listener.local_addr().unwrap();
    drop(listener);
    fixture(&format!("http://{unavailable}/v1")).run_candidate(async |fixture, address| {
        let wire = json_request(
            fixture.port(),
            "/v1/decisions",
            &[&fixture.session()],
            br#"{"model":"upstream-model","input":"fixture"}"#,
        );
        let reply = buffered_parity(fixture, address, &wire, 503).await;
        assert_eq!(json_body(&reply)["error"]["code"], "network_error");
    });
}

struct SseClient {
    head: ResponseHead,
    io: tokio::io::BufReader<TcpStream>,
    chunked: bool,
}

impl SseClient {
    async fn open(address: SocketAddr, wire: &[u8], candidate: bool) -> Self {
        let RawClient { io, head } = RawClient::request(address, wire).await;
        let mut head = ResponseHead::parse(&head);
        let chunked = head.headers.remove("transfer-encoding");
        assert_eq!(chunked, candidate.then(|| vec!["chunked".to_owned()]));
        assert_eq!(head.status, "HTTP/1.1 200 OK");
        assert_eq!(head.headers["content-type"], ["text/event-stream"]);
        assert_eq!(head.headers["cache-control"], ["no-cache"]);
        assert!(!head.headers.contains_key("content-length"));
        head.assert_security();
        Self {
            head,
            io: tokio::io::BufReader::new(io),
            chunked: candidate,
        }
    }

    async fn next(&mut self) -> Option<Value> {
        use tokio::io::AsyncBufReadExt as _;
        loop {
            let mut bytes = Vec::new();
            let mut line = String::new();
            if self.chunked {
                assert_ne!(self.io.read_line(&mut line).await.unwrap(), 0);
                let len = usize::from_str_radix(line.trim_end(), 16).unwrap();
                bytes.resize(len, 0);
                self.io.read_exact(&mut bytes).await.unwrap();
                let ending = self.io.read_u16().await.unwrap();
                assert_eq!(ending, u16::from_be_bytes(*b"\r\n"));
                if len == 0 {
                    return None;
                }
            } else {
                loop {
                    line.clear();
                    if self.io.read_line(&mut line).await.unwrap() == 0 {
                        assert!(bytes.is_empty(), "truncated SSE event");
                        return None;
                    }
                    bytes.extend_from_slice(line.as_bytes());
                    if line == "\n" {
                        break;
                    }
                }
            }
            let text = std::str::from_utf8(&bytes).unwrap();
            let Some(event_type) = text.lines().find_map(|line| line.strip_prefix("event: "))
            else {
                assert!(text.starts_with(':'));
                continue;
            };
            let payload = text
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap();
            let event: Value = serde_json::from_str(payload).unwrap();
            assert_eq!(event["type"], event_type);
            return Some(event);
        }
    }

    async fn first_output(&mut self) -> Vec<Value> {
        timeout(DEADLINE, async {
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
        .expect("first SSE output arrived while the upstream tail was gated")
    }
}

fn assert_upstream_cancelled(socket: &mut std::net::TcpStream) {
    let mut byte = [0];
    match socket.read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        outcome => panic!("upstream connection did not close: {outcome:?}"),
    }
}

async fn assert_settled(
    fixture: &Fixture,
    ids: Vec<String>,
    terminal_written: bool,
    token_reports: u64,
) {
    let root = fixture.directory.path().to_owned();
    tokio::task::spawn_blocking(move || {
        let receipts = || {
            live_journal(&root)
                .into_iter()
                .filter(|row| {
                    row["event"] == "request_finished"
                        && row["fields"]["request_id"]
                            .as_str()
                            .is_some_and(|id| ids.iter().any(|expected| expected == id))
                })
                .collect::<Vec<_>>()
        };
        wait_for(DEADLINE, || receipts().len() >= ids.len());
        let finished = receipts();
        let outcomes = ids
            .iter()
            .map(|id| {
                let entries = finished
                    .iter()
                    .filter(|row| row["fields"]["request_id"] == *id)
                    .collect::<Vec<_>>();
                assert_eq!(entries.len(), 1, "{finished:?}");
                let receipt = &entries[0]["fields"];
                assert_eq!(receipt["terminal_written"], terminal_written, "{entries:?}");
                [
                    receipt["delivery"].clone(),
                    receipt["writes_completed"].clone(),
                    receipt["response_status"].clone(),
                    receipt["downstream_terminal"].clone(),
                    receipt["last_phase"].clone(),
                ]
            })
            .collect::<Vec<_>>();
        assert_eq!(outcomes[0], outcomes[1], "{finished:?}");
    })
    .await
    .unwrap();

    let now = system_now();
    let ledger = &fixture.legacy.state.backend.usage.ledger;
    let usage = ledger.query(0.0, now, "external", now, None).unwrap();
    let totals = &usage["totals"];
    assert_eq!(totals["requests"], 2, "{totals}");
    assert_eq!(totals["reported_requests"], token_reports, "{totals}");
    assert_eq!(totals["input_tokens"], 3 * token_reports, "{totals}");
    assert_eq!(totals["output_tokens"], 2 * token_reports, "{totals}");
}

#[test]
fn inference_sse_first_output_precedes_upstream_release_and_settles_once() {
    let (release, released) = mpsc::channel();
    let upstream = OneShotUpstream::serve(2, move |socket| {
        upstream_reply(
            socket,
            200,
            "text/event-stream",
            FIRST.len() + LAST.len(),
            "",
        );
        socket.write_all(FIRST).unwrap();
        released
            .recv_timeout(DEADLINE)
            .expect("client received visible output before release");
        socket.write_all(LAST).unwrap();
    });
    fixture(&upstream.base_url()).run_candidate(async |fixture, address| {
        let wire = json_request(
            fixture.port(),
            "/v1/responses",
            &[&fixture.session()],
            STREAM_REQUEST,
        );
        let mut results = Vec::new();
        let mut ids = Vec::new();
        for (address, candidate) in [(fixture.legacy.local_addr(), false), (address, true)] {
            let mut downstream = SseClient::open(address, &wire, candidate).await;
            let mut events = downstream.first_output().await;
            assert!(
                !fixture
                    .legacy
                    .state
                    .lifecycle
                    .updates()
                    .drain_for_test(Duration::ZERO),
                "SSE released the update permit before its body ended"
            );
            release.send(()).unwrap();
            timeout(DEADLINE, async {
                while let Some(event) = downstream.next().await {
                    events.push(event);
                }
            })
            .await
            .unwrap();
            assert_eq!(events.last().unwrap()["type"], "response.completed");
            assert_eq!(events.last().unwrap()["response"]["status"], "completed");
            assert_eq!(
                events.last().unwrap()["response"]["usage"]["total_tokens"],
                5
            );
            let sequence = events
                .iter()
                .map(|event| event["type"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            results.push((downstream.head, sequence));
            ids.push(upstream.observed().1["x-emp-request-id"].clone());
        }
        assert_eq!(results[0], results[1]);
        assert_settled(fixture, ids, true, 2).await;
    });
}

#[test]
fn inference_stream_disconnect_closes_upstream_and_settles_once() {
    let (closed, mut closures) = tokio::sync::mpsc::unbounded_channel();
    let upstream = OneShotUpstream::serve(2, move |socket| {
        upstream_reply(
            socket,
            200,
            "text/event-stream",
            FIRST.len() + LAST.len(),
            "",
        );
        socket.write_all(FIRST).unwrap();
        assert_upstream_cancelled(socket);
        closed.send(()).unwrap();
    });
    fixture(&upstream.base_url()).run_candidate(async |fixture, address| {
        let wire = json_request(
            fixture.port(),
            "/v1/responses",
            &[&fixture.session()],
            STREAM_REQUEST,
        );
        let mut ids = Vec::new();
        for (address, candidate) in [(fixture.legacy.local_addr(), false), (address, true)] {
            let mut downstream = SseClient::open(address, &wire, candidate).await;
            downstream.first_output().await;
            downstream.io.get_ref().set_zero_linger().unwrap();
            drop(downstream);
            timeout(DEADLINE, closures.recv())
                .await
                .expect("upstream was closed")
                .unwrap();
            ids.push(upstream.observed().1["x-emp-request-id"].clone());
        }
        assert_settled(fixture, ids, false, 0).await;
    });
}

#[test]
fn inference_buffered_disconnect_preserves_cancel_and_finish_policies() {
    for (path, cancel) in [("/v1/responses/compact", true), ("/v1/responses", false)] {
        let (dispatched, mut dispatches) = tokio::sync::mpsc::unbounded_channel();
        let upstream = OneShotUpstream::serve(2, move |socket| {
            let (release, released) = mpsc::channel();
            let (closed, closure) = oneshot::channel();
            dispatched.send((release, closure)).unwrap();
            if cancel {
                assert_upstream_cancelled(socket);
            } else {
                released
                    .recv_timeout(DEADLINE)
                    .expect("downstream is closed");
                upstream_reply(socket, 200, "application/json", CHAT_COMPLETE.len(), "");
                socket.write_all(CHAT_COMPLETE).unwrap();
            }
            closed.send(()).unwrap();
        });
        fixture(&upstream.base_url()).run_candidate(async |fixture, address| {
            let wire = json_request(
                fixture.port(),
                path,
                &[&fixture.session()],
                COMPLETE_REQUEST,
            );
            let mut ids = Vec::new();
            for address in [fixture.legacy.local_addr(), address] {
                let mut downstream = TcpStream::connect(address).await.unwrap();
                downstream.write_all(&wire).await.unwrap();
                let (release, closure) = timeout(DEADLINE, dispatches.recv())
                    .await
                    .expect("EMP dispatched upstream")
                    .unwrap();
                downstream.set_zero_linger().unwrap();
                drop(downstream);
                if !cancel {
                    release.send(()).unwrap();
                }
                timeout(DEADLINE, closure)
                    .await
                    .expect("upstream finished")
                    .unwrap();
                ids.push(upstream.observed().1["x-emp-request-id"].clone());
            }
            assert_settled(fixture, ids, false, if cancel { 0 } else { 2 }).await;
        });
    }
}
