use crate::{
    modules::process::{ModuleProcess, StartError},
    process_tree::ProcessTree,
};
use emp_modules::{
    manifest::{ApiRequirement, HOST_API_V3, ManifestError, ModuleManifest},
    peer::{CallError, Handlers, Peer, Side},
};
use emp_test_support::{Deadline, fixture_dir, test_timeout};
use rustix::{
    io::Errno,
    process::{Pid, Signal, WaitId, WaitIdOptions, waitid},
};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    fs::OpenOptions,
    io::{self, BufRead, BufReader, Read, Write},
    net::Ipv4Addr,
    os::unix::process::ExitStatusExt,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot, time::timeout};

mod hooks;
mod registry;

const CHILD_MODE: &str = "EMP_TEST_API3_HANDSHAKE";
const SIDEBAND: &str = "EMP_TEST_API3_SIDEBAND";
const MANIFEST: &str = r#"{"id":"fixture-module","module_version":"1.0.0","host_api":{"major":3,"min_minor":0},"contributions":[]}"#;
const FINAL_RESPONSE_BYTES: usize = 256 * 1024;

fn budget() -> Duration {
    test_timeout(Duration::from_secs(5)).duration()
}

fn fixture(mode: &str) -> Command {
    let mut command = Command::new("sh");
    command
        .args(["-c", r#"exec "$@" 3>&1 1>&2"#, "sh"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::module_process_contract::module_fixture",
            "--nocapture",
        ])
        .env(CHILD_MODE, mode);
    command
}

fn sideband() -> std::net::TcpStream {
    let deadline = Deadline::after(test_timeout(Duration::from_secs(20)));
    deadline
        .connect(std::env::var(SIDEBAND).unwrap().parse().unwrap())
        .unwrap()
}

#[test]
fn module_fixture() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    if mode == "escaped" {
        rustix::process::setsid().unwrap();
        let socket = sideband();
        let mut ready = OpenOptions::new().write(true).open("/dev/fd/4").unwrap();
        ready.write_all(b"x").unwrap();
        let deadline = Deadline::after(test_timeout(Duration::from_secs(20)));
        let _ = deadline.io(socket).read(&mut [0]);
        return;
    }
    let mut output = OpenOptions::new().write(true).open("/dev/fd/3").unwrap();
    if mode.starts_with("escape-") {
        let mut descendant = Command::new("sh")
            .args(["-c", r#"exec "$@" 4>&1 1>&2"#, "sh"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::module_process_contract::module_fixture",
                "--nocapture",
            ])
            .env(CHILD_MODE, "escaped")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = [0];
        descendant
            .stdout
            .take()
            .unwrap()
            .read_exact(&mut ready)
            .unwrap();
        assert_eq!(ready, [b'x']);
        let mut input = BufReader::new(io::stdin());
        let initialize = read_request(&mut input, "module.initialize");
        if mode != "escape-timeout" {
            write_response(&mut output, &initialize, MANIFEST);
        }
        if mode == "escape-final" {
            let request = read_request(&mut input, "module.exit");
            write_response(&mut output, &request, "42");
            std::process::exit(0);
        }
        descendant.wait().unwrap();
        return;
    }
    if mode == "registry" {
        let mut input = BufReader::new(io::stdin());
        let initialize = read_request(&mut input, "module.initialize");
        let manifest = std::env::var("EMP_TEST_API3_REGISTRY_MANIFEST").unwrap();
        write_response(&mut output, &initialize, &manifest);
        read_request(&mut input, "module.exit");
        return;
    }
    if mode == "hooks" {
        hooks::child(output);
        return;
    }
    if matches!(mode.as_str(), "exit" | "exit-tree" | "duplicate-manifest") {
        let mut input = BufReader::new(io::stdin());
        let initialize = read_request(&mut input, "module.initialize");
        if mode == "duplicate-manifest" {
            let duplicate = MANIFEST.replace(r#""major":3"#, r#""major":2,"major":3"#);
            write_response(&mut output, &initialize, &duplicate);
            let _confirmation = sideband();
            return;
        }
        write_response(&mut output, &initialize, MANIFEST);
        if mode == "exit-tree" {
            let request = read_request(&mut input, "module.descendant");
            let descendant = Command::new("sleep")
                .arg("86400")
                .stdin(Stdio::null())
                .stdout(output.try_clone().unwrap())
                .spawn()
                .unwrap();
            write_response(&mut output, &request, &descendant.id().to_string());
            read_request(&mut input, "module.exit");
            std::process::exit(0);
        }
        read_request(&mut input, "module.pending");
        let last = read_request(&mut input, "module.exit");
        let response = serde_json::to_string(&"x".repeat(FINAL_RESPONSE_BYTES)).unwrap();
        write_response(&mut output, &last, &response);
        return;
    }
    let mut handlers = Handlers::default();
    handlers.register("module.initialize", move |params, _, peer| {
        peer.call(
            "host.initializing",
            json!([std::process::id(), params]),
            budget(),
        )
        .unwrap();
        let mut manifest = ModuleManifest::from_json(MANIFEST.as_bytes()).unwrap();
        if mode == "invalid" {
            manifest.id = "INVALID".into();
            manifest.module_version.clear();
        }
        if mode == "incompatible" {
            manifest.host_api.min_minor = 1;
        }
        Ok(serde_json::to_value(manifest).unwrap())
    });
    Peer::connect(Side::Module, BufReader::new(io::stdin()), output, handlers)
        .unwrap()
        .wait()
        .unwrap();
}

fn read_request(input: &mut impl BufRead, method: &str) -> Value {
    let mut header = String::new();
    input.read_line(&mut header).unwrap();
    let length = header
        .strip_prefix("Content-Length: ")
        .unwrap()
        .strip_suffix("\r\n")
        .unwrap()
        .parse()
        .unwrap();
    header.clear();
    input.read_line(&mut header).unwrap();
    assert_eq!(header, "\r\n");
    let request: Value = serde_json::from_reader(input.take(length)).unwrap();
    assert_eq!(request["method"], method);
    request["id"].clone()
}

fn write_response(output: &mut impl Write, id: &Value, result: &str) {
    let body = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#);
    let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
    output.write_all(frame.as_bytes()).unwrap();
    output.flush().unwrap();
}

fn host_handlers() -> (Handlers, mpsc::Receiver<Value>) {
    let (reported, report) = mpsc::channel();
    let mut handlers = Handlers::default();
    handlers.register("host.initializing", move |params, _, _| {
        reported.send(params).unwrap();
        Ok(Value::Null)
    });
    (handlers, report)
}

#[tokio::test]
async fn connect_passes_the_host_api_and_state_dir_and_accepts_a_valid_manifest() {
    let state_dir = fixture_dir().unwrap();
    let (handlers, report) = host_handlers();
    let (process, manifest) =
        ModuleProcess::start(fixture("valid"), state_dir.path(), handlers, budget())
            .await
            .unwrap();
    let params = report.recv_timeout(budget()).unwrap();
    assert_eq!(
        params[1],
        json!({"host_api": HOST_API_V3, "state_dir": state_dir.path()})
    );
    assert_eq!(manifest.id, "fixture-module");
    process.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_modules_final_response_is_delivered_before_disconnect() {
    let state_dir = fixture_dir().unwrap();
    let expected = json!("x".repeat(FINAL_RESPONSE_BYTES));
    for _ in 0..16 {
        let (process, _) = ModuleProcess::start(
            fixture("exit"),
            state_dir.path(),
            Handlers::default(),
            budget(),
        )
        .await
        .unwrap();
        let connection = process.connection();
        let pending = connection
            .start_request("module.pending", Value::Null)
            .unwrap();
        assert_eq!(
            connection
                .call("module.exit", Value::Null, budget())
                .unwrap(),
            expected
        );
        assert!(matches!(
            pending.wait(budget()),
            Err(CallError::Disconnected)
        ));
        assert!(process.stop().await.unwrap().success());
    }
}

struct Descendant(u64);

impl Drop for Descendant {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let _ = Command::new("kill")
                .args(["-KILL", &self.0.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_exit_terminates_the_descendant_in_its_process_group() {
    let state_dir = fixture_dir().unwrap();
    let (process, _) = ModuleProcess::start(
        fixture("exit-tree"),
        state_dir.path(),
        Handlers::default(),
        budget(),
    )
    .await
    .unwrap();
    let connection = process.connection();
    let descendant = Descendant(
        connection
            .call("module.descendant", Value::Null, budget())
            .unwrap()
            .as_u64()
            .unwrap(),
    );
    assert!(matches!(
        connection.call("module.exit", Value::Null, budget()),
        Err(CallError::Disconnected)
    ));
    assert!(
        tokio::time::timeout(budget(), process.stop())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let state = Command::new("ps")
        .args(["-o", "stat=", "-p", &descendant.0.to_string()])
        .output()
        .unwrap();
    let state = String::from_utf8(state.stdout).unwrap();
    assert!(
        state.trim().is_empty() || state.trim_start().starts_with('Z'),
        "{state}"
    );
}

#[tokio::test]
async fn duplicate_keys_in_the_handshake_manifest_disconnect() {
    let state_dir = fixture_dir().unwrap();
    let (command, listener) = fixture_with_sideband("duplicate-manifest").await;
    let (confirmed, confirmation) = mpsc::channel();
    let (read, read_result) = mpsc::channel();
    let start = ModuleProcess::start_with_connector(
        command,
        state_dir.path(),
        Handlers::default(),
        budget(),
        |side, reader, writer, handlers| {
            Peer::connect_observed(
                side,
                reader,
                writer,
                handlers,
                move |error| {
                    read.send(error).unwrap();
                    confirmation.recv_timeout(budget()).unwrap();
                },
                || {},
            )
        },
        || {},
    );
    let (outcome, ()) = tokio::join!(start, async {
        let _report = accept_sideband(&listener).await;
        confirmed.send(()).unwrap();
    });
    assert_eq!(
        read_result.recv_timeout(budget()).unwrap(),
        Some(io::ErrorKind::InvalidData)
    );
    assert!(matches!(
        outcome,
        Err(StartError::Handshake(CallError::Disconnected))
    ));
}

async fn fixture_with_sideband(mode: &str) -> (Command, TcpListener) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let mut command = fixture(mode);
    command.env(SIDEBAND, listener.local_addr().unwrap().to_string());
    (command, listener)
}

async fn accept_sideband(listener: &TcpListener) -> tokio::net::TcpStream {
    timeout(budget(), listener.accept())
        .await
        .unwrap()
        .unwrap()
        .0
}

#[tokio::test]
async fn handshake_timeout_stops_even_when_a_detached_descendant_holds_the_pipes() {
    let state_dir = fixture_dir().unwrap();
    let (command, listener) = fixture_with_sideband("escape-timeout").await;
    let start = ModuleProcess::start(command, state_dir.path(), Handlers::default(), budget());
    let handshake_and_cleanup =
        (test_timeout(Duration::from_secs(5)) + test_timeout(Duration::from_secs(5))).duration();
    let (outcome, descendant) = tokio::join!(
        timeout(handshake_and_cleanup, start),
        accept_sideband(&listener)
    );
    assert!(matches!(
        outcome.unwrap(),
        Err(StartError::Handshake(CallError::TimedOut))
    ));
    assert_eq!(
        descendant.try_read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

struct NotifyLargeWrite<W> {
    writer: W,
    started: Option<oneshot::Sender<()>>,
}

impl<W: Write> Write for NotifyLargeWrite<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() >= 1024 * 1024
            && let Some(started) = self.started.take()
        {
            let _ = started.send(());
        }
        self.writer.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

struct IoExitGate {
    entered: oneshot::Receiver<()>,
    release: mpsc::Sender<()>,
    released: oneshot::Receiver<()>,
}

fn io_exit_gate() -> (IoExitGate, impl FnOnce() + Send + 'static) {
    let (enter, entered) = oneshot::channel();
    let (release, held) = mpsc::channel();
    let (leave, released) = oneshot::channel();
    (
        IoExitGate {
            entered,
            release,
            released,
        },
        move || {
            enter.send(()).unwrap();
            held.recv_timeout(budget()).unwrap();
            leave.send(()).unwrap();
        },
    )
}

#[tokio::test]
async fn stop_waits_for_each_io_thread_after_interrupting_detached_pipes() {
    let state_dir = fixture_dir().unwrap();
    for hold_reader in [true, false] {
        let (command, listener) = fixture_with_sideband("escape-stop").await;
        let (writing, writer_started) = oneshot::channel();
        let (mut read_gate, reader_exit) = io_exit_gate();
        let (mut write_gate, writer_exit) = io_exit_gate();
        let (process, _) = ModuleProcess::start_with_connector(
            command,
            state_dir.path(),
            Handlers::default(),
            budget(),
            |side, reader, writer, handlers| {
                Peer::connect_observed(
                    side,
                    reader,
                    NotifyLargeWrite {
                        writer,
                        started: Some(writing),
                    },
                    handlers,
                    move |_| reader_exit(),
                    writer_exit,
                )
            },
            || {},
        )
        .await
        .unwrap();
        let descendant = accept_sideband(&listener).await;
        let pending = process
            .connection()
            .start_request("module.blocked", json!("x".repeat(2 * 1024 * 1024)))
            .unwrap();
        timeout(budget(), writer_started).await.unwrap().unwrap();
        let mut stopping = std::pin::pin!(process.stop());
        tokio::select! {
            result = &mut stopping => panic!("stop returned while I/O was held: {result:?}"),
            ready = timeout(budget(), async {
                (&mut read_gate.entered).await.unwrap();
                (&mut write_gate.entered).await.unwrap();
            }) => ready.unwrap(),
        }
        let (held, released) = if hold_reader {
            (read_gate, write_gate)
        } else {
            (write_gate, read_gate)
        };
        released.release.send(()).unwrap();
        timeout(budget(), released.released).await.unwrap().unwrap();
        let observation = test_timeout(Duration::from_millis(200)).duration();
        assert!(timeout(observation, &mut stopping).await.is_err());
        held.release.send(()).unwrap();
        timeout(budget(), &mut stopping).await.unwrap().unwrap();
        timeout(budget(), held.released).await.unwrap().unwrap();
        assert!(matches!(
            pending.wait(budget()),
            Err(CallError::Disconnected)
        ));
        assert_eq!(
            descendant.try_read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}

struct GateAfterHandshake<R> {
    reader: R,
    gate: Option<mpsc::Receiver<()>>,
    read_handshake: bool,
}

impl<R: Read> Read for GateAfterHandshake<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.read_handshake
            && let Some(gate) = self.gate.take()
        {
            gate.recv_timeout(budget()).unwrap();
        }
        let read = self.reader.read(bytes)?;
        self.read_handshake |= read != 0;
        Ok(read)
    }
}

#[tokio::test]
async fn a_final_response_is_drained_after_exit_even_with_a_detached_descendant() {
    let state_dir = fixture_dir().unwrap();
    let (command, listener) = fixture_with_sideband("escape-final").await;
    let (release_reader, held) = mpsc::channel();
    let (pipes_closed, closed) = oneshot::channel();
    let (writer_ended, writer_exit) = oneshot::channel();
    let (process, _) = ModuleProcess::start_with_connector(
        command,
        state_dir.path(),
        Handlers::default(),
        budget(),
        |side, reader, writer, handlers| {
            let reader = BufReader::new(GateAfterHandshake {
                reader: reader.into_inner(),
                gate: Some(held),
                read_handshake: false,
            });
            Peer::connect_observed(
                side,
                reader,
                writer,
                handlers,
                |_| {},
                move || {
                    writer_ended.send(()).unwrap();
                },
            )
        },
        move || {
            pipes_closed.send(()).unwrap();
        },
    )
    .await
    .unwrap();
    let descendant = accept_sideband(&listener).await;
    let connection = process.connection();
    let final_response = connection
        .start_request("module.exit", Value::Null)
        .unwrap();
    let pending = connection
        .start_request("module.blocked", json!("x".repeat(2 * 1024 * 1024)))
        .unwrap();
    timeout(budget(), closed).await.unwrap().unwrap();
    release_reader.send(()).unwrap();
    assert_eq!(final_response.wait(budget()).unwrap(), json!(42));
    timeout(budget(), writer_exit).await.unwrap().unwrap();
    assert!(matches!(
        pending.wait(budget()),
        Err(CallError::Disconnected)
    ));
    assert!(
        timeout(budget(), process.stop())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert_eq!(
        descendant.try_read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

fn child_waiting_for_input() -> (Child, ProcessTree) {
    let mut command = Command::new("cat");
    command.stdin(Stdio::piped()).stdout(Stdio::null());
    ProcessTree::configure(&mut command);
    let child = command.spawn().unwrap();
    let tree = ProcessTree::attach(&child).unwrap();
    (child, tree)
}

thread_local! {
    static OBSERVATION_ERROR: Cell<Option<Errno>> = const { Cell::new(None) };
}

impl ProcessTree {
    pub(crate) fn take_observation_error() -> Option<Errno> {
        OBSERVATION_ERROR.take()
    }
}

#[test]
fn observation_echild_disables_process_tree_signals() {
    let (mut child, tree) = child_waiting_for_input();
    OBSERVATION_ERROR.set(Some(Errno::CHILD));
    let error = tree.wait(&mut child).unwrap_err();
    let stopped = tree.is_stopped();
    tree.request_stop();
    tree.request_graceful_stop();
    drop(tree);
    assert!(child.wait().unwrap().success());
    assert!(stopped);
    assert_eq!(error.raw_os_error(), Some(Errno::CHILD.raw_os_error()));
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "验证 ProcessTree 已回收指定的测试子进程"
)]
fn observation_failure_kills_and_reaps_the_child_before_returning_the_original_error() {
    let (mut child, tree) = child_waiting_for_input();
    let leader = Pid::from_child(&child);
    OBSERVATION_ERROR.set(Some(Errno::PERM));
    let error = tree.has_exited(&mut child).unwrap_err();
    let observed = waitid(
        WaitId::Pid(leader),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
    );
    let status = child.wait().unwrap();
    assert!(matches!(observed, Err(Errno::CHILD)));
    assert_eq!(status.signal(), Some(Signal::KILL.as_raw()));
    assert!(tree.is_stopped());
    assert_eq!(error.raw_os_error(), Some(Errno::PERM.raw_os_error()));
}

#[tokio::test]
async fn rejected_manifests_report_all_validation_errors_and_reap_the_child() {
    let state_dir = fixture_dir().unwrap();
    for (mode, expected) in [
        (
            "invalid",
            vec![
                ManifestError::InvalidModuleId {
                    id: "INVALID".into(),
                },
                ManifestError::EmptyModuleVersion,
            ],
        ),
        (
            "incompatible",
            vec![ManifestError::IncompatibleHostApi {
                required: ApiRequirement {
                    major: 3,
                    min_minor: 1,
                },
                host: HOST_API_V3,
            }],
        ),
    ] {
        let (handlers, report) = host_handlers();
        let outcome =
            ModuleProcess::start(fixture(mode), state_dir.path(), handlers, budget()).await;
        let Err(ref error @ StartError::Manifest(ref errors)) = outcome else {
            panic!("{mode}: expected manifest validation errors");
        };
        assert_eq!(*errors, expected);
        for error_detail in expected {
            assert!(error.to_string().contains(&error_detail.to_string()));
        }
        let pid = report.recv_timeout(budget()).unwrap()[0].as_u64().unwrap();
        assert!(
            !Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }
}
