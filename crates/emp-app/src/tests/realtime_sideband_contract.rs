use super::realtime_contract::{RealtimeUpstream, UpstreamResponse, app_server_for};
use super::*;
use crate::api::realtime::sideband::SIDEBAND_EVENT_CAPACITY;
use crate::api::realtime::sideband::{MAX_REALTIME_SIDEBAND_MESSAGE_BYTES, prepare_sideband};
use emp_transport::websocket_accept;
use std::net::TcpListener;

fn server_fixture(with_native_auth: bool) -> (TempDir, TempDir, RealtimeUpstream, ServerHandle) {
    let fixture = emp_test_support::fixture_dir().expect("fixture directory");
    let native_auth = canonical_root(&fixture).join("codex/auth.json");
    std::fs::create_dir_all(native_auth.parent().unwrap()).expect("native auth directory");
    if with_native_auth {
        std::fs::write(
            &native_auth,
            br#"{"tokens":{"access_token":"native-secret","account_id":"acct-native"}}"#,
        )
        .expect("write native auth");
    }
    let upstream = RealtimeUpstream::start(UpstreamResponse {
        status: 404,
        reason: "Not Found",
        content_type: "application/json",
        location: None,
        body: b"",
    });
    let (app_directory, server) = app_server_for(&upstream, &native_auth);
    (fixture, app_directory, upstream, server)
}

fn upgrade_headers(cookie: &str) -> Vec<String> {
    vec![
        cookie.to_owned(),
        "Upgrade: websocket".to_owned(),
        "Connection: keep-alive, UpGrAdE".to_owned(),
        "Sec-WebSocket-Version: 13".to_owned(),
        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==".to_owned(),
    ]
}

fn request_with_owned_headers(server: &ServerHandle, path: &str, headers: &[String]) -> String {
    let headers = headers.iter().map(String::as_str).collect::<Vec<_>>();
    request(server, path, &headers)
}

fn status_and_body(wire: &str) -> (u16, Value) {
    let separator = wire.find("\r\n\r\n").expect("HTTP separator");
    let status = wire[..separator]
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse::<u16>().ok())
        .expect("HTTP status");
    let body = serde_json::from_str(&wire[separator + 4..]).expect("JSON error body");
    (status, body)
}

#[test]
fn sideband_rejects_caller_before_call_id_or_upstream() {
    let (_fixture, _app_directory, upstream, server) = server_fixture(true);
    let headers = vec![
        "Authorization: Bearer invalid".to_owned(),
        "Upgrade: websocket".to_owned(),
        "Connection: Upgrade".to_owned(),
        "Sec-WebSocket-Version: 13".to_owned(),
        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==".to_owned(),
    ];
    let (status, payload) = status_and_body(&request_with_owned_headers(
        &server,
        "/v1/live/not-a-call-id",
        &headers,
    ));
    assert_eq!(status, 401);
    assert_eq!(payload["error"]["code"], "realtime_caller_unauthorized");
    server.shutdown().expect("shutdown server");
    upstream.assert_no_more_requests();
}

#[test]
fn sideband_validates_call_id_and_upgrade_before_admission() {
    let (_fixture, _app_directory, upstream, server) = server_fixture(true);
    let cookie = session_header(&server);
    let headers = upgrade_headers(&cookie);
    let (status, payload) = status_and_body(&request_with_owned_headers(
        &server,
        "/v1/live/not-a-call-id",
        &headers,
    ));
    assert_eq!(status, 400);
    assert_eq!(payload["error"]["code"], "realtime_invalid_call_id");
    assert_eq!(server.state.lifecycle.admission.active_websockets(), 0);

    let headers = vec![cookie.clone()];
    let (status, payload) = status_and_body(&request_with_owned_headers(
        &server,
        "/v1/live/rtc_valid_call",
        &headers,
    ));
    assert_eq!(status, 400);
    assert_eq!(payload["error"]["message"], "invalid websocket upgrade");
    assert_eq!(server.state.lifecycle.admission.active_websockets(), 0);

    let mut headers = upgrade_headers(&cookie);
    headers.retain(|header| !header.starts_with("Sec-WebSocket-Key:"));
    headers.push("Sec-WebSocket-Key: not-base64".to_owned());
    let (status, payload) = status_and_body(&request_with_owned_headers(
        &server,
        "/v1/live/rtc_valid_call",
        &headers,
    ));
    assert_eq!(status, 400);
    assert_eq!(payload["error"]["message"], "invalid Sec-WebSocket-Key");
    assert_eq!(server.state.lifecycle.admission.active_websockets(), 0);
    server.shutdown().expect("shutdown server");
    upstream.assert_no_more_requests();
}

#[test]
fn sideband_capacity_is_reserved_after_upgrade_validation_before_native_connect() {
    let (_fixture, _app_directory, upstream, server) = server_fixture(false);
    let permits = (0..224)
        .map(|_| {
            server
                .state
                .lifecycle
                .admission
                .acquire_websocket()
                .expect("fill adaptive websocket pool")
        })
        .collect::<Vec<_>>();
    assert_eq!(server.state.lifecycle.admission.active_websockets(), 224);
    let cookie = session_header(&server);
    let headers = upgrade_headers(&cookie);
    let wire = request_with_owned_headers(&server, "/v1/live/rtc_capacity_test", &headers);
    let (status, payload) = status_and_body(&wire);
    assert_eq!(status, 503);
    assert_eq!(payload["error"]["code"], "realtime_capacity_unavailable");
    assert!(wire.lines().any(|line| line == "Retry-After: 2"));
    assert!(
        request(&server, "/api/request-limits", &[&session_header(&server)])
            .starts_with("HTTP/1.1 200 OK\r\n")
    );
    assert!(upstream.no_request());
    drop(permits);
    assert_eq!(server.state.lifecycle.admission.active_websockets(), 0);
    server.shutdown().expect("shutdown server");
}

#[test]
fn sideband_target_uses_native_credentials_allowlisted_headers_proxy_and_message_cap() {
    let (_fixture, _app_directory, upstream, server) = server_fixture(true);
    let cookie = session_header(&server);
    let raw = format!(
        "GET /v1/live/rtc_voice_proxy HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{}\r\nAuthorization: Bearer caller-secret\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nOpenAI-Alpha: quicksilver=v2\r\nSession-Id: session-voice\r\nX-Ignored-Secret: do-not-forward\r\n\r\n",
        server.local_addr().port(),
        cookie
    );
    let request = parse_request(&raw).expect("parse fixture upgrade");
    let prepared = prepare_sideband(
        request,
        "rtc_voice_proxy",
        &server.state,
        crate::util::system_now(),
    )
    .expect("prepare sideband connection");
    assert_eq!(prepared.url, "wss://api.openai.com/v1/live/rtc_voice_proxy");
    assert_eq!(prepared.accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    assert_eq!(
        prepared.max_message_bytes,
        MAX_REALTIME_SIDEBAND_MESSAGE_BYTES
    );
    assert_eq!(prepared.headers["Authorization"], "Bearer native-secret");
    assert_eq!(prepared.headers["chatgpt-account-id"], "acct-native");
    assert_eq!(prepared.headers["OpenAI-Alpha"], "quicksilver=v2");
    assert_eq!(prepared.headers["session-id"], "session-voice");
    assert!(!prepared.headers.contains_key("X-Ignored-Secret"));
    assert!(
        !prepared
            .headers
            .values()
            .any(|value| value == "Bearer caller-secret")
    );
    // The proxy comes only from HttpClient's selected route; target construction
    // does not log or otherwise expose it.
    assert_eq!(
        prepared.proxy,
        server
            .state
            .backend
            .transport
            .client
            .websocket_proxy_for(&prepared.url)
            .unwrap()
    );
    assert_eq!(server.state.lifecycle.admission.active_websockets(), 1);
    drop(prepared);
    assert_eq!(server.state.lifecycle.admission.active_websockets(), 0);
    server.shutdown().expect("shutdown server");
    upstream.assert_no_more_requests();
}

#[test]
fn sideband_without_native_subscription_is_rejected_after_caller_auth() {
    let (_fixture, _app_directory, upstream, server) = server_fixture(false);
    let cookie = session_header(&server);
    let headers = upgrade_headers(&cookie);
    let (status, payload) = status_and_body(&request_with_owned_headers(
        &server,
        "/v1/live/rtc_missing_native",
        &headers,
    ));
    assert_eq!(status, 401);
    assert_eq!(payload["error"]["code"], "native_subscription_unavailable");
    assert_eq!(server.state.lifecycle.admission.active_websockets(), 0);
    server.shutdown().expect("shutdown server");
    upstream.assert_no_more_requests();
}

#[test]
fn sideband_relays_raw_frames_ping_close_and_coalesced_first_frame() {
    let (_fixture, app_directory, idle_upstream, server) = server_fixture(true);
    let fake = FakeSideband::start();
    let address = server.local_addr();
    let cookie = session_header(&server);
    set_sideband_upstream(&server, &fake.url);

    let first_client = masked_frame(1, br#"{"type":"client.first"}"#);
    let request = format!(
        "GET /v1/live/rtc_relay_contract HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{cookie}\r\nAuthorization: Bearer caller-secret\r\nSession-Id: session-voice\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        server.local_addr().port()
    );
    let downstream = TcpStream::connect(address).expect("connect downstream fixture");
    let mut one_write = request.into_bytes();
    one_write.extend_from_slice(&first_client);
    let deadline = Deadline::after(test_timeout(Duration::from_secs(2)));
    let handshake = fake
        .observation
        .begin(deadline, "fake sideband handshake and queued events");
    let command = fake.observation.begin(
        deadline,
        "downstream command is serviced while upstream events are queued",
    );
    let mut downstream = deadline.io(downstream);
    downstream
        .write_all(&one_write)
        .expect("send handshake+first frame");
    let response_head = read_http_head(&mut downstream);
    assert!(response_head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));

    for index in 0..SIDEBAND_EVENT_CAPACITY {
        let (opcode, payload) = read_ws_frame(&mut downstream);
        assert_eq!(opcode, 1);
        if index == 0 {
            assert_eq!(payload, br#"{ "type" : "session.started", "x" : 1 }"#);
        } else {
            assert_eq!(
                payload,
                format!(
                    "{{ \"type\" : \"session.progress\", \"index\" : {} }}",
                    index - 1
                )
                .as_bytes()
            );
        }
    }
    let SidebandObservation::FirstClient(first) = command
        .wait_for(test_timeout(Duration::from_millis(250)))
        .unwrap()
    else {
        panic!("expected first sideband client message");
    };
    assert_eq!(first, br#"{"type":"client.first"}"#);
    assert!(matches!(
        handshake.wait().unwrap(),
        SidebandObservation::Handshake
    ));
    for index in SIDEBAND_EVENT_CAPACITY - 1..11 {
        let (opcode, payload) = read_ws_frame(&mut downstream);
        assert_eq!(opcode, 1);
        assert_eq!(
            payload,
            format!("{{ \"type\" : \"session.progress\", \"index\" : {index} }}").as_bytes()
        );
    }
    let client_ping = masked_frame(9, b"down-ping");
    downstream.set_deadline(&Deadline::after(test_timeout(Duration::from_secs(2))));
    downstream
        .write_all(&client_ping)
        .expect("send client ping");
    let (opcode, payload) = read_ws_frame(&mut downstream);
    assert_eq!(opcode, 10);
    assert_eq!(payload, b"down-ping");

    let second_client = masked_frame(1, br#"{"type":"client.second"}"#);
    let deadline = Deadline::after(test_timeout(Duration::from_secs(2)));
    let closed = fake
        .observation
        .begin(deadline, "fake sideband observed complete relay");
    downstream.set_deadline(&deadline);
    downstream
        .write_all(&second_client)
        .expect("send second client event");
    let (opcode, payload) = read_ws_frame(&mut downstream);
    assert_eq!(opcode, 1);
    assert_eq!(payload, br#"{ "type" : "session.updated", "x" : 2 }"#);
    let (opcode, payload) = read_ws_frame(&mut downstream);
    assert_eq!(opcode, 8);
    assert_eq!(payload, [3, 232]);

    let SidebandObservation::Closed(observed) = closed.wait().unwrap() else {
        panic!("expected completed sideband relay");
    };
    assert_eq!(
        observed.client_texts,
        [
            br#"{"type":"client.first"}"#.to_vec(),
            br#"{"type":"client.second"}"#.to_vec(),
        ]
    );
    assert_eq!(observed.upstream_pong, b"up-ping");
    assert_eq!(observed.path, "/v1/live/rtc_relay_contract");
    assert_eq!(observed.headers["authorization"], "Bearer native-secret");
    assert_eq!(observed.headers["session-id"], "session-voice");
    let deadline = Deadline::after(test_timeout(Duration::from_secs(1)));
    while server.state.lifecycle.admission.active_websockets() != 0 && !deadline.is_expired() {
        #[expect(
            clippy::disallowed_methods,
            reason = "WebSocket 准入释放没有通知接口，以固定间隔轮询"
        )]
        thread::sleep(Duration::from_millis(5));
    }
    deadline
        .accept((), "sideband websocket slot release")
        .unwrap();
    assert_eq!(server.state.lifecycle.admission.active_websockets(), 0);
    server.shutdown().expect("shutdown server");
    idle_upstream.assert_no_more_requests();
    let records = super::internal_events_contract::final_journal(app_directory.path());
    let admitted: Vec<_> = records
        .iter()
        .filter(|record| record["event"] == "command_admitted")
        .collect();
    assert_eq!(admitted.len(), 2);
    for command in admitted {
        assert!(
            command["fields"]["connection_id"]
                .as_str()
                .is_some_and(|id| id.len() == 16)
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record["event"] == "command_confirmed"
                    && record["fields"]["connection_id"] == command["fields"]["connection_id"]
                    && record["fields"]["command_id"] == command["fields"]["command_id"])
                .count(),
            1
        );
    }
    assert!(
        !serde_json::to_string(&records)
            .unwrap()
            .contains("native-secret")
    );
}

#[test]
fn sideband_upstream_handshake_errors_return_http_before_downstream_101() {
    for (upstream_status, expected_code) in [
        (401, "native_subscription_auth_failed"),
        (404, "native_realtime_unsupported"),
        (502, "native_realtime_transport_error"),
    ] {
        let (_fixture, _app_directory, idle_upstream, server) = server_fixture(true);
        let (fake_url, fake_worker) = rejecting_sideband(upstream_status);
        let response = direct_sideband_request(&server, "rtc_rejected_handshake", &fake_url);
        let (status, payload) = status_and_body(&response);
        assert_eq!(status, upstream_status);
        assert_eq!(payload["error"]["code"], expected_code);
        assert!(!response.starts_with("HTTP/1.1 101"));
        fake_worker.join().expect("join rejected fake upstream");
        assert_eq!(server.state.lifecycle.admission.active_websockets(), 0);
        server.shutdown().expect("shutdown fixture server");
        idle_upstream.assert_no_more_requests();
    }
}

#[test]
fn sideband_connection_refused_maps_to_502_and_releases_admission() {
    let (_fixture, _app_directory, idle_upstream, server) = server_fixture(true);
    let refused = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("reserve refused port");
    let url = format!(
        "ws://{}/v1/live/rtc_connection_refused",
        refused.local_addr().unwrap()
    );
    drop(refused);

    let response = direct_sideband_request(&server, "rtc_connection_refused", &url);
    let (status, payload) = status_and_body(&response);
    assert_eq!(status, 502);
    assert_eq!(payload["error"]["code"], "native_realtime_transport_error");
    assert!(!response.starts_with("HTTP/1.1 101"));
    assert_eq!(server.state.lifecycle.admission.active_websockets(), 0);
    server.shutdown().expect("shutdown fixture server");
    idle_upstream.assert_no_more_requests();
}

struct FakeSideband {
    url: String,
    observation: Observation<SidebandObservation>,
}

enum SidebandObservation {
    Handshake,
    FirstClient(Vec<u8>),
    Closed(FakeSidebandObserved),
}

struct FakeSidebandObserved {
    path: String,
    headers: BTreeMap<String, String>,
    client_texts: Vec<Vec<u8>>,
    upstream_pong: Vec<u8>,
}

impl FakeSideband {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fake sideband");
        let url = format!(
            "ws://{}/v1/live/rtc_relay_contract",
            listener.local_addr().unwrap()
        );
        listener.set_nonblocking(true).unwrap();
        let observation = Observation::spawn(move |probe| {
            let Some(handshake) = probe.next_request() else {
                return;
            };
            let socket = accept_before(&listener, handshake.deadline());
            let mut stream = handshake.deadline().io(socket);
            let request_head = read_http_head(&mut stream);
            let request = parse_request(&request_head).expect("parse upstream handshake");
            let path = request.target.to_owned();
            let headers = request
                .headers
                .lines()
                .skip(1)
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
                .collect::<BTreeMap<_, _>>();
            let accept = websocket_accept(&headers["sec-websocket-key"]).unwrap();
            write!(stream, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
            stream.flush().unwrap();
            write_ws_frame(
                &mut stream,
                1,
                br#"{ "type" : "session.started", "x" : 1 }"#,
            );
            for index in 0..11 {
                let event = format!("{{ \"type\" : \"session.progress\", \"index\" : {index} }}");
                write_ws_frame(&mut stream, 1, event.as_bytes());
            }
            handshake.complete(SidebandObservation::Handshake);
            let Some(first_request) = probe.next_request() else {
                return;
            };
            stream.set_deadline(first_request.deadline());
            let (opcode, first) = read_ws_frame(&mut stream);
            assert_eq!(opcode, 1);
            first_request.complete(SidebandObservation::FirstClient(first.clone()));

            let ping = Deadline::after(test_timeout(Duration::from_secs(2)));
            stream.set_deadline(&ping);
            write_ws_frame(&mut stream, 9, b"up-ping");
            let (opcode, pong) = read_ws_frame(&mut stream);
            assert_eq!(opcode, 10);

            let Some(second_request) = probe.next_request() else {
                return;
            };
            stream.set_deadline(second_request.deadline());
            let (opcode, second) = read_ws_frame(&mut stream);
            assert_eq!(opcode, 1);
            write_ws_frame(
                &mut stream,
                1,
                br#"{ "type" : "session.updated", "x" : 2 }"#,
            );
            write_ws_frame(&mut stream, 8, &[3, 232]);
            let (opcode, close) = read_ws_frame(&mut stream);
            assert_eq!(opcode, 8);
            assert_eq!(close, [3, 232]);
            second_request.complete(SidebandObservation::Closed(FakeSidebandObserved {
                path,
                headers,
                client_texts: vec![first, second],
                upstream_pong: pong,
            }));
        });
        Self { url, observation }
    }
}

fn rejecting_sideband(status: u16) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind rejecting sideband");
    let url = format!(
        "ws://{}/v1/live/rtc_rejected_handshake",
        listener.local_addr().unwrap()
    );
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept rejected sideband");
        let _request = read_request_head(&mut stream).expect("read rejected handshake");
        let reason = match status {
            401 => "Unauthorized",
            404 => "Not Found",
            _ => "Bad Gateway",
        };
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.flush().unwrap();
    });
    (url, worker)
}

fn set_sideband_upstream(server: &ServerHandle, url: &str) {
    let address = url
        .strip_prefix("ws://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    server.state.test_sideband_address.set(address).unwrap();
}

fn direct_sideband_request(server: &ServerHandle, call_id: &str, fake_url: &str) -> String {
    let address = server.local_addr();
    set_sideband_upstream(server, fake_url);
    let cookie = session_header(server);
    let raw = format!(
        "GET /v1/live/{call_id} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{cookie}\r\nAuthorization: Bearer caller-secret\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        server.local_addr().port()
    );
    let mut client = TcpStream::connect(address).expect("connect sideband request");
    client
        .write_all(raw.as_bytes())
        .expect("write sideband request");
    response_until_close(&mut client)
}

fn read_http_head(stream: &mut DeadlineIo<TcpStream>) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).expect("read HTTP head");
        bytes.push(byte[0]);
        assert!(bytes.len() < 16 * 1024, "HTTP head bounded");
    }
    stream
        .deadline()
        .accept(String::from_utf8(bytes).unwrap(), "sideband HTTP handshake")
        .unwrap()
}

fn masked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [0x19, 0x27, 0x35, 0x43];
    let mut frame = vec![0x80 | opcode, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % mask.len()]),
    );
    frame
}

fn write_ws_frame(stream: &mut impl Write, opcode: u8, payload: &[u8]) {
    let mut frame = vec![0x80 | opcode];
    if payload.len() < 126 {
        frame.push(payload.len() as u8);
    } else {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    stream.write_all(&frame).unwrap();
    stream.flush().unwrap();
}

fn read_ws_frame(stream: &mut DeadlineIo<TcpStream>) -> (u8, Vec<u8>) {
    let mut header = [0_u8; 2];
    stream
        .read_exact(&mut header)
        .expect("read websocket header");
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    let mut length = usize::from(header[1] & 0x7f);
    if length == 126 {
        let mut extended = [0_u8; 2];
        stream.read_exact(&mut extended).unwrap();
        length = usize::from(u16::from_be_bytes(extended));
    } else if length == 127 {
        let mut extended = [0_u8; 8];
        stream.read_exact(&mut extended).unwrap();
        length = usize::try_from(u64::from_be_bytes(extended)).unwrap();
    }
    let mask = if masked {
        let mut mask = [0_u8; 4];
        stream.read_exact(&mut mask).unwrap();
        Some(mask)
    } else {
        None
    };
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).unwrap();
    if let Some(mask) = mask {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % mask.len()];
        }
    }
    stream
        .deadline()
        .accept((opcode, payload), "complete sideband WebSocket frame")
        .unwrap()
}
