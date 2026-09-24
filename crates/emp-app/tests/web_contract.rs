use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

fn repository_index_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../easy_multi_provider/web/index.html")
}

fn assert_embedded_index_matches(expected: &[u8], source: &str) {
    let actual = emp_app::WEB_INDEX_BYTES;
    assert!(
        actual == expected,
        "embedded Web UI differs from {source}: Rust {} bytes (SHA-256 {:x}), source {} bytes (SHA-256 {:x})",
        actual.len(),
        Sha256::digest(actual),
        expected.len(),
        Sha256::digest(expected),
    );
}

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
        "GET {target} HTTP/1.1\r\n{host}{headers}Connection: close\r\n\r\n"
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

fn cookie_from(response: &[u8]) -> String {
    header_text(response)
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: "))
        .expect("Set-Cookie header")
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
fn embedded_index_matches_repository_bytes_exactly() {
    let expected = std::fs::read(repository_index_path()).expect("read source Web UI");
    assert_embedded_index_matches(&expected, "repository source");
}

#[test]
fn embedded_index_matches_current_python_release_bytes() {
    let python = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../EasyMultiProvider/easy_multi_provider/web/index.html");
    // CI checkouts may not include the adjacent oracle worktree; local
    // differential runs assert exact release bytes when it is available.
    if let Ok(expected) = std::fs::read(python) {
        assert_embedded_index_matches(&expected, "Python release");
    }
}

#[test]
fn version_output_matches_the_existing_cli() {
    let output = Command::new(env!("CARGO_BIN_EXE_EMP"))
        .arg("--version")
        .output()
        .expect("run EMP --version");
    assert!(output.status.success());
    let line_ending = if cfg!(windows) { "\r\n" } else { "\n" };
    assert_eq!(
        output.stdout,
        format!("EMP 0.11.10{line_ending}").as_bytes()
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn management_bootstrap_contract_is_python_compatible() {
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

    let unauthenticated = request(port, "/", &[]);
    assert!(unauthenticated.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"));
    let login = String::from_utf8_lossy(body(&unauthenticated));
    assert!(login.contains("请从 EMP 打开管理页"));

    let wrong_host = request(port, "/", &["Host: 127.0.0.2"]);
    assert!(wrong_host.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));
    let cross_origin = request(
        port,
        "/",
        &[&format!("Origin: http://127.0.0.1:{}", port + 1)],
    );
    assert!(cross_origin.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));

    let login_redirect = request(port, &format!("/?bootstrap={token}"), &[]);
    assert!(login_redirect.starts_with(b"HTTP/1.1 303 See Other\r\n"));
    let cookie = cookie_from(&login_redirect);
    assert!(cookie.starts_with("emp_session="));
    assert!(cookie.contains("; HttpOnly; SameSite=Strict; Path=/; Max-Age="));
    assert!(cookie.contains("; Max-Age="));

    let reused_bootstrap = request(port, &format!("/?bootstrap={token}"), &[]);
    assert!(reused_bootstrap.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"));

    let cookie_pair = cookie.split(';').next().expect("session cookie pair");
    let authenticated = request(port, "/", &[&format!("Cookie: {cookie_pair}")]);
    assert!(authenticated.starts_with(b"HTTP/1.1 200 OK\r\n"));
    let expected = std::fs::read(repository_index_path()).expect("read source Web UI");
    assert_eq!(body(&authenticated), expected.as_slice());
    assert!(cookie_from(&authenticated).starts_with("emp_session="));

    let api_unauthorized = request(port, "/api/config", &[]);
    assert!(api_unauthorized.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"));
    let api_authorized = request(port, "/api/config", &[&format!("Cookie: {cookie_pair}")]);
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

    for target in [
        "/?bootstrap=%2",
        "/?bootstrap=%GG",
        "/?bootstrap=%FF",
        &format!("/?bootstrap={}中", token),
    ] {
        let response = request(port, target, &[]);
        assert!(
            response.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"),
            "{target}"
        );
    }

    child.kill().expect("stop test EMP");
    child.wait().expect("reap test EMP");
}

#[test]
fn valid_session_cookie_persists_across_restart() {
    let directory = TempDir::new().expect("temporary directory");
    let config = canonical_root(&directory).join("config.json");
    let (port, mut child, bootstrap_line) = spawn_emp(&config);
    let token = bootstrap_line
        .trim_start_matches("Open in browser: ")
        .trim_end()
        .rsplit_once("bootstrap=")
        .map(|(_, token)| token.to_string())
        .expect("bootstrap token");
    let login = request(port, &format!("/?bootstrap={token}"), &[]);
    let cookie = cookie_from(&login);
    let session_value = cookie
        .split(';')
        .next()
        .and_then(|pair| pair.split_once('='))
        .map(|(_, value)| value.to_string())
        .expect("session cookie value");
    child.kill().expect("stop first EMP");
    child.wait().expect("reap first EMP");

    let (port, mut child, _) = spawn_emp(&config);
    let restored = request(
        port,
        "/",
        &[&format!("Cookie: emp_session={session_value}")],
    );
    assert!(restored.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(
        body(&restored),
        std::fs::read(repository_index_path()).expect("source UI")
    );
    child.kill().expect("stop restarted EMP");
    child.wait().expect("reap restarted EMP");
}
