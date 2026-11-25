use super::*;
use crate::{
    error::AppError,
    server::{
        ShutdownReason,
        raw_tcp::{RawClient, wire_timeout},
        test_hooks::{RequestProbe, WriteProbe, WriteTrigger},
        tests::TestDrainClock,
    },
    tests::live_journal,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Finishes,
    BusyAtDeadline,
    Sending,
    FinishesAfterDeadline,
    HandoffAtDeadline,
    RetriesQuit,
}

use Phase::*;

const ACTIVE: &[Phase] = &[Finishes, BusyAtDeadline, Sending];

type WritePause = (oneshot::Receiver<Option<Vec<u8>>>, oneshot::Sender<()>);

struct Probe {
    id: oneshot::Receiver<String>,
    write: Option<WritePause>,
}

impl Probe {
    fn arm(fixture: &Fixture, path: &'static str, trigger: Option<WriteTrigger>) -> Self {
        let (id, received) = oneshot::channel();
        let (write, probe) = trigger.map_or((None, None), |trigger| {
            let (reached, writing) = oneshot::channel();
            let (resume, release) = oneshot::channel();
            (
                Some((writing, resume)),
                Some(WriteProbe {
                    trigger,
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
    client: BufReader<TcpStream>,
    release: Option<oneshot::Sender<()>>,
    write: Option<WritePause>,
    status: u16,
    completed: &'static str,
    expected_frame: Option<&'static [u8]>,
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
            client: BufReader::new(client),
            release,
            write: probe.write,
            status,
            completed,
            expected_frame: None,
        }
    }

    #[cfg(unix)]
    fn expect_frame(mut self, frame: &'static [u8]) -> Self {
        self.expected_frame = Some(frame);
        self
    }

    fn finish_business(&mut self) {
        if let Some(release) = self.release.take() {
            release.send(()).expect("business work still held");
        }
    }

    async fn writing(&mut self) {
        if let Some((reached, _)) = &mut self.write {
            let frame = receive(reached, "paused response write").await;
            if let Some(expected) = self.expected_frame {
                assert_eq!(
                    frame.as_deref(),
                    Some(expected),
                    "final bytes queued for delivery"
                );
            }
            if let Some(frame) = frame {
                self.assert_queued_event(&frame);
            }
        }
    }

    fn assert_queued_event(&self, frame: &[u8]) {
        if let Some(expected) = self.completed.strip_prefix("event: ") {
            let name = str::from_utf8(frame)
                .ok()
                .and_then(|frame| frame.lines().find_map(|line| line.strip_prefix("event: ")));
            assert_eq!(
                name,
                Some(expected),
                "event in the frame queued for delivery"
            );
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
    timeout(wire_timeout().duration(), receiver)
        .await
        .unwrap_or_else(|_| panic!("waiting for {event}"))
        .unwrap_or_else(|_| panic!("sender dropped before {event}"))
}

async fn submit(address: SocketAddr, wire: &[u8]) -> TcpStream {
    timeout(wire_timeout().duration(), async {
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
    if phase == HandoffAtDeadline {
        let clock = clock.clone();
        *control.test_hooks.on_stream_handoff.lock().unwrap() = Some(Box::new(move || {
            clock.expire();
        }));
    }
    let mut work = open(&fixture).await;
    if phase == Sending {
        work.finish_business();
        work.writing().await;
        timeout(
            wire_timeout().duration(),
            control.admission.wait_for_business_idle(),
        )
        .await
        .unwrap_or_else(|_| panic!("{case}: producer still counted while final write was paused"));
    }

    let mut quit_requests = Vec::new();
    if via_quit {
        let probe = Probe::arm(&fixture, "/api/quit", None);
        let first_session = session.clone();
        let quit = tokio::spawn(async move { quit_response(address, &first_session).await });
        if phase == Finishes {
            timeout(
                wire_timeout().duration(),
                control.admission.wait_for_websocket_drain(),
            )
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
            timeout(wire_timeout().duration(), control.signal().wait())
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
    let reason = timeout(wire_timeout().duration(), control.signal().wait())
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

    if !via_quit && matches!(phase, Finishes | HandoffAtDeadline) {
        work.finish_business();
    }
    if phase == HandoffAtDeadline {
        work.writing().await;
    }
    if phase == FinishesAfterDeadline {
        clock.pass_deadline();
        work.finish_business();
        work.writing().await;
    }
    if phase == FinishesAfterDeadline {
        timeout(
            wire_timeout().duration(),
            control.admission.wait_for_business_idle(),
        )
        .await
        .unwrap_or_else(|_| panic!("{case}: business still counted while write was paused"));
    }
    if phase == BusyAtDeadline {
        work.writing().await;
    }
    match phase {
        Finishes | HandoffAtDeadline | RetriesQuit => {}
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
    let outcome = timeout(wire_timeout().duration(), shutdown)
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
            wire_timeout().with_product_wait(crate::services::shutdown::QUIT_PRECHECK_WAIT),
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
                        .then_some(WriteTrigger::Output),
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
                            .then_some(WriteTrigger::Bytes(b"EMP connection is draining")),
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
                    timeout(wire_timeout().duration(), async {
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
                        (phase == Sending).then_some(WriteTrigger::Bytes(b"event: quota-updated")),
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
                        .then_some(WriteTrigger::Output),
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
            timeout(wire_timeout().duration(), control.signal().wait())
                .await
                .unwrap(),
            ShutdownReason::Requested,
        );
        clock.expire();
        let mut reply = Vec::new();
        let closed = timeout(wire_timeout().duration(), client.read_to_end(&mut reply)).await;
        let _ = resume.send(());

        assert_eq!(closed.unwrap().unwrap(), 0, "connection closed at cutoff");
        assert!(
            timeout(wire_timeout().duration(), shutdown)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
    });
}

#[cfg(unix)]
#[test]
fn drain_inference_history_failure_before_last_flush() {
    run_case(
        super::inference::fixture("http://127.0.0.1:1/v1"),
        "inference history failure",
        Sending,
        false,
        async |fixture| {
            let path = "/v1/responses";
            let probe = Probe::arm(
                fixture,
                path,
                Some(WriteTrigger::Frame(Box::new(|frame| {
                    frame.starts_with(b"event: response.failed\n")
                }))),
            );
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
            Work::new(probe, client.io, None, 200, "event: response.failed").await
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
fn drain_native_eof_before_last_flush() {
    use super::inference::{fixture, upstream_reply};
    use crate::tests::OneShotUpstream;

    let frame = b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n";
    let upstream = OneShotUpstream::serve(1, move |socket| {
        upstream_reply(socket, 200, "text/event-stream", frame.len(), "");
        socket.write_all(frame).unwrap();
    });
    run_case(
        fixture(&upstream.base_url()),
        "Native EOF",
        Sending,
        false,
        async |fixture| {
            let path = "/v1/responses";
            let probe = Probe::arm(
                fixture,
                path,
                Some(WriteTrigger::Frame(Box::new(|frame| {
                    frame.starts_with(b"event: response.failed\n")
                }))),
            );
            let client = RawClient::request(
                fixture.server.local_addr(),
                &json_request(
                    fixture.port(),
                    path,
                    &[&fixture.session(), "Authorization: Bearer caller"],
                    br#"{"model":"native/alias","stream":true,"input":"fixture"}"#,
                ),
            )
            .await;
            assert!(client.head.starts_with("HTTP/1.1 200 OK\r\n"));
            Work::new(probe, client.io, None, 200, "event: response.failed").await
        },
    );
}

#[cfg(unix)]
#[test]
fn drain_aero_json_eof_before_last_flush() {
    aero_last_flush(
        "application/json",
        br#"{"id":"resp_aero_fixture","output":[]"#,
        b"}",
    );
}

#[cfg(unix)]
#[test]
fn drain_aero_sse_eof_before_last_flush() {
    aero_last_flush(
        "text/event-stream",
        b"event: response.completed\ndata: {\"type\":\"response.completed\"}\n",
        b"\n",
    );
}

async fn assert_stream_prefix(client: &mut BufReader<TcpStream>, prefix: &[u8]) {
    timeout(wire_timeout().duration(), async {
        let mut received = Vec::new();
        while received.len() < prefix.len() {
            let mut length = String::new();
            assert_ne!(client.read_line(&mut length).await.unwrap(), 0);
            let length = usize::from_str_radix(length.trim_end(), 16).unwrap();
            assert!(length > 0);
            let start = received.len();
            received.resize(start + length, 0);
            client.read_exact(&mut received[start..]).await.unwrap();
            assert_eq!(
                client.read_u16().await.unwrap(),
                u16::from_be_bytes(*b"\r\n")
            );
        }
        assert_eq!(received, prefix);
    })
    .await
    .expect("upstream bytes reach the downstream client");
}

#[cfg(unix)]
async fn assert_stream_end(client: &mut BufReader<TcpStream>) {
    let mut rest = Vec::new();
    timeout(wire_timeout().duration(), client.read_to_end(&mut rest))
        .await
        .expect("HTTP stream ends")
        .unwrap();
    assert_eq!(rest, b"0\r\n\r\n");
}

#[cfg(unix)]
fn aero_last_flush(content_type: &'static str, prefix: &'static [u8], last: &'static [u8]) {
    use super::inference::fixture_directory;
    use crate::tests::OneShotUpstream;

    let (release, wait) = oneshot::channel();
    let mut wait = Some(wait);
    let upstream = OneShotUpstream::serve(1, move |socket| {
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        socket.write_all(prefix).unwrap();
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
            let prefix_read = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let probe = Probe::arm(
                fixture,
                "/v1/responses",
                Some(WriteTrigger::Frame(Box::new({
                    let prefix_read = prefix_read.clone();
                    move |frame| prefix_read.load(Ordering::Acquire) && frame == last
                }))),
            );
            let client = RawClient::request(
                fixture.server.local_addr(),
                &aero_request(fixture, content_type == "text/event-stream"),
            )
            .await;
            assert!(client.head.starts_with("HTTP/1.1 200 OK\r\n"));
            assert_eq!(upstream.observed().0, "/v1/responses");
            let mut work = Work::new(probe, client.io, Some(release), 200, "").await;
            assert_stream_prefix(&mut work.client, prefix).await;
            prefix_read.store(true, Ordering::Release);
            assert!(fixture.server.state.lifecycle.admission.has_business_work());
            work.expect_frame(last)
        },
    );
}

#[cfg(unix)]
#[test]
fn drain_aero_sse_terminal_without_eof_at_deadline_stops_server() {
    use super::inference::fixture_directory;
    use crate::tests::OneShotUpstream;

    let terminal = b"event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
    let (close, keep_open) = oneshot::channel::<()>();
    let mut keep_open = Some(keep_open);
    let upstream = OneShotUpstream::serve(1, move |socket| {
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        socket.write_all(terminal).unwrap();
        let _ = keep_open.take().unwrap().blocking_recv();
    });
    let directory = fixture_directory("http://127.0.0.1:1/v1");
    install_aero_fixture(directory.path(), &upstream.base_url());
    run_case(
        Fixture::with_directory(directory),
        "Aero SSE terminal without EOF",
        BusyAtDeadline,
        false,
        async |fixture| {
            let probe = Probe::arm(fixture, "/v1/responses", None);
            let client =
                RawClient::request(fixture.server.local_addr(), &aero_request(fixture, true)).await;
            let mut work = Work::new(probe, client.io, Some(close), 200, "").await;
            assert_stream_prefix(&mut work.client, terminal).await;
            assert!(fixture.server.state.lifecycle.admission.has_business_work());
            work
        },
    );
}

#[cfg(unix)]
#[test]
fn drain_native_and_aero_preserve_sse_bytes_and_json_diagnostics() {
    use super::inference::{fixture_directory, upstream_reply};
    use crate::tests::OneShotUpstream;

    let conflict = b"event: response.completed\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n";
    let malformed = b"event: response.completed\ndata: broken-json\n\n";
    for (model, frame) in [
        ("native/alias", conflict.as_slice()),
        ("aero/fixture", conflict.as_slice()),
        ("aero/fixture", malformed.as_slice()),
    ] {
        let upstream = OneShotUpstream::serve(1, move |socket| {
            upstream_reply(socket, 200, "text/event-stream", frame.len(), "");
            socket.write_all(frame).unwrap();
        });
        let directory = fixture_directory(&upstream.base_url());
        if model == "aero/fixture" {
            install_aero_fixture(directory.path(), &upstream.base_url());
        }
        let fixture = Fixture::with_directory(directory);
        let id = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let path = "/v1/responses";
                let probe = Probe::arm(&fixture, path, None);
                let wire = json_request(
                    fixture.port(),
                    path,
                    &[
                        &fixture.session(),
                        "session_id: fixture",
                        "Authorization: Bearer caller",
                    ],
                    &serde_json::to_vec(&json!({"model":model,"stream":true,"input":"fixture"}))
                        .unwrap(),
                );
                let reply = assert_sse_wire(fixture.server.local_addr(), &wire).await;
                let body = reply
                    .windows(4)
                    .position(|bytes| bytes == b"\r\n\r\n")
                    .unwrap()
                    + 4;
                if model == "native/alias" {
                    assert!(reply[body..].starts_with(frame));
                } else {
                    assert_eq!(&reply[body..], frame);
                }
                receive(probe.id, "request ID").await
            });
        fixture.server.shutdown().unwrap();
        if model == "native/alias" {
            continue;
        }
        let journal = live_journal(fixture.directory.path());
        let receipt = &journal
            .iter()
            .find(|event| {
                event["event"] == "request_finished" && event["fields"]["request_id"] == id
            })
            .unwrap()["fields"];
        assert_eq!(receipt["delivery"], "partial");
        assert_eq!(receipt["terminal_written"], false);
        assert_eq!(receipt["downstream_terminal"], "unknown");
    }
}

#[cfg(unix)]
const NATIVE_DELTA: &[u8] =
    b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n";
#[cfg(unix)]
const ACCOUNTED_TERMINAL: &[u8] = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":3,\"output_tokens\":2,\"total_tokens\":5}}}\n\n";
#[cfg(unix)]
const FAILED_EVENT: &[u8] =
    b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"origin\":\"emp\",\"code\":\"fixture_failure\",\"message\":\"private failure\"}}}\n\n";

#[cfg(unix)]
fn wait_for_diagnostic(fixture: &Fixture, id: &str, name: &str, matches: impl Fn(&Value) -> bool) {
    let deadline = Deadline::after(wire_timeout());
    crate::tests::wait_for(&deadline, || {
        crate::tests::live_journal_until(fixture.directory.path(), &deadline)
            .iter()
            .any(|row| {
                row["event"] == name && row["fields"]["request_id"] == id && matches(&row["fields"])
            })
    });
}

#[cfg(unix)]
fn terminal_after_delta() -> (crate::tests::OneShotUpstream, oneshot::Sender<()>) {
    use super::inference::upstream_reply;
    use crate::tests::OneShotUpstream;

    let (send_terminal, ready) = oneshot::channel();
    let mut ready = Some(ready);
    let upstream = OneShotUpstream::serve(1, move |socket| {
        upstream_reply(
            socket,
            200,
            "text/event-stream",
            NATIVE_DELTA.len() + ACCOUNTED_TERMINAL.len(),
            "",
        );
        socket.write_all(NATIVE_DELTA).unwrap();
        if ready.take().unwrap().blocking_recv().is_ok() {
            socket.write_all(ACCOUNTED_TERMINAL).unwrap();
        }
    });
    (upstream, send_terminal)
}

#[cfg(unix)]
#[test]
fn drain_native_accounts_for_a_read_terminal_before_previous_frame_flush() {
    use super::inference::fixture;

    let (upstream, send_terminal) = terminal_after_delta();
    fixture(&upstream.base_url()).run_wire(async move |fixture, address| {
        let configuration = &fixture.server.state.backend.configuration;
        let probe = Probe::arm(
            fixture,
            "/v1/responses",
            Some(WriteTrigger::Frame(Box::new(|frame| frame == NATIVE_DELTA))),
        );
        let body = json!({
            "model": "native/alias", "stream": true, "input": "fixture",
            "tools": [{"type": "namespace", "name": "collaboration", "tools": [{
                "type": "function", "name": "spawn_agent", "parameters": {
                    "properties": {"message": {"type": "string", "encrypted": true}}
                }
            }]}]
        });
        let wire = json_request(
            fixture.port(),
            "/v1/responses",
            &[&fixture.session(), "Authorization: Bearer caller"],
            &serde_json::to_vec(&body).unwrap(),
        );
        let client = RawClient::request(address, &wire).await;
        let (reached, hold_write) = probe.write.unwrap();
        assert_eq!(
            receive(reached, "paused upstream delta write")
                .await
                .as_deref(),
            Some(NATIVE_DELTA)
        );
        let id = receive(probe.id, "request ID").await;
        let (_, _, mut sent) = upstream.observed();
        assert_eq!(sent["tools"][0]["name"], "emp_collaboration");
        let expected = {
            let mut config = configuration.test_config().lock().unwrap();
            let provider = config["providers"][0].as_object().unwrap();
            let model = config["models"][1].as_object().unwrap();
            let sent_estimate = emp_context::context::assess(provider, model, "responses", &sent)
                .input_estimate
                .unwrap();
            sent["tools"] = body["tools"].clone();
            assert_ne!(
                emp_context::context::assess(provider, model, "responses", &sent).input_estimate,
                Some(sent_estimate)
            );
            send_terminal.send(()).unwrap();
            wait_for_diagnostic(fixture, &id, "route_observation", |fields| {
                fields["terminal_event_observed"] == true
            });
            config["external_collaboration"] = json!("off");
            sent_estimate
        };
        let deadline = Deadline::after(wire_timeout());
        crate::tests::wait_for(&deadline, || {
            configuration.read().unwrap()["models"][1]["context_calibrations"][0]
                ["largest_success_estimate"] == expected
        });
        client.io.set_zero_linger().unwrap();
        drop(client);
        drop(hold_write);
        wait_for_diagnostic(fixture, &id, "request_finished", |fields| {
            fields["delivery"] == "write_failed"
                && fields["terminal_written"] == false
                && fields["writes_completed"] == 1
        });

        let db = rusqlite::Connection::open(fixture.directory.path().join("state/usage.sqlite3"))
            .unwrap();
        let (facts, delivery): (String, String) = db
            .query_row(
                "SELECT facts,delivery FROM call_records WHERE request_id = ?",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let facts: Value = serde_json::from_str(&facts).unwrap();
        let delivery: Value = serde_json::from_str(&delivery).unwrap();
        assert_eq!(facts["input_tokens"], 3);
        assert_eq!(facts["output_tokens"], 2);
        assert_eq!(facts["response_status"], "completed");
        assert_eq!(delivery["delivery"], "write_failed");
    });
}

#[cfg(unix)]
#[test]
fn drain_external_releases_business_before_saving_the_learned_protocol() {
    drain_external_protocol_learning(false, None, "demo/model");
}

#[cfg(unix)]
#[test]
fn drain_external_discards_learning_when_the_provider_endpoint_changes() {
    drain_external_protocol_learning(false, Some("http://127.0.0.1:1/replacement"), "demo/model");
}

#[cfg(unix)]
#[test]
fn drain_external_saves_the_learned_protocol_after_write_failure() {
    drain_external_protocol_learning(true, None, "demo/model");
}

#[cfg(unix)]
#[test]
fn drain_external_saves_learning_for_the_selected_aggregate_member() {
    drain_external_protocol_learning(false, None, "auto/learning");
}

#[cfg(unix)]
async fn hold_protocol_save(
    fixture: &Fixture,
    change: impl FnOnce(&mut Value) + Send + 'static,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let (locked, at_lock) = oneshot::channel();
    let (unlock, held) = oneshot::channel();
    let state = fixture.server.state.clone();
    let saver_gate = tokio::task::spawn_blocking(move || {
        let mut config = state.backend.configuration.test_config().lock().unwrap();
        let _ = locked.send(());
        if held.blocking_recv().is_ok() {
            change(&mut config);
        }
    });
    receive(at_lock, "configuration save gate held").await;
    (unlock, saver_gate)
}

#[cfg(unix)]
fn drain_external_protocol_learning(
    fail_write: bool,
    replacement_endpoint: Option<&'static str>,
    requested_model: &str,
) {
    use super::inference::{
        FIRST, LAST, STREAM_REQUEST, SseClient, fixture_directory, upstream_reply,
    };
    use crate::tests::OneShotUpstream;

    let (send_terminal, ready) = oneshot::channel();
    let mut ready = Some(ready);
    let upstream = OneShotUpstream::serve(1, move |socket| {
        upstream_reply(
            socket,
            200,
            "text/event-stream",
            FIRST.len() + LAST.len(),
            "",
        );
        socket.write_all(FIRST).unwrap();
        if ready.take().unwrap().blocking_recv().is_ok() {
            socket.write_all(LAST).unwrap();
        }
    });
    let directory = fixture_directory(&upstream.base_url());
    let path = directory.path().join("config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["providers"][1]["protocol"] = json!("auto");
    config["models"][0]["upstream_id"] = json!("demo/upstream-model");
    config["model_groups"] = json!([{
        "id": "auto/learning", "policy": "priority", "sources": ["demo/model", "native/alias"]
    }]);
    std::fs::write(&path, config.to_string()).unwrap();
    let mut body: Value = serde_json::from_slice(STREAM_REQUEST).unwrap();
    body["model"] = json!(requested_model);

    Fixture::with_directory(directory).run_wire(async move |fixture, address| {
        let state = &fixture.server.state;
        let control = &state.lifecycle;
        let _clock = TestDrainClock::install(control);
        let probe = Probe::arm(
            fixture,
            "/v1/responses",
            fail_write.then(|| {
                WriteTrigger::Frame(Box::new(|frame| {
                    frame.starts_with(b"event: response.output_item.done\n")
                }))
            }),
        );
        let request = json_request(
            fixture.port(),
            "/v1/responses",
            &[&fixture.session(), "session-id: protocol-learning"],
            body.to_string().as_bytes(),
        );
        let client = RawClient::request(address, &request).await;
        let id = receive(probe.id, "request ID").await;

        if let Some((reached, hold_write)) = probe.write {
            send_terminal.send(()).unwrap();
            receive(reached, "previous frame queued with its write paused").await;
            wait_for_diagnostic(fixture, &id, "route_observation", |fields| {
                fields["terminal_event_observed"] == true
            });
            client.io.set_zero_linger().unwrap();
            drop(client);
            drop(hold_write);
            wait_for_diagnostic(fixture, &id, "request_finished", |fields| {
                fields["delivery"] == "write_failed" && fields["terminal_written"] == false
            });
        } else {
            let (handing_off, at_handoff) = oneshot::channel();
            let (handoff, resume_handoff) = oneshot::channel::<()>();
            *control.test_hooks.on_stream_handoff.lock().unwrap() = Some(Box::new(move || {
                let _ = handing_off.send(());
                let _ = resume_handoff.blocking_recv();
            }));
            let (closed, mut at_close) = oneshot::channel();
            let config_path = path.clone();
            *control.test_hooks.on_stream_close.lock().unwrap() = Some(Box::new(move || {
                let saved: Value =
                    serde_json::from_slice(&std::fs::read(config_path).unwrap()).unwrap();
                let _ = closed.send(saved["providers"][1]["resolved_protocol"].clone());
            }));
            control.stop(ShutdownReason::Requested);
            send_terminal.send(()).unwrap();
            receive(at_handoff, "terminal queued and calibration complete").await;
            let (unlock, saver_gate) = hold_protocol_save(fixture, move |config| {
                if let Some(endpoint) = replacement_endpoint {
                    config["providers"][1]["base_url"] = json!(endpoint);
                }
            })
            .await;
            handoff.send(()).unwrap();
            let mut client = SseClient::from_raw(client);
            timeout(wire_timeout().duration(), async {
                while client.next().await.expect("terminal SSE event")["type"]
                    != "response.completed"
                {}
            })
            .await
            .expect("terminal frame reaches the client while configuration is locked");
            timeout(
                wire_timeout().duration(),
                control.admission.wait_for_business_idle(),
            )
            .await
            .expect("business handed off while configuration is locked");
            assert_eq!(
                at_close.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            );
            unlock.send(()).unwrap();
            saver_gate.await.unwrap();
            let at_close = receive(at_close, "stream output closed").await;
            if replacement_endpoint.is_none() {
                assert_eq!(at_close, "chat_completions", "saved before output closed");
            }
            assert!(
                timeout(wire_timeout().duration(), client.next())
                    .await
                    .expect("HTTP stream ends after the save")
                    .is_none()
            );
        }

        let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let provider = saved["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["id"] == "demo")
            .unwrap();
        let model = saved["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == "demo/model")
            .unwrap();
        if let Some(endpoint) = replacement_endpoint {
            let config = state.backend.configuration.read().unwrap();
            assert_eq!(config["providers"][1]["base_url"], endpoint);
            assert_eq!(config["providers"][1]["resolved_protocol"], "");
            assert_eq!(config["models"][0]["resolved_protocol"], "");
            assert_eq!(provider["resolved_protocol"], "");
            assert_eq!(model["resolved_protocol"], "");
            drop(config);
            let journal = fixture.flushed_journal().await;
            assert!(journal.iter().any(|row| {
                row["event"] == "protocol_observation_discarded"
                    && row["fields"]["provider_id"] == "demo"
                    && row["fields"]["reason"] == "route_identity_changed"
            }));
        } else {
            assert_eq!(provider["resolved_protocol"], "chat_completions");
            assert_eq!(model["resolved_protocol"], "chat_completions");
            let observation = &provider["protocol_observation"];
            assert_eq!(observation, &model["protocol_observation"]);
            assert_eq!(observation["upstream_model"], "demo/upstream-model");
            assert_eq!(observation["deployment_identity"], "default");
            assert_eq!(
                observation["endpoint_fingerprint"],
                emp_domain::endpoint_fingerprint(provider["base_url"].as_str())
            );
            assert!(observation["observed_at"].is_string());
            let next = emp_domain::resolve_route_without_catalog(&saved, "demo/model").unwrap();
            assert_eq!(
                emp_router::observed_protocol(&next),
                Some(emp_domain::Protocol::ChatCompletions)
            );
            let journal = fixture.flushed_journal().await;
            assert!(
                journal
                    .iter()
                    .all(|row| row["event"] != "protocol_observation_discarded")
            );
        }
    });
    assert_eq!(upstream.observed().0, "/v1/chat/completions");
}

#[cfg(unix)]
#[test]
fn drain_vertex_discards_learning_when_the_region_changes() {
    use super::inference::{SseClient, fixture_directory};
    use crate::tests::VertexFixture;

    let upstream = VertexFixture::start_for("aiplatform.googleapis.com");
    let directory = fixture_directory("http://127.0.0.1:1/v1");
    let root = emp_state::config::resolve_user_path(directory.path());
    let path = root.join("config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["providers"][1]["protocol"] = json!("auto");
    config["providers"][1]["auth_mode"] = json!("vertex_api_key");
    config["providers"][1]["vertex"] = json!({"project":"fixture-project","location":"global"});
    config["models"][0]["upstream_id"] = json!("gemini-3-flash-preview");
    std::fs::write(&path, config.to_string()).unwrap();
    let server = RunningServer::start_with_test_options(
        LOOPBACK.ip(),
        &path,
        root.join("codex/auth.json"),
        "missing-test-codex",
        Some(upstream.client()),
    )
    .unwrap();
    Fixture { directory, server, upstream: Arc::default() }.run_wire(async move |fixture, address| {
        let state = &fixture.server.state;
        let control = &state.lifecycle;
        let (handing_off, at_handoff) = oneshot::channel();
        let (handoff, resume_handoff) = oneshot::channel::<()>();
        *control.test_hooks.on_stream_handoff.lock().unwrap() = Some(Box::new(move || {
            let _ = handing_off.send(());
            let _ = resume_handoff.blocking_recv();
        }));
        let body = json!({"model":"demo/model","stream":true,"input":"Read",
            "tools":[{"type":"function","name":"read","parameters":{"type":"object","properties":{}}}]});
        let request = json_request(fixture.port(), "/v1/responses", &[&fixture.session()], body.to_string().as_bytes());
        let mut client = SseClient::from_raw(RawClient::request(address, &request).await);
        receive(at_handoff, "Vertex terminal queued").await;
        let (unlock, saver_gate) = hold_protocol_save(fixture, |config| {
            config["providers"][1]["vertex"]["location"] = json!("us");
        }).await;
        handoff.send(()).unwrap();
        timeout(wire_timeout().duration(), control.admission.wait_for_business_idle())
            .await.expect("Vertex save waits after business handoff");
        unlock.send(()).unwrap();
        saver_gate.await.unwrap();
        timeout(wire_timeout().duration(), async {
            while client.next().await.expect("terminal Vertex event")["type"] != "response.completed" {}
            assert!(client.next().await.is_none());
        }).await.expect("Vertex stream closes after discarding the candidate");
        let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["providers"][1]["resolved_protocol"], "");
        assert_eq!(saved["models"][0]["resolved_protocol"], "");
        assert_eq!(state.backend.configuration.read().unwrap()["providers"][1]["vertex"]["location"], "us");
        let journal = fixture.flushed_journal().await;
        assert!(journal.iter().any(|row| {
            row["event"] == "protocol_observation_discarded"
                && row["fields"]["provider_id"] == "demo"
                && row["fields"]["reason"] == "route_identity_changed"
        }));
    });
}

#[cfg(unix)]
#[test]
fn drain_aero_observes_the_parent_terminal_before_previous_frame_flush() {
    use super::inference::fixture_directory;

    let (upstream, send_terminal) = terminal_after_delta();
    let native = crate::tests::OneShotUpstream::start_repeated_wire(
        200,
        "text/event-stream",
        None,
        1,
        vec![ACCOUNTED_TERMINAL.to_vec()],
    );
    let directory = fixture_directory(&native.base_url());
    install_aero_fixture(directory.path(), &upstream.base_url());
    Fixture::with_directory(directory).run_wire(async move |fixture, address| {
        let native_request = json_request(
            fixture.port(),
            "/v1/responses",
            &[&fixture.session(), "Authorization: Bearer caller"],
            br#"{"model":"native/alias","stream":true,"input":"fixture"}"#,
        );
        assert_status(&exchange_wire(address, &native_request, false).await, 200);
        let diagnostics = request(
            fixture.port(),
            "GET",
            "/api/diagnostics",
            &[&fixture.session()],
            b"",
        );
        let snapshot = json_body(&exchange_wire(address, &diagnostics, false).await);
        assert_eq!(snapshot["sample_count"], 1);
        assert!(snapshot["records"][0]["observation_id"].is_string());
        assert!(snapshot["records"][0]["observed_at"].is_string());
        assert!(snapshot["records"][0]["request_id"].is_string());
        let probe = Probe::arm(
            fixture,
            "/v1/responses",
            Some(WriteTrigger::Frame(Box::new(|frame| frame == NATIVE_DELTA))),
        );
        let client = RawClient::request(address, &aero_request(fixture, true)).await;
        let (reached, hold_write) = probe.write.unwrap();
        assert_eq!(
            receive(reached, "paused Aero delta write").await.as_deref(),
            Some(NATIVE_DELTA)
        );
        let id = receive(probe.id, "request ID").await;
        send_terminal.send(()).unwrap();
        wait_for_diagnostic(fixture, &id, "request_terminal_observed", |fields| {
            fields["route"] == "aero" && fields["terminal_event_observed"] == true
        });
        client.io.set_zero_linger().unwrap();
        drop(client);
        drop(hold_write);
        wait_for_diagnostic(fixture, &id, "request_finished", |fields| {
            fields["terminal_event_observed"] == true
                && fields["delivery"] == "write_failed"
                && fields["terminal_written"] == false
                && fields["downstream_terminal"] == "unknown"
        });
        for _ in 0..2 {
            let replay = json_body(&exchange_wire(address, &diagnostics, false).await);
            assert_eq!(replay["sample_count"], snapshot["sample_count"]);
            assert_eq!(replay["records"], snapshot["records"]);
        }

        let wire = request(
            fixture.port(),
            "GET",
            "/api/calls?start=0&end=4102444800",
            &[&fixture.session()],
            b"",
        );
        let reply = exchange_wire(address, &wire, false).await;
        assert_status(&reply, 200);
        let calls = json_body(&reply);
        let records = calls["records"].as_array().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0]["request_id"],
            snapshot["records"][0]["request_id"]
        );
    });
}

#[cfg(unix)]
#[test]
fn drain_aero_resumes_diagnostics_after_malformed_sse_chunk() {
    use super::inference::{fixture_directory, upstream_reply};
    use crate::tests::OneShotUpstream;

    let malformed = b"event: response.failed\ndata: broken-json\n\n";
    let (send_next, next) = oneshot::channel();
    let mut next = Some(next);
    let upstream = OneShotUpstream::serve(1, move |socket| {
        upstream_reply(
            socket,
            200,
            "text/event-stream",
            malformed.len() + FAILED_EVENT.len(),
            "",
        );
        socket.write_all(malformed).unwrap();
        if next.take().unwrap().blocking_recv().is_ok() {
            socket.write_all(FAILED_EVENT).unwrap();
        }
    });
    let directory = fixture_directory("http://127.0.0.1:1/v1");
    install_aero_fixture(directory.path(), &upstream.base_url());
    Fixture::with_directory(directory).run_wire(async move |fixture, address| {
        let probe = Probe::arm(fixture, "/v1/responses", None);
        let raw = RawClient::request(address, &aero_request(fixture, true)).await;
        let mut client = BufReader::new(raw.io);
        assert_stream_prefix(&mut client, malformed).await;
        send_next.send(()).unwrap();
        assert_stream_prefix(&mut client, FAILED_EVENT).await;
        assert_stream_end(&mut client).await;
        let id = receive(probe.id, "request ID").await;
        wait_for_diagnostic(fixture, &id, "request_finished", |fields| {
            fields["downstream_terminal"] == "failed"
        });
        let journal = fixture.flushed_journal().await;
        let failures = journal
            .iter()
            .filter(|row| row["event"] == "response_error" && row["fields"]["request_id"] == id)
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0]["fields"]["error_code"], "fixture_failure");
        assert_eq!(failures[0]["fields"]["delivered"], true);
    });
}

#[cfg(unix)]
#[test]
fn drain_native_and_aero_publish_error_at_flush_while_producer_is_held() {
    use super::inference::{SseClient, fixture_directory};
    use crate::tests::OneShotUpstream;

    for aero in [false, true] {
        let (close_upstream, keep_open) = oneshot::channel::<()>();
        let mut keep_open = Some(keep_open);
        let upstream = OneShotUpstream::serve(1, move |socket| {
            socket.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            ).unwrap();
            socket.write_all(NATIVE_DELTA).unwrap();
            socket.write_all(FAILED_EVENT).unwrap();
            let _ = keep_open.take().unwrap().blocking_recv();
        });
        let directory = fixture_directory(&upstream.base_url());
        if aero {
            install_aero_fixture(directory.path(), &upstream.base_url());
        }
        Fixture::with_directory(directory).run_wire(async move |fixture, address| {
            let (entered, at_handoff) = oneshot::channel();
            let (resume, wait) = oneshot::channel::<()>();
            if !aero {
                *fixture
                    .server
                    .state
                    .lifecycle
                    .test_hooks
                    .on_stream_handoff
                    .lock()
                    .unwrap() = Some(Box::new(move || {
                    let _ = entered.send(());
                    let _ = wait.blocking_recv();
                }));
            }
            let probe = Probe::arm(fixture, "/v1/responses", None);
            let wire = if aero {
                aero_request(fixture, true)
            } else {
                json_request(
                    fixture.port(),
                    "/v1/responses",
                    &[&fixture.session(), "Authorization: Bearer caller"],
                    br#"{"model":"native/alias","stream":true,"input":"fixture"}"#,
                )
            };
            let mut client = SseClient::from_raw(RawClient::request(address, &wire).await);
            timeout(wire_timeout().duration(), async {
                while client.next().await.expect("failure frame")["type"] != "response.failed" {}
            })
            .await
            .unwrap();
            if !aero {
                receive(at_handoff, "native producer paused at business handoff").await;
            }
            let id = receive(probe.id, "request ID").await;
            let origin = if aero { "emp" } else { "upstream" };
            wait_for_diagnostic(fixture, &id, "response_error", |fields| {
                fields["error_origin"] == origin
                    && fields["error_code"] == "fixture_failure"
                    && fields["phase"] == "execute_and_relay"
                    && fields["delivered"] == true
            });
            drop(resume);
            close_upstream.send(()).unwrap();
            timeout(wire_timeout().duration(), async {
                while client.next().await.is_some() {}
            })
            .await
            .unwrap();
            let journal = fixture.flushed_journal().await;
            assert_eq!(
                journal
                    .iter()
                    .filter(
                        |row| row["event"] == "response_error" && row["fields"]["request_id"] == id
                    )
                    .count(),
                1
            );
        });
    }
}

#[cfg(unix)]
#[test]
fn drain_aero_revokes_delegation_at_eof_handoff() {
    use super::inference::{STREAM_REQUEST, fixture_directory};
    use crate::{
        http::request::parse_request, server::http::proxy::ConnectionIntake, tests::OneShotUpstream,
    };
    use std::io::ErrorKind;

    let (close, wait_for_eof) = oneshot::channel::<()>();
    let mut wait_for_eof = Some(wait_for_eof);
    let response = br#"{"id":"resp_aero_fixture","output":[]}"#;
    let aero = OneShotUpstream::serve(1, move |socket| {
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        socket.write_all(response).unwrap();
        let _ = wait_for_eof.take().unwrap().blocking_recv();
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
        let state = &fixture.server.state;
        let control = &state.lifecycle;
        let parent = RawClient::request(address, &aero_request(fixture, false)).await;
        let mut parent = BufReader::new(parent.io);
        assert_stream_prefix(&mut parent, response).await;
        let (_, headers, _) = aero.observed();
        let key = &headers["x-aero-caller-key"];
        let authorization = format!("Authorization: Bearer {key}");
        let callback = json_request(
            fixture.port(),
            "/v1/responses",
            &[&authorization],
            STREAM_REQUEST,
        );
        assert!(state.backend.aero_callers.identify(key).is_some());
        assert!(control.admission.has_business_work());
        let header = state.backend.aero_callers.acquire_header().unwrap();
        let (revoked, before_publish) = oneshot::channel();
        let (publish, wait_for_publish) = oneshot::channel::<()>();
        *control.test_hooks.on_stream_handoff.lock().unwrap() = Some(Box::new(move || {
            let _ = revoked.send(());
            let _ = wait_for_publish.blocking_recv();
        }));
        close.send(()).unwrap();
        receive(before_publish, "Aero entered the EOF handoff locks").await;

        let (observed, contended) = oneshot::channel();
        *control.test_hooks.callback_lock_contended.lock().unwrap() = Some(observed);
        let racing_callback = tokio::task::spawn_blocking({
            let state = state.clone();
            let callback = callback.clone();
            move || {
                let request = parse_request(str::from_utf8(&callback).unwrap()).unwrap();
                let (_, delegation) = ConnectionIntake::CallbackHead(header)
                    .admit(Some(request), &state, Some(address.ip()))
                    .unwrap();
                assert!(delegation.is_none());
            }
        });
        assert!(receive(contended, "callback reached the locked delegation table").await);
        publish.send(()).unwrap();
        racing_callback.await.unwrap();
        assert!(!control.admission.has_business_work());
        let reply = exchange_wire(address, &callback, false).await;
        assert_status(&reply, 401);
        assert!(
            json_body(&reply)["error"]["message"]
                .as_str()
                .unwrap()
                .ends_with("proxy caller authentication is required")
        );

        let quit = Probe::arm(fixture, "/api/quit", Some(WriteTrigger::Output));
        let wire = json_request(fixture.port(), "/api/quit", &[&fixture.session()], b"{}");
        let quit_client = submit(address, &wire).await;
        let mut quit = Work::new(quit, quit_client, None, 200, "stopping").await;
        quit.writing().await;
        assert!(!control.admission.has_business_work());

        let mut reply = Vec::new();
        let result = timeout(wire_timeout().duration(), async {
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

        assert_stream_end(&mut parent).await;
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
                (phase == Sending).then(|| {
                    WriteTrigger::Frame(Box::new(|frame| {
                        frame.starts_with(b"event: response.completed\n")
                    }))
                }),
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
            Work::new(
                probe,
                client.io,
                Some(release),
                200,
                "event: response.completed",
            )
            .await
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
        HandoffAtDeadline,
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
        HandoffAtDeadline,
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
    let marker = if broken_facts {
        "event: error"
    } else {
        "event: result"
    };
    let name = format!("{path} {marker}");
    run_case(fixture, &name, phase, via_quit, async |fixture| {
        let probe = Probe::arm(
            fixture,
            path,
            matches!(phase, Sending | HandoffAtDeadline).then(|| {
                WriteTrigger::Frame(Box::new(|frame| {
                    frame.starts_with(b"event: result\n") || frame.starts_with(b"event: error\n")
                }))
            }),
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
        let mut work = Work::new(probe, client.io, None, 200, marker).await;
        if let Some((reached, release)) = progress {
            assert!(
                !receive(reached, "report query paused after queueing progress")
                    .await
                    .load(Ordering::Acquire)
            );
            let total = if path == "/api/calls" { 4 } else { 1 };
            let first = crate::services::events::sse_frame(
                "progress",
                &json!({"completed":0,"total":total}),
            )
            .unwrap();
            assert_stream_prefix(&mut work.client, &first).await;
            work.release = Some(release);
        }
        work
    });
}
