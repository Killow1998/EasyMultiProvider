use super::*;
#[cfg(unix)]
use crate::server::raw_tcp::RawClient;

macro_rules! json_head {
    ($status:literal, $length:literal $(, $extra:literal)?) => {
        concat!(
            "HTTP/1.1 ", $status, "\r\n",
            "Content-Type: application/json\r\n",
            "Content-Length: ", $length, "\r\n",
            "Cache-Control: no-store\r\n",
            "X-Frame-Options: DENY\r\n",
            "Content-Security-Policy: frame-ancestors 'none'\r\n",
            "X-Content-Type-Options: nosniff\r\n",
            $($extra, "\r\n",)?
            "Connection: close\r\n\r\n",
        )
    };
}

pub(crate) use json_head;

macro_rules! asset_head {
    ($content_type:literal, $length:expr) => {
        format!(
            concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Type: ",
                $content_type,
                "\r\n",
                "Content-Length: {}\r\n",
                "X-Frame-Options: DENY\r\n",
                "Content-Security-Policy: frame-ancestors 'none'\r\n",
                "X-Content-Type-Options: nosniff\r\n",
                "Cache-Control: no-store\r\n",
                "Connection: close\r\n\r\n",
            ),
            $length
        )
    };
}

pub(crate) use asset_head;

const INDEX: &str = concat!(
    "HTTP/1.1 200 OK\r\n",
    "Content-Type: text/html; charset=utf-8\r\n",
    "Content-Length: {}\r\n",
    "X-Frame-Options: DENY\r\n",
    "Content-Security-Policy: frame-ancestors 'none'\r\n",
    "X-Content-Type-Options: nosniff\r\n",
    "Cache-Control: no-store\r\n",
    "Referrer-Policy: no-referrer\r\n",
    "Set-Cookie: emp_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0\r\n",
    "Connection: close\r\n\r\n",
);
const NOT_FOUND: &str = json_head!("404 Not Found", "33");
const MISSING: &[u8] = br#"{"error":{"message":"not found"}}"#;
const SESSION_REQUIRED: &[u8] = br#"{"error":{"message":"management session is required"}}"#;

pub(crate) use crate::lifecycle::TestDriver as Driver;

impl Driver {
    pub(crate) const ALL: [Self; 2] = [Self::Legacy, Self::Candidate];
}

fn assert_head(driver: Driver, name: &str, head: &str, expected: &str) {
    assert_eq!(head, expected, "{driver:?} {name}");
}

pub(crate) fn assert_reply(
    driver: Driver,
    name: &str,
    reply: &[u8],
    expected_head: &str,
    expected_body: &[u8],
) {
    let split = reply
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("{driver:?} {name}: missing complete response head"))
        + 4;
    assert_head(
        driver,
        name,
        std::str::from_utf8(&reply[..split]).unwrap(),
        expected_head,
    );
    assert!(
        reply[split..] == *expected_body,
        "{driver:?} {name}: response body differs"
    );
}

#[test]
fn fixture_defaults_stay_under_config_root() {
    const UNUSED_HOME: &str = "EMP_FIXTURE_UNUSED_HOME";
    if let Some(unused_home) = std::env::var_os(UNUSED_HOME) {
        for driver in Driver::ALL {
            Fixture::with_driver(tempfile::tempdir().unwrap(), driver).run_wire(
                async |fixture, address| {
                    let root = emp_state::config::resolve_user_path(fixture.directory.path());
                    let backend = &fixture.legacy.state.backend;
                    let auth = root.join("codex/auth.json");
                    assert_eq!(backend.accounts.native_auth_path, auth);
                    assert_eq!(
                        backend.integration.manager.config_path(),
                        root.join("codex/config.toml")
                    );
                    assert_eq!(backend.configuration.config_path, root.join("config.json"));
                    assert!(root.join("state/service.lock").is_file());
                    let reply = assert_wire(
                        address,
                        &request(
                            address.port(),
                            "GET",
                            "/api/config",
                            &[&fixture.session()],
                            b"",
                        ),
                        false,
                        200,
                    )
                    .await;
                    assert_eq!(
                        json_body(&reply)["native_catalog_path"],
                        json!(root.join("codex/models_cache.json"))
                    );
                },
            );
        }
        let unused_home = std::path::Path::new(&unused_home);
        assert!(
            !unused_home.exists(),
            "unexpected entries in unused home: {:?}",
            std::fs::read_dir(unused_home)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>()
        );
        return;
    }

    let parent = tempfile::tempdir().unwrap();
    let home = parent.path().join("unused");
    let (_, module) = module_path!().split_once("::").unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        &format!("{module}::fixture_defaults_stay_under_config_root"),
    ]);
    for variable in [
        UNUSED_HOME,
        "HOME",
        "USERPROFILE",
        "CODEX_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        command.env(variable, &home);
    }
    command.env("XDG_CACHE_HOME", parent.path().join("cache"));
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn pages_and_embedded_assets() {
    for driver in Driver::ALL {
        Fixture::with_driver(tempfile::tempdir().unwrap(), driver).run_wire(
            async |fixture, address| {
                let port = fixture.port();
                let index = include_bytes!("../../../../web/index.html").as_slice();
                let script = include_bytes!("../../../../web/i18n.js").as_slice();
                let css = include_bytes!("../../../../web/guide.css").as_slice();
                let icon = include_bytes!(
                    "../../../../../../assets/branding/native/easy-multi-provider-tray.svg"
                )
                .as_slice();
                let index_head = INDEX.replace("{}", &index.len().to_string());
                for (method, path, head, body) in [
                    ("GET", "/", index_head.as_str(), index),
                    ("GET", "/index.html", index_head.as_str(), index),
                    (
                        "GET",
                        "/assets/i18n.js",
                        &asset_head!("text/javascript; charset=utf-8", script.len()),
                        script,
                    ),
                    (
                        "GET",
                        "/assets/guide.css",
                        &asset_head!("text/css; charset=utf-8", css.len()),
                        css,
                    ),
                    (
                        "GET",
                        "/assets/emp-icon-mono.svg",
                        &asset_head!("image/svg+xml", icon.len()),
                        icon,
                    ),
                    (
                        "GET",
                        "/healthz",
                        json_head!("200 OK", "15"),
                        br#"{"status":"ok"}"#.as_slice(),
                    ),
                    ("HEAD", "/", index_head.as_str(), index),
                    ("HEAD", "/assets/i18n.js", NOT_FOUND, MISSING),
                ] {
                    let wire = request(port, method, path, &[], b"");
                    let reply = exchange_wire(address, &wire, false).await;
                    assert_reply(driver, &format!("{method} {path}"), &reply, head, body);
                }
            },
        );
    }
}

#[test]
fn management_json_and_error_responses() {
    const ACCOUNTS: &[u8] = br#"{"accounts":[],"claude_quota":null,"memory_routing_status":null,"native_account":{"auth_mode":"missing","credential_set":false,"hidden_models":[],"id":"@native","model_context_windows":{},"name":"Current Codex login","native":true,"prefix":"","quota":null},"provider_quotas":{},"refresh_errors":{}}"#;
    const TOO_LARGE: &[u8] = br#"{"error":{"code":"request_too_large","limit_bytes":5242880,"message":"request body is too large (EMP limit: 5242880 bytes)","reason":"hard_limit"}}"#;
    for driver in Driver::ALL {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.json");
        std::fs::write(&config, br#"{"language":"en"}"#).unwrap();
        Fixture::with_driver(directory, driver).run_wire(async |fixture, address| {
            let port = fixture.port();
            let session = fixture.session();
            let origin = format!("Origin: http://{address}");
            let locale = br#"{"language":"zh-CN"}"#;
            let auth = [session.as_str(), origin.as_str()];
            for (name, wire, head, body) in [
                ("management read", request(port, "GET", "/api/accounts", &auth, b""), json_head!("200 OK", "298"), ACCOUNTS),
                ("management write", json_request(port, "/api/language", &auth, locale), json_head!("200 OK", "20"), locale.as_slice()),
                ("unauthorized", request(port, "GET", "/api/accounts", &[], b""), json_head!("401 Unauthorized", "54"), SESSION_REQUIRED),
                ("cross origin", request(port, "GET", "/api/accounts", &[&session, "Origin: https://other.invalid"], b""), json_head!("403 Forbidden", "54"), SESSION_REQUIRED),
                ("unmatched", request(port, "GET", "/not-present", &[], b""), NOT_FOUND, MISSING),
                ("write fragment", json_request(port, "/api/language#x", &auth, br#"{"language":"en"}"#), NOT_FOUND, MISSING),
                ("wrong method", request(port, "GET", "/api/language", &auth, b""), NOT_FOUND, MISSING),
                ("conflict", json_request(port, "/api/updates/install", &auth, b"{}"), json_head!("409 Error", "85"), br#"{"error":{"code":"update_unavailable","message":"update_unavailable","origin":"emp"}}"#.as_slice()),
                ("too large", request(port, "POST", "/api/accounts/import", &[&session, "Content-Type: application/json", "Content-Length: 5242881"], b""), json_head!("413 Content Too Large", "147"), TOO_LARGE),
                ("invalid request line", b"GET /healthz HTTP/1.x\r\n\r\n".to_vec(), json_head!("400 Bad Request", "44"), br#"{"error":{"message":"invalid HTTP request"}}"#.as_slice()),
            ] {
                let reply = exchange_wire(address, &wire, false).await;
                assert_reply(driver, name, &reply, head, body);
            }
            let config = std::fs::read(config).unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&config).unwrap()["language"], "zh-CN");
        });
    }
}

#[cfg(unix)]
#[test]
fn management_sse_and_update_drain() {
    // H05.1 accepts Hyper's header order and chunked framing for management streams.
    const CANDIDATE: &str = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: text/event-stream\r\n",
        "Connection: close\r\n",
        "X-Frame-Options: DENY\r\n",
        "Content-Security-Policy: frame-ancestors 'none'\r\n",
        "X-Content-Type-Options: nosniff\r\n",
        "Cache-Control: no-store\r\n",
        "X-Accel-Buffering: no\r\n",
        "Transfer-Encoding: chunked\r\n\r\n",
    );
    const LEGACY: &str = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: text/event-stream\r\n",
        "Cache-Control: no-store\r\n",
        "X-Accel-Buffering: no\r\n",
        "X-Frame-Options: DENY\r\n",
        "Content-Security-Policy: frame-ancestors 'none'\r\n",
        "X-Content-Type-Options: nosniff\r\n",
        "Connection: close\r\n\r\n",
    );
    for driver in Driver::ALL {
        Fixture::with_driver(tempfile::tempdir().unwrap(), driver).run_wire(async |fixture, address| {
            let port = fixture.port();
            let session = fixture.session();
            let stream = RawClient::request(
                address,
                &request(port, "GET", "/api/accounts/events", &[&session], b""),
            ).await;
            assert_head(driver, "management SSE", &stream.head, match driver {
                Driver::Candidate => CANDIDATE,
                Driver::Legacy => LEGACY,
            });
            drop(stream);

            assert!(fixture.legacy.state.lifecycle.updates().drain_for_test(Duration::ZERO));
            let wire = json_request(port, "/api/language", &[&session], br#"{"language":"en"}"#);
            let reply = exchange_wire(address, &wire, false).await;
            assert_reply(
                driver, "update drain", &reply,
                json_head!("503 Service Unavailable", "107", "Retry-After: 2"),
                br#"{"error":{"code":"updating","message":"EMP is installing an update. Please retry shortly.","origin":"emp"}}"#,
            );
        });
    }
}

#[cfg(unix)]
#[test]
fn inference_responses() {
    use super::inference::{SseClient, fixture_with_driver};
    use crate::tests::OneShotUpstream;

    // H05.0 accepts Hyper's header order for proxy replies and chunked framing for SSE.
    macro_rules! candidate_json {
        ($status:literal, $length:literal) => {
            concat!(
                "HTTP/1.1 ",
                $status,
                "\r\n",
                "Content-Type: application/json\r\n",
                "Connection: close\r\n",
                "X-Frame-Options: DENY\r\n",
                "Content-Security-Policy: frame-ancestors 'none'\r\n",
                "X-Content-Type-Options: nosniff\r\n",
                "Content-Length: ",
                $length,
                "\r\n",
                "Cache-Control: no-store\r\n",
                "Retry-After: 7\r\n\r\n",
            )
        };
    }
    const SSE_CANDIDATE: &str = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: text/event-stream\r\n",
        "Connection: close\r\n",
        "X-Frame-Options: DENY\r\n",
        "Content-Security-Policy: frame-ancestors 'none'\r\n",
        "X-Content-Type-Options: nosniff\r\n",
        "Cache-Control: no-cache\r\n",
        "X-Models-Etag: \"emp-c06f76a4a5f2e0f1ba43a5d9b87dc25a8bd5544f424041a6df950a3402b776d5\"\r\n",
        "Openai-Model: native/alias\r\n",
        "X-Codex-Turn-State: stream-turn\r\n",
        "Transfer-Encoding: chunked\r\n\r\n",
    );
    const SSE_LEGACY: &str = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: text/event-stream\r\n",
        "Cache-Control: no-cache\r\n",
        "X-Frame-Options: DENY\r\n",
        "Content-Security-Policy: frame-ancestors 'none'\r\n",
        "X-Content-Type-Options: nosniff\r\n",
        "X-Models-Etag: \"emp-c06f76a4a5f2e0f1ba43a5d9b87dc25a8bd5544f424041a6df950a3402b776d5\"\r\n",
        "openai-model: native/alias\r\n",
        "x-codex-turn-state: stream-turn\r\n",
        "Connection: close\r\n\r\n",
    );

    for driver in Driver::ALL {
        for (status, raw, candidate, legacy) in [
            (
                200,
                br#"{ "data":[{"opaque":true}], "z":1.2300e+2, "a":null }"#.as_slice(),
                candidate_json!("200 OK", "53"),
                json_head!("200 OK", "53", "Retry-After: 7"),
            ),
            (
                502,
                br#"{ "error":{"message":"fixture"}, "z":2.00 }"#.as_slice(),
                candidate_json!("502 Bad Gateway", "43"),
                json_head!("502 Bad Gateway", "43", "Retry-After: 7"),
            ),
        ] {
            let upstream = OneShotUpstream::start_repeated_wire(
                status,
                "application/json",
                Some(7),
                1,
                vec![raw.to_vec()],
            );
            fixture_with_driver(&upstream.base_url(), driver).run_wire(async |fixture, address| {
                let wire = json_request(
                    fixture.port(),
                    "/v1/decisions",
                    &[&fixture.session()],
                    br#"{"model":"upstream-model","input":"fixture"}"#,
                );
                let reply = exchange_wire(address, &wire, false).await;
                assert_reply(
                    driver,
                    "POST /v1/decisions",
                    &reply,
                    match driver {
                        Driver::Candidate => candidate,
                        Driver::Legacy => legacy,
                    },
                    raw,
                );
            });
            assert_eq!(upstream.observed().0, "/v1/decisions");
        }
        let upstream = OneShotUpstream::serve(1, |socket| {
            socket.write_all(concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Type: text/event-stream\r\n",
                "OpenAI-Model: upstream\r\n",
                "X-Codex-Turn-State: stream-turn\r\n",
                "Connection: close\r\n\r\n",
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"completed\",\"output\":[],\"future\":{\"opaque\":true}}}\n\n",
            ).as_bytes()).unwrap();
        });
        fixture_with_driver(&upstream.base_url(), driver).run_wire(async |fixture, address| {
            let wire = json_request(
                fixture.port(),
                "/v1/responses",
                &[&fixture.session(), "Authorization: Bearer fixture-caller"],
                br#"{"model":"native/alias","stream":true,"input":"fixture"}"#,
            );
            let stream = RawClient::request(address, &wire).await;
            assert_head(
                driver,
                "POST /v1/responses (SSE)",
                &stream.head,
                match driver {
                    Driver::Candidate => SSE_CANDIDATE,
                    Driver::Legacy => SSE_LEGACY,
                },
            );
            let mut stream = SseClient::from_raw(stream, matches!(driver, Driver::Candidate));
            timeout(crate::server::raw_tcp::DEADLINE, async {
                assert_eq!(
                    stream.next().await,
                    Some(json!({"type":"response.output_text.delta","delta":"hello"})),
                );
                assert_eq!(
                    stream.next().await,
                    Some(json!({
                        "type":"response.completed",
                        "response":{"id":"resp_fixture","status":"completed","output":[],
                                    "future":{"opaque":true}},
                    })),
                );
                assert_eq!(stream.next().await, None);
            })
            .await
            .expect("complete Native SSE stream");
        });
        assert_eq!(upstream.observed().0, "/v1/responses");
    }
}

#[cfg(unix)]
#[test]
fn responses_and_realtime_websocket_upgrades() {
    use super::websocket::{accept_upstream, native_fixture_with_driver, upgrade};

    // H06b records Hyper's Sec-Websocket-Accept spelling on both upgrade routes.
    const CANDIDATE: &str = concat!(
        "HTTP/1.1 101 Switching Protocols\r\n",
        "Upgrade: websocket\r\n",
        "Connection: Upgrade\r\n",
        "Sec-Websocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
    );
    const LEGACY: &str = concat!(
        "HTTP/1.1 101 Switching Protocols\r\n",
        "Upgrade: websocket\r\n",
        "Connection: Upgrade\r\n",
        "Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
    );
    for driver in Driver::ALL {
        let expected = match driver {
            Driver::Candidate => CANDIDATE,
            Driver::Legacy => LEGACY,
        };
        let listener = TcpListener::bind(LOOPBACK).unwrap();
        listener.set_nonblocking(true).unwrap();
        native_fixture_with_driver(listener.local_addr().unwrap(), driver).run_wire(
            async |fixture, address| {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                fixture
                    .legacy
                    .state
                    .test_sideband_address
                    .set(listener.local_addr().unwrap())
                    .unwrap();
                let port = fixture.port();
                let session = fixture.session();
                let responses = RawClient::request(
                    address,
                    &upgrade(port, "/v1/responses", &session, &[], b""),
                )
                .await;
                assert_head(driver, "Responses WS", &responses.head, expected);
                drop(responses);

                let wire = upgrade(
                    port,
                    "/v1/live/rtc_fixture",
                    &session,
                    &[
                        "Authorization: Bearer caller-secret",
                        "Session-Id: voice-fixture",
                    ],
                    b"",
                );
                let (realtime, (_upstream, _)) = tokio::join!(
                    RawClient::request(address, &wire),
                    accept_upstream(&listener, ""),
                );
                assert_head(driver, "realtime WS", &realtime.head, expected);
            },
        );
    }
}

#[cfg(unix)]
#[test]
fn realtime_upstream_method_not_allowed() {
    use super::websocket::realtime_fixture;
    use crate::tests::realtime_contract::{RealtimeUpstream, UpstreamResponse, multipart_offer};

    for driver in Driver::ALL {
        let upstream = RealtimeUpstream::start(UpstreamResponse {
            status: 405,
            reason: "Method Not Allowed",
            content_type: "application/json",
            location: None,
            body: b"",
        });
        let (_native, fixture) = realtime_fixture(&upstream, driver);
        fixture.run_wire(async |fixture, address| {
            let body = multipart_offer();
            let wire = request(
                fixture.port(),
                "POST",
                "/v1/live",
                &[&fixture.session(), "Content-Type: multipart/form-data; boundary=VoiceFixtureBoundary", &format!("Content-Length: {}", body.len())],
                &body,
            );
            let reply = exchange_wire(address, &wire, false).await;
            assert_reply(driver, "realtime upstream 405", &reply, json_head!("405 Method Not Allowed", "121"),
                br#"{"error":{"code":"native_realtime_unsupported","message":"The native subscription backend does not support Codex Voice"}}"#);
        });
    }
}
