use super::*;
use crate::{
    error::AppError,
    server::{
        ShutdownReason,
        raw_tcp::{RawClient, wire_timeout},
        test_hooks::{RequestProbe, WriteProbe},
        tests::TestDrainClock,
    },
    tests::live_journal,
};
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Finishes,
    BusyAtDeadline,
    Sending,
    FinishesAfterDeadline,
    TerminalFrameAtDeadline,
    RetriesQuit,
}

use Phase::*;

const ACTIVE: &[Phase] = &[Finishes, BusyAtDeadline, Sending];

struct Probe {
    id: oneshot::Receiver<String>,
    write: Option<(oneshot::Receiver<()>, oneshot::Sender<()>)>,
}

impl Probe {
    fn arm(fixture: &Fixture, path: &'static str, marker: Option<&'static [u8]>) -> Self {
        let (id, received) = oneshot::channel();
        let (write, probe) = marker.map_or((None, None), |marker| {
            let (reached, writing) = oneshot::channel();
            let (resume, release) = oneshot::channel();
            (
                Some((writing, resume)),
                Some(WriteProbe {
                    marker,
                    reached: Some(reached),
                    release,
                }),
            )
        });
        let mut armed = fixture
            .server
            .state
            .lifecycle
            .test_hooks
            .request
            .lock()
            .unwrap();
        assert!(
            armed
                .replace(RequestProbe {
                    path,
                    id,
                    write: probe
                })
                .is_none()
        );
        Self {
            id: received,
            write,
        }
    }
}

struct Work {
    id: String,
    client: TcpStream,
    release: Option<oneshot::Sender<()>>,
    write: Option<(oneshot::Receiver<()>, oneshot::Sender<()>)>,
    status: u16,
    completed: &'static str,
}

impl Work {
    async fn new(
        probe: Probe,
        client: TcpStream,
        release: Option<oneshot::Sender<()>>,
        status: u16,
        completed: &'static str,
    ) -> Self {
        Self {
            id: receive(probe.id, "request ID").await,
            client,
            release,
            write: probe.write,
            status,
            completed,
        }
    }

    fn finish_business(&mut self) {
        if let Some(release) = self.release.take() {
            release.send(()).expect("business work still held");
        }
    }

    async fn writing(&mut self) {
        if let Some((reached, _)) = &mut self.write {
            receive(reached, "paused response write").await;
        }
    }

    async fn read_to_end(&mut self, waiting_for: &str) -> Vec<u8> {
        let deadline = Deadline::after(wire_timeout());
        let mut received = Vec::new();
        let result = timeout(deadline.remaining(), self.client.read_to_end(&mut received)).await;
        assert!(
            matches!(result, Ok(Ok(_))),
            "{waiting_for}: {result:?}; received {:?}",
            String::from_utf8_lossy(&received)
        );
        received
    }

    async fn read_completed(&mut self, case: &str) {
        let received = self.read_to_end(case).await;
        assert!(
            received
                .windows(self.completed.len())
                .any(|part| part == self.completed.as_bytes()),
            "{case}: missing completed response, received {:?}",
            String::from_utf8_lossy(&received),
        );
    }
}

async fn receive<T>(
    receiver: impl Future<Output = Result<T, oneshot::error::RecvError>>,
    event: &str,
) -> T {
    timeout(wire_timeout(), receiver)
        .await
        .unwrap_or_else(|_| panic!("waiting for {event}"))
        .unwrap_or_else(|_| panic!("sender dropped before {event}"))
}

async fn submit(address: SocketAddr, wire: &[u8]) -> TcpStream {
    timeout(wire_timeout(), async {
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(wire).await.unwrap();
        client
    })
    .await
    .expect("request sent")
}

fn run_case(
    fixture: Fixture,
    name: &str,
    phase: Phase,
    via_quit: bool,
    open: impl AsyncFnOnce(&Fixture) -> Work,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(exercise(fixture, name, phase, via_quit, open));
}

async fn exercise(
    fixture: Fixture,
    name: &str,
    phase: Phase,
    via_quit: bool,
    open: impl AsyncFnOnce(&Fixture) -> Work,
) {
    let case = format!("{name} {phase:?} quit={via_quit}");
    let control = fixture.server.state.lifecycle.clone();
    let clock = TestDrainClock::install(&control);
    let address = fixture.server.local_addr();
    let session = fixture.session();
    let terminal = (phase == TerminalFrameAtDeadline).then(|| {
        let (queued, observed) = oneshot::channel();
        let clock = clock.clone();
        *control.test_hooks.output_queued.lock().unwrap() = Some(Box::new(move || {
            clock.expire();
            let _ = queued.send(());
        }));
        observed
    });
    let mut work = open(&fixture).await;
    if phase == Sending {
        work.finish_business();
        work.writing().await;
    }

    let mut quit_requests = Vec::new();
    if via_quit {
        let probe = Probe::arm(&fixture, "/api/quit", None);
        let first_session = session.clone();
        let quit = tokio::spawn(async move { quit_response(address, &first_session).await });
        if phase == Finishes {
            timeout(wire_timeout(), control.admission.wait_for_websocket_drain())
                .await
                .expect("quit closed admission for its precheck");
            work.finish_business();
        }
        let reply = quit.await.expect(&case);
        let quit_id = receive(probe.id, "quit request ID").await;
        if matches!(phase, BusyAtDeadline | RetriesQuit) {
            wire_contract::assert_json_reply(
                &case,
                &reply,
                "409 Error",
                &json!({"error":{"code":"active_conversations","message":"Finish active Codex requests and WebSockets, then retry shutdown","origin":"emp"}}),
            );
            assert!(
                !control.shutdown.load(Ordering::Acquire),
                "{case}: HTTP 409 stopped the listener"
            );
            quit_requests.push((quit_id, 409, "rejected"));
        } else {
            wire_contract::assert_json_reply(
                &case,
                &reply,
                "200 OK",
                &json!({"status":"stopping"}),
            );
            quit_requests.push((quit_id, 200, "responded"));
        }
    }
    if phase == RetriesQuit {
        work.finish_business();
        work.read_completed(&case).await;
        let probe = Probe::arm(&fixture, "/api/quit", None);
        let reply = quit_response(address, &session).await;
        wire_contract::assert_json_reply(
            "quit after Aero callback completed",
            &reply,
            "200 OK",
            &json!({"status":"stopping"}),
        );
        quit_requests.push((
            receive(probe.id, "retried quit request ID").await,
            200,
            "responded",
        ));
    }

    let response_stopped = via_quit && phase != BusyAtDeadline;
    if response_stopped {
        assert_eq!(
            timeout(wire_timeout(), control.signal().wait())
                .await
                .expect(&case),
            ShutdownReason::ResponseWritten,
            "{case}"
        );
    }
    let Fixture {
        directory, server, ..
    } = fixture;
    let shutdown = tokio::task::spawn_blocking(move || server.shutdown());
    let reason = timeout(wire_timeout(), control.signal().wait())
        .await
        .expect(&case);
    assert_eq!(
        reason,
        if response_stopped {
            ShutdownReason::ResponseWritten
        } else {
            ShutdownReason::Requested
        },
        "{case}"
    );

    if !via_quit && matches!(phase, Finishes | TerminalFrameAtDeadline) {
        work.finish_business();
    }
    if let Some(terminal) = terminal {
        receive(terminal, "report terminal frame queued and cutoff fired").await;
    }
    if phase == FinishesAfterDeadline {
        clock.pass_deadline();
        work.finish_business();
        work.writing().await;
    }
    if phase == FinishesAfterDeadline {
        timeout(wire_timeout(), control.admission.wait_for_business_idle())
            .await
            .unwrap_or_else(|_| panic!("{case}: business still counted while write was paused"));
    }
    if phase == BusyAtDeadline {
        work.writing().await;
    }
    match phase {
        Finishes | TerminalFrameAtDeadline | RetriesQuit => {}
        FinishesAfterDeadline => clock.cutoff.notify_one(),
        BusyAtDeadline | Sending => clock.expire(),
    }
    if phase == Finishes {
        work.read_completed(&case).await;
    } else if phase != RetriesQuit {
        work.read_to_end(&case).await;
        if phase == BusyAtDeadline {
            work.finish_business();
        }
    }
    let outcome = timeout(wire_timeout(), shutdown)
        .await
        .unwrap_or_else(|_| panic!("{case}: waiting for server shutdown"))
        .unwrap();
    if matches!(phase, BusyAtDeadline | FinishesAfterDeadline) {
        assert!(
            matches!(outcome, Err(AppError::ServerStopped)),
            "{case}: {outcome:?}"
        );
    } else {
        outcome.unwrap_or_else(|error| panic!("{case}: {error:?}"));
    }

    let journal = live_journal(directory.path());
    let result = if matches!(phase, Finishes | RetriesQuit) || work.status == 101 {
        "responded"
    } else {
        "write_failed"
    };
    assert_settlement(&journal, &case, &work.id, json!(work.status), result);
    for (quit, status, result) in quit_requests {
        assert_settlement(&journal, &case, &quit, json!(status), result);
    }
}

async fn quit_response(address: SocketAddr, session: &str) -> Vec<u8> {
    exchange_before(
        address,
        &json_request(address.port(), "/api/quit", &[session], b"{}"),
        false,
        &Deadline::after(
            crate::services::shutdown::QUIT_PRECHECK_WAIT + crate::server::raw_tcp::wire_timeout(),
        ),
    )
    .await
}

fn assert_settlement(journal: &[Value], case: &str, id: &str, status: Value, result: &str) {
    let records = journal
        .iter()
        .filter(|entry| {
            entry["event"] == "http_request_completed" && entry["fields"]["request_id"] == id
        })
        .map(|entry| (&entry["fields"]["status"], &entry["fields"]["result"]))
        .collect::<Vec<_>>();
    assert_eq!(records, [(&status, &json!(result))], "{case} request {id}");
}

#[test]
fn drain_ordinary_handler() {
    for (phase, via_quit) in ACTIVE
        .iter()
        .flat_map(|&phase| [(phase, false), (phase, true)])
        .chain([(FinishesAfterDeadline, false)])
    {
        run_case(
            Fixture::new(),
            "/healthz",
            phase,
            via_quit,
            async |fixture| {
                let (entered, ready) = oneshot::channel();
                let (release, wait) = oneshot::channel();
                *fixture
                    .server
                    .state
                    .lifecycle
                    .test_hooks
                    .health_handler
                    .lock()
                    .unwrap() = Some((entered, wait));
                let probe = Probe::arm(
                    fixture,
                    "/healthz",
                    matches!(phase, Sending | FinishesAfterDeadline)
                        .then_some(b"HTTP/1.1".as_slice()),
                );
                let client = submit(
                    fixture.server.local_addr(),
                    &request(fixture.port(), "GET", "/healthz", &[], b""),
                )
                .await;
                receive(ready, "health handler entered").await;
                Work::new(probe, client, Some(release), 200, r#"{"status":"ok"}"#).await
            },
        );
    }
}

#[test]
fn drain_websocket_session() {
    for phase in [Finishes, BusyAtDeadline] {
        for via_quit in [false, true] {
            run_case(
                Fixture::new(),
                "WebSocket",
                phase,
                via_quit,
                async |fixture| {
                    let probe = Probe::arm(
                        fixture,
                        "/v1/responses",
                        (phase == BusyAtDeadline)
                            .then_some(b"EMP connection is draining".as_slice()),
                    );
                    let mut client = RawClient::request(
                        fixture.server.local_addr(),
                        &request(
                            fixture.port(),
                            "GET",
                            "/v1/responses",
                            &[
                                &fixture.session(),
                                "Connection: Upgrade",
                                "Upgrade: websocket",
                                "Sec-WebSocket-Version: 13",
                                "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
                            ],
                            b"",
                        ),
                    )
                    .await;
                    assert!(
                        client
                            .head
                            .starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
                        "{}",
                        client.head
                    );
                    timeout(wire_timeout(), async {
                        client
                            .io
                            .write_all(b"\x89\x80\x00\x00\x00\x00")
                            .await
                            .unwrap();
                        assert_eq!(client.io.read_u16().await.unwrap(), 0x8a00);
                    })
                    .await
                    .expect("WebSocket pong after admission");
                    Work::new(probe, client.io, None, 101, "EMP connection is draining").await
                },
            );
        }
    }
}

#[test]
fn drain_management_subscription() {
    for phase in [Finishes, Sending] {
        for via_quit in [false, true] {
            run_case(
                Fixture::new(),
                "accounts events",
                phase,
                via_quit,
                async |fixture| {
                    let path = "/api/accounts/events";
                    let probe = Probe::arm(
                        fixture,
                        path,
                        (phase == Sending).then_some(b"event: quota-updated".as_slice()),
                    );
                    let client = RawClient::request(
                        fixture.server.local_addr(),
                        &request(fixture.port(), "GET", path, &[&fixture.session()], b""),
                    )
                    .await;
                    assert!(
                        client.head.starts_with("HTTP/1.1 200 OK\r\n"),
                        "{}",
                        client.head
                    );
                    Work::new(
                        probe,
                        client.io,
                        None,
                        200,
                        if via_quit {
                            "event: emp-exiting"
                        } else {
                            "0\r\n\r\n"
                        },
                    )
                    .await
                },
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn drain_submitted_search() {
    use super::inference::{RESULT, fixture, upstream_reply};
    use crate::tests::OneShotUpstream;

    for &phase in ACTIVE {
        for via_quit in [false, true] {
            search_case(phase, via_quit);
        }
    }
    search_case(FinishesAfterDeadline, false);

    fn search_case(phase: Phase, via_quit: bool) {
        let (upstream, release) = {
            let (release, wait) = oneshot::channel();
            let mut wait = Some(wait);
            let upstream = OneShotUpstream::serve(1, move |socket| {
                upstream_reply(socket, 200, "application/json", RESULT.len(), "");
                if wait.take().unwrap().blocking_recv().is_ok() {
                    socket.write_all(RESULT).unwrap();
                }
            });
            (upstream, release)
        };
        run_case(
            fixture(&upstream.base_url()),
            "submitted search",
            phase,
            via_quit,
            async |fixture| {
                let path = "/v1/alpha/search";
                let probe = Probe::arm(
                    fixture,
                    path,
                    matches!(phase, Sending | FinishesAfterDeadline)
                        .then_some(b"HTTP/1.1".as_slice()),
                );
                let client = submit(
                    fixture.server.local_addr(),
                    &json_request(
                        fixture.port(),
                        path,
                        &[&fixture.session(), "Authorization: Bearer caller"],
                        br#"{"query":"fixture"}"#,
                    ),
                )
                .await;
                assert_eq!(upstream.observed().0, path);
                Work::new(
                    probe,
                    client,
                    Some(release),
                    200,
                    str::from_utf8(RESULT).unwrap(),
                )
                .await
            },
        );
    }
}

#[cfg(unix)]
#[test]
fn drain_buffered_search_handoff_before_connection_resumes() {
    use super::inference::{RESULT, fixture, upstream_reply};
    use crate::tests::OneShotUpstream;

    let upstream = OneShotUpstream::serve(1, |socket| {
        upstream_reply(socket, 200, "application/json", RESULT.len(), "");
        socket.write_all(RESULT).unwrap();
    });
    drain_buffered_handoff(fixture(&upstream.base_url()), |fixture| {
        json_request(
            fixture.port(),
            "/v1/alpha/search",
            &[&fixture.session(), "Authorization: Bearer caller"],
            br#"{"query":"fixture"}"#,
        )
    });
    assert_eq!(upstream.observed().0, "/v1/alpha/search");
}

#[cfg(unix)]
#[test]
fn drain_search_switching_protocols_without_local_upgrade() {
    use super::inference::{fixture, upstream_reply};
    use crate::tests::OneShotUpstream;

    let upstream = OneShotUpstream::serve(1, |socket| {
        upstream_reply(socket, 101, "application/json", 0, "");
    });
    fixture(&upstream.base_url()).run_wire(async |fixture, address| {
        let reply = exchange_wire(
            address,
            &json_request(
                fixture.port(),
                "/v1/alpha/search",
                &[&fixture.session(), "Authorization: Bearer caller"],
                br#"{"query":"fixture"}"#,
            ),
            false,
        )
        .await;
        assert_status(&reply, 101);
        assert_eq!(upstream.observed().0, "/v1/alpha/search");
    });
}

#[test]
fn drain_buffered_reports_handoff_before_connection_resumes() {
    for path in ["/api/calls?start=1&end=10", "/api/usage"] {
        drain_buffered_handoff(Fixture::new(), |fixture| {
            request(fixture.port(), "GET", path, &[&fixture.session()], b"")
        });
    }
}

#[test]
fn drain_report_early_reply_handoff_before_connection_resumes() {
    drain_buffered_handoff(Fixture::new(), |fixture| {
        request(
            fixture.port(),
            "GET",
            "/api/calls?start=invalid",
            &[&fixture.session(), "Accept: text/event-stream"],
            b"",
        )
    });
}

fn drain_buffered_handoff(fixture: Fixture, wire: impl FnOnce(&Fixture) -> Vec<u8>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let control = fixture.server.state.lifecycle.clone();
        let clock = TestDrainClock::install(&control);
        let (paused, connection_paused) = oneshot::channel();
        let (resume, resumed) = oneshot::channel();
        *control.test_hooks.response_receive.lock().unwrap() = Some((paused, resumed));
        let (queued, response_queued) = oneshot::channel();
        *control.test_hooks.output_queued.lock().unwrap() = Some(Box::new(move || {
            let _ = queued.send(());
        }));

        let mut client = submit(fixture.server.local_addr(), &wire(&fixture)).await;
        receive(
            connection_paused,
            "connection paused before receiving response",
        )
        .await;
        receive(response_queued, "worker queued the buffered response").await;

        let shutdown = tokio::task::spawn_blocking(move || fixture.server.shutdown());
        assert_eq!(
            timeout(wire_timeout(), control.signal().wait())
                .await
                .unwrap(),
            ShutdownReason::Requested,
        );
        clock.expire();
        let mut reply = Vec::new();
        let closed = timeout(wire_timeout(), client.read_to_end(&mut reply)).await;
        let _ = resume.send(());

        assert_eq!(closed.unwrap().unwrap(), 0, "connection closed at cutoff");
        assert!(
            timeout(wire_timeout(), shutdown)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
    });
}

#[cfg(unix)]
#[test]
fn drain_inference_history_failure_at_terminal_write() {
    run_case(
        super::inference::fixture("http://127.0.0.1:1/v1"),
        "inference history failure",
        Sending,
        false,
        async |fixture| {
            let path = "/v1/responses";
            let probe = Probe::arm(fixture, path, Some(b"event: response.failed"));
            let client = RawClient::request(
                fixture.server.local_addr(),
                &json_request(
                    fixture.port(),
                    path,
                    &[&fixture.session()],
                    br#"{"model":"demo/model","stream":true,"input":[{"type":"compaction","encrypted_content":"opaque"}]}"#,
                ),
            )
            .await;
            assert!(client.head.starts_with("HTTP/1.1 200 OK\r\n"));
            Work::new(probe, client.io, None, 200, "response.failed").await
        },
    );
}

#[cfg(unix)]
#[test]
fn drain_inference_stream() {
    for &phase in ACTIVE {
        for via_quit in [false, true] {
            inference_case(phase, via_quit, false);
        }
    }
}

#[cfg(unix)]
#[test]
fn drain_aero_callback() {
    for &phase in ACTIVE {
        for via_quit in [false, true] {
            inference_case(phase, via_quit, true);
        }
    }
    inference_case(RetriesQuit, true, true);
}

#[cfg(unix)]
fn install_aero_fixture(directory: &std::path::Path, endpoint: &str) {
    let path = directory.join("config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["aero_profiles"] = json!([{
        "id":"aero/fixture","name":"fixture","planner":{"model":"demo/model"},"executor":{"model":"demo/model"}
    }]);
    std::fs::write(path, config.to_string()).unwrap();
    let started = json!({"value":{"endpoint":endpoint},"error":null});
    install_worker_fixture(
        directory,
        emp_modules::Module::Aero,
        "{}",
        &format!("while read -r line; do printf '%s\\n' '{started}'; done"),
    );
}

#[cfg(unix)]
fn aero_request(fixture: &Fixture, stream: bool) -> Vec<u8> {
    json_request(
        fixture.port(),
        "/v1/responses",
        &[&fixture.session(), "session_id: fixture"],
        &serde_json::to_vec(&json!({"model":"aero/fixture","stream":stream,"input":"fixture"}))
            .unwrap(),
    )
}

#[cfg(unix)]
#[test]
fn drain_aero_json_at_content_length_before_write_finishes() {
    aero_terminal_case(
        "application/json",
        br#"{"id":"resp_aero_fixture","output":""#.to_vec(),
        b"aero-terminal\"}",
    );
}

#[cfg(unix)]
#[test]
fn drain_aero_oversized_sse_at_event_boundary_before_write_finishes() {
    let mut prefix = b"event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"output_text\":\"".to_vec();
    prefix.extend(std::iter::repeat_n(b'x', 1024 * 1024));
    aero_terminal_case("text/event-stream", prefix, b"aero-terminal\"}}\r\n\r\n");
}

#[cfg(unix)]
fn aero_terminal_case(content_type: &'static str, prefix: Vec<u8>, last: &'static [u8]) {
    use super::inference::{fixture_directory, upstream_reply};
    use crate::tests::OneShotUpstream;

    let (release, wait) = oneshot::channel();
    let mut wait = Some(wait);
    let prefix_len = prefix.len();
    let upstream = OneShotUpstream::serve(1, move |socket| {
        upstream_reply(socket, 200, content_type, prefix_len + last.len(), "");
        socket.write_all(&prefix).unwrap();
        if wait.take().unwrap().blocking_recv().is_ok() {
            socket.write_all(last).unwrap();
        }
    });
    let directory = fixture_directory("http://127.0.0.1:1/v1");
    install_aero_fixture(directory.path(), &upstream.base_url());
    run_case(
        Fixture::with_directory(directory),
        content_type,
        Sending,
        false,
        async |fixture| {
            let address = fixture.server.local_addr();
            let probe = Probe::arm(fixture, "/v1/responses", Some(b"aero-terminal"));
            let mut client = RawClient::request(
                address,
                &aero_request(fixture, content_type == "text/event-stream"),
            )
            .await;
            assert!(client.head.starts_with("HTTP/1.1 200 OK\r\n"));
            assert_eq!(upstream.observed().0, "/v1/responses");
            timeout(wire_timeout(), async {
                let mut prefix = vec![0; prefix_len];
                client.io.read_exact(&mut prefix).await.unwrap();
            })
            .await
            .expect("the prefix reaches the client before the final chunk");
            assert!(fixture.server.state.lifecycle.admission.has_business_work());
            Work::new(probe, client.io, Some(release), 200, "aero-terminal").await
        },
    );
}

#[cfg(unix)]
#[test]
fn drain_aero_revokes_delegation_at_terminal_handoff() {
    use super::inference::{STREAM_REQUEST, fixture_directory, upstream_reply};
    use crate::tests::OneShotUpstream;
    use std::io::ErrorKind;

    let terminal = b"event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
    let aero = OneShotUpstream::serve(1, |socket| {
        upstream_reply(socket, 200, "text/event-stream", terminal.len(), "");
        socket.write_all(terminal).unwrap();
    });
    let inference_calls = Arc::new(AtomicUsize::new(0));
    let upstream = OneShotUpstream::serve(1, {
        let inference_calls = inference_calls.clone();
        move |_| {
            inference_calls.fetch_add(1, Ordering::Relaxed);
        }
    });
    let directory = fixture_directory(&upstream.base_url());
    install_aero_fixture(directory.path(), &aero.base_url());
    Fixture::with_directory(directory).run_wire(async |fixture, address| {
        let control = &fixture.server.state.lifecycle;
        let parent = Probe::arm(fixture, "/v1/responses", Some(b"event: response.completed"));
        let _parent_client = RawClient::request(address, &aero_request(fixture, true)).await;
        let (paused, resume_parent) = parent.write.unwrap();
        receive(paused, "Aero terminal write paused after queueing").await;
        assert!(!control.admission.has_business_work());
        let authorization = format!(
            "Authorization: Bearer {}",
            aero.observed().1["x-aero-caller-key"]
        );
        let callback = json_request(
            fixture.port(),
            "/v1/responses",
            &[&authorization],
            STREAM_REQUEST,
        );
        let reply = exchange_wire(address, &callback, false).await;
        assert_status(&reply, 401);
        assert!(
            json_body(&reply)["error"]["message"]
                .as_str()
                .unwrap()
                .ends_with("proxy caller authentication is required")
        );

        let quit = Probe::arm(fixture, "/api/quit", Some(b"200 OK"));
        let wire = json_request(fixture.port(), "/api/quit", &[&fixture.session()], b"{}");
        let quit_client = submit(address, &wire).await;
        let mut quit = Work::new(quit, quit_client, None, 200, "stopping").await;
        quit.writing().await;
        assert!(!control.admission.has_business_work());

        let mut reply = Vec::new();
        let result = timeout(wire_timeout(), async {
            let mut client = TcpStream::connect(address).await?;
            client.write_all(&callback).await?;
            client.read_to_end(&mut reply).await
        })
        .await
        .expect("callback rejected while quit response is paused");
        assert!(
            matches!(result, Ok(0))
                || result.is_err_and(|error| matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
                ))
        );
        assert!(reply.is_empty());
        assert!(!control.admission.has_business_work());
        assert_eq!(inference_calls.load(Ordering::Relaxed), 0);

        resume_parent.send(()).unwrap();
        quit.write.take().unwrap().1.send(()).unwrap();
        wire_contract::assert_json_reply(
            "quit after Aero handoff",
            &quit.read_to_end("quit response").await,
            "200 OK",
            &json!({"status":"stopping"}),
        );
    });
}

#[cfg(unix)]
fn inference_case(phase: Phase, via_quit: bool, delegated: bool) {
    use super::inference::{FIRST, LAST, STREAM_REQUEST, fixture_directory, upstream_reply};
    use crate::tests::OneShotUpstream;

    let (upstream, release) = {
        let (release, wait) = oneshot::channel();
        let mut wait = Some(wait);
        let upstream = OneShotUpstream::serve(1, move |socket| {
            upstream_reply(
                socket,
                200,
                "text/event-stream",
                FIRST.len() + LAST.len(),
                "",
            );
            socket.write_all(FIRST).unwrap();
            if wait.take().unwrap().blocking_recv().is_ok() {
                let result = socket.write_all(LAST);
                if phase != BusyAtDeadline {
                    result.unwrap();
                }
            }
        });
        (upstream, release)
    };
    let directory = fixture_directory(&upstream.base_url());
    let parent = delegated.then(|| {
        let (disconnect, wait) = oneshot::channel();
        let mut wait = Some(wait);
        let upstream = (
            disconnect,
            OneShotUpstream::serve(1, move |_| {
                let _ = wait.take().unwrap().blocking_recv();
            }),
        );
        install_aero_fixture(directory.path(), &upstream.1.base_url());
        upstream
    });
    let name = if delegated {
        "Aero callback"
    } else {
        "inference stream"
    };
    run_case(
        Fixture::with_directory(directory),
        name,
        phase,
        via_quit,
        async |fixture| {
            let address = fixture.server.local_addr();
            let path = "/v1/responses";
            let session = fixture.session();
            let parent = if let Some(upstream) = parent {
                let wire = aero_request(fixture, false);
                let task = tokio::spawn(async move { exchange_wire(address, &wire, false).await });
                let (upstream, headers) = tokio::task::spawn_blocking(move || {
                    let (_, headers, _) = upstream.1.observed();
                    (upstream, headers)
                })
                .await
                .unwrap();
                Some((
                    upstream,
                    task,
                    format!("Authorization: Bearer {}", headers["x-aero-caller-key"]),
                ))
            } else {
                None
            };
            let probe = Probe::arm(
                fixture,
                path,
                (phase == Sending).then_some(b"event: response.completed".as_slice()),
            );
            let headers = parent.as_ref().map_or_else(
                || vec![session.as_str()],
                |(_, _, token)| vec![token.as_str()],
            );
            let client = RawClient::request(
                address,
                &json_request(fixture.port(), path, &headers, STREAM_REQUEST),
            )
            .await;
            assert!(
                client.head.starts_with("HTTP/1.1 200 OK\r\n"),
                "{}",
                client.head
            );
            assert_eq!(upstream.observed().0, "/v1/chat/completions");
            if let Some((upstream, task, _)) = parent {
                let slots = fixture.server.state.lifecycle.admission.active_requests();
                upstream.0.send(()).unwrap();
                assert_eq!(slots, 1);
                let reply = task.await.unwrap();
                assert_status(&reply, 503);
                assert_eq!(
                    json_body(&reply)["error"]["code"],
                    "aero_runtime_unavailable"
                );
            }
            Work::new(probe, client.io, Some(release), 200, "response.completed").await
        },
    );
}

#[test]
fn drain_calls_report() {
    for &phase in ACTIVE {
        for via_quit in [false, true] {
            report_case("/api/calls", "?start=1&end=10", phase, via_quit, false);
        }
    }
    for via_quit in [false, true] {
        report_case("/api/calls", "?start=1&end=10", Sending, via_quit, true);
    }
    report_case(
        "/api/calls",
        "?start=1&end=10",
        TerminalFrameAtDeadline,
        false,
        false,
    );
}

#[test]
fn drain_usage_report() {
    for &phase in ACTIVE {
        for via_quit in [false, true] {
            report_case(
                "/api/usage",
                "?lifetime=true&owner=fixture",
                phase,
                via_quit,
                false,
            );
        }
    }
    report_case(
        "/api/usage",
        "?lifetime=true&owner=fixture",
        TerminalFrameAtDeadline,
        false,
        false,
    );
}

fn report_case(path: &'static str, query: &str, phase: Phase, via_quit: bool, broken_facts: bool) {
    use super::report_fixture::{report_gate, seed_reports};

    let fixture = Fixture::new();
    seed_reports(&fixture);
    if broken_facts {
        rusqlite::Connection::open(fixture.directory.path().join("state/usage.sqlite3"))
            .unwrap()
            .execute_batch("UPDATE call_records SET facts = 'invalid JSON'")
            .unwrap();
    }
    let marker: &'static [u8] = if broken_facts {
        b"event: error"
    } else {
        b"event: result"
    };
    let name = format!("{path} {}", str::from_utf8(marker).unwrap());
    run_case(fixture, &name, phase, via_quit, async |fixture| {
        let probe = Probe::arm(
            fixture,
            path,
            matches!(phase, Sending | TerminalFrameAtDeadline).then_some(marker),
        );
        let progress = (!broken_facts).then(|| report_gate(fixture));
        let client = RawClient::request(
            fixture.server.local_addr(),
            &request(
                fixture.port(),
                "GET",
                &format!("{path}{query}"),
                &[&fixture.session(), "Accept: text/event-stream"],
                b"",
            ),
        )
        .await;
        assert!(
            client.head.starts_with("HTTP/1.1 200 OK\r\n"),
            "{}",
            client.head
        );
        let release = if let Some((reached, release)) = progress {
            assert!(
                !receive(reached, "report progress frame flushed")
                    .await
                    .load(Ordering::Acquire)
            );
            Some(release)
        } else {
            None
        };
        Work::new(probe, client.io, release, 200, "event: result").await
    });
}
