use super::*;
use crate::{
    error::AppError,
    server::{
        ShutdownReason,
        raw_tcp::{RawClient, wire_timeout},
        tests::{RequestProbe, TestDrainClock, WriteProbe},
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
        let mut armed = fixture.server.state.lifecycle.test_request.lock().unwrap();
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
    let mut work = open(&fixture).await;
    if phase == Sending {
        work.finish_business();
        work.writing().await;
    }

    let mut quit_id = None;
    if via_quit {
        let probe = Probe::arm(&fixture, "/api/quit", None);
        let quit = tokio::spawn(async move {
            exchange_before(
                address,
                &json_request(address.port(), "/api/quit", &[&session], b"{}"),
                false,
                &Deadline::after(
                    crate::services::shutdown::QUIT_PRECHECK_WAIT
                        + crate::server::raw_tcp::wire_timeout(),
                ),
            )
            .await
        });
        if phase == Finishes {
            timeout(wire_timeout(), control.admission.wait_for_websocket_drain())
                .await
                .expect("quit closed admission for its precheck");
            work.finish_business();
        }
        let reply = quit.await.expect(&case);
        quit_id = Some(receive(probe.id, "quit request ID").await);
        if phase == BusyAtDeadline {
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
        } else {
            wire_contract::assert_json_reply(
                &case,
                &reply,
                "200 OK",
                &json!({"status":"stopping"}),
            );
        }
    }

    let Fixture {
        directory, server, ..
    } = fixture;
    if quit_id.is_some() && phase != BusyAtDeadline {
        assert_eq!(
            timeout(wire_timeout(), control.signal().wait())
                .await
                .expect(&case),
            ShutdownReason::ResponseWritten,
            "{case}"
        );
    }
    let shutdown = tokio::task::spawn_blocking(move || server.shutdown());
    let reason = timeout(wire_timeout(), control.signal().wait())
        .await
        .expect(&case);
    assert_eq!(
        reason,
        if quit_id.is_some() && phase != BusyAtDeadline {
            ShutdownReason::ResponseWritten
        } else {
            ShutdownReason::Requested
        },
        "{case}"
    );

    if !via_quit && phase == Finishes {
        work.finish_business();
    }
    if phase == FinishesAfterDeadline {
        clock.pass_deadline();
        work.finish_business();
        work.writing().await;
    }
    if matches!(phase, Sending | FinishesAfterDeadline) {
        timeout(wire_timeout(), control.admission.wait_for_business_idle())
            .await
            .unwrap_or_else(|_| panic!("{case}: business still counted while write was paused"));
    }
    if phase == BusyAtDeadline {
        work.writing().await;
    }
    match phase {
        Finishes => {}
        FinishesAfterDeadline => clock.cutoff.notify_one(),
        BusyAtDeadline | Sending => clock.expire(),
    }
    let received = work.read_to_end(&case).await;
    if phase == BusyAtDeadline {
        work.finish_business();
    }
    if phase == Finishes {
        assert!(
            received
                .windows(work.completed.len())
                .any(|part| part == work.completed.as_bytes()),
            "{case}: missing completed response, received {:?}",
            String::from_utf8_lossy(&received),
        );
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
    let result = if phase == Finishes || work.status == 101 {
        "responded"
    } else {
        "write_failed"
    };
    assert_settlement(&journal, &case, &work.id, json!(work.status), result);
    if let Some(quit) = quit_id {
        let (status, result) = if phase == BusyAtDeadline {
            (409, "rejected")
        } else {
            (200, "responded")
        };
        assert_settlement(&journal, &case, &quit, json!(status), result);
    }
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
    for &phase in ACTIVE {
        for via_quit in [false, true] {
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
                        .test_health_handler
                        .lock()
                        .unwrap() = Some((entered, wait));
                    let probe = Probe::arm(
                        fixture,
                        "/healthz",
                        (phase == Sending).then_some(b"HTTP/1.1".as_slice()),
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
        let (release, wait) = oneshot::channel();
        let mut wait = Some(wait);
        let upstream = OneShotUpstream::serve(1, move |socket| {
            upstream_reply(socket, 200, "application/json", RESULT.len(), "");
            if wait.take().unwrap().blocking_recv().is_ok() {
                socket.write_all(RESULT).unwrap();
            }
        });
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
}

#[cfg(unix)]
fn inference_case(phase: Phase, via_quit: bool, delegated: bool) {
    use super::inference::{FIRST, LAST, STREAM_REQUEST, fixture_directory, upstream_reply};
    use crate::tests::OneShotUpstream;

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
    let directory = fixture_directory(&upstream.base_url());
    let parent = delegated.then(|| {
        let (disconnect, wait) = oneshot::channel();
        let mut wait = Some(wait);
        let aero = OneShotUpstream::serve(1, move |_| {
            let _ = wait.take().unwrap().blocking_recv();
        });
        let path = directory.path().join("config.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["aero_profiles"] = json!([{
            "id":"aero/fixture","name":"fixture","planner":{"model":"demo/model"},"executor":{"model":"demo/model"}
        }]);
        std::fs::write(path, config.to_string()).unwrap();
        let started = json!({"value":{"endpoint":aero.base_url()},"error":null});
        install_worker_fixture(
            directory.path(),
            emp_modules::Module::Aero,
            "{}",
            &format!("while read -r line; do printf '%s\\n' '{started}'; done"),
        );
        (disconnect, aero)
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
            let parent = if let Some((disconnect, aero)) = parent {
                let wire = json_request(
                    fixture.port(),
                    path,
                    &[&session, "session_id: fixture"],
                    br#"{"model":"aero/fixture","stream":false,"input":"fixture"}"#,
                );
                let task = tokio::spawn(async move { exchange_wire(address, &wire, false).await });
                let (disconnect, aero, headers) = tokio::task::spawn_blocking(move || {
                    let (_, headers, _) = aero.observed();
                    (disconnect, aero, headers)
                })
                .await
                .unwrap();
                Some((
                    disconnect,
                    aero,
                    task,
                    format!("Authorization: Bearer {}", headers["x-aero-caller-key"]),
                ))
            } else {
                None
            };
            let probe = Probe::arm(
                fixture,
                path,
                (phase == Sending).then_some(b"0\r\n\r\n".as_slice()),
            );
            let headers = parent.as_ref().map_or_else(
                || vec![session.as_str()],
                |(_, _, _, token)| vec![token.as_str()],
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
            if let Some((disconnect, _aero, task, _)) = parent {
                assert_eq!(
                    fixture.server.state.lifecycle.admission.active_requests(),
                    1
                );
                disconnect.send(()).unwrap();
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

#[cfg(unix)]
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
}

#[cfg(unix)]
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
}

#[cfg(unix)]
fn report_case(path: &'static str, query: &str, phase: Phase, via_quit: bool, broken_facts: bool) {
    use super::management_streams::{report_gate, seed_reports};

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
        let probe = Probe::arm(fixture, path, (phase == Sending).then_some(marker));
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
