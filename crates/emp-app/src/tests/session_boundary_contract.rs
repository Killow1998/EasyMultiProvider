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

#[test]
fn login_page_has_the_chinese_contract() {
    let (_directory, server) = test_server();
    let response = request(&server, "/", &[]);
    assert!(response.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    assert!(response.contains("请从 EMP 打开管理页"));
    assert!(!response.contains("Set-Cookie"));
    server.shutdown().expect("shutdown");
}

#[test]
fn malformed_and_duplicate_bootstrap_never_login() {
    let (_directory, server) = test_server();
    let long_token = "A".repeat(WEB_SESSION_TOKEN_LENGTH);
    for target in [
        "/?bootstrap=",
        "/?bootstrap=wrong",
        &format!("/?bootstrap={long_token}"),
        &format!(
            "/?bootstrap={}&bootstrap={}",
            server.state.bootstrap.token, server.state.bootstrap.token
        ),
    ] {
        let response = request(&server, target, &[]);
        assert!(
            response.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "target: {target}"
        );
    }
    server.shutdown().expect("shutdown");
}

#[test]
fn encoded_query_key_and_python_origin_forms_match() {
    let (_directory, server) = test_server();
    let encoded_key = request(
        &server,
        &format!("/?%62ootstrap={}", server.state.bootstrap.token),
        &[],
    );
    assert!(encoded_key.starts_with("HTTP/1.1 303 See Other\r\n"));
    server.shutdown().expect("shutdown");

    let (_directory, server) = test_server();
    let port = server.local_addr().port();
    for origin in [
        format!("Origin: HTTP://LOCALHOST:{port}"),
        format!("Origin: http://localhost:{port:05}"),
        format!("Origin: https://127.0.0.1:{port}/path"),
    ] {
        let response = request(&server, "/", &[&origin]);
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    }
    for origin in [
        format!("Origin: http://localhost.:{port}"),
        format!("Origin: http://%31%32%37.0.0.1:{port}"),
    ] {
        let response = request(&server, "/", &[&origin]);
        assert!(response.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    }
    server.shutdown().expect("shutdown");
}

#[test]
fn percent_encoded_bootstrap_is_decoded_once() {
    let (_directory, server) = test_server();
    let encoded: String = server
        .state
        .bootstrap
        .token
        .chars()
        .map(|character| format!("%{:02X}", character as u8))
        .collect();
    let first = request(&server, &format!("/?bootstrap={encoded}"), &[]);
    assert!(first.starts_with("HTTP/1.1 303 See Other\r\n"));
    assert!(first.contains("Location: /\r\n"));
    let second = request(
        &server,
        &format!("/?bootstrap={}", server.state.bootstrap.token),
        &[],
    );
    assert!(second.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    server.shutdown().expect("shutdown");
}

#[test]
fn bootstrap_login_sets_exact_cookie_and_session_serves_ui() {
    let (_directory, server) = test_server();
    let bootstrap = request(
        &server,
        &format!("/?bootstrap={}", server.state.bootstrap.token),
        &[],
    );
    let separator = bootstrap.find("\r\n\r\n").expect("separator");
    let headers = &bootstrap[..separator];
    assert!(headers.contains("\r\nSet-Cookie: emp_session="));
    assert!(headers.contains("; HttpOnly; SameSite=Strict; Path=/; Max-Age="));
    let cookie = headers
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: "))
        .expect("cookie header");
    let value = cookie.split(';').next().expect("cookie value");
    let session = request(&server, "/", &[&format!("Cookie: {value}")]);
    let body_start = session.find("\r\n\r\n").expect("separator") + 4;
    assert!(session.starts_with("HTTP/1.1 200 OK\r\n"));
    assert_eq!(&session.as_bytes()[body_start..], WEB_INDEX_BYTES);
    let refreshed = session
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: "))
        .expect("refreshed session cookie");
    assert!(refreshed.starts_with(&format!(
        "{value}; HttpOnly; SameSite=Strict; Path=/; Max-Age="
    )));
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
    let api = request(&server, "/api/config", &[&origin]);
    assert!(api.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    server.shutdown().expect("shutdown");
}

#[test]
fn api_session_boundary_is_exact() {
    let (_directory, server) = test_server();
    let unauthorized = request(&server, "/api/config", &[]);
    assert!(unauthorized.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    let login = request(
        &server,
        &format!("/?bootstrap={}", server.state.bootstrap.token),
        &[],
    );
    let cookie = login
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: "))
        .expect("cookie header");
    let value = cookie.split(';').next().expect("cookie value");
    let api = request(&server, "/api/config", &[&format!("Cookie: {value}")]);
    assert!(api.starts_with("HTTP/1.1 200 OK\r\n"));
    server.shutdown().expect("shutdown");
}

#[test]
fn web_session_persists_across_restart() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config = canonical_root(&directory).join("config.json");
    let first = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
        .expect("start first server");
    let cookie = first.session_cookie();
    let token = cookie
        .trim_start_matches("emp_session=")
        .split(';')
        .next()
        .expect("token")
        .to_string();
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
            .write_all(format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nCookie: emp_session={token}\r\nConnection: close\r\n\r\n", second.local_addr().port()).as_bytes())
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
    let cookie = server.session_cookie();
    assert!(!cookie.contains("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
    server.shutdown().expect("shutdown");
}

#[test]
fn bootstrap_rotates_a_session_that_expires_while_running() {
    let (_directory, server) = test_server();
    let (old_token, future) = {
        let session = server.state.sessions.session.lock().expect("session lock");
        (session.token().to_owned(), session.expires_at() + 1.0)
    };
    let raw = format!(
        "GET /?bootstrap={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        server.state.bootstrap.token,
        server.local_addr().port()
    );
    let request = parse_request(&raw).expect("request");
    let response = route_request_at(request, &server.state, future);
    assert!(response.starts_with(b"HTTP/1.1 303 See Other\r\n"));
    let session = server.state.sessions.session.lock().expect("session lock");
    assert_ne!(session.token(), old_token);
    assert!(session.is_active_at(future));
    drop(session);
    server.shutdown().expect("shutdown");
}

#[test]
fn cookie_parser_uses_the_last_value_and_rejects_malformed_input() {
    assert_eq!(
        parse_session_cookie("emp_session=first; emp_session=second").as_deref(),
        Some("second")
    );
    assert_eq!(
        parse_session_cookie("emp_session=\"quoted\"").as_deref(),
        Some("quoted")
    );
    assert_eq!(parse_session_cookie("emp_session=valid; malformed"), None);
    assert_eq!(parse_session_cookie("emp_session=\"unterminated"), None);
    assert_eq!(
        parse_session_cookie("emp_session=\"é\"").as_deref(),
        Some("é")
    );
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
