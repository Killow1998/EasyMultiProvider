mod report;
mod upstream;
mod wire;

use crate::Result;
use report::{HttpRun, ProcessUsage, Quantiles, Report, WebSockets, milliseconds};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Barrier, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use upstream::Upstream;
use wire::{IO_TIMEOUT, Socket};

const USAGE: &str = "usage: cargo xtask server-bench --bin PATH [--label NAME] [--out JSON]\n       cargo xtask server-bench --compare BASELINE.json CANDIDATE.json";
const TOKEN: &str = "server-bench-native-token";
const HTTP_REQUESTS: usize = 2000;
const SSE_CLIENTS: usize = 16;
const WS_SESSIONS: usize = 64;
const SETTLE: Duration = Duration::from_secs(1);

pub(crate) fn run(args: &[String]) -> Result {
    if let [flag, baseline, candidate] = args
        && flag == "--compare"
    {
        let baseline = Report::load(Path::new(baseline))?;
        let candidate = Report::load(Path::new(candidate))?;
        print!("{}", baseline.compare(&candidate)?);
        return Ok(());
    }
    if matches!(args, [flag] if flag == "--help" || flag == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let (mut binary, mut label, mut out) = (None, None, None);
    let mut pairs = args.chunks_exact(2);
    for pair in &mut pairs {
        let slot = match pair[0].as_str() {
            "--bin" => &mut binary,
            "--label" => &mut label,
            "--out" => &mut out,
            _ => return Err(USAGE.into()),
        };
        if slot.replace(pair[1].as_str()).is_some() {
            return Err(USAGE.into());
        }
    }
    if !pairs.remainder().is_empty() {
        return Err(USAGE.into());
    }
    let binary = crate::physical(Path::new(binary.ok_or(USAGE)?))?;
    let label = label.unwrap_or_else(|| {
        binary
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("EMP")
    });
    let directory = crate::temporary_directory("emp-bench-")?;
    let root = crate::physical(directory.path())?;
    let upstream =
        Upstream::start().map_err(|error| format!("start loopback upstream: {error}"))?;
    let with_stderr = |error: io::Error| {
        let stderr = fs::read_to_string(root.join("stderr.log")).unwrap_or_default();
        if stderr.trim().is_empty() {
            error.to_string()
        } else {
            format!("{error}\nEMP stderr: {}", stderr.trim())
        }
    };
    let (report, exit) = measure(&binary, label, &root, &upstream).map_err(with_stderr)?;
    if let Some(out) = out {
        report.save(Path::new(out))?;
    }
    print!("{}", report.table());
    exit.map_err(with_stderr)?;
    Ok(())
}

fn measure(
    binary: &Path,
    label: &str,
    root: &Path,
    upstream: &Upstream,
) -> io::Result<(Report, io::Result<Duration>)> {
    let listener = wire::bind_high_port()?;
    let port = listener.local_addr()?.port();
    let mut command = emp_command(binary, root, port, upstream.port)?;
    drop(listener);
    let started = Instant::now();
    let mut process = Emp(command.spawn()?);
    let (startup, bootstrap) = process.ready(port, started)?;
    let exchange = wire::http_request(
        port,
        "POST",
        "/api/session",
        &format!("X-EMP-Bootstrap: {bootstrap}\r\n"),
        "",
    );
    let (head, socket) = wire::request(port, &exchange, IO_TIMEOUT)?;
    head.expect_status(200)?;
    let session: Value = serde_json::from_reader(head.body(socket)?).map_err(io::Error::other)?;
    let token = session["session"]
        .as_str()
        .ok_or(io::Error::other("missing management session"))?;

    settle(SETTLE);
    let idle = ProcessUsage::read(process.0.id())?;
    let json = inference_request(port, "bench/model", false);
    measure_json(port, &json)?;
    let mut http = Vec::new();
    for concurrency in [1, 4, 16] {
        let (latency, wall) = parallel(concurrency, HTTP_REQUESTS, || measure_json(port, &json))
            .map_err(|error| {
                io::Error::other(format!("JSON load at concurrency {concurrency}: {error}"))
            })?;
        http.push(HttpRun {
            concurrency,
            requests: HTTP_REQUESTS,
            latency_ms: Quantiles::from_samples(latency),
            requests_per_second: HTTP_REQUESTS as f64 / wall.as_secs_f64(),
        });
    }
    let sse = inference_request(port, "bench/model", true);
    let (first_bytes, _) = parallel(SSE_CLIENTS, SSE_CLIENTS, || measure_sse(port, &sse))
        .map_err(|error| io::Error::other(format!("SSE load: {error}")))?;
    let ws = measure_ws(port, process.0.id())?;
    let cancellation = measure_cancel(port, upstream)?;

    let quit = wire::http_request(
        port,
        "POST",
        "/api/quit",
        &format!("X-EMP-Session: {token}\r\nContent-Type: application/json\r\n"),
        "{}",
    );
    let exit = process.quit(port, &quit);

    let report = Report {
        label: label.into(),
        startup_ms: milliseconds(startup),
        exit_ms: exit.as_ref().ok().copied().map(milliseconds),
        idle,
        http,
        sse_requests: SSE_CLIENTS,
        sse_first_byte_ms: Quantiles::from_samples(first_bytes),
        ws,
        cancellation_within_1s: cancellation,
    };
    Ok((report, exit))
}

fn emp_command(binary: &Path, root: &Path, port: u16, upstream: u16) -> io::Result<Command> {
    let home = root.join("home");
    let codex = root.join("codex");
    let catalog = root.join("catalog.json");
    let config = root.join("config.json");
    fs::create_dir(&home)?;
    fs::create_dir(&codex)?;
    fs::write(
        codex.join("auth.json"),
        json!({
            "tokens":{"access_token": TOKEN,"account_id":"bench-account"}
        })
        .to_string(),
    )?;
    fs::write(&catalog, r#"{"models":[]}"#)?;
    let base = format!("http://127.0.0.1:{upstream}");
    fs::write(&config, json!({
        "port":port,
        "auto_enable_on_start":false,
        "native_catalog_path":catalog,
        "codex_base_url":format!("{base}/v1"),
        "providers":[
            {"id":"bench","base_url":format!("{base}/v1"),"protocol":"responses",
             "auth_mode":"api_key","api_key":"bench-upstream-key"},
            {"id":"gated","base_url":format!("{base}/gated/v1"),"protocol":"responses",
             "auth_mode":"api_key","api_key":"bench-upstream-key"},
            {"id":"native","base_url":format!("{base}/v1"),"protocol":"responses","auth_mode":"forward"}
        ],
        "models":[
            {"id":"bench/model","provider":"bench","upstream_id":"upstream-model","enabled":true},
            {"id":"gated/model","provider":"gated","upstream_id":"upstream-model","enabled":true},
            {"id":"native/model","provider":"native","upstream_id":"upstream-model","enabled":true}
        ]
    }).to_string())?;
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CODEX_HOME", &codex)
        .env("PATH", &home)
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("TZ", "UTC")
        .current_dir(&home)
        .args([
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--config",
        ])
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(File::create(root.join("stderr.log"))?);
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    Ok(command)
}

struct Emp(Child);

impl Emp {
    fn ready(&mut self, port: u16, started: Instant) -> io::Result<(Duration, String)> {
        let stdout = self
            .0
            .stdout
            .take()
            .ok_or(io::Error::other("missing EMP stdout"))?;
        let (send, receive) = mpsc::channel();
        let prefix = format!("Open in browser: http://127.0.0.1:{port}/?bootstrap=");
        thread::spawn(move || {
            for line in BufReader::new(stdout)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if let Some(token) = line.strip_prefix(&prefix) {
                    let _ = send.send(Ok(token.to_owned()));
                } else if line.starts_with("EMP startup needs attention") {
                    let _ = send.send(Err(io::Error::other(line)));
                }
            }
        });
        let health = wire::http_request(port, "GET", "/healthz", "", "");
        let probe_timeout = Duration::from_millis(100);
        let mut bootstrap = None;
        let startup = loop {
            if bootstrap.is_none()
                && let Ok(announcement) = receive.try_recv()
            {
                bootstrap = Some(announcement?);
            }
            if wire::request(port, &health, probe_timeout)
                .is_ok_and(|(head, _)| head.status() == Some(200))
            {
                break started.elapsed();
            }
            if let Some(status) = self.0.try_wait()? {
                return Err(io::Error::other(format!(
                    "EMP exited before /healthz: {status}"
                )));
            }
            if started.elapsed() >= IO_TIMEOUT {
                return Err(io::Error::other("EMP /healthz did not return 200"));
            }
            poll_interval();
        };
        let bootstrap = match bootstrap {
            Some(token) => token,
            None => receive
                .recv_timeout(IO_TIMEOUT.saturating_sub(started.elapsed()))
                .map_err(|error| {
                    io::Error::other(format!(
                        "EMP did not announce its benchmark listener on port {port}: {error}"
                    ))
                })??,
        };
        Ok((startup, bootstrap))
    }

    fn quit(&mut self, port: u16, quit: &str) -> io::Result<Duration> {
        let started = Instant::now();
        let (head, _) = wire::request(port, quit, IO_TIMEOUT)?;
        head.expect_status(200)?;
        loop {
            if let Some(status) = self.0.try_wait()? {
                if status.success() {
                    return Ok(started.elapsed());
                }
                return Err(io::Error::other(format!("EMP quit failed: {status}")));
            }
            if started.elapsed() >= IO_TIMEOUT {
                return Err(io::Error::other("EMP did not exit after POST /api/quit"));
            }
            poll_interval();
        }
    }
}

impl Drop for Emp {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "health and process exit have no shared readiness notification"
)]
fn poll_interval() {
    thread::sleep(Duration::from_millis(1));
}

#[expect(
    clippy::disallowed_methods,
    reason = "resource snapshots use a fixed settling interval"
)]
fn settle(duration: Duration) {
    thread::sleep(duration);
}

fn inference_request(port: u16, model: &str, stream: bool) -> String {
    wire::http_request(
        port,
        "POST",
        "/v1/responses",
        &format!("Authorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\n"),
        &json!({"model":model,"input":"hello","stream":stream}).to_string(),
    )
}

fn parallel(
    concurrency: usize,
    requests: usize,
    measure: impl Fn() -> io::Result<Duration> + Sync,
) -> io::Result<(Vec<Duration>, Duration)> {
    let start = Barrier::new(concurrency + 1);
    thread::scope(|scope| {
        let workers: Vec<_> = (0..concurrency)
            .map(|worker| {
                let start = &start;
                let measure = &measure;
                scope.spawn(move || {
                    start.wait();
                    (worker..requests)
                        .step_by(concurrency)
                        .map(|_| measure())
                        .collect::<io::Result<Vec<_>>>()
                })
            })
            .collect();
        let started = Instant::now();
        start.wait();
        let mut samples = Vec::with_capacity(requests);
        for worker in workers {
            samples.extend(
                worker
                    .join()
                    .map_err(|_| io::Error::other("benchmark client panicked"))??,
            );
        }
        Ok((samples, started.elapsed()))
    })
}

fn measure_json(port: u16, request: &str) -> io::Result<Duration> {
    let started = Instant::now();
    let (head, socket) = wire::request(port, request, IO_TIMEOUT)?;
    head.expect_status(200)?;
    let mut bytes = Vec::new();
    head.body(socket)?.read_to_end(&mut bytes)?;
    let latency = started.elapsed();
    let response: Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if response["id"] != upstream::HTTP_ID || response["status"] != "completed" {
        return Err(io::Error::other(format!(
            "unexpected inference response: {response}"
        )));
    }
    Ok(latency)
}

fn sse_body(port: u16, request: &str) -> io::Result<wire::Body> {
    let (head, socket) = wire::request(port, request, IO_TIMEOUT)?;
    head.expect_status(200)?;
    if !head
        .header("content-type")
        .is_some_and(|value| value.starts_with("text/event-stream"))
    {
        return Err(io::Error::other("expected text/event-stream"));
    }
    head.body(socket)
}

fn measure_sse(port: u16, request: &str) -> io::Result<Duration> {
    let started = Instant::now();
    let mut body = sse_body(port, request)?;
    let mut first = [0];
    body.read_exact(&mut first)?;
    let latency = started.elapsed();
    let mut bytes = first.to_vec();
    body.read_to_end(&mut bytes)?;
    if !String::from_utf8_lossy(&bytes).contains("\"response.completed\"") {
        return Err(io::Error::other(
            "SSE response ended before response.completed",
        ));
    }
    Ok(latency)
}

fn measure_cancel(port: u16, upstream: &Upstream) -> io::Result<bool> {
    let request = inference_request(port, "gated/model", true);
    let mut body = BufReader::new(sse_body(port, &request)?);
    let mut line = String::new();
    while body.read_line(&mut line)? != 0 {
        if line.starts_with("data:") && line.contains("\"response.output_text.delta\"") {
            let gate = upstream
                .gated
                .recv_timeout(IO_TIMEOUT)
                .map_err(io::Error::other)?;
            let disconnected = Instant::now();
            drop(body);
            return Ok(gate.observed_disconnect(disconnected));
        }
        line.clear();
    }
    Err(io::Error::other(
        "gated SSE response ended before its first delta",
    ))
}

fn measure_ws(port: u16, pid: u32) -> io::Result<WebSockets> {
    settle(SETTLE);
    let before = ProcessUsage::read(pid)?;
    let mut sessions = Vec::with_capacity(WS_SESSIONS);
    for index in 0..WS_SESSIONS {
        let socket = open_ws(port, index).map_err(|error| {
            io::Error::other(format!("Responses WS session {}: {error}", index + 1))
        })?;
        sessions.push(socket);
    }
    for socket in &mut sessions {
        wire::write_frame(socket, 9, b"bench", true)?;
    }
    for socket in &mut sessions {
        let (opcode, payload) = wire::read_frame(socket, false)?;
        if opcode != 10 || payload != b"bench" {
            return Err(io::Error::other(
                "expected pong on every open Responses WS session",
            ));
        }
    }
    let open = ProcessUsage::read(pid)?;
    for socket in &mut sessions {
        wire::write_frame(socket, 8, &1000_u16.to_be_bytes(), true)?;
    }
    for mut socket in sessions {
        let (opcode, _) = wire::read_frame(&mut socket, false)?;
        if opcode != 8 {
            return Err(io::Error::other(
                "expected a Responses WS close acknowledgement",
            ));
        }
    }
    let closed = Instant::now();
    settle(SETTLE);
    let after = ProcessUsage::read(pid)?;
    settle(Duration::from_secs(15).saturating_sub(closed.elapsed()));
    let after_15s = ProcessUsage::read(pid)?;
    Ok(WebSockets {
        sessions: WS_SESSIONS,
        before,
        open,
        after,
        after_15s,
    })
}

fn open_ws(port: u16, index: usize) -> io::Result<Socket> {
    const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
    let handshake = format!(
        "GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {KEY}\r\nAuthorization: Bearer {TOKEN}\r\noriginator: codex_cli_rs\r\nthread-id: bench-{index}\r\n\r\n"
    );
    let (head, mut socket) = wire::request(port, &handshake, IO_TIMEOUT)?;
    head.expect_status(101)?;
    if head.header("sec-websocket-accept") != Some(&wire::websocket_accept(KEY)) {
        return Err(io::Error::other("invalid EMP WebSocket accept key"));
    }
    let turn = json!({"type":"response.create","model":"native/model","input":[{
        "role":"user","content":[{"type":"input_text","text":"hello"}]
    }]});
    wire::write_frame(&mut socket, 1, turn.to_string().as_bytes(), true)?;
    loop {
        let (opcode, payload) = wire::read_frame(&mut socket, false)?;
        match opcode {
            1 => {
                let event: Value = serde_json::from_slice(&payload).map_err(io::Error::other)?;
                match event["type"].as_str() {
                    Some("response.completed") if event["response"]["id"] == upstream::WS_ID => {
                        return Ok(socket);
                    }
                    Some(
                        "response.completed" | "response.failed" | "response.incomplete" | "error",
                    ) => {
                        return Err(io::Error::other(format!(
                            "unexpected Responses WS result: {event}"
                        )));
                    }
                    _ => {}
                }
            }
            9 => wire::write_frame(&mut socket, 10, &payload, true)?,
            10 => {}
            opcode => {
                return Err(io::Error::other(format!(
                    "unexpected downstream WebSocket opcode {opcode}"
                )));
            }
        }
    }
}
