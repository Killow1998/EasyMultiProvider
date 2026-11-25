//! Exhaust descriptors in a disposable child, never in the parallel test runner.
#![cfg(unix)]

use crate::lifecycle::ServerHandle;
use emp_test_support::{Deadline, OwnedChild, test_timeout};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const CHILD_FLAG: &str = "EMP_TEST_ACCEPT_EMFILE_CHILD";
const TEST_NAME: &str = "server::accept::tests::listener_survives_real_descriptor_exhaustion";

fn command(input: &mut impl Write, value: &str) {
    writeln!(input, "{value}").unwrap();
    input.flush().unwrap();
}

fn expect_command(input: &mut impl BufRead, expected: &str) {
    let mut line = String::new();
    assert_ne!(input.read_line(&mut line).unwrap(), 0);
    assert_eq!(line.trim(), expected);
}

fn marker(value: &str) {
    println!("EMP_FD_{value}");
    std::io::stdout().flush().unwrap();
}

fn child_fixture() {
    let limit = libc::rlimit {
        rlim_cur: 128,
        rlim_max: 128,
    };
    // This is a fresh, disposable process. The parent and live EMP are untouched.
    // SAFETY: limit is initialized and lives through setrlimit in this disposable child process.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    let root = emp_test_support::fixture_dir().unwrap();
    let config = root.path().canonicalize().unwrap().join("config.json");
    let server =
        ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config).unwrap();
    marker(&format!("READY {}", server.local_addr()));
    let mut input = std::io::stdin().lock();
    expect_command(&mut input, "exhaust");
    let mut files = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(file) => files.push(file),
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                break;
            }
        }
    }
    marker("EXHAUSTED");
    expect_command(&mut input, "release");
    drop(files);
    marker("RELEASED");
    expect_command(&mut input, "stop");
    let address = server.local_addr();
    let mut websocket =
        TcpStream::connect_timeout(&address, test_timeout(Duration::from_secs(2)).duration())
            .unwrap();
    write!(websocket, "GET /v1/responses HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nX-EMP-Session: {}\r\n\r\n", server.session_token()).unwrap();
    let mut reader =
        BufReader::new(Deadline::after(test_timeout(Duration::from_secs(5))).io(websocket));
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    assert!(status.starts_with("HTTP/1.1 101"), "{status}");
    drop(reader);
    server.shutdown().unwrap();
    let events: Vec<serde_json::Value> = std::fs::read_dir(root.path().join("state/logs"))
        .unwrap()
        .flat_map(|entry| {
            std::fs::read_to_string(entry.unwrap().path())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "listener_accept_failed"
                && event["fields"]["os_error"] == libc::EMFILE)
    );
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "listener_accept_recovered")
    );
}

#[test]
fn listener_survives_real_descriptor_exhaustion() {
    if std::env::var_os(CHILD_FLAG).is_some() {
        child_fixture();
        return;
    }
    let mut fixture = Command::new(std::env::current_exe().unwrap());
    fixture
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_FLAG, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = OwnedChild::spawn(fixture).unwrap();
    let mut input = child.take_stdin().unwrap();
    let next_marker = || {
        let deadline = Deadline::after(test_timeout(Duration::from_secs(20)));
        loop {
            let line = child
                .stdout_line(deadline)
                .expect("child marker before timeout")
                .expect("child output ended before marker");
            if let Some((_, marker)) = line.split_once("EMP_FD_") {
                break marker.to_owned();
            }
        }
    };
    let ready = next_marker();
    let address: SocketAddr = ready.strip_prefix("READY ").unwrap().parse().unwrap();
    command(&mut input, "exhaust");
    assert_eq!(next_marker(), "EXHAUSTED");
    // A new connection makes accept() report real EMFILE in the child.
    let queued =
        TcpStream::connect_timeout(&address, test_timeout(Duration::from_secs(2)).duration())
            .unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "维持文件描述符耗尽窗口，观察真实 EMFILE 下的 accept 退避"
    )]
    thread::sleep(Duration::from_millis(400));
    command(&mut input, "release");
    assert_eq!(next_marker(), "RELEASED");
    drop(queued);
    let mut health =
        TcpStream::connect_timeout(&address, test_timeout(Duration::from_secs(2)).duration())
            .unwrap();
    write!(
        health,
        "GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    Deadline::after(test_timeout(Duration::from_secs(5)))
        .io(&health)
        .read_to_string(&mut response)
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    drop(health);
    command(&mut input, "stop");
    let output = child
        .wait_until(Deadline::after(test_timeout(Duration::from_secs(10))))
        .unwrap_or_else(|error| panic!("descriptor exhaustion child: {error}"));
    assert!(
        output.status.success(),
        "child regression failed: {}",
        output.status
    );
}
