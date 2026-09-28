use emp_state::{CONFIG_PATH_ENV, config_path, load_configuration, normalize_configuration};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use tempfile::tempdir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-config-load.json"
    ))
    .expect("valid config-load fixture")
}

fn temp_path(directory: &tempfile::TempDir) -> PathBuf {
    fs::canonicalize(directory.path()).expect("canonical temporary directory")
}

fn write_case(path: &std::path::Path, case: &Value) {
    if let Some(bytes) = case.get("bytes").and_then(Value::as_array) {
        let bytes = bytes
            .iter()
            .map(|byte| byte.as_u64().expect("fixture byte") as u8)
            .collect::<Vec<_>>();
        fs::write(path, bytes).expect("write byte fixture");
    } else {
        fs::write(
            path,
            case.get("content").and_then(Value::as_str).unwrap_or(""),
        )
        .expect("write text fixture");
    }
}

fn error_suffix(error: &emp_state::ConfigError) -> String {
    let message = error.to_string();
    assert!(
        message.starts_with("invalid JSON in "),
        "unexpected configuration error: {message}"
    );
    message
        .rsplit_once(": ")
        .map(|(_, suffix)| suffix.to_owned())
        .expect("JSON message suffix")
}

#[test]
fn config_load_matches_frozen_fixture() {
    let _lock = ENV_LOCK.lock().expect("environment lock");
    let fixture = fixture();
    for case in fixture["invalid_json_cases"]
        .as_array()
        .expect("invalid JSON cases")
    {
        let name = case["name"].as_str().expect("case name");
        if name == "directory" && cfg!(not(target_os = "linux")) {
            continue;
        }
        let directory = tempdir().expect("temporary case directory");
        let path = temp_path(&directory).join("config.json");
        if name == "missing-file" {
            assert_eq!(
                load_configuration(Some(&path)).expect("missing configuration file"),
                normalize_configuration(None).expect("default configuration")
            );
            continue;
        }
        if name == "directory" {
            fs::create_dir(&path).expect("create configuration directory");
            let error = load_configuration(Some(&path)).expect_err("directory configuration");
            assert_eq!(error.python_type(), "IsADirectoryError", "case: {name}");
            assert_eq!(
                error.to_string(),
                "configuration file could not be read",
                "case: {name}"
            );
            continue;
        }

        write_case(&path, case);
        let error = load_configuration(Some(&path)).expect_err("invalid configuration");
        if name == "invalid-utf8" {
            assert_eq!(error.python_type(), "UnicodeDecodeError", "case: {name}");
            assert_eq!(
                error.to_string(),
                "configuration file is not valid UTF-8",
                "case: {name}"
            );
        } else {
            assert_eq!(error.python_type(), "ConfigError", "case: {name}");
            let expected = case
                .get("error_suffix")
                .and_then(Value::as_str)
                .unwrap_or_else(|| case["content"].as_str().expect("text"));
            assert_eq!(error_suffix(&error), expected, "case: {name}");
        }
    }
}

#[test]
fn valid_json_is_normalized_and_private_paths_are_canonicalized() {
    let _lock = ENV_LOCK.lock().expect("environment lock");
    let directory = tempdir().expect("temporary directory");
    let root = temp_path(&directory);
    let path = root.join("configuration/config.json");
    fs::create_dir_all(path.parent().expect("configuration parent"))
        .expect("create configuration parent");
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "port": 5000,
            "secret_store_path": "secrets",
            "providers": [{
                "id": "example",
                "base_url": "https://example.com/v1",
                "api_key_file": "secrets/example.key.enc"
            }]
        }))
        .expect("configuration JSON"),
    )
    .expect("write configuration");

    let loaded = load_configuration(Some(&path)).expect("load valid configuration");
    let mut expected = normalize_configuration(Some(&json!({
        "port": 5000,
        "secret_store_path": "secrets",
        "providers": [{
            "id": "example",
            "base_url": "https://example.com/v1",
            "api_key_file": "secrets/example.key.enc"
        }]
    })))
    .expect("expected configuration");
    emp_state::canonicalize_private_paths(&mut expected, &path).expect("canonicalize expected");
    assert_eq!(loaded, expected);
}

#[test]
fn config_path_and_default_load_follow_environment_precedence() {
    let _lock = ENV_LOCK.lock().expect("environment lock");
    let directory = tempdir().expect("temporary directory");
    let root = temp_path(&directory);
    let configured = root.join("custom/config.json");
    let home = root.join("home");
    let xdg = root.join("xdg");
    let local = root.join("local");
    let roaming = root.join("roaming");
    fs::create_dir_all(configured.parent().expect("configuration parent"))
        .expect("create configuration parent");
    fs::write(&configured, b"{\"port\":5100}").expect("write configured configuration");
    fs::create_dir_all(&home).expect("create home");
    let environment_names = [
        CONFIG_PATH_ENV,
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "LOCALAPPDATA",
        "APPDATA",
    ];
    let original = environment_names
        .map(|name| (name, std::env::var_os(name)))
        .into_iter()
        .collect::<Vec<_>>();

    // SAFETY: this test serializes environment mutation, restores the original
    // value on every exit path, and does not mutate configuration state.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg);
        std::env::set_var("LOCALAPPDATA", &local);
        std::env::set_var("APPDATA", &roaming);
        std::env::set_var(CONFIG_PATH_ENV, " ");
    }
    let expected_default = if cfg!(windows) {
        local.join("EasyMultiProvider/config.json")
    } else if cfg!(target_os = "macos") {
        home.join("Library/Application Support/EasyMultiProvider/config.json")
    } else {
        xdg.join("easy-multi-provider/config.json")
    };
    assert_eq!(config_path(), expected_default);
    fs::create_dir_all(expected_default.parent().expect("default config parent"))
        .expect("create default config parent");
    fs::write(&expected_default, b"{\"port\":5200}").expect("write default configuration");
    assert_eq!(
        load_configuration(None).expect("load default config")["port"],
        5200
    );

    // SAFETY: guarded and restored below.
    unsafe {
        std::env::set_var(CONFIG_PATH_ENV, &configured);
    }
    assert_eq!(config_path(), configured);
    let loaded = load_configuration(None).expect("load configured path");
    assert_eq!(loaded["port"], 5100);

    // SAFETY: restore the caller environment.
    unsafe {
        for (name, value) in original {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}
