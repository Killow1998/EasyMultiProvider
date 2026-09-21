use emp_state::{account_auth_path, canonicalize_private_paths, normalize_configuration};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::symlink;

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-private-path-canonicalization.json"
    ))
    .expect("valid private-path fixture")
}

fn canonical_temp_path(directory: &tempfile::TempDir) -> PathBuf {
    fs::canonicalize(directory.path()).expect("canonical temporary directory")
}

fn create_case(root: &Path, case: &Value) -> PathBuf {
    let config_path = root.join(case["config_path"].as_str().expect("config path"));
    fs::create_dir_all(config_path.parent().expect("configuration parent"))
        .expect("create configuration parent");
    #[cfg(unix)]
    if let Some(link) = case.get("symlink") {
        let link_path = root.join(link["link"].as_str().expect("link path"));
        let target_path = root.join(link["target"].as_str().expect("target path"));
        fs::create_dir_all(link_path.parent().expect("link parent")).expect("create link parent");
        fs::create_dir_all(target_path.parent().expect("target parent"))
            .expect("create target parent");
        fs::write(&target_path, b"outside\n").expect("write link target");
        symlink(&target_path, &link_path).expect("create managed-file symlink");
    }
    config_path
}

fn outcome(config: &mut Value, config_path: &Path) -> Value {
    match canonicalize_private_paths(config, config_path) {
        Ok(()) => json!({
            "account_auth_file": config["accounts"]
                .as_array()
                .and_then(|accounts| accounts.first())
                .and_then(|account| account["auth_file"].as_str()),
            "provider_api_key_file": config["providers"]
                .as_array()
                .and_then(|providers| providers.first())
                .and_then(|provider| provider["api_key_file"].as_str()),
        }),
        Err(error) => json!({
            "error_type": error.python_type(),
            "error": error.to_string(),
        }),
    }
}

fn home_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .expect("test home directory");
    let home = PathBuf::from(home);
    fs::canonicalize(&home).unwrap_or(home)
}

#[test]
fn private_paths_match_frozen_python_fixture() {
    let fixture = fixture();
    for case in fixture["cases"].as_array().expect("private-path cases") {
        if case["platform"] == "unix" && cfg!(not(unix)) {
            continue;
        }
        let directory = tempdir().expect("temporary case directory");
        let root = canonical_temp_path(&directory);
        let config_path = create_case(&root, case);
        let mut config = normalize_configuration(Some(&case["config"]))
            .expect("valid private-path configuration");
        let actual = outcome(&mut config, &config_path);
        let expected = &case["expected"];
        if expected.get("error").is_some() {
            assert_eq!(
                actual["error_type"], expected["error_type"],
                "case: {}",
                case["name"]
            );
            assert_eq!(actual["error"], expected["error"], "case: {}", case["name"]);
            continue;
        }
        for (result_field, root_field, home_field) in [
            (
                "account_auth_file",
                "account_root_relative",
                "account_home_relative",
            ),
            (
                "provider_api_key_file",
                "provider_root_relative",
                "provider_home_relative",
            ),
        ] {
            let expected_path =
                if let Some(relative) = expected.get(root_field).and_then(Value::as_str) {
                    Some(root.join(relative))
                } else {
                    expected
                        .get(home_field)
                        .and_then(Value::as_str)
                        .map(|relative| home_path().join(relative))
                };
            if let Some(expected_path) = expected_path {
                assert_eq!(
                    actual[result_field].as_str().map(Path::new),
                    Some(expected_path.as_path()),
                    "case: {}, field: {result_field}",
                    case["name"]
                );
            }
        }
    }
}

#[test]
fn account_auth_path_uses_config_parent_and_rejects_unsafe_ids() {
    let directory = tempdir().expect("temporary directory");
    let root = canonical_temp_path(&directory);
    let config_path = root.join("configuration/config.json");
    let config = json!({"account_store_path": "managed/accounts"});
    assert_eq!(
        account_auth_path(&config, "egg", &config_path).expect("managed account path"),
        root.join("configuration/managed/accounts/egg/auth.json.enc")
    );
    assert_eq!(
        account_auth_path(&config, "../escape", &config_path)
            .expect_err("unsafe account id")
            .to_string(),
        "account.id must be a safe single path segment"
    );
}

#[test]
fn private_paths_match_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = fixture();
    let script = r#"
import json, sys
from pathlib import Path
from easy_multi_provider.config import _canonicalize_private_paths, normalize
payload = json.load(sys.stdin)
try:
    config = _canonicalize_private_paths(normalize(payload["config"]), Path(payload["config_path"]))
    outcome = {
        "account_auth_file": config["accounts"][0]["auth_file"] if config["accounts"] else None,
        "provider_api_key_file": config["providers"][0]["api_key_file"] if config["providers"] else None,
    }
except Exception as exc:
    outcome = {"error_type": type(exc).__name__, "error": str(exc)}
json.dump(outcome, sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for case in fixture["cases"].as_array().expect("private-path cases") {
        if case["platform"] == "unix" && cfg!(not(unix)) {
            continue;
        }
        let directory = tempdir().expect("temporary case directory");
        let root = canonical_temp_path(&directory);
        let config_path = create_case(&root, case);
        let payload = json!({"config": case["config"], "config_path": config_path});
        let mut child = Command::new(&python)
            .arg("-c")
            .arg(script)
            .current_dir(&project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn Python private-path oracle");
        child
            .stdin
            .take()
            .expect("Python stdin")
            .write_all(
                serde_json::to_string(&payload)
                    .expect("private-path payload")
                    .as_bytes(),
            )
            .expect("write private-path payload");
        let output = child.wait_with_output().expect("wait for Python oracle");
        assert!(
            output.status.success(),
            "Python private-path oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let oracle: Value = serde_json::from_slice(&output.stdout).expect("Python oracle JSON");
        let mut config = normalize_configuration(Some(&case["config"]))
            .expect("valid private-path configuration");
        assert_eq!(
            outcome(&mut config, &config_path),
            oracle,
            "case: {}",
            case["name"]
        );
    }
}
