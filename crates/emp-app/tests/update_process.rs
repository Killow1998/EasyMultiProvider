#![cfg(target_os = "linux")]

#[allow(dead_code, reason = "Update tests only use the shared HTTP fixture")]
mod support;

use emp_state::VaultStore;
use emp_test_support::{Deadline, test_timeout};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::thread;
use std::time::Duration;

static EMP_STARTUP_FIXTURE: Mutex<()> = Mutex::new(());

/// The fake release is always one patch ahead of the version under test.
fn candidate_version() -> String {
    let current = env!("CARGO_PKG_VERSION");
    let (prefix, patch) = current.rsplit_once('.').expect("three-part version");
    format!(
        "{prefix}.{}",
        patch.parse::<u64>().expect("numeric patch") + 1
    )
}

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

fn request(
    port: u16,
    method: &str,
    path: &str,
    session: Option<&str>,
    body: Option<&[u8]>,
) -> Vec<u8> {
    try_request(port, method, path, session, body).expect("connect to EMP")
}

fn try_request(
    port: u16,
    method: &str,
    path: &str,
    session: Option<&str>,
    body: Option<&[u8]>,
) -> std::io::Result<Vec<u8>> {
    try_request_before(
        port,
        method,
        path,
        session,
        body,
        &Deadline::after(test_timeout(Duration::from_secs(10))),
    )
}

fn try_request_before(
    port: u16,
    method: &str,
    path: &str,
    session: Option<&str>,
    body: Option<&[u8]>,
    deadline: &Deadline,
) -> std::io::Result<Vec<u8>> {
    let stream = deadline.connect(([127, 0, 0, 1], port).into())?;
    let mut stream = deadline.io(stream);
    let payload = body.unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
    )?;
    if let Some(session) = session {
        write!(stream, "X-EMP-Session: {session}\r\n")?;
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
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    deadline.accept(response, path)
}

fn update_accepted(port: u16, session: &str, path: &str) -> Deadline {
    let response = support::http::http_before(
        port,
        "POST",
        path,
        &[
            ("X-EMP-Session", session),
            ("Content-Type", "application/json"),
        ],
        b"{}",
        &Deadline::after(test_timeout(Duration::from_secs(10))),
    );
    let deadline = Deadline::after(test_timeout(Duration::from_secs(10)));
    assert_eq!(response.status, 202, "{path}: {}", response.text());
    deadline
}

#[test]
fn accepted_update_does_not_wait_for_the_connection_to_close() {
    for framing in [
        b"Content-Length: 2\r\n\r\n{}".as_slice(),
        b"Transfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n",
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (received, finish) = mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = Deadline::after(test_timeout(Duration::from_secs(10))).io(stream);
            let _ = support::http::read_message(&mut stream, false);
            stream.write_all(b"HTTP/1.1 202 Accepted\r\n").unwrap();
            stream.write_all(framing).unwrap();
            finish.recv().unwrap();
        });
        let deadline = update_accepted(port, "fixture", "/api/updates/check");
        assert!(!deadline.is_expired());
        received.send(()).unwrap();
        server.join().unwrap();
    }
}

/// Exchange the one-use bootstrap token for a management session header.
fn bootstrap_session(port: u16, bootstrap: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to EMP");
    write!(
        stream,
        "POST /api/session HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         X-EMP-Bootstrap: {bootstrap}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = Vec::new();
    Deadline::after(test_timeout(Duration::from_secs(10)))
        .io(stream)
        .read_to_end(&mut response)
        .unwrap();
    assert_eq!(status(&response), 200, "bootstrap session response");
    let body: serde_json::Value =
        serde_json::from_slice(response_body(&response)).expect("session JSON");
    body["session"].as_str().expect("session token").to_owned()
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
if [ "${1:-}" = "--version" ]; then printf 'EMP {VERSION}\n'; exit 0; fi
exit 17
"#
    } else {
        r#"#!/bin/sh
set -eu
if [ "${1:-}" = "--version" ]; then printf 'EMP {VERSION}\n'; exit 0; fi
printf '%s' "$$" > "$EMP_UPDATE_TEST_PID"
if [ -n "${EMP_UPDATE_READY:-}" ]; then
  job="${EMP_UPDATE_READY%/ready.json}"
  nonce=$(sed -n 's/.*"nonce":"\([^"]*\)".*/\1/p' "$job/plan.json")
  printf '{"version":"{VERSION}","nonce":"%s"}\n' "$nonce" > "$EMP_UPDATE_READY"
fi
exec sleep 90
"#
    };
    std::fs::write(
        &candidate,
        script.replace("{VERSION}", &candidate_version()),
    )
    .unwrap();
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755)).unwrap();
    let archive = root.join("candidate.tar.gz");
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
    let archive = root.join("candidate.tar.gz");
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

fn release_server(package: Vec<u8>) -> (String, FakeReleaseServer) {
    let digest = format!("{:x}", Sha256::digest(&package));
    release_server_with_mode(package, digest, FakeReleaseMode::Valid)
}

struct FakeReleaseServer {
    ready: mpsc::Sender<()>,
    worker: thread::JoinHandle<()>,
}

impl FakeReleaseServer {
    fn start(&self) {
        self.ready.send(()).expect("start serving the fake release");
    }

    fn join(self) -> thread::Result<()> {
        self.worker.join()
    }
}

#[derive(Clone, Copy)]
enum FakeReleaseMode {
    Valid,
    InvalidJson,
    ReleaseError,
    PackageError,
    TemporaryPackageError,
}

fn release_server_with_mode(
    package: Vec<u8>,
    digest: String,
    mode: FakeReleaseMode,
) -> (String, FakeReleaseServer) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    let name = if cfg!(target_arch = "aarch64") {
        "EMP-linux-arm64.tar.gz"
    } else {
        "EMP-linux-x86_64.tar.gz"
    };
    let version = candidate_version();
    let asset_url = format!("{base}/releases/download/v{version}/{name}");
    let metadata = serde_json::json!({
        "tag_name":format!("v{version}"), "draft":false, "prerelease":false,
        "assets":[{"name":name,"digest":format!("sha256:{digest}"),"size":package.len(),"browser_download_url":asset_url}]
    }).to_string().into_bytes();
    let api_redirect = format!("{base}/api/latest");
    let artifact_redirect = format!("{base}/assets/{name}");
    let (ready, started) = mpsc::channel();
    let worker = thread::spawn(move || {
        if started.recv().is_err() {
            return;
        }
        let deadline = Deadline::after(test_timeout(Duration::from_secs(20)));
        let mut handled = 0;
        let expected_requests = match mode {
            FakeReleaseMode::InvalidJson => 2,
            FakeReleaseMode::ReleaseError => 8,
            FakeReleaseMode::Valid => 4,
            FakeReleaseMode::PackageError => 10,
            FakeReleaseMode::TemporaryPackageError => 6,
        };
        while handled < expected_requests && !deadline.is_expired() {
            let (stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "假上游以固定间隔重试非阻塞 accept，避免空转"
                    )]
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("fake release accept failed: {error}"),
            };
            let mut stream = deadline.io(stream);
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
                    FakeReleaseMode::Valid
                    | FakeReleaseMode::PackageError
                    | FakeReleaseMode::TemporaryPackageError => {
                        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", metadata.len()).unwrap();
                        stream.write_all(&metadata).unwrap();
                    }
                }
            } else if path == format!("/releases/download/v{version}/{name}") {
                write!(stream, "HTTP/1.1 302 Found\r\nLocation: {artifact_redirect}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            } else if path == format!("/assets/{name}") {
                if matches!(mode, FakeReleaseMode::PackageError)
                    || (matches!(mode, FakeReleaseMode::TemporaryPackageError) && handled == 3)
                {
                    stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", package.len()).unwrap();
                    let split = package.len() / 2;
                    stream.write_all(&package[..split]).unwrap();
                    stream.flush().unwrap();
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "分段发送更新包并固定延迟，模拟慢下载期间的并发请求"
                    )]
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
        let extra_deadline = Deadline::after(test_timeout(Duration::from_millis(300)));
        while !extra_deadline.is_expired() {
            match listener.accept() {
                Ok(_) => panic!("duplicate release or package request observed"),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "假上游以固定间隔重试非阻塞 accept，避免空转"
                    )]
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fake release server failed: {error}"),
            }
        }
    });
    (base, FakeReleaseServer { ready, worker })
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
    // Do not let parallel process fixtures probe the developer's Codex/nvm
    // installations through runtime discovery.
    let fixture_home = config.parent().unwrap().join("fixture-home");
    let dpkg = fixture_home.join("dpkg");
    std::fs::create_dir_all(dpkg.join("info")).unwrap();
    std::fs::write(dpkg.join("status"), []).unwrap();
    let fixture_bin = fixture_home.join("bin");
    std::fs::create_dir_all(&fixture_bin).unwrap();
    for utility in ["sed", "sleep"] {
        let executable = [Path::new("/usr/bin"), Path::new("/bin")]
            .into_iter()
            .map(|directory| directory.join(utility))
            .find(|path| path.is_file())
            .unwrap_or_else(|| panic!("system {utility} executable is required"));
        std::os::unix::fs::symlink(executable, fixture_bin.join(utility)).unwrap();
    }
    let reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port().to_string();
    drop(reservation);
    let mut command = Command::new(executable);
    command
        .args([
            "serve",
            "--config",
            &config.to_string_lossy(),
            "--host",
            "127.0.0.1",
            "--port",
            &port,
        ])
        .env("EMP_UPDATE_TEST_REPOSITORY_URL", repository)
        .env("EMP_UPDATE_TEST_API_URL", api)
        .env("EMP_UPDATE_TEST_PID", pid_file)
        .env("DPKG_ADMINDIR", &dpkg)
        .env("HOME", &fixture_home)
        .env("CODEX_HOME", fixture_home.join(".codex"))
        .env("XDG_CONFIG_HOME", fixture_home.join(".config"))
        .env("XDG_DATA_HOME", fixture_home.join(".local/share"))
        .env("XDG_CACHE_HOME", fixture_home.join(".cache"))
        .env_remove("EASY_MULTI_PROVIDER_CONFIG")
        .env_remove("EASY_MULTI_PROVIDER_MASTER_KEY_FILE")
        .env_remove("EASY_MULTI_PROVIDER_MASTER_KEY")
        .env_remove("EMP_OFFICIAL_MODELS_PATH")
        .env_remove("EMP_UPDATE_READY")
        .env_remove("EMP_UPDATE_RESULT")
        .env_remove("NVM_DIR")
        .env_remove("USERPROFILE")
        .env("PATH", &fixture_bin)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if fail_worker_ready {
        command.env("EMP_UPDATE_TEST_WORKER_FAIL_READY", "1");
    } else {
        command.env_remove("EMP_UPDATE_TEST_WORKER_FAIL_READY");
    }
    let mut process = command.spawn().unwrap_or_else(|error| {
        panic!(
            "start EMP process for test {} at {}: {error}",
            std::thread::current().name().unwrap_or("unnamed"),
            executable.display()
        )
    });
    let mut stdout = BufReader::new(process.stdout.take().unwrap());
    let child = ChildGuard(process);
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let port = line
        .trim_end()
        .strip_prefix("EMP listening on http://127.0.0.1:")
        .unwrap_or_else(|| panic!("unexpected EMP readiness line: {line:?}"))
        .parse::<u16>()
        .unwrap_or_else(|_| panic!("EMP readiness line has invalid port: {line:?}"));
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let expected_config = std::fs::canonicalize(config).unwrap();
    assert_eq!(
        line.trim_end(),
        format!("Configuration file: {}", expected_config.display()),
        "unexpected configuration startup line"
    );
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert!(
        matches!(
            line.trim_end(),
            "Network proxy: environment" | "Network proxy: system" | "Network proxy: direct"
        ),
        "unexpected network startup line: {line:?}"
    );
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let url = line
        .trim_end()
        .strip_prefix("Open in browser: ")
        .unwrap_or_else(|| panic!("unexpected browser startup line: {line:?}"));
    let token = url
        .split_once("bootstrap=")
        .unwrap_or_else(|| panic!("browser URL has no bootstrap token: {url:?}"))
        .1
        .to_owned();
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

fn wait_for_state(
    port: u16,
    session: &str,
    expected: &str,
    deadline: &Deadline,
) -> serde_json::Value {
    loop {
        let response =
            try_request_before(port, "GET", "/api/updates", Some(session), None, deadline)
                .unwrap_or_else(|error| panic!("waiting for update {expected}: {error}"));
        assert_eq!(status(&response), 200, "update snapshot request");
        let snapshot = body_json(&response);
        if snapshot["state"] == expected {
            return deadline
                .accept(snapshot, format_args!("update {expected}"))
                .expect("update result arrived after deadline");
        }
        assert_ne!(snapshot["state"], "error", "update failed: {snapshot}");
        assert!(
            !deadline.is_expired(),
            "timed out waiting for {expected}: {snapshot}"
        );
        #[expect(
            clippy::disallowed_methods,
            reason = "管理接口没有状态推送，此 helper 按固定间隔轮询更新状态"
        )]
        thread::sleep(deadline.remaining().min(Duration::from_millis(25)));
    }
}

fn wait_for_error(
    port: u16,
    session: &str,
    expected: &str,
    deadline: &Deadline,
) -> serde_json::Value {
    loop {
        let response =
            try_request_before(port, "GET", "/api/updates", Some(session), None, deadline)
                .unwrap_or_else(|error| panic!("waiting for update {expected}: {error}"));
        assert_eq!(status(&response), 200, "update snapshot request");
        let snapshot = body_json(&response);
        if snapshot["state"] == "error" {
            assert_eq!(snapshot["error"], expected, "update error snapshot");
            return deadline
                .accept(snapshot, format_args!("update {expected}"))
                .expect("update result arrived after deadline");
        }
        assert!(
            !deadline.is_expired(),
            "timed out waiting for error {expected}: {snapshot}"
        );
        #[expect(
            clippy::disallowed_methods,
            reason = "管理接口没有状态推送，此 helper 按固定间隔轮询更新错误"
        )]
        thread::sleep(deadline.remaining().min(Duration::from_millis(10)));
    }
}

fn assert_update_accepted_wire(response: &[u8]) {
    let body = response_body(response);
    let head = format!(
        concat!(
            "HTTP/1.1 202 Accepted\r\n",
            "Content-Type: application/json\r\n",
            "Content-Length: {}\r\n",
            "Cache-Control: no-store\r\n",
            "X-Frame-Options: DENY\r\n",
            "Content-Security-Policy: frame-ancestors 'none'\r\n",
            "X-Content-Type-Options: nosniff\r\n",
            "Connection: close\r\n\r\n"
        ),
        body.len()
    );
    assert_eq!(&response[..response.len() - body.len()], head.as_bytes());
    assert_eq!(
        body_json(response)["current_version"],
        env!("CARGO_PKG_VERSION")
    );
}

fn check_for_available(emp: &RunningEmp) {
    let check = request(
        emp.port,
        "POST",
        "/api/updates/check",
        Some(&emp.session),
        Some(b"{}"),
    );
    assert_update_accepted_wire(&check);
    let available = wait_for_state(
        emp.port,
        &emp.session,
        "available",
        &Deadline::after(test_timeout(Duration::from_secs(10))),
    );
    assert_eq!(available["latest_version"], candidate_version());
}

fn stop_emp(emp: &mut RunningEmp) {
    if emp.process.0.try_wait().unwrap().is_none() {
        let quit = request(
            emp.port,
            "POST",
            "/api/quit",
            Some(&emp.session),
            Some(b"{}"),
        );
        assert_eq!(status(&quit), 200, "idle EMP quits cleanly");
        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        while emp.process.0.try_wait().unwrap().is_none() {
            assert!(!deadline.is_expired(), "EMP did not stop after quit");
            #[expect(
                clippy::disallowed_methods,
                reason = "子进程退出需保留取消或超时检查，按既有固定间隔轮询"
            )]
            thread::sleep(Duration::from_millis(10));
        }
        deadline.accept((), "EMP process exit").unwrap();
    }
}

struct RunningEmp {
    port: u16,
    session: String,
    process: ChildGuard,
    executable: std::path::PathBuf,
    original_executable: Vec<u8>,
    config: std::path::PathBuf,
    original_config: Vec<u8>,
    account_auth: std::path::PathBuf,
    original_account_auth: Vec<u8>,
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
    let _startup = EMP_STARTUP_FIXTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let installation = root.join("install");
    std::fs::create_dir(&installation).unwrap();
    let executable = installation.join("EMP");
    let original_executable = std::fs::read(env!("CARGO_BIN_EXE_EMP")).unwrap();
    let copy = Command::new("cp")
        .args(["-p", env!("CARGO_BIN_EXE_EMP")])
        .arg(&executable)
        .status()
        .expect("copy EMP installation");
    assert!(copy.success(), "copy EMP installation: {copy}");
    let config = root.join("config.json");
    let account_store = root.join("state/accounts");
    let account_auth = account_store.join("update-fixture/auth.json.enc");
    let original_config = serde_json::to_vec(&serde_json::json!({
        "native_catalog_path":"/synthetic/codex/models_cache.json",
        "providers":[],
        "preserved":"through update",
        "account_store_path":account_store,
        "accounts":[{
            "id":"update-fixture",
            "name":"Update fixture",
            "prefix":"fixture",
            "auth_file":account_auth,
            // What EMP's startup quota sample records for this token, so the
            // sample leaves the config bytes as written.
            "credential_status":"invalid",
        }],
    }))
    .unwrap();
    std::fs::write(&config, &original_config).unwrap();
    let vault = VaultStore::from_sources(None, &root.join("state/master.key")).unwrap();
    vault
        .write_encrypted_json(
            &account_auth,
            &serde_json::json!({"tokens":{"access_token":"update-fixture-token"}}),
        )
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    emp_state::usage::pricing::PriceCatalog::new(root.join("state/api_prices.json"), now)
        .save_litellm_snapshot(
            &serde_json::json!({"update-fixture":{"input_cost_per_token":0,"output_cost_per_token":0}}),
            now,
        )
        .unwrap();
    let original_account_auth = std::fs::read(&account_auth).unwrap();
    let pid_file = root.join("candidate.pid");
    let (port, bootstrap, process, stdout) = start_emp(
        &config,
        &executable,
        repository,
        api,
        &pid_file,
        fail_worker_ready,
    );
    let session = bootstrap_session(port, &bootstrap);
    RunningEmp {
        port,
        session,
        process,
        executable,
        original_executable,
        config,
        original_config,
        account_auth,
        original_account_auth,
        _stdout: stdout,
    }
}

#[test]
fn fake_release_http_install_replaces_running_process_and_preserves_config() {
    let temp = emp_test_support::fixture_dir().unwrap();
    let package = create_package(temp.path());
    let digest = format!("{:x}", Sha256::digest(&package));
    let (repository, release_server) =
        release_server_with_mode(package, digest, FakeReleaseMode::TemporaryPackageError);
    let mut emp = start_authenticated_emp(temp.path(), &repository, false);

    let idle = request(emp.port, "GET", "/api/updates", Some(&emp.session), None);
    assert_eq!(status(&idle), 200);
    assert_eq!(body_json(&idle)["state"], "idle");

    release_server.start();
    check_for_available(&emp);

    let install_gate = Arc::new(Barrier::new(3));
    let mut installs = Vec::new();
    for _ in 0..2 {
        let gate = Arc::clone(&install_gate);
        let session = emp.session.clone();
        let port = emp.port;
        installs.push(thread::spawn(move || {
            gate.wait();
            request(
                port,
                "POST",
                "/api/updates/install",
                Some(&session),
                Some(b"{}"),
            )
        }));
    }
    install_gate.wait();
    for install in installs {
        assert_update_accepted_wire(&install.join().unwrap());
    }

    let gate_deadline = Deadline::after(test_timeout(Duration::from_secs(10)));
    let mut observed_closed_gate = false;
    let mut delete_during_update = None;
    let mut config_during_update = None;
    let mut auth_during_update = None;
    let mut observed_retry = false;
    while !gate_deadline.is_expired() && emp.process.0.try_wait().unwrap().is_none() {
        let snapshot = body_json(
            &try_request_before(
                emp.port,
                "GET",
                "/api/updates",
                Some(&emp.session),
                None,
                &gate_deadline,
            )
            .expect("request while observing update gate"),
        );
        if snapshot["state"] == "downloading" && snapshot["retry_count"] == 1 {
            assert_eq!(snapshot["retry_limit"], 3);
            observed_retry = true;
        }
        if matches!(snapshot["state"].as_str(), Some("waiting" | "installing")) {
            let quit = try_request_before(
                emp.port,
                "POST",
                "/api/quit",
                Some(&emp.session),
                Some(b"{}"),
                &gate_deadline,
            )
            .expect("request while observing update gate");
            match status(&quit) {
                503 => {
                    observed_closed_gate = true;
                    let update_request = try_request_before(
                        emp.port,
                        "POST",
                        "/api/updates/check",
                        Some(&emp.session),
                        Some(b"{}"),
                        &gate_deadline,
                    )
                    .expect("request while observing update gate");
                    assert_eq!(
                        status(&update_request),
                        202,
                        "updater endpoints remain available while ordinary mutations drain"
                    );
                    assert!(matches!(
                        body_json(&update_request)["state"].as_str(),
                        Some("waiting" | "installing")
                    ));
                    delete_during_update = Some(
                        try_request_before(
                            emp.port,
                            "DELETE",
                            "/api/accounts/update-fixture",
                            Some(&emp.session),
                            None,
                            &gate_deadline,
                        )
                        .expect("request while observing update gate"),
                    );
                    config_during_update = Some(std::fs::read(&emp.config).unwrap());
                    auth_during_update = Some(std::fs::read(&emp.account_auth).unwrap());
                    gate_deadline.accept((), "closed update gate").unwrap();
                    break;
                }
                409 => {}
                code => panic!("quit during update returned unexpected status {code}"),
            }
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "按既有间隔采样更新重试和管理请求排空状态"
        )]
        thread::sleep(gate_deadline.remaining().min(Duration::from_millis(2)));
    }
    assert!(
        observed_closed_gate,
        "waiting update drains ordinary POST requests"
    );
    assert!(
        observed_retry,
        "the management API exposes the automatic retry"
    );
    let health = request(emp.port, "GET", "/healthz", None, None);
    assert_eq!(
        status(&health),
        200,
        "health remains available during update"
    );

    let deadline = Deadline::after(test_timeout(Duration::from_secs(20)));
    loop {
        if emp.process.0.try_wait().unwrap().is_some() {
            deadline.accept((), "old EMP exit").unwrap();
            break;
        }
        assert!(
            !deadline.is_expired(),
            "old EMP process did not shut down after handoff"
        );
        #[expect(
            clippy::disallowed_methods,
            reason = "子进程退出需保留取消或超时检查，按既有固定间隔轮询"
        )]
        thread::sleep(Duration::from_millis(25));
    }
    let candidate_pid = loop {
        if let Ok(pid) = std::fs::read_to_string(temp.path().join("candidate.pid"))
            && let Ok(pid) = pid.trim().parse::<u32>()
        {
            break deadline.accept(pid, "candidate startup").unwrap();
        }
        assert!(!deadline.is_expired(), "candidate did not start");
        #[expect(
            clippy::disallowed_methods,
            reason = "候选进程通过 PID 文件报告启动，以固定间隔轮询"
        )]
        thread::sleep(Duration::from_millis(25));
    };
    let success_deadline = Deadline::after(test_timeout(Duration::from_secs(20)));
    loop {
        let installation = emp.executable.parent().expect("installation directory");
        let success_marker_exists = std::fs::read_dir(installation)
            .expect("read installation jobs")
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".emp-update-")
                    && entry.path().join("success").is_file()
            });
        if success_marker_exists {
            success_deadline.accept((), "candidate readiness").unwrap();
            break;
        }
        assert!(
            !success_deadline.is_expired(),
            "update worker did not accept candidate readiness before timeout"
        );
        #[expect(
            clippy::disallowed_methods,
            reason = "更新成功通过文件标记报告，以固定间隔轮询"
        )]
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        Command::new("kill")
            .args(["-TERM", &candidate_pid.to_string()])
            .status()
            .expect("terminate candidate process")
            .success()
    );
    let candidate_exit_deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    while Path::new(&format!("/proc/{candidate_pid}")).exists() {
        assert!(
            !candidate_exit_deadline.is_expired(),
            "candidate process remained alive"
        );
        #[expect(
            clippy::disallowed_methods,
            reason = "候选进程退出通过 procfs 观察，以固定间隔轮询"
        )]
        thread::sleep(Duration::from_millis(25));
    }
    candidate_exit_deadline
        .accept((), "candidate exit")
        .unwrap();
    release_server
        .join()
        .expect("fake release server completed");
    let delete = delete_during_update.expect("DELETE was attempted with the gate closed");
    assert_eq!(
        status(&delete),
        503,
        "account DELETE is rejected while installation drains requests"
    );
    assert_eq!(
        config_during_update.expect("capture config during drain"),
        emp.original_config,
        "a rejected DELETE leaves configuration bytes unchanged"
    );
    assert_eq!(
        auth_during_update.expect("capture encrypted auth during drain"),
        emp.original_account_auth,
        "a rejected DELETE leaves encrypted credentials unchanged"
    );
    assert_eq!(std::fs::read(&emp.config).unwrap(), emp.original_config);
    assert_eq!(
        std::fs::read(&emp.account_auth).unwrap(),
        emp.original_account_auth
    );
    let installed_hash = format!(
        "{:x}",
        Sha256::digest(std::fs::read(&emp.executable).unwrap())
    );
    let candidate_hash = format!(
        "{:x}",
        Sha256::digest(std::fs::read(temp.path().join("stage/EMP/EMP")).unwrap())
    );
    assert_eq!(
        installed_hash, candidate_hash,
        "installed executable digest differs from candidate"
    );
    // The successful handoff cleans staging, but timings must remain in the
    // ordinary journal so a slow update can be diagnosed after restart.
    let mut events = Vec::new();
    for entry in std::fs::read_dir(temp.path().join("state/logs")).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            for line in std::fs::read_to_string(path).unwrap().lines() {
                events.push(serde_json::from_str::<serde_json::Value>(line).unwrap());
            }
        }
    }
    assert!(events.iter().any(|event| event["event"] == "update_retry"));
    let download = events
        .iter()
        .find(|event| event["event"] == "update_download")
        .unwrap();
    assert_eq!(
        download["fields"]["received_bytes"],
        download["fields"]["expected_bytes"]
    );
    assert!(download["fields"]["duration_ms"].is_number());
    for stage in [
        "download_package",
        "verify_checksum",
        "verify_version",
        "drain_requests",
        "wait_worker",
    ] {
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "update_stage_finished"
                    && event["fields"]["stage"] == stage),
            "missing timing for {stage}"
        );
    }
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
        let root = emp_test_support::fixture_dir().unwrap();
        let case = root.path().join(name);
        std::fs::create_dir(&case).unwrap();
        let (repository, server) =
            release_server_with_mode(b"unused".to_vec(), "0".repeat(64), mode);
        let mut emp = start_authenticated_emp(&case, &repository, false);
        server.start();
        let deadline = update_accepted(emp.port, &emp.session, "/api/updates/check");
        wait_for_error(emp.port, &emp.session, expected, &deadline);
        server.join().expect("fake release error server completed");
        stop_emp(&mut emp);
    }

    let root = emp_test_support::fixture_dir().unwrap();
    let case = root.path().join("connection");
    std::fs::create_dir(&case).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let unused = listener.local_addr().unwrap();
    drop(listener);
    let base = format!("http://{unused}");
    let mut emp = start_authenticated_emp_with_api(&case, &base, &format!("{base}/latest"), false);
    let deadline = update_accepted(emp.port, &emp.session, "/api/updates/check");
    wait_for_error(emp.port, &emp.session, "update_failed", &deadline);
    stop_emp(&mut emp);

    let root = emp_test_support::fixture_dir().unwrap();
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
    let deadline = update_accepted(emp.port, &emp.session, "/api/updates/check");
    wait_for_error(emp.port, &emp.session, "update_failed", &deadline);
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
        let root = emp_test_support::fixture_dir().unwrap();
        let case = root.path().join(name);
        std::fs::create_dir(&case).unwrap();
        let package = if matches!(mode, FakeReleaseMode::PackageError) {
            b"unused".to_vec()
        } else {
            create_package(&case)
        };
        let (repository, server) = release_server_with_mode(package, digest, mode);
        let mut emp = start_authenticated_emp(&case, &repository, false);
        server.start();
        check_for_available(&emp);
        let deadline = update_accepted(emp.port, &emp.session, "/api/updates/install");
        wait_for_error(emp.port, &emp.session, expected, &deadline);
        server.join().expect("fake package error server completed");
        assert_eq!(
            std::fs::read(&emp.executable).unwrap(),
            emp.original_executable
        );
        assert_eq!(std::fs::read(&emp.config).unwrap(), emp.original_config);
        stop_emp(&mut emp);
    }
}

#[test]
fn malicious_symlink_package_is_rejected_before_replacement() {
    let temp = emp_test_support::fixture_dir().unwrap();
    let package = create_symlink_package(temp.path());
    let (repository, server) = release_server(package);
    let mut emp = start_authenticated_emp(temp.path(), &repository, false);
    server.start();
    check_for_available(&emp);
    let deadline = update_accepted(emp.port, &emp.session, "/api/updates/install");
    wait_for_error(emp.port, &emp.session, "invalid_package", &deadline);
    assert_eq!(
        std::fs::read(&emp.executable).unwrap(),
        emp.original_executable
    );
    stop_emp(&mut emp);
    server.join().expect("fake malicious release completed");
}

#[test]
fn worker_ready_failure_reopens_gate_and_reports_error() {
    let temp = emp_test_support::fixture_dir().unwrap();
    let package = create_package(temp.path());
    let (repository, server) = release_server(package);
    let mut emp = start_authenticated_emp(temp.path(), &repository, true);
    server.start();
    check_for_available(&emp);
    let deadline = update_accepted(emp.port, &emp.session, "/api/updates/install");
    wait_for_error(emp.port, &emp.session, "worker_failed", &deadline);
    let delete = request(
        emp.port,
        "DELETE",
        "/api/accounts/update-fixture",
        Some(&emp.session),
        None,
    );
    assert_eq!(
        status(&delete),
        200,
        "DELETE resumes after worker failure: {}",
        String::from_utf8_lossy(&delete)
    );
    assert_eq!(body_json(&delete)["status"], "ok");
    assert!(!emp.account_auth.exists(), "successful DELETE removes auth");
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&emp.config).unwrap()).unwrap();
    assert_eq!(config["accounts"], serde_json::json!([]));
    let quit = request(
        emp.port,
        "POST",
        "/api/quit",
        Some(&emp.session),
        Some(b"{}"),
    );
    assert_eq!(
        status(&quit),
        200,
        "ordinary POST resumes after worker failure"
    );
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    while emp.process.0.try_wait().unwrap().is_none() {
        assert!(!deadline.is_expired(), "EMP did not stop after quit");
        #[expect(
            clippy::disallowed_methods,
            reason = "子进程退出需保留取消或超时检查，按既有固定间隔轮询"
        )]
        thread::sleep(Duration::from_millis(10));
    }
    deadline.accept((), "EMP process exit").unwrap();
    server.join().expect("fake release server completed");
}

#[test]
fn failed_candidate_rolls_back_and_restarts_old_emp_with_visible_error() {
    let temp = emp_test_support::fixture_dir().unwrap();
    let package = create_failing_package(temp.path());
    let (repository, server) = release_server(package);
    let mut emp = start_authenticated_emp(temp.path(), &repository, false);
    server.start();
    check_for_available(&emp);
    let install = request(
        emp.port,
        "POST",
        "/api/updates/install",
        Some(&emp.session),
        Some(b"{}"),
    );
    assert_eq!(status(&install), 202);
    let old_exit_deadline = Deadline::after(test_timeout(Duration::from_secs(15)));
    while emp.process.0.try_wait().unwrap().is_none() {
        assert!(!old_exit_deadline.is_expired(), "old EMP did not hand off");
        #[expect(
            clippy::disallowed_methods,
            reason = "子进程退出需保留取消或超时检查，按既有固定间隔轮询"
        )]
        thread::sleep(Duration::from_millis(10));
    }
    old_exit_deadline.accept((), "EMP process exit").unwrap();

    let restart_deadline = Deadline::after(test_timeout(Duration::from_secs(15)));
    loop {
        if let Ok(response) = try_request_before(
            emp.port,
            "GET",
            "/healthz",
            None,
            None,
            &restart_deadline.capped(test_timeout(Duration::from_secs(10))),
        ) && status(&response) == 200
        {
            restart_deadline
                .accept((), "rollback health check")
                .unwrap();
            break;
        }
        assert!(
            !restart_deadline.is_expired(),
            "old EMP did not return after rollback"
        );
        #[expect(clippy::disallowed_methods, reason = "回滚服务以固定间隔轮询健康状态")]
        thread::sleep(Duration::from_millis(20));
    }
    let rolled_back = request(emp.port, "GET", "/api/updates", Some(&emp.session), None);
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
        Some(&emp.session),
        Some(b"{}"),
    );
    assert_eq!(status(&quit), 200);
    let shutdown_deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    loop {
        if let Err(error) =
            try_request_before(emp.port, "GET", "/healthz", None, None, &shutdown_deadline)
        {
            shutdown_deadline
                .accept((), format_args!("rolled-back EMP exit: {error}"))
                .unwrap();
            break;
        }
        assert!(
            !shutdown_deadline.is_expired(),
            "rolled-back EMP stayed alive"
        );
        #[expect(
            clippy::disallowed_methods,
            reason = "回滚后的退出以 HTTP 连接失败报告，以固定间隔轮询"
        )]
        thread::sleep(Duration::from_millis(20));
    }
    server.join().expect("fake rollback release completed");
}
