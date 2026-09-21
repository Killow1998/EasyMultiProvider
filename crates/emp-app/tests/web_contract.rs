use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

fn repository_index_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../easy_multi_provider/web/index.html")
}

fn get_body(port: u16, path: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to EMP");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .expect("write HTTP request");
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

fn spawn_emp() -> (u16, std::process::Child) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_EMP"))
        .args(["serve", "--host", "127.0.0.1", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start EMP");
    let mut stdout = child.stdout.take().expect("EMP stdout");
    let mut ready_line = String::new();
    loop {
        let mut byte = [0_u8; 1];
        stdout.read_exact(&mut byte).expect("read readiness output");
        ready_line.push(byte[0] as char);
        if byte[0] == b'\n' {
            break;
        }
    }
    assert!(
        ready_line.starts_with("EMP listening on http://127.0.0.1:"),
        "unexpected readiness line: {ready_line:?}"
    );
    let port = ready_line
        .rsplit(':')
        .next()
        .and_then(|raw| raw.trim_end().parse::<u16>().ok())
        .expect("readiness line contains a port");
    (port, child)
}

#[test]
fn embedded_index_matches_repository_bytes_exactly() {
    let expected = std::fs::read(repository_index_path()).expect("read source Web UI");
    assert_eq!(emp_app::WEB_INDEX_BYTES.len(), expected.len());
    assert_eq!(emp_app::WEB_INDEX_BYTES, expected.as_slice());
}

#[test]
fn version_output_matches_the_existing_cli() {
    let output = Command::new(env!("CARGO_BIN_EXE_EMP"))
        .arg("--version")
        .output()
        .expect("run EMP --version");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"EMP 0.11.6\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn emp_serves_exact_web_and_health_bytes() {
    let (port, mut child) = spawn_emp();
    let health = get_body(port, "/healthz");
    let health_body = String::from_utf8_lossy(&health);
    assert_eq!(
        health_body.rsplit("\r\n\r\n").next().unwrap(),
        "{\"status\":\"ok\"}"
    );

    let index = get_body(port, "/");
    let separator = index
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response separator");
    let ui_body = &index[separator + 4..];
    let expected = std::fs::read(repository_index_path()).expect("read source Web UI");
    assert_eq!(ui_body, expected);

    child.kill().expect("stop test EMP");
    child.wait().expect("reap test EMP");
}
