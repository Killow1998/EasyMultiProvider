use emp_state::{load_or_create_web_session, web_session_path};
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn unix_time() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after Unix epoch")
        .as_secs_f64()
}

fn python_session(python: &str, config_path: &Path) -> Value {
    let script = r#"
import json
import sys
from pathlib import Path
from easy_multi_provider.server import AppState

state = AppState(Path(sys.argv[1]))
print(json.dumps({"token": state.session_token, "expires_at": state.session_expires_at}))
"#;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(config_path)
        .current_dir(root)
        .output()
        .expect("run Python web-session oracle");
    assert!(
        output.status.success(),
        "Python web-session oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python session JSON")
}

fn assert_same_expiry(serialized: &Value, expected: f64) {
    let actual = serialized.as_f64().expect("numeric expiry");
    // Python and serde_json may choose adjacent IEEE-754 values when the same
    // epoch float is rendered and parsed a second time. Session validity is
    // measured in whole seconds, so retain a strict sub-microsecond boundary.
    assert!(
        (actual - expected).abs() < 1e-6,
        "expiry differs after Python/Rust JSON round trip: {actual} != {expected}"
    );
}

#[test]
fn rust_and_live_python_reuse_each_others_web_sessions() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        eprintln!("skipping live Python oracle: EMP_PYTHON_INTEROP is unset");
        return;
    };
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_path = directory
        .path()
        .canonicalize()
        .expect("canonical temporary directory")
        .join("config.json");
    let session_path = web_session_path(&config_path).expect("session path");

    let rust_created =
        load_or_create_web_session(&session_path, unix_time()).expect("Rust creates session");
    let python_loaded = python_session(&python, &config_path);
    assert_eq!(python_loaded["token"], rust_created.token());
    assert_same_expiry(&python_loaded["expires_at"], rust_created.expires_at());

    std::fs::remove_file(&session_path).expect("remove Rust session");
    let python_created = python_session(&python, &config_path);
    let rust_loaded =
        load_or_create_web_session(&session_path, unix_time()).expect("Rust loads Python session");
    assert_eq!(python_created["token"], rust_loaded.token());
    assert_same_expiry(&python_created["expires_at"], rust_loaded.expires_at());
}
