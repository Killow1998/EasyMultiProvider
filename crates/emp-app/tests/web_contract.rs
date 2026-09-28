use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use tempfile::TempDir;

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

fn canonical_root(directory: &TempDir) -> PathBuf {
    directory
        .path()
        .canonicalize()
        .expect("canonical temporary root")
}

fn complete_response(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("response timeout");
    let mut response = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).expect("read HTTP response");
        assert!(count > 0, "response ended before Content-Length bytes");
        response.extend_from_slice(&buffer[..count]);
        assert!(
            response.len() <= MAX_RESPONSE_BYTES,
            "response exceeded the bounded test contract"
        );
        let Some(separator) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&response[..separator]).expect("ASCII headers");
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .expect("Content-Length header");
        let expected = separator + 4 + content_length;
        assert!(response.len() <= expected, "unexpected pipelined bytes");
        if response.len() == expected {
            return response;
        }
    }
}

fn request(port: u16, target: &str, headers: &[&str]) -> Vec<u8> {
    request_with_method(port, "GET", target, headers)
}

fn request_with_method(port: u16, method: &str, target: &str, headers: &[&str]) -> Vec<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to EMP");
    let host = if headers.iter().any(|header| {
        header
            .split_once(':')
            .is_some_and(|(name, _)| name.eq_ignore_ascii_case("host"))
    }) {
        String::new()
    } else {
        format!("Host: 127.0.0.1:{port}\r\n")
    };
    let headers = if headers.is_empty() {
        String::new()
    } else {
        format!("{}\r\n", headers.join("\r\n"))
    };
    write!(
        stream,
        "{method} {target} HTTP/1.1\r\n{host}{headers}Connection: close\r\n\r\n"
    )
    .expect("write HTTP request");
    complete_response(&mut stream)
}

fn body(response: &[u8]) -> &[u8] {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response separator");
    &response[separator + 4..]
}

fn header_text(response: &[u8]) -> String {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response separator");
    String::from_utf8_lossy(&response[..separator]).into_owned()
}

/// Exchange a bootstrap token for a session through `POST /api/session`.
fn bootstrap_session(port: u16, bootstrap: &str) -> Vec<u8> {
    request_with_method(
        port,
        "POST",
        "/api/session",
        &[
            &format!("X-EMP-Bootstrap: {bootstrap}"),
            "Content-Length: 0",
        ],
    )
}

fn session_from(response: &[u8]) -> String {
    let value: serde_json::Value = serde_json::from_slice(body(response)).expect("session JSON");
    value["session"]
        .as_str()
        .expect("session token")
        .to_string()
}

fn spawn_emp(config: &std::path::Path) -> (u16, std::process::Child, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_EMP"))
        .args([
            "serve",
            "--config",
            &config.to_string_lossy(),
            "--host",
            "127.0.0.1",
            "--port",
            "0",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start EMP");
    let stdout = child.stdout.take().expect("EMP stdout");
    let mut stdout = BufReader::new(stdout);
    let mut ready_line = String::new();
    stdout
        .read_line(&mut ready_line)
        .expect("read readiness output");
    assert!(
        ready_line.starts_with("EMP listening on http://127.0.0.1:"),
        "unexpected readiness line: {ready_line:?}"
    );
    let port = ready_line
        .rsplit(':')
        .next()
        .and_then(|raw| raw.trim_end().parse::<u16>().ok())
        .expect("readiness line contains a port");
    let mut config_line = String::new();
    stdout
        .read_line(&mut config_line)
        .expect("read configuration path");
    assert_eq!(
        config_line.trim_end(),
        format!("Configuration file: {}", config.display())
    );
    let mut proxy_line = String::new();
    stdout
        .read_line(&mut proxy_line)
        .expect("read network proxy");
    assert!(
        matches!(
            proxy_line.trim_end(),
            "Network proxy: environment" | "Network proxy: system" | "Network proxy: direct"
        ),
        "unexpected network line: {proxy_line:?}"
    );
    let mut bootstrap_line = String::new();
    stdout
        .read_line(&mut bootstrap_line)
        .expect("read bootstrap URL");
    assert!(bootstrap_line.starts_with("Open in browser: "));
    (port, child, bootstrap_line)
}

#[test]
fn embedded_web_ui_is_a_complete_self_served_page() {
    // The binary serves the page it embeds; the page is a complete HTML
    // document with a script that performs the bootstrap exchange, so the UI
    // works without any external asset fetches.
    let page = emp_app::WEB_INDEX_BYTES;
    let document = std::str::from_utf8(page).expect("embedded Web UI is UTF-8");
    assert!(
        document.to_ascii_lowercase().starts_with("<!doctype html>"),
        "document starts with a doctype"
    );
    assert!(document.contains("</html>"), "document is closed");
    assert!(
        document.contains("<script"),
        "the page carries the bootstrap script"
    );
    assert!(
        document.contains("/api/session"),
        "the script exchanges the bootstrap token for a session"
    );
}

#[test]
fn served_index_matches_the_embedded_bytes() {
    let directory = TempDir::new().expect("temporary directory");
    let config = canonical_root(&directory).join("config.json");
    let (port, mut child, _) = spawn_emp(&config);
    let page = request(port, "/", &[]);
    assert!(page.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(body(&page), emp_app::WEB_INDEX_BYTES);
    child.kill().expect("stop test EMP");
    child.wait().expect("reap test EMP");
}

#[test]
fn version_output_reports_the_running_release() {
    let output = Command::new(env!("CARGO_BIN_EXE_EMP"))
        .arg("--version")
        .output()
        .expect("run EMP --version");
    assert!(output.status.success());
    let line_ending = if cfg!(windows) { "\r\n" } else { "\n" };
    assert_eq!(output.stdout, format!("EMP {}{line_ending}", env!("CARGO_PKG_VERSION")).as_bytes());
    assert!(output.stderr.is_empty());
}

#[test]
fn management_bootstrap_exchanges_a_single_use_session_token() {
    let directory = TempDir::new().expect("temporary directory");
    let config = canonical_root(&directory).join("config.json");
    let (port, mut child, bootstrap_line) = spawn_emp(&config);
    let token = bootstrap_line
        .trim_start_matches("Open in browser: ")
        .trim_end()
        .rsplit_once("bootstrap=")
        .map(|(_, token)| token.to_string())
        .expect("bootstrap token");
    assert_eq!(token.len(), 43);
    assert!(token.is_ascii());

    let health = request(port, "/healthz", &[]);
    assert!(health.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(body(&health), b"{\"status\":\"ok\"}");

    // The page carries no secrets; its script exchanges the bootstrap token.
    let page = request(port, "/", &[]);
    assert!(page.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(body(&page), emp_app::WEB_INDEX_BYTES);

    let wrong_host = request(port, "/", &["Host: 127.0.0.2"]);
    assert!(wrong_host.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));
    let cross_origin = request(
        port,
        "/",
        &[&format!("Origin: http://127.0.0.1:{}", port + 1)],
    );
    assert!(cross_origin.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));

    // The legacy query-string login neither sets a session nor spends the
    // bootstrap token; only the script's POST below exchanges it.
    let query_login = request(port, &format!("/?bootstrap={token}"), &[]);
    assert!(query_login.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(
        header_text(&query_login)
            .lines()
            .filter_map(|line| line.strip_prefix("Set-Cookie: "))
            .all(|cookie| cookie.starts_with("emp_session=;") && cookie.ends_with("Max-Age=0"))
    );

    let login = bootstrap_session(port, &token);
    assert!(login.starts_with(b"HTTP/1.1 200 OK\r\n"));
    let session = session_from(&login);
    assert_eq!(session.len(), 43);

    let reused_bootstrap = bootstrap_session(port, &token);
    assert!(reused_bootstrap.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"));

    let api_unauthorized = request(port, "/api/config", &[]);
    assert!(api_unauthorized.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"));
    let cookie_only = request(
        port,
        "/api/config",
        &[&format!("Cookie: emp_session={session}")],
    );
    assert!(cookie_only.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"));
    let api_authorized = request(port, "/api/config", &[&format!("X-EMP-Session: {session}")]);
    assert!(api_authorized.starts_with(b"HTTP/1.1 200 OK\r\n"));

    child.kill().expect("stop test EMP");
    child.wait().expect("reap test EMP");
}

#[test]
fn rejects_malformed_percent_escapes_and_non_ascii_bootstrap() {
    let directory = TempDir::new().expect("temporary directory");
    let config = canonical_root(&directory).join("config.json");
    let (port, mut child, bootstrap_line) = spawn_emp(&config);
    let token = bootstrap_line
        .trim_start_matches("Open in browser: ")
        .trim_end()
        .rsplit_once("bootstrap=")
        .map(|(_, token)| token.to_string())
        .expect("bootstrap token");

    for supplied in ["%2", "%GG", "%FF", &format!("{token}中"), ""] {
        let response = bootstrap_session(port, supplied);
        assert!(
            response.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"),
            "{supplied}"
        );
    }
    // Rejected attempts do not spend the real token.
    assert!(bootstrap_session(port, &token).starts_with(b"HTTP/1.1 200 OK\r\n"));

    child.kill().expect("stop test EMP");
    child.wait().expect("reap test EMP");
}

#[test]
fn valid_session_persists_across_restart() {
    let directory = TempDir::new().expect("temporary directory");
    let config = canonical_root(&directory).join("config.json");
    let (port, mut child, bootstrap_line) = spawn_emp(&config);
    let token = bootstrap_line
        .trim_start_matches("Open in browser: ")
        .trim_end()
        .rsplit_once("bootstrap=")
        .map(|(_, token)| token.to_string())
        .expect("bootstrap token");
    let session = session_from(&bootstrap_session(port, &token));
    child.kill().expect("stop first EMP");
    child.wait().expect("reap first EMP");

    let (port, mut child, _) = spawn_emp(&config);
    let restored = request(port, "/api/config", &[&format!("X-EMP-Session: {session}")]);
    assert!(restored.starts_with(b"HTTP/1.1 200 OK\r\n"));
    child.kill().expect("stop restarted EMP");
    child.wait().expect("reap restarted EMP");
}
