use super::{
    report_fixture::{report_gate, seed_reports},
    *,
};
use crate::{
    server::raw_tcp::{RawClient, wire_timeout},
    services::management_events::Change,
    tests::wait_for,
};
use emp_test_support::Deadline;
use std::{collections::BTreeMap, time::Instant};
use tokio::io::BufReader;

const ACCOUNT_EVENTS: &str = "/api/accounts/events";
const CALLS: &str = "/api/calls?start=1&end=10";
const USAGE: &str = "/api/usage?lifetime=%74rue&owner=fixture";
const ACCEPT: &str = "Accept: text/event-stream";

struct Stream {
    io: BufReader<TcpStream>,
    headers: BTreeMap<String, String>,
    chunked: ChunkDecoder,
    pending: Vec<u8>,
}

impl Stream {
    async fn open(address: SocketAddr, wire: &[u8], cache: &str) -> Self {
        let RawClient { io, head } = RawClient::request(address, wire).await;
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        let mut headers = head
            .lines()
            .skip(1)
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.to_ascii_lowercase(), value.trim().to_owned()))
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(headers.remove("transfer-encoding"), Some("chunked".into()));
        assert!(!headers.contains_key("content-length"));
        assert_eq!(headers["content-type"], "text/event-stream");
        assert_eq!(headers["cache-control"], cache);
        assert_eq!(headers["connection"], "close");
        assert_eq!(headers["x-frame-options"], "DENY");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["content-security-policy"], "frame-ancestors 'none'");
        Self {
            io: BufReader::new(io),
            headers,
            chunked: ChunkDecoder::default(),
            pending: Vec::new(),
        }
    }

    async fn next(&mut self, deadline: Duration) -> Option<String> {
        timeout(deadline, async {
            loop {
                if let Some(end) = self.pending.windows(2).position(|bytes| bytes == b"\n\n") {
                    return Some(
                        String::from_utf8(self.pending.drain(..end + 2).collect()).unwrap(),
                    );
                }
                if self.chunked.is_finished() {
                    break;
                }
                read_chunk_data(&mut self.io, &mut self.chunked, &mut self.pending)
                    .await
                    .expect("read management SSE body");
            }
            assert!(self.pending.is_empty(), "partial SSE event");
            assert!(self.io.fill_buf().await.unwrap().is_empty());
            None
        })
        .await
        .expect("SSE event or end of stream before deadline")
    }

    async fn stop_at_chunk_boundary(&mut self) {
        if self.pending.is_empty() {
            timeout(
                wire_timeout().duration(),
                stop_at_chunk_boundary(&mut self.io, &mut self.chunked),
            )
            .await
            .expect("management SSE chunk ending before deadline")
            .expect("validate management SSE chunk ending before disconnect");
        }
    }
}

fn event(frame: &str) -> (&str, Value) {
    let (name, payload) = frame
        .strip_suffix("\n\n")
        .unwrap()
        .split_once("\n")
        .unwrap();
    (
        name.strip_prefix("event: ").unwrap(),
        serde_json::from_str(payload.strip_prefix("data: ").unwrap()).unwrap(),
    )
}

async fn account_snapshot(stream: &mut Stream) -> String {
    assert_eq!(stream.headers["x-accel-buffering"], "no");
    let quota = stream.next(wire_timeout().duration()).await.unwrap();
    let integration = stream.next(wire_timeout().duration()).await.unwrap();
    let activity = stream.next(wire_timeout().duration()).await.unwrap();
    let usage = stream.next(wire_timeout().duration()).await.unwrap();
    assert_eq!(quota, "event: quota-updated\ndata: {}\n\n");
    assert_eq!(integration, "event: integration-updated\ndata: {}\n\n");
    assert_eq!(usage, "event: usage-updated\ndata: {}\n\n");
    let (name, payload) = event(&activity);
    assert_eq!(name, "activity-updated");
    let timestamp = payload["observed_at"]
        .as_u64()
        .expect("integer activity timestamp");
    assert!(timestamp > 0);
    let original = format!(r#""observed_at": {timestamp}"#);
    assert!(activity.contains(&original));
    let activity = activity.replacen(&original, r#""observed_at": 0"#, 1);
    format!("{quota}{integration}{activity}{usage}")
}

#[test]
fn idle_account_events_close_at_shutdown_without_a_heartbeat() {
    for quit in [false, true] {
        let fixture = Fixture::new();
        let address = fixture.server.local_addr();
        let session = fixture.session();
        let wire = request(fixture.port(), "GET", ACCOUNT_EVENTS, &[&session], b"");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut stream = runtime.block_on(async {
            let mut stream = Stream::open(address, &wire, "no-store").await;
            account_snapshot(&mut stream).await;
            assert_eq!(
                fixture.server.state.lifecycle.admission.active_requests(),
                0
            );
            if quit {
                let reply = exchange_wire(
                    address,
                    &json_request(address.port(), "/api/quit", &[&session], b"{}"),
                    false,
                )
                .await;
                wire_contract::assert_json_reply(
                    "quit with an idle subscriber",
                    &reply,
                    "200 OK",
                    &json!({"status":"stopping"}),
                );
            }
            stream
        });
        let shutdown = std::thread::spawn(move || fixture.server.shutdown());
        runtime.block_on(async {
            if quit {
                assert_eq!(
                    stream.next(wire_timeout().duration()).await.as_deref(),
                    Some("event: emp-exiting\ndata: {}\n\n")
                );
            }
            assert!(stream.next(wire_timeout().duration()).await.is_none());
        });
        shutdown.join().unwrap().unwrap();
        let journal = crate::tests::live_journal(fixture.directory.path());
        assert_eq!(
            journal
                .iter()
                .filter(|entry| {
                    entry["event"] == "http_request_completed"
                        && entry["fields"]["path"] == ACCOUNT_EVENTS
                })
                .count(),
            1
        );
    }
}

#[test]
fn account_stream_snapshot_heartbeat_and_quit_match_while_updates_drain() {
    Fixture::new().run_wire(async |fixture, endpoint| {
        let state = &fixture.server.state;
        let session = fixture.session();
        let wire = request(fixture.port(), "GET", ACCOUNT_EVENTS, &[&session], b"");
        let mut stream = Stream::open(endpoint, &wire, "no-store").await;
        account_snapshot(&mut stream).await;
        assert_eq!(state.lifecycle.admission.active_requests(), 0);
        assert!(
            state
                .lifecycle
                .updates()
                .drain_for_test(wire_timeout())
                .await
        );
        state.backend.management_events.publish(Change::Integration);
        assert_eq!(
            stream.next(wire_timeout().duration()).await.as_deref(),
            Some("event: integration-updated\ndata: {}\n\n")
        );

        let started = Instant::now();
        let frame = stream.next(Duration::from_secs(22)).await;
        assert_eq!(frame.as_deref(), Some(": keep-alive\n\n"));
        assert!(
            (Duration::from_millis(14_500)..Duration::from_secs(22)).contains(&started.elapsed())
        );

        state.backend.management_events.publish(Change::Quit);
        state.backend.management_events.close();
        assert_eq!(
            stream.next(wire_timeout().duration()).await.as_deref(),
            Some("event: emp-exiting\ndata: {}\n\n")
        );
        assert!(stream.next(wire_timeout().duration()).await.is_none());
        let admission = state.lifecycle.admission.quiesce(0, Duration::ZERO);
        assert!(admission.is_some());
    });
}

#[test]
fn account_stream_disconnect_releases_subscription_without_another_event() {
    Fixture::new().run_wire(async |fixture, address| {
        let state = &fixture.server.state;
        let wire = request(
            fixture.port(),
            "GET",
            ACCOUNT_EVENTS,
            &[&fixture.session()],
            b"",
        );
        let mut streams = Vec::new();
        for _ in 0..crate::api::quota::QUOTA_EVENT_SLOT_LIMIT {
            let mut stream = Stream::open(address, &wire, "no-store").await;
            account_snapshot(&mut stream).await;
            stream.stop_at_chunk_boundary().await;
            streams.push(stream);
        }
        let denied = exchange_wire(address, &wire, false).await;
        let expected = br#"{"error":{"message":"Too many quota subscribers","origin":"emp"}}"#;
        wire_contract::assert_reply(
            "management subscriber capacity",
            &denied,
            &format!(
                wire_contract::json_head!("503 Service Unavailable", "{}", "Retry-After: 15"),
                expected.len()
            ),
            expected,
        );

        let count = state.backend.management_events.subscriber_count();
        assert_eq!(count, streams.len());
        assert_eq!(state.lifecycle.admission.active_requests(), 0);
        drop(streams.pop().unwrap());
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            wait_for(&Deadline::after(wire_timeout()), || {
                state.backend.management_events.subscriber_count() == count - 1
            });
        })
        .await
        .unwrap();

        let mut replacement = Stream::open(address, &wire, "no-store").await;
        account_snapshot(&mut replacement).await;
        replacement.stop_at_chunk_boundary().await;
        assert_eq!(
            fixture
                .server
                .state
                .backend
                .management_events
                .subscriber_count(),
            count
        );
        drop(replacement);
        drop(streams);
        let state = fixture.server.state.clone();
        tokio::task::spawn_blocking(move || {
            wait_for(&Deadline::after(wire_timeout()), || {
                state.backend.management_events.subscriber_count() == 0
            });
            assert!(
                state
                    .lifecycle
                    .admission
                    .quiesce(0, Duration::ZERO)
                    .is_some()
            );
        })
        .await
        .unwrap();
    });
}

#[test]
fn report_first_progress_precedes_later_events_and_matches_during_update_drain() {
    let fixture = Fixture::new();
    seed_reports(&fixture);
    fixture.run_wire(async |fixture, address| {
        let state = &fixture.server.state;
        for path in [CALLS, USAGE] {
            let wire = request(
                fixture.port(),
                "GET",
                path,
                &[&fixture.session(), ACCEPT],
                b"",
            );
            {
                let endpoint = address;
                let (reached, release) = report_gate(fixture);
                let mut stream = Stream::open(endpoint, &wire, "no-cache").await;
                let first = stream.next(wire_timeout().duration()).await.unwrap();
                assert_eq!(
                    event(&first),
                    (
                        "progress",
                        json!({"completed":0, "total":if path == CALLS { 4 } else { 1 }})
                    )
                );
                let cancelled = timeout(wire_timeout().duration(), reached)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(!cancelled.load(Ordering::Acquire));
                assert_eq!(state.lifecycle.admission.active_requests(), 1);
                assert!(
                    state
                        .lifecycle
                        .updates()
                        .drain_for_test(wire_timeout())
                        .await
                );
                release.send(()).unwrap();
                let mut frames = vec![first];
                while let Some(frame) = stream.next(wire_timeout().duration()).await {
                    frames.push(frame);
                }
                let (name, payload) = event(frames.last().unwrap());
                assert_eq!(name, "result");
                if path == CALLS {
                    assert_eq!(payload["records"][0]["request_id"], "call-fixture");
                } else {
                    assert_eq!(payload["groups"][0]["owner"], "fixture");
                    assert_eq!(payload["first_record_at"].as_f64(), Some(2.0));
                }
                let state = state.clone();
                tokio::task::spawn_blocking(move || {
                    wait_for(&Deadline::after(wire_timeout()), || {
                        state.lifecycle.admission.active_requests() == 0
                    })
                })
                .await
                .unwrap();
            }
        }
    });
}

#[test]
fn report_disconnect_cancels_query_and_releases_admission() {
    Fixture::new().run_wire(async |fixture, address| {
        for path in [CALLS, USAGE] {
            let wire = request(
                fixture.port(),
                "GET",
                path,
                &[&fixture.session(), ACCEPT],
                b"",
            );
            {
                let endpoint = address;
                let (reached, release) = report_gate(fixture);
                let mut stream = Stream::open(endpoint, &wire, "no-cache").await;
                let first = stream.next(wire_timeout().duration()).await.unwrap();
                assert_eq!(
                    event(&first),
                    ("progress", json!({"completed":0,"total":0}))
                );
                let cancelled = timeout(wire_timeout().duration(), reached)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(!cancelled.load(Ordering::Acquire));
                assert_eq!(
                    fixture.server.state.lifecycle.admission.active_requests(),
                    1
                );
                stream.stop_at_chunk_boundary().await;
                stream.io.get_ref().set_zero_linger().unwrap();
                drop(stream);

                let during_query = cancelled.clone();
                tokio::task::spawn_blocking(move || {
                    wait_for(&Deadline::after(wire_timeout()), || {
                        during_query.load(Ordering::Acquire)
                    })
                })
                .await
                .unwrap();
                release.send(()).unwrap();
                let state = fixture.server.state.clone();
                tokio::task::spawn_blocking(move || {
                    wait_for(&Deadline::after(wire_timeout()), || {
                        Arc::strong_count(&cancelled) == 1
                            && state.lifecycle.admission.active_requests() == 0
                    });
                    assert!(
                        state
                            .lifecycle
                            .admission
                            .quiesce(0, Duration::ZERO)
                            .is_some()
                    );
                })
                .await
                .unwrap();
            }
        }
    });
}

#[test]
fn management_stream_access_and_selection_match_during_update_drain() {
    Fixture::new().run_wire(async |fixture, address| {
        assert!(
            fixture
                .server
                .state
                .lifecycle
                .updates()
                .drain_for_test(wire_timeout())
                .await
        );
        let session = fixture.session();
        for path in [ACCOUNT_EVENTS, CALLS, USAGE] {
            for (headers, status) in [
                (vec![ACCEPT], 401),
                (vec![ACCEPT, &session, &session], 401),
                (vec![ACCEPT, &session, "Origin: https://other.invalid"], 403),
            ] {
                assert_entry_error(
                    address,
                    &request(fixture.port(), "GET", path, &headers, b""),
                    status,
                )
                .await;
            }
        }
        for (path, status, message) in [
            (
                "/api/calls?start=x",
                "400 Bad Request",
                "Invalid call filter",
            ),
            (
                "/api/usage?lifetime=true",
                "400 Bad Request",
                "Invalid usage period or category",
            ),
            (
                "/api/usage?lifetime=true&account_id=unknown",
                "404 Not Found",
                "Account usage is unavailable",
            ),
        ] {
            let wire = request(fixture.port(), "GET", path, &[&session, ACCEPT], b"");
            let reply = exchange_wire(address, &wire, false).await;
            wire_contract::assert_json_reply(
                path,
                &reply,
                status,
                &json!({"error":{"message":message,"origin":"emp"}}),
            );
        }
        for (path, headers) in [
            (CALLS, vec!["Accept: application/json", ACCEPT]),
            (CALLS, vec!["Accept: Text/Event-Stream"]),
            (USAGE, vec!["Accept: text/event-stream;q=1"]),
            (
                "/api/usage?lifetime=true&lifetime=false&owner=fixture",
                vec![ACCEPT],
            ),
        ] {
            let mut headers = headers;
            headers.push(&session);
            let wire = request(fixture.port(), "GET", path, &headers, b"");
            let reply = exchange_wire(address, &wire, false).await;
            let body = wire_contract::assert_json_read(path, &reply, None);
            if path == CALLS {
                assert_eq!(body["records"], json!([]));
            } else {
                assert_eq!(body["groups"], json!([]));
            }
        }
        let wire = request(
            fixture.port(),
            "GET",
            CALLS,
            &[&session, ACCEPT, "Accept: application/json"],
            b"",
        );
        assert_sse_wire(address, &wire).await;

        let wire = request(
            fixture.port(),
            "GET",
            ACCOUNT_EVENTS,
            &[&session, "Accept: application/json"],
            b"",
        );
        let mut stream = Stream::open(address, &wire, "no-store").await;
        account_snapshot(&mut stream).await;
        let state = &fixture.server.state;
        state.sessions.rotate(system_now()).unwrap();
        state.backend.management_events.publish(Change::Quota);
        assert!(stream.next(wire_timeout().duration()).await.is_none());
    });
}

#[test]
fn report_query_failures_send_error_terminator() {
    let fixture = Fixture::new();
    let path = fixture.directory.path().join("state/usage.sqlite3");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "invalid SQLite database").unwrap();
    fixture.run_wire(async |fixture, address| {
        for (path, message) in [
            (CALLS, "Call history is unavailable"),
            (USAGE, "Usage history is unavailable"),
        ] {
            let wire = request(
                fixture.port(),
                "GET",
                path,
                &[&fixture.session(), ACCEPT],
                b"",
            );

            {
                let endpoint = address;
                let mut stream = Stream::open(endpoint, &wire, "no-cache").await;
                let error = stream.next(wire_timeout().duration()).await.unwrap();
                assert_eq!(
                    event(&error),
                    ("error", json!({"error":{"message":message}}))
                );
                assert!(stream.next(wire_timeout().duration()).await.is_none());
            }
        }
    });
}
