#![cfg(target_os = "linux")]

use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn response_body(response: &[u8]) -> &[u8] {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    &response[separator + 4..]
}

fn response_header<'a>(response: &'a [u8], name: &str) -> Option<&'a str> {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    std::str::from_utf8(&response[..separator])
        .ok()?
        .lines()
        .find_map(|line| {
            let (header, value) = line.split_once(':')?;
            header.eq_ignore_ascii_case(name).then(|| value.trim())
        })
}

fn request(
    port: u16,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<&[u8]>,
) -> Vec<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to EMP");
    let payload = body.unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
    )
    .unwrap();
    if let Some(cookie) = cookie {
        write!(stream, "Cookie: {cookie}\r\n").unwrap();
    }
    if body.is_some() {
        write!(
            stream,
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        )
        .unwrap();
    }
    stream.write_all(b"Connection: close\r\n\r\n").unwrap();
    stream.write_all(payload).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    response
}

fn create_package(root: &Path) -> Vec<u8> {
    let stage = root.join("stage");
    let app = stage.join("EMP");
    std::fs::create_dir_all(&app).unwrap();
    let candidate = app.join("EMP");
    std::fs::write(
        &candidate,
        r#"#!/bin/sh
set -eu
if [ "${1:-}" = "--version" ]; then printf 'EMP 0.12.0\n'; exit 0; fi
printf '%s' "$$" > "$EMP_UPDATE_TEST_PID"
if [ -n "${EMP_UPDATE_READY:-}" ]; then
  job="${EMP_UPDATE_READY%/ready.json}"
  nonce=$(sed -n 's/.*"nonce":"\([^"]*\)".*/\1/p' "$job/plan.json")
  printf '{"version":"0.12.0","nonce":"%s"}\n' "$nonce" > "$EMP_UPDATE_READY"
fi
exec sleep 90
"#,
    )
    .unwrap();
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755)).unwrap();
    let archive = root.join("EMP-linux-x86_64.tar.gz");
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(&archive)
        .args(["-C"])
        .arg(&stage)
        .arg("EMP")
        .status()
        .expect("run tar");
    assert!(status.success());
    std::fs::read(archive).unwrap()
}

fn release_server(package: Vec<u8>) -> (String, thread::JoinHandle<()>) {
    let digest = format!("{:x}", Sha256::digest(&package));
    release_server_with_digest(package, digest)
}

fn release_server_with_digest(
    package: Vec<u8>,
    digest: String,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    let name = "EMP-linux-x86_64.tar.gz";
    let asset_url = format!("{base}/releases/download/v0.12.0/{name}");
    let metadata = serde_json::json!({
        "tag_name":"v0.12.0", "draft":false, "prerelease":false,
        "assets":[{"name":name,"digest":format!("sha256:{digest}"),"size":package.len(),"browser_download_url":asset_url}]
    }).to_string().into_bytes();
    let api_redirect = format!("{base}/api/latest");
    let artifact_redirect = format!("{base}/assets/{name}");
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut handled = 0;
        while handled < 4 && Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("fake release accept failed: {error}"),
            };
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let line = std::str::from_utf8(&request)
                .unwrap()
                .lines()
                .next()
                .unwrap();
            let path = line.split_whitespace().nth(1).unwrap();
            if path == "/latest" {
                write!(stream, "HTTP/1.1 302 Found\r\nLocation: {api_redirect}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            } else if path == "/api/latest" {
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", metadata.len()).unwrap();
                stream.write_all(&metadata).unwrap();
            } else if path == format!("/releases/download/v0.12.0/{name}") {
                write!(stream, "HTTP/1.1 302 Found\r\nLocation: {artifact_redirect}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            } else if path == format!("/assets/{name}") {
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", package.len()).unwrap();
                let split = package.len() / 2;
                stream.write_all(&package[..split]).unwrap();
                stream.flush().unwrap();
                thread::sleep(Duration::from_millis(800));
                stream.write_all(&package[split..]).unwrap();
            } else {
                write!(
                    stream,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
            handled += 1;
        }
        assert_eq!(handled, 4, "release API, redirect and artifact requests");
    });
    (base, thread)
}

fn start_emp(
    config: &Path,
    executable: &Path,
    repository: &str,
    api: &str,
    pid_file: &Path,
) -> (
    u16,
    String,
    ChildGuard,
    BufReader<std::process::ChildStdout>,
) {
    let mut process = Command::new(executable)
        .args([
            "serve",
            "--config",
            &config.to_string_lossy(),
            "--host",
            "127.0.0.1",
            "--port",
            "0",
        ])
        .env("EMP_UPDATE_TEST_REPOSITORY_URL", repository)
        .env("EMP_UPDATE_TEST_API_URL", api)
        .env("EMP_UPDATE_TEST_PID", pid_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start EMP process");
    let mut stdout = BufReader::new(process.stdout.take().unwrap());
    let child = ChildGuard(process);
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert!(
        line.starts_with("EMP listening on http://127.0.0.1:"),
        "{line:?}"
    );
    let port = line.rsplit(':').next().unwrap().trim().parse().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let token = line.trim().rsplit_once("bootstrap=").unwrap().1.to_owned();
    (port, token, child, stdout)
}

fn body_json(response: &[u8]) -> serde_json::Value {
    serde_json::from_slice(response_body(response)).expect("JSON HTTP response")
}

fn status(response: &[u8]) -> u16 {
    std::str::from_utf8(response)
        .expect("HTTP response is UTF-8")
        .split_whitespace()
        .nth(1)
        .expect("HTTP status")
        .parse()
        .expect("numeric HTTP status")
}

fn wait_for_state(port: u16, cookie: &str, expected: &str, timeout: Duration) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        let response = request(port, "GET", "/api/updates", Some(cookie), None);
        assert_eq!(status(&response), 200, "update snapshot request");
        let snapshot = body_json(&response);
        if snapshot["state"] == expected {
            return snapshot;
        }
        assert_ne!(snapshot["state"], "error", "update failed: {snapshot}");
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected}: {snapshot}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

struct RunningEmp {
    port: u16,
    cookie: String,
    process: ChildGuard,
    executable: std::path::PathBuf,
    config: std::path::PathBuf,
    original_config: Vec<u8>,
    _stdout: BufReader<std::process::ChildStdout>,
}

fn start_authenticated_emp(root: &Path, repository: &str) -> RunningEmp {
    let installation = root.join("install");
    std::fs::create_dir(&installation).unwrap();
    let executable = installation.join("EMP");
    std::fs::copy(env!("CARGO_BIN_EXE_EMP"), &executable).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = root.join("config.json");
    let original_config = br#"{"native_catalog_path":"/synthetic/codex/models_cache.json","providers":[],"preserved":"through update"}"#.to_vec();
    std::fs::write(&config, &original_config).unwrap();
    let pid_file = root.join("candidate.pid");
    let (port, bootstrap, process, stdout) = start_emp(
        &config,
        &executable,
        repository,
        &format!("{repository}/latest"),
        &pid_file,
    );
    let bootstrap_response = request(port, "GET", &format!("/?bootstrap={bootstrap}"), None, None);
    assert_eq!(status(&bootstrap_response), 303, "bootstrap login response");
    let cookie = response_header(&bootstrap_response, "Set-Cookie")
        .expect("session cookie")
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    RunningEmp {
        port,
        cookie,
        process,
        executable,
        config,
        original_config,
        _stdout: stdout,
    }
}

#[test]
fn fake_release_http_install_replaces_running_process_and_preserves_config() {
    let temp = TempDir::new().unwrap();
    let package = create_package(temp.path());
    let (repository, release_server) = release_server(package);
    let mut emp = start_authenticated_emp(temp.path(), &repository);

    let idle = request(emp.port, "GET", "/api/updates", Some(&emp.cookie), None);
    assert_eq!(status(&idle), 200);
    assert_eq!(body_json(&idle)["state"], "idle");

    let check = request(
        emp.port,
        "POST",
        "/api/updates/check",
        Some(&emp.cookie),
        Some(b"{}"),
    );
    assert_eq!(status(&check), 202, "check request");
    let available = wait_for_state(emp.port, &emp.cookie, "available", Duration::from_secs(10));
    assert_eq!(available["latest_version"], "0.12.0");

    let install = request(
        emp.port,
        "POST",
        "/api/updates/install",
        Some(&emp.cookie),
        Some(b"{}"),
    );
    assert_eq!(status(&install), 202, "install request");
    let quit = request(
        emp.port,
        "POST",
        "/api/quit",
        Some(&emp.cookie),
        Some(b"{}"),
    );
    assert_eq!(
        status(&quit),
        409,
        "quit is rejected during update installation"
    );
    let health = request(emp.port, "GET", "/healthz", None, None);
    assert_eq!(
        status(&health),
        200,
        "health remains available during update"
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if emp.process.0.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "old EMP process did not shut down after handoff"
        );
        thread::sleep(Duration::from_millis(25));
    }
    let candidate_pid = loop {
        if let Ok(pid) = std::fs::read_to_string(temp.path().join("candidate.pid")) {
            break pid.trim().parse::<u32>().expect("candidate PID");
        }
        assert!(Instant::now() < deadline, "candidate did not start");
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(std::fs::read(&emp.config).unwrap(), emp.original_config);
    assert_eq!(
        std::fs::read(&emp.executable).unwrap(),
        std::fs::read(temp.path().join("stage/EMP/EMP")).unwrap()
    );
    assert!(
        Command::new("kill")
            .args(["-TERM", &candidate_pid.to_string()])
            .status()
            .expect("terminate candidate process")
            .success()
    );
    let candidate_exit_deadline = Instant::now() + Duration::from_secs(5);
    while Path::new(&format!("/proc/{candidate_pid}")).exists() {
        assert!(
            Instant::now() < candidate_exit_deadline,
            "candidate process remained alive"
        );
        thread::sleep(Duration::from_millis(25));
    }
    release_server
        .join()
        .expect("fake release server completed");
}
