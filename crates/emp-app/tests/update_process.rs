#![cfg(target_os = "linux")]

use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
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
    try_request(port, method, path, cookie, body).expect("connect to EMP")
}

fn try_request(
    port: u16,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<&[u8]>,
) -> std::io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    let payload = body.unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
    )?;
    if let Some(cookie) = cookie {
        write!(stream, "Cookie: {cookie}\r\n")?;
    }
    if body.is_some() {
        write!(
            stream,
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        )?;
    }
    stream.write_all(b"Connection: close\r\n\r\n")?;
    stream.write_all(payload)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    Ok(response)
}

fn create_package(root: &Path) -> Vec<u8> {
    create_package_mode(root, false)
}

fn create_failing_package(root: &Path) -> Vec<u8> {
    create_package_mode(root, true)
}

fn create_package_mode(root: &Path, fail_startup: bool) -> Vec<u8> {
    let stage = root.join("stage");
    let app = stage.join("EMP");
    std::fs::create_dir_all(&app).unwrap();
    let candidate = app.join("EMP");
    let script = if fail_startup {
        r#"#!/bin/sh
set -eu
if [ "${1:-}" = "--version" ]; then printf 'EMP 0.12.0\n'; exit 0; fi
exit 17
"#
    } else {
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
"#
    };
    std::fs::write(&candidate, script).unwrap();
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

fn create_symlink_package(root: &Path) -> Vec<u8> {
    use std::os::unix::fs::symlink;

    let stage = root.join("malicious-stage");
    let app = stage.join("EMP");
    std::fs::create_dir_all(&app).unwrap();
    symlink("/bin/sh", app.join("EMP")).unwrap();
    let archive = root.join("EMP-linux-x86_64.tar.gz");
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(&archive)
        .args(["-C"])
        .arg(&stage)
        .arg("EMP")
        .status()
        .expect("run tar for symlink fixture");
    assert!(status.success());
    std::fs::read(archive).unwrap()
}

fn release_server(package: Vec<u8>) -> (String, thread::JoinHandle<()>) {
    let digest = format!("{:x}", Sha256::digest(&package));
    release_server_with_mode(package, digest, FakeReleaseMode::Valid)
}

#[derive(Clone, Copy)]
enum FakeReleaseMode {
    Valid,
    InvalidJson,
    ReleaseError,
    PackageError,
}

fn release_server_with_mode(
    package: Vec<u8>,
    digest: String,
    mode: FakeReleaseMode,
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
        let expected_requests = match mode {
            FakeReleaseMode::InvalidJson | FakeReleaseMode::ReleaseError => 2,
            FakeReleaseMode::Valid | FakeReleaseMode::PackageError => 4,
        };
        while handled < expected_requests && Instant::now() < deadline {
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
                match mode {
                    FakeReleaseMode::InvalidJson => {
                        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 9\r\nConnection: close\r\n\r\n{bad json").unwrap();
                    }
                    FakeReleaseMode::ReleaseError => {
                        stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    }
                    FakeReleaseMode::Valid | FakeReleaseMode::PackageError => {
                        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", metadata.len()).unwrap();
                        stream.write_all(&metadata).unwrap();
                    }
                }
            } else if path == format!("/releases/download/v0.12.0/{name}") {
                write!(stream, "HTTP/1.1 302 Found\r\nLocation: {artifact_redirect}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            } else if path == format!("/assets/{name}") {
                if matches!(mode, FakeReleaseMode::PackageError) {
                    stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", package.len()).unwrap();
                    let split = package.len() / 2;
                    stream.write_all(&package[..split]).unwrap();
                    stream.flush().unwrap();
                    thread::sleep(Duration::from_millis(800));
                    stream.write_all(&package[split..]).unwrap();
                }
            } else {
                write!(
                    stream,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
            handled += 1;
        }
        assert_eq!(
            handled, expected_requests,
            "release API, redirect and artifact requests"
        );
        listener.set_nonblocking(true).unwrap();
        let extra_deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < extra_deadline {
            match listener.accept() {
                Ok(_) => panic!("duplicate release or package request observed"),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fake release server failed: {error}"),
            }
        }
    });
    (base, thread)
}

fn start_emp(
    config: &Path,
    executable: &Path,
    repository: &str,
    api: &str,
    pid_file: &Path,
    fail_worker_ready: bool,
) -> (
    u16,
    String,
    ChildGuard,
    BufReader<std::process::ChildStdout>,
) {
    let mut command = Command::new(executable);
    command
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
        .stderr(Stdio::inherit());
    if fail_worker_ready {
        command.env("EMP_UPDATE_TEST_WORKER_FAIL_READY", "1");
    } else {
        command.env_remove("EMP_UPDATE_TEST_WORKER_FAIL_READY");
    }
    let mut process = command
        .spawn()
        .unwrap_or_else(|error| panic!("start EMP process {}: {error}", executable.display()));
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

fn wait_for_error(port: u16, cookie: &str, expected: &str, timeout: Duration) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        let response = request(port, "GET", "/api/updates", Some(cookie), None);
        assert_eq!(status(&response), 200, "update snapshot request");
        let snapshot = body_json(&response);
        if snapshot["state"] == "error" {
            assert_eq!(snapshot["error"], expected, "update error snapshot");
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for error {expected}: {snapshot}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn check_for_available(emp: &RunningEmp) {
    let check = request(
        emp.port,
        "POST",
        "/api/updates/check",
        Some(&emp.cookie),
        Some(b"{}"),
    );
    assert_eq!(
        status(&check),
        202,
        "check request: {}",
        String::from_utf8_lossy(&check)
    );
    let available = wait_for_state(emp.port, &emp.cookie, "available", Duration::from_secs(10));
    assert_eq!(available["latest_version"], "0.12.0");
}

fn stop_emp(emp: &mut RunningEmp) {
    if emp.process.0.try_wait().unwrap().is_none() {
        let quit = request(
            emp.port,
            "POST",
            "/api/quit",
            Some(&emp.cookie),
            Some(b"{}"),
        );
        assert_eq!(status(&quit), 200, "idle EMP quits cleanly");
        let deadline = Instant::now() + Duration::from_secs(5);
        while emp.process.0.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "EMP did not stop after quit");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

struct RunningEmp {
    port: u16,
    cookie: String,
    process: ChildGuard,
    executable: std::path::PathBuf,
    original_executable: Vec<u8>,
    config: std::path::PathBuf,
    original_config: Vec<u8>,
    _stdout: BufReader<std::process::ChildStdout>,
}

fn start_authenticated_emp(root: &Path, repository: &str, fail_worker_ready: bool) -> RunningEmp {
    start_authenticated_emp_with_api(
        root,
        repository,
        &format!("{repository}/latest"),
        fail_worker_ready,
    )
}

fn start_authenticated_emp_with_api(
    root: &Path,
    repository: &str,
    api: &str,
    fail_worker_ready: bool,
) -> RunningEmp {
    let installation = root.join("install");
    std::fs::create_dir(&installation).unwrap();
    let executable = installation.join("EMP");
    let original_executable = std::fs::read(env!("CARGO_BIN_EXE_EMP")).unwrap();
    let staged_executable = installation.join("EMP.staged");
    std::fs::write(&staged_executable, &original_executable).unwrap();
    std::fs::set_permissions(&staged_executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::OpenOptions::new()
        .read(true)
        .open(&staged_executable)
        .unwrap()
        .sync_all()
        .unwrap();
    std::fs::rename(&staged_executable, &executable).unwrap();
    let config = root.join("config.json");
    let original_config = br#"{"native_catalog_path":"/synthetic/codex/models_cache.json","providers":[],"preserved":"through update"}"#.to_vec();
    std::fs::write(&config, &original_config).unwrap();
    let pid_file = root.join("candidate.pid");
    let (port, bootstrap, process, stdout) = start_emp(
        &config,
        &executable,
        repository,
        api,
        &pid_file,
        fail_worker_ready,
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
        original_executable,
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
    let mut emp = start_authenticated_emp(temp.path(), &repository, false);

    let idle = request(emp.port, "GET", "/api/updates", Some(&emp.cookie), None);
    assert_eq!(status(&idle), 200);
    assert_eq!(body_json(&idle)["state"], "idle");

    check_for_available(&emp);

    let install_gate = Arc::new(Barrier::new(3));
    let mut installs = Vec::new();
    for _ in 0..2 {
        let gate = Arc::clone(&install_gate);
        let cookie = emp.cookie.clone();
        let port = emp.port;
        installs.push(thread::spawn(move || {
            gate.wait();
            request(
                port,
                "POST",
                "/api/updates/install",
                Some(&cookie),
                Some(b"{}"),
            )
        }));
    }
    install_gate.wait();
    for install in installs {
        assert_eq!(
            status(&install.join().unwrap()),
            202,
            "concurrent install request"
        );
    }

    let gate_deadline = Instant::now() + Duration::from_secs(10);
    let mut observed_closed_gate = false;
    while Instant::now() < gate_deadline && emp.process.0.try_wait().unwrap().is_none() {
        let snapshot = body_json(&request(
            emp.port,
            "GET",
            "/api/updates",
            Some(&emp.cookie),
            None,
        ));
        if matches!(snapshot["state"].as_str(), Some("waiting" | "installing")) {
            let quit = request(
                emp.port,
                "POST",
                "/api/quit",
                Some(&emp.cookie),
                Some(b"{}"),
            );
            match status(&quit) {
                503 => {
                    observed_closed_gate = true;
                    break;
                }
                409 => {}
                code => panic!("quit during update returned unexpected status {code}"),
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(
        observed_closed_gate,
        "waiting update drains ordinary POST requests"
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

#[test]
fn release_transport_json_and_http_failures_have_python_error_codes() {
    for (name, mode, expected) in [
        (
            "invalid-json",
            FakeReleaseMode::InvalidJson,
            "update_failed",
        ),
        (
            "release-http",
            FakeReleaseMode::ReleaseError,
            "check_failed",
        ),
    ] {
        let root = TempDir::new().unwrap();
        let case = root.path().join(name);
        std::fs::create_dir(&case).unwrap();
        let (repository, server) =
            release_server_with_mode(b"unused".to_vec(), "0".repeat(64), mode);
        let mut emp = start_authenticated_emp(&case, &repository, false);
        let check = request(
            emp.port,
            "POST",
            "/api/updates/check",
            Some(&emp.cookie),
            Some(b"{}"),
        );
        assert_eq!(
            status(&check),
            202,
            "check request: {}",
            String::from_utf8_lossy(&check)
        );
        wait_for_error(emp.port, &emp.cookie, expected, Duration::from_secs(10));
        stop_emp(&mut emp);
        server.join().expect("fake release error server completed");
    }

    let root = TempDir::new().unwrap();
    let case = root.path().join("connection");
    std::fs::create_dir(&case).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let unused = listener.local_addr().unwrap();
    drop(listener);
    let base = format!("http://{unused}");
    let mut emp = start_authenticated_emp_with_api(&case, &base, &format!("{base}/latest"), false);
    let check = request(
        emp.port,
        "POST",
        "/api/updates/check",
        Some(&emp.cookie),
        Some(b"{}"),
    );
    assert_eq!(status(&check), 202);
    wait_for_error(
        emp.port,
        &emp.cookie,
        "update_failed",
        Duration::from_secs(10),
    );
    stop_emp(&mut emp);

    let root = TempDir::new().unwrap();
    let case = root.path().join("truncated-response");
    std::fs::create_dir(&case).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let partial_address = listener.local_addr().unwrap();
    let partial_server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 512];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\n{")
            .unwrap();
        stream.flush().unwrap();
    });
    let partial_base = format!("http://{partial_address}");
    let mut emp = start_authenticated_emp_with_api(
        &case,
        &partial_base,
        &format!("{partial_base}/latest"),
        false,
    );
    let check = request(
        emp.port,
        "POST",
        "/api/updates/check",
        Some(&emp.cookie),
        Some(b"{}"),
    );
    assert_eq!(status(&check), 202);
    wait_for_error(
        emp.port,
        &emp.cookie,
        "update_failed",
        Duration::from_secs(10),
    );
    partial_server.join().unwrap();
    stop_emp(&mut emp);
}

#[test]
fn package_http_and_checksum_failures_preserve_the_running_installation() {
    for (name, mode, digest, expected) in [
        (
            "package-http",
            FakeReleaseMode::PackageError,
            format!("{:x}", Sha256::digest(b"unused")),
            "update_failed",
        ),
        (
            "checksum",
            FakeReleaseMode::Valid,
            "0".repeat(64),
            "checksum_mismatch",
        ),
    ] {
        let root = TempDir::new().unwrap();
        let case = root.path().join(name);
        std::fs::create_dir(&case).unwrap();
        let package = if matches!(mode, FakeReleaseMode::PackageError) {
            b"unused".to_vec()
        } else {
            create_package(&case)
        };
        let (repository, server) = release_server_with_mode(package, digest, mode);
        let mut emp = start_authenticated_emp(&case, &repository, false);
        check_for_available(&emp);
        let install = request(
            emp.port,
            "POST",
            "/api/updates/install",
            Some(&emp.cookie),
            Some(b"{}"),
        );
        assert_eq!(status(&install), 202);
        wait_for_error(emp.port, &emp.cookie, expected, Duration::from_secs(10));
        assert_eq!(
            std::fs::read(&emp.executable).unwrap(),
            emp.original_executable
        );
        assert_eq!(std::fs::read(&emp.config).unwrap(), emp.original_config);
        stop_emp(&mut emp);
        server.join().expect("fake package error server completed");
    }
}

#[test]
fn malicious_symlink_package_is_rejected_before_replacement() {
    let temp = TempDir::new().unwrap();
    let package = create_symlink_package(temp.path());
    let (repository, server) = release_server(package);
    let mut emp = start_authenticated_emp(temp.path(), &repository, false);
    check_for_available(&emp);
    let install = request(
        emp.port,
        "POST",
        "/api/updates/install",
        Some(&emp.cookie),
        Some(b"{}"),
    );
    assert_eq!(status(&install), 202);
    wait_for_error(
        emp.port,
        &emp.cookie,
        "invalid_package",
        Duration::from_secs(10),
    );
    assert_eq!(
        std::fs::read(&emp.executable).unwrap(),
        emp.original_executable
    );
    stop_emp(&mut emp);
    server.join().expect("fake malicious release completed");
}

#[test]
fn worker_ready_failure_reopens_gate_and_reports_error() {
    let temp = TempDir::new().unwrap();
    let package = create_package(temp.path());
    let (repository, server) = release_server(package);
    let mut emp = start_authenticated_emp(temp.path(), &repository, true);
    check_for_available(&emp);
    let install = request(
        emp.port,
        "POST",
        "/api/updates/install",
        Some(&emp.cookie),
        Some(b"{}"),
    );
    assert_eq!(status(&install), 202);
    wait_for_error(
        emp.port,
        &emp.cookie,
        "worker_failed",
        Duration::from_secs(10),
    );
    let quit = request(
        emp.port,
        "POST",
        "/api/quit",
        Some(&emp.cookie),
        Some(b"{}"),
    );
    assert_eq!(
        status(&quit),
        200,
        "ordinary POST resumes after worker failure"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while emp.process.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "EMP did not stop after quit");
        thread::sleep(Duration::from_millis(10));
    }
    server.join().expect("fake release server completed");
}

#[test]
fn failed_candidate_rolls_back_and_restarts_old_emp_with_visible_error() {
    let temp = TempDir::new().unwrap();
    let package = create_failing_package(temp.path());
    let (repository, server) = release_server(package);
    let mut emp = start_authenticated_emp(temp.path(), &repository, false);
    check_for_available(&emp);
    let install = request(
        emp.port,
        "POST",
        "/api/updates/install",
        Some(&emp.cookie),
        Some(b"{}"),
    );
    assert_eq!(status(&install), 202);
    let old_exit_deadline = Instant::now() + Duration::from_secs(15);
    while emp.process.0.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < old_exit_deadline,
            "old EMP did not hand off"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let restart_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = try_request(emp.port, "GET", "/healthz", None, None)
            && status(&response) == 200
        {
            break;
        }
        assert!(
            Instant::now() < restart_deadline,
            "old EMP did not return after rollback"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let rolled_back = request(emp.port, "GET", "/api/updates", Some(&emp.cookie), None);
    assert_eq!(status(&rolled_back), 200);
    assert_eq!(body_json(&rolled_back)["state"], "error");
    assert_eq!(body_json(&rolled_back)["error"], "install_rolled_back");
    assert_eq!(
        std::fs::read(&emp.executable).unwrap(),
        emp.original_executable
    );

    let quit = request(
        emp.port,
        "POST",
        "/api/quit",
        Some(&emp.cookie),
        Some(b"{}"),
    );
    assert_eq!(status(&quit), 200);
    let shutdown_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if try_request(emp.port, "GET", "/healthz", None, None).is_err() {
            break;
        }
        assert!(
            Instant::now() < shutdown_deadline,
            "rolled-back EMP stayed alive"
        );
        thread::sleep(Duration::from_millis(20));
    }
    server.join().expect("fake rollback release completed");
}
