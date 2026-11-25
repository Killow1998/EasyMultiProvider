use crate::modules::process::{ModuleProcess, StartError};
use emp_modules::{
    manifest::{ApiRequirement, HOST_API_V3, ManifestError, ModuleManifest},
    peer::{Handlers, Peer, Side},
};
use emp_test_support::{fixture_dir, test_timeout};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    io::{self, BufReader},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

const CHILD_MODE: &str = "EMP_TEST_API3_HANDSHAKE";

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
    let mut handlers = Handlers::default();
    handlers.register("module.initialize", move |params, _, peer| {
        peer.call(
            "host.initializing",
            json!([std::process::id(), params]),
            budget(),
        )
        .unwrap();
        let mut manifest = ModuleManifest {
            id: "fixture-module".into(),
            module_version: "1.0.0".into(),
            host_api: ApiRequirement {
                major: 3,
                min_minor: 0,
            },
            contributions: Vec::new(),
        };
        if mode == "invalid" {
            manifest.id = "INVALID".into();
            manifest.module_version.clear();
        }
        if mode == "incompatible" {
            manifest.host_api.min_minor = 1;
        }
        Ok(serde_json::to_value(manifest).unwrap())
    });
    let output = OpenOptions::new().write(true).open("/dev/fd/3").unwrap();
    Peer::connect(Side::Module, BufReader::new(io::stdin()), output, handlers)
        .unwrap()
        .wait()
        .unwrap();
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
    let (_process, manifest) =
        ModuleProcess::start(fixture("valid"), state_dir.path(), handlers, budget())
            .await
            .unwrap();
    let params = report.recv_timeout(budget()).unwrap();
    assert_eq!(
        params[1],
        json!({"host_api": HOST_API_V3, "state_dir": state_dir.path()})
    );
    assert_eq!(manifest.id, "fixture-module");
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
