//! Real server contract tests.
use super::*;

#[test]
fn cli_accepts_optional_config() {
    let parsed = parse_cli([
        "serve".to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        "0".to_string(),
    ])
    .expect("parse CLI");
    assert_eq!(
        parsed,
        Cli::Serve {
            config: None,
            host: Some("127.0.0.1".to_owned()),
            port: Some(0),
            open_browser: false,
        }
    );
}

#[test]
fn desktop_launch_defers_config_selection_to_shared_state_resolver() {
    assert_eq!(
        parse_cli(std::iter::empty()).expect("parse desktop launch"),
        Cli::Serve {
            config: None,
            host: None,
            port: None,
            open_browser: true,
        }
    );
}

#[test]
fn health_stays_unauthenticated() {
    let (_directory, server) = test_server();
    let response = request(&server, "/healthz", &[]);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with("{\"status\":\"ok\"}"));
    server.shutdown().expect("shutdown");
}

#[test]
fn idle_accept_worker_wakes_for_shutdown_after_serving_a_request() {
    let (_directory, server) = test_server();
    let response = request(&server, "/healthz", &[]);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));

    thread::sleep(Duration::from_millis(25));
    let started = Instant::now();
    server.shutdown().expect("idle listener shutdown");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "idle blocking accept did not wake promptly for shutdown"
    );
}

fn bootstrap_exchange(server: &ServerHandle, token: &str, extra: &[&str]) -> String {
    let mut headers = vec![format!("X-EMP-Bootstrap: {token}")];
    headers.extend(extra.iter().map(|header| (*header).to_owned()));
    let headers = headers.iter().map(String::as_str).collect::<Vec<_>>();
    post(server, "/api/session", b"", &headers)
}

fn exchanged_session(response: &str) -> String {
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let body: Value = serde_json::from_str(response.split_once("\r\n\r\n").expect("separator").1)
        .expect("session JSON");
    assert!(body["expires_in"].as_u64().is_some_and(|value| value > 0));
    body["session"].as_str().expect("session token").to_owned()
}

#[test]
fn ui_is_served_without_secrets_or_session_cookie() {
    let (_directory, server) = test_server();
    for target in ["/", "/index.html", "/?bootstrap=anything"] {
        let response = request(&server, target, &[]);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{target}");
        let (head, body) = response.split_once("\r\n\r\n").expect("separator");
        assert_eq!(body.as_bytes(), WEB_INDEX_BYTES);
        assert!(!body.contains(&server.session_token()));
        assert!(!body.contains(&server.state.bootstrap.token));
        // Only the expiry of any legacy cookie is ever sent.
        assert!(head.contains(
            "\r\nSet-Cookie: emp_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0\r\n"
        ));
        assert!(head.contains("\r\nReferrer-Policy: no-referrer\r\n"));
    }
    let web = String::from_utf8_lossy(WEB_INDEX_BYTES);
    assert!(web.contains("establishSession()"));
    assert!(web.contains("X-EMP-Session"));
    assert!(web.contains("X-EMP-Bootstrap"));
    assert!(web.contains("managementFetch('/api/accounts/events'"));
    assert!(web.contains("managementFetch('/api/migration/export'"));
    assert!(!web.contains("new EventSource("));
    assert!(web.contains("请从 EMP 启动时提供的链接打开管理页"));
    assert!(!server.state.bootstrap.used.load(Ordering::Acquire));
    server.shutdown().expect("shutdown");
}

#[test]
fn responses_carry_framing_and_sniffing_protections() {
    let (_directory, server) = test_server();
    for response in [
        request(&server, "/", &[]),
        request(&server, "/healthz", &[]),
        request(&server, "/api/config", &[]),
        request(&server, "/api/config", &[&session_header(&server)]),
        request(&server, "/missing", &[]),
    ] {
        let head = response.split_once("\r\n\r\n").expect("separator").0;
        assert!(head.contains("\r\nX-Frame-Options: DENY\r\n"), "{head}");
        assert!(head.contains("\r\nContent-Security-Policy: frame-ancestors 'none'\r\n"));
        assert!(head.contains("\r\nX-Content-Type-Options: nosniff\r\n"));
    }
    server.shutdown().expect("shutdown");
}

#[test]
fn malformed_and_duplicate_bootstrap_never_login() {
    let (_directory, server) = test_server();
    let long_token = "A".repeat(WEB_SESSION_TOKEN_LENGTH);
    let prefixed = format!("{}x", server.state.bootstrap.token);
    for token in ["", "wrong", long_token.as_str(), prefixed.as_str()] {
        let response = bootstrap_exchange(&server, token, &[]);
        assert!(
            response.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "token: {token}"
        );
    }
    let missing = post(&server, "/api/session", b"", &[]);
    assert!(missing.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    // The query string is no longer an authentication input.
    let query = post(
        &server,
        &format!("/api/session?bootstrap={}", server.state.bootstrap.token),
        b"",
        &[],
    );
    assert!(query.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    let duplicate = bootstrap_exchange(
        &server,
        &server.state.bootstrap.token,
        &[&format!(
            "X-EMP-Bootstrap: {}",
            server.state.bootstrap.token
        )],
    );
    assert!(duplicate.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    assert!(!server.state.bootstrap.used.load(Ordering::Acquire));
    server.shutdown().expect("shutdown");
}

#[test]
fn bootstrap_exchange_is_same_origin_only() {
    let (_directory, server) = test_server();
    let port = server.local_addr().port();
    let token = server.state.bootstrap.token.clone();
    for origin in [
        format!("Origin: http://127.0.0.1:{}", port + 1),
        format!("Origin: http://localhost.:{port}"),
        format!("Origin: http://%31%32%37.0.0.1:{port}"),
    ] {
        let response = bootstrap_exchange(&server, &token, &[&origin]);
        assert!(
            response.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "{origin}"
        );
    }
    let rebinding = bootstrap_exchange(&server, &token, &["Host: attacker.example"]);
    assert!(rebinding.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    let accepted = bootstrap_exchange(
        &server,
        &token,
        &[&format!("Origin: HTTP://LOCALHOST:{port}")],
    );
    exchanged_session(&accepted);
    server.shutdown().expect("shutdown");
}

#[test]
fn bootstrap_is_single_use_and_rotates_the_session() {
    let (_directory, server) = test_server();
    let previous = server.session_token();
    let token = server.state.bootstrap.token.clone();
    let first = bootstrap_exchange(&server, &token, &[]);
    let session = exchanged_session(&first);
    assert_ne!(session, previous);
    assert_eq!(session, server.session_token());
    assert!(first.contains(
        "\r\nSet-Cookie: emp_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0\r\n"
    ));
    assert!(first.contains("\r\nCache-Control: no-store\r\n"));

    let second = bootstrap_exchange(&server, &token, &[]);
    assert!(second.starts_with("HTTP/1.1 401 Unauthorized\r\n"));

    let stale = request(
        &server,
        "/api/config",
        &[&format!("X-EMP-Session: {previous}")],
    );
    assert!(stale.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    let api = request(
        &server,
        "/api/config",
        &[&format!("X-EMP-Session: {session}")],
    );
    assert!(api.starts_with("HTTP/1.1 200 OK\r\n"), "{api}");
    server.shutdown().expect("shutdown");
}

#[test]
fn bootstrap_token_expires_unused() {
    let (_directory, server) = test_server();
    let expires_at = server.state.bootstrap.expires_at;
    assert!(expires_at <= crate::util::system_now() + 10.0 * 60.0);
    let raw = format!(
        "POST /api/session HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-EMP-Bootstrap: {}\r\n\r\n",
        server.local_addr().port(),
        server.state.bootstrap.token,
    );
    let request = parse_request(&raw).expect("request");
    let response = route_request_at(request, &server.state, expires_at);
    assert!(response.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"));
    assert!(!server.state.bootstrap.used.load(Ordering::Acquire));
    let response = route_request_at(request, &server.state, expires_at - 1.0);
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    server.shutdown().expect("shutdown");
}

#[test]
fn cross_origin_ui_and_api_are_rejected() {
    let (_directory, server) = test_server();
    let origin = format!(
        "Origin: http://127.0.0.1:{}",
        server.local_addr().port() + 1
    );
    let ui = request(&server, "/", &[&origin]);
    assert!(ui.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    assert!(ui.contains("cross-origin Web UI request rejected"));
    let api = request(&server, "/api/config", &[&origin, &session_header(&server)]);
    assert!(api.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    server.shutdown().expect("shutdown");
}

#[test]
fn catalog_and_health_reject_rebinding_hosts_and_foreign_origins() {
    let (_directory, server) = test_server();
    let origin = format!(
        "Origin: http://127.0.0.1:{}",
        server.local_addr().port() + 1
    );
    for target in ["/healthz", "/v1/models", "/v1/models/demo%2Fold"] {
        let rebinding = request(&server, target, &["Host: attacker.example"]);
        assert!(
            rebinding.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "{target}"
        );
        let foreign = request(&server, target, &[&origin]);
        assert!(
            foreign.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "{target}"
        );
    }
    let catalog = request(&server, "/v1/models", &[]);
    assert!(catalog.starts_with("HTTP/1.1 200 OK\r\n"));
    let host = format!("Host: 127.0.0.1:{}", server.local_addr().port());
    let duplicate_host = request(&server, "/healthz", &[&host, "Host: attacker.example"]);
    assert!(duplicate_host.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    let accepted_origin = format!("Origin: http://127.0.0.1:{}", server.local_addr().port());
    let duplicate_origin = request(
        &server,
        "/healthz",
        &[&accepted_origin, "Origin: http://attacker.example"],
    );
    assert!(duplicate_origin.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    server.shutdown().expect("shutdown");
}

#[test]
fn api_session_boundary_is_header_only() {
    let (_directory, server) = test_server();
    let token = server.session_token();
    let unauthorized = request(&server, "/api/config", &[]);
    assert!(unauthorized.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    // An ambient cookie is shared across local ports and is never accepted.
    for header in [
        format!("Cookie: emp_session={token}"),
        format!("Authorization: Bearer {token}"),
        "X-EMP-Session: wrong".to_owned(),
        "X-EMP-Session: ".to_owned(),
    ] {
        let response = request(&server, "/api/config", &[&header]);
        assert!(
            response.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "{header}"
        );
    }
    let api = request(
        &server,
        "/api/config",
        &[&format!("X-EMP-Session: {token}")],
    );
    assert!(api.starts_with("HTTP/1.1 200 OK\r\n"));
    let duplicate_session = request(
        &server,
        "/api/config",
        &[&format!("X-EMP-Session: {token}"), "X-EMP-Session: wrong"],
    );
    assert!(duplicate_session.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    let proxy = post(
        &server,
        "/v1/responses",
        b"{}",
        &[&format!("Cookie: emp_session={token}")],
    );
    assert!(
        proxy.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{proxy}"
    );
    server.shutdown().expect("shutdown");
}

#[test]
fn account_suffix_routes_without_an_id_do_not_panic() {
    let (_directory, server) = test_server();
    let session = session_header(&server);
    for target in [
        "/api/accounts/quota-history",
        "/api/accounts//quota-history",
    ] {
        let response = request(&server, target, &[&session]);
        assert!(
            response.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{target}: {response}"
        );
    }
    for target in [
        "/api/accounts/quota",
        "/api/accounts/quota-reset",
        "/api/accounts//quota",
        "/api/accounts///quota-reset",
    ] {
        let response = post(&server, target, b"{}", &[&session]);
        assert!(
            response.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{target}: {response}"
        );
    }
    for target in ["/api/accounts/", "/api/accounts//"] {
        let response = delete(&server, target, &[&session]);
        assert!(
            response.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{target}: {response}"
        );
    }
    let health = request(&server, "/healthz", &[]);
    assert!(health.starts_with("HTTP/1.1 200 OK\r\n"));
    server.shutdown().expect("shutdown");
}

#[test]
fn management_body_rejects_ambiguous_framing_headers() {
    let (_directory, server) = test_server();
    let session = session_header(&server);
    for framing in ["Content-Length: 2", "Transfer-Encoding: chunked"] {
        let response = post(&server, "/api/config", b"{}", &[&session, framing]);
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "{response}"
        );
    }
    server.shutdown().expect("shutdown");
}

#[test]
fn request_head_has_an_overall_deadline() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let address = listener.local_addr().expect("address");
    let writer = thread::spawn(move || {
        let mut client = TcpStream::connect(address).expect("connect");
        client
            .write_all(b"GET / HTTP/1.1\r\n")
            .expect("request line");
        // Trickle one byte at a time, never finishing the head and never
        // idling long enough for the per-read timeout.
        for _ in 0..40 {
            thread::sleep(Duration::from_millis(50));
            if client.write_all(b"X").is_err() {
                break;
            }
        }
    });
    let (mut stream, _) = listener.accept().expect("accept");
    let started = Instant::now();
    let head = crate::http::request::read_request_head_before(
        &mut stream,
        started + Duration::from_millis(400),
    );
    assert!(head.is_none());
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(400), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
    drop(stream);
    writer.join().expect("writer");
}

#[test]
fn request_body_has_an_overall_deadline() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let address = listener.local_addr().expect("address");
    let writer = thread::spawn(move || {
        let mut client = TcpStream::connect(address).expect("connect");
        for _ in 0..40 {
            thread::sleep(Duration::from_millis(50));
            if client.write_all(b"X").is_err() {
                break;
            }
        }
    });
    let (mut stream, _) = listener.accept().expect("accept");
    let started = Instant::now();
    let mut body = Vec::new();
    let result = crate::http::request::read_exact_before(
        &mut stream,
        &mut body,
        40,
        started + Duration::from_millis(400),
    );
    assert_eq!(
        result.expect_err("body deadline").kind(),
        std::io::ErrorKind::TimedOut
    );
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(400), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
    assert!(body.len() < 40);
    drop(stream);
    writer.join().expect("writer");
}

#[test]
fn web_session_persists_across_restart() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config = canonical_root(&directory).join("config.json");
    let first = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
        .expect("start first server");
    let token = first.session_token();
    let first_addr = first.local_addr();
    first.shutdown().expect("shutdown first server");
    let second = ServerHandle::start_with_config(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        first_addr.port(),
        &config,
    )
    .expect("start second server");
    let mut stream = TcpStream::connect(second.local_addr()).expect("connect");
    stream
            .write_all(format!("GET /api/config HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-EMP-Session: {token}\r\nConnection: close\r\n\r\n", second.local_addr().port()).as_bytes())
            .expect("write request");
    assert!(complete_response(&mut stream).starts_with("HTTP/1.1 200 OK\r\n"));
    second.shutdown().expect("shutdown second server");
}

#[test]
fn expired_web_session_is_rotated_at_startup() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let session_path = canonical_root(&directory).join("state/web-session.json");
    std::fs::create_dir_all(session_path.parent().expect("state directory")).expect("create state");
    std::fs::write(
        &session_path,
        br#"{"token":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","expires_at":1}"#,
    )
    .expect("write expired session");
    let server = ServerHandle::start_with_config(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &canonical_root(&directory).join("config.json"),
    )
    .expect("rotate expired session");
    assert_ne!(
        server.session_token(),
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
    );
    server.shutdown().expect("shutdown");
}

#[test]
fn caller_authorization_tracks_the_live_native_token() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth = directory.path().join("auth.json");
    std::fs::write(&auth, br#"{"tokens":{"access_token":"native-secret"}}"#).expect("write auth");
    assert!(valid_caller_authorization(
        Some("Bearer native-secret"),
        &auth
    ));
    assert!(!valid_caller_authorization(
        Some("bearer native-secret"),
        &auth
    ));
    assert!(!valid_caller_authorization(Some("Bearer wrong"), &auth));
    std::fs::write(&auth, br#"{"access_token":"rotated"}"#).expect("rotate auth");
    assert!(valid_caller_authorization(Some("Bearer rotated"), &auth));
    assert!(!valid_caller_authorization(
        Some("Bearer native-secret"),
        &auth
    ));

    if let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let script = r#"
import json
from easy_multi_provider.accounts import valid_caller_authorization
values = ["Bearer rotated", "bearer rotated", "Bearer wrong", "Bearer ", ""]
print(json.dumps([valid_caller_authorization(value) for value in values]))
"#;
        let output = Command::new(python)
            .arg("-c")
            .arg(script)
            .env("CODEX_HOME", directory.path())
            .current_dir(root)
            .output()
            .expect("spawn Python authorization oracle");
        assert!(
            output.status.success(),
            "Python authorization oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let python: Value =
            serde_json::from_slice(&output.stdout).expect("Python authorization JSON");
        let rust = json!([
            valid_caller_authorization(Some("Bearer rotated"), &auth),
            valid_caller_authorization(Some("bearer rotated"), &auth),
            valid_caller_authorization(Some("Bearer wrong"), &auth),
            valid_caller_authorization(Some("Bearer "), &auth),
            valid_caller_authorization(Some(""), &auth),
        ]);
        assert_eq!(rust, python);
    }
}

#[test]
fn responses_authentication_precedes_request_body_reads() {
    let (_directory, server) = test_server();
    let mut stream = TcpStream::connect(server.local_addr()).expect("connect");
    stream
            .write_all(
                format!(
                    "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n",
                    server.local_addr().port()
                )
                .as_bytes(),
            )
            .expect("write unauthenticated head only");
    let response = complete_response(&mut stream);
    assert!(response.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    assert!(response.contains("proxy caller authentication is required"));
    server.shutdown().expect("shutdown");
}
