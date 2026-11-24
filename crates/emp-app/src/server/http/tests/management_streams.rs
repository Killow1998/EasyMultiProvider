use super::*;
use crate::{
    server::{
        http::usage::ReportGate,
        raw_tcp::{RawClient, wire_timeout},
    },
    services::management_events::Change,
    tests::wait_for,
};
use std::{collections::BTreeMap, sync::atomic::AtomicBool, time::Instant};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::oneshot,
};

const ACCOUNT_EVENTS: &str = "/api/accounts/events";
const CALLS: &str = "/api/calls?start=1&end=10";
const USAGE: &str = "/api/usage?lifetime=%74rue&owner=fixture";
const ACCEPT: &str = "Accept: text/event-stream";

struct Stream {
    io: BufReader<TcpStream>,
    headers: BTreeMap<String, String>,
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
                {
                    let mut line = String::new();
                    assert_ne!(self.io.read_line(&mut line).await.unwrap(), 0);
                    let size = usize::from_str_radix(line.trim_end(), 16).unwrap();
                    let start = self.pending.len();
                    self.pending.resize(start + size, 0);
                    self.io
                        .read_exact(&mut self.pending[start..])
                        .await
                        .unwrap();
                    assert_eq!(
                        self.io.read_u16().await.unwrap(),
                        u16::from_be_bytes(*b"\r\n")
                    );
                    if size == 0 {
                        break;
                    }
                }
            }
            assert!(self.pending.is_empty(), "partial SSE event");
            assert_eq!(
                self.io.read_u8().await.unwrap_err().kind(),
                std::io::ErrorKind::UnexpectedEof
            );
            None
        })
        .await
        .expect("SSE event or end of stream before deadline")
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
    let quota = stream.next(wire_timeout()).await.unwrap();
    let integration = stream.next(wire_timeout()).await.unwrap();
    let activity = stream.next(wire_timeout()).await.unwrap();
    let usage = stream.next(wire_timeout()).await.unwrap();
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
fn account_stream_snapshot_heartbeat_and_quit_match_while_updates_drain() {
    Fixture::new().run_wire(async |fixture, endpoint| {
        let state = &fixture.server.state;
        let session = fixture.session();
        let wire = request(fixture.port(), "GET", ACCOUNT_EVENTS, &[&session], b"");
        let mut stream = Stream::open(endpoint, &wire, "no-store").await;
        account_snapshot(&mut stream).await;
        assert_eq!(state.lifecycle.admission.active_requests(), 0);
        assert!(state.lifecycle.updates().drain_for_test(Duration::ZERO));
        state.backend.management_events.publish(Change::Integration);
        assert_eq!(
            stream.next(wire_timeout()).await.as_deref(),
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
            stream.next(wire_timeout()).await.as_deref(),
            Some("event: emp-exiting\ndata: {}\n\n")
        );
        assert!(stream.next(wire_timeout()).await.is_none());
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
            streams.push(stream);
        }
        let denied = assert_wire(address, &wire, false, 503).await;
        assert!(
            str::from_utf8(&denied)
                .unwrap()
                .contains("Retry-After: 15\r\n")
        );

        let count = state.backend.management_events.subscriber_count();
        assert_eq!(count, streams.len());
        assert_eq!(state.lifecycle.admission.active_requests(), 0);
        drop(streams.pop().unwrap());
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            wait_for(wire_timeout(), || {
                state.backend.management_events.subscriber_count() == count - 1
            });
        })
        .await
        .unwrap();

        let mut replacement = Stream::open(address, &wire, "no-store").await;
        account_snapshot(&mut replacement).await;
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
            wait_for(wire_timeout(), || {
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

fn report_gate(fixture: &Fixture) -> (oneshot::Receiver<Arc<AtomicBool>>, oneshot::Sender<()>) {
    let (first_frame, received) = oneshot::channel();
    let (release, wait) = oneshot::channel();
    *fixture
        .server
        .state
        .backend
        .usage
        .report_gate
        .lock()
        .unwrap() = Some(ReportGate {
        first_frame,
        release: wait,
    });
    (received, release)
}

fn seed_reports(fixture: &Fixture) {
    let ledger = &fixture.server.state.backend.usage.ledger;
    ledger.query(1.0, 10.0, "all", system_now(), None).unwrap();
    let database =
        rusqlite::Connection::open(fixture.directory.path().join("state/usage.sqlite3")).unwrap();
    database.execute_batch(
        "INSERT INTO call_records(request_id,operation,started_at,finished_at,provider_id,account_id,model_id,session_id,state,facts)
         VALUES('call-fixture','responses',2,3,'fixture','','model','','completed','{\"request_id\":\"call-fixture\"}');
         INSERT INTO usage_events(id,observed_at,category,owner,model,service_tier,operation,rates,origin,usage_turn,route_model,match_key)
         VALUES('usage-fixture',2,'external','fixture','model','','responses','{}','realtime','','model','');",
    ).unwrap();
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
                let first = stream.next(wire_timeout()).await.unwrap();
                assert_eq!(
                    event(&first),
                    (
                        "progress",
                        json!({"completed":0, "total":if path == CALLS { 4 } else { 1 }})
                    )
                );
                let cancelled = timeout(wire_timeout(), reached).await.unwrap().unwrap();
                assert!(!cancelled.load(Ordering::Acquire));
                assert_eq!(state.lifecycle.admission.active_requests(), 1);
                assert!(state.lifecycle.updates().drain_for_test(Duration::ZERO));
                release.send(()).unwrap();
                let mut frames = vec![first];
                while let Some(frame) = stream.next(wire_timeout()).await {
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
                    wait_for(wire_timeout(), || {
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
                let first = stream.next(wire_timeout()).await.unwrap();
                assert_eq!(
                    event(&first),
                    ("progress", json!({"completed":0,"total":0}))
                );
                let cancelled = timeout(wire_timeout(), reached).await.unwrap().unwrap();
                assert!(!cancelled.load(Ordering::Acquire));
                assert_eq!(
                    fixture.server.state.lifecycle.admission.active_requests(),
                    1
                );
                stream.io.get_ref().set_zero_linger().unwrap();
                drop(stream);

                let during_query = cancelled.clone();
                tokio::task::spawn_blocking(move || {
                    wait_for(wire_timeout(), || during_query.load(Ordering::Acquire))
                })
                .await
                .unwrap();
                release.send(()).unwrap();
                let state = fixture.server.state.clone();
                tokio::task::spawn_blocking(move || {
                    wait_for(wire_timeout(), || {
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
                .drain_for_test(Duration::ZERO)
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
            let reply = assert_wire(address, &wire, false, 200).await;
            assert!(
                str::from_utf8(&reply)
                    .unwrap()
                    .contains("Content-Type: application/json\r\n")
            );
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
        assert!(stream.next(wire_timeout()).await.is_none());
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
                let error = stream.next(wire_timeout()).await.unwrap();
                assert_eq!(
                    event(&error),
                    ("error", json!({"error":{"message":message}}))
                );
                assert!(stream.next(wire_timeout()).await.is_none());
            }
        }
    });
}
