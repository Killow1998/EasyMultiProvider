use crate::modules::process::{ModuleProcess, StartError};
use emp_modules::{
    manifest::{ApiRequirement, HOST_API_V3, ManifestError, ModuleManifest},
    peer::{CallError, Handlers, Peer, Side},
};
use emp_test_support::{fixture_dir, test_timeout};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    io::{self, BufRead, BufReader, Read, Write},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

const CHILD_MODE: &str = "EMP_TEST_API3_HANDSHAKE";
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

#[test]
fn module_fixture() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let mut output = OpenOptions::new().write(true).open("/dev/fd/3").unwrap();
    if matches!(mode.as_str(), "exit" | "exit-tree" | "duplicate-manifest") {
        let mut input = BufReader::new(io::stdin());
        let initialize = read_request(&mut input, "module.initialize");
        if mode == "duplicate-manifest" {
            let duplicate = MANIFEST.replace(r#""major":3"#, r#""major":2,"major":3"#);
            write_response(&mut output, &initialize, &duplicate);
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
    write!(output, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
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
async fn module_exit_terminates_the_pipe_holding_descendant_and_joins_io() {
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
    let outcome = ModuleProcess::start(
        fixture("duplicate-manifest"),
        state_dir.path(),
        Handlers::default(),
        budget(),
    )
    .await;
    assert!(matches!(
        outcome,
        Err(StartError::Handshake(CallError::Disconnected))
    ));
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
