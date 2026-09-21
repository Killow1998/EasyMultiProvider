use emp_state::{
    CONFIG_PATH_ENV, ConfigError, FilesystemError, VaultStore, config_path, load_configuration,
    save_configuration, save_configuration_in_transaction, with_file_transaction,
};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

static ENV_LOCK: Mutex<()> = Mutex::new(());

const TEST_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
const MASKED_API_KEY: &str = "••••••••";

fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().expect("configuration parent"))
        .expect("create configuration parent");
    fs::write(
        path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(value).expect("pretty JSON")
        ),
    )
    .expect("write JSON");
}

fn root(directory: &tempfile::TempDir) -> PathBuf {
    fs::canonicalize(directory.path()).expect("canonical temporary directory")
}

#[test]
fn config_save_round_trips_and_manages_derived_secrets() {
    let directory = tempdir().expect("temporary directory");
    let root = root(&directory);
    let path = root.join("nested/config.json");
    let vault =
        VaultStore::from_sources(Some(TEST_KEY), &root.join("unused.key")).expect("vault store");

    let first = json!({
        "port": 5100,
        "secret_store_path": "secrets",
        "providers": [{
            "id": "unicode-provider",
            "base_url": "https://example.test/v1",
            "api_key": "ünicode credential"
        }]
    });
    assert_eq!(
        save_configuration(&first, Some(&path), &vault).expect("save new configuration"),
        path
    );
    let secret_path = root.join("nested/secrets/unicode-provider.key.enc");

    #[cfg(unix)]
    {
        assert_eq!(
            fs::metadata(&path)
                .expect("configuration metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&secret_path)
                .expect("secret metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(secret_path.parent().expect("secret parent"))
                .expect("secret parent metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    let raw = fs::read(&path).expect("configuration bytes");
    assert_eq!(raw.last().copied(), Some(b'\n'));
    let actual = load_configuration(Some(&path)).expect("load saved configuration");
    assert_eq!(actual["providers"][0]["api_key"], "");
    assert_eq!(
        actual["providers"][0]["api_key_file"],
        secret_path.to_string_lossy().as_ref()
    );
    assert_eq!(
        vault
            .read_encrypted_text(&secret_path)
            .expect("read managed secret")
            .as_str(),
        "ünicode credential"
    );
}

#[test]
fn config_save_preserves_masked_files_and_cleans_removed_providers() {
    let directory = tempdir().expect("temporary directory");
    let root = root(&directory);
    let path = root.join("configuration/config.json");
    let vault =
        VaultStore::from_sources(Some(TEST_KEY), &root.join("unused.key")).expect("vault store");
    let existing = root.join("configuration/secrets/existing.key.enc");
    let obsolete = root.join("configuration/secrets/obsolete.key.enc");
    let unrelated = root.join("configuration/unmanaged.key.enc");
    write_json(
        &path,
        &json!({
            "secret_store_path": "secrets",
            "providers": [
                {
                    "id": "existing",
                    "base_url": "https://existing.test/v1",
                    "api_key_file": "secrets/existing.key.enc"
                },
                {
                    "id": "obsolete",
                    "base_url": "https://obsolete.test/v1",
                    "api_key_file": "secrets/obsolete.key.enc"
                }
            ]
        }),
    );
    vault
        .write_encrypted_text(&existing, "existing")
        .expect("write existing");
    vault
        .write_encrypted_text(&obsolete, "obsolete")
        .expect("write obsolete");
    fs::write(&unrelated, b"unmanaged").expect("write unrelated file");

    let incoming = json!({
        "secret_store_path": "secrets",
        "providers": [{
            "id": "existing",
            "base_url": "https://existing.test/v1",
            "api_key": MASKED_API_KEY,
            "api_key_file": "secrets/existing.key.enc"
        }]
    });
    save_configuration(&incoming, Some(&path), &vault).expect("save configuration");
    assert!(existing.is_file(), "existing managed secret must remain");
    assert!(
        !obsolete.exists(),
        "removed provider secret must be deleted"
    );
    assert_eq!(fs::read(&unrelated).expect("unmanaged bytes"), b"unmanaged");
}

#[test]
fn caller_owned_transaction_rolls_back_config_and_secret_files() {
    let directory = tempdir().expect("temporary directory");
    let root = root(&directory);
    let path = root.join("configuration/config.json");
    let vault =
        VaultStore::from_sources(Some(TEST_KEY), &root.join("unused.key")).expect("vault store");
    let previous_secret = root.join("configuration/secrets/previous.key.enc");
    write_json(
        &path,
        &json!({
            "secret_store_path": "secrets",
            "providers": [{
                "id": "previous",
                "base_url": "https://previous.test/v1",
                "api_key_file": "secrets/previous.key.enc"
            }]
        }),
    );
    vault
        .write_encrypted_text(&previous_secret, "previous credential")
        .expect("write previous secret");
    let original_config = fs::read(&path).expect("original configuration bytes");
    let original_secret = fs::read(&previous_secret).expect("original secret bytes");
    let new_secret = root.join("configuration/secrets/new.key.enc");
    let incoming = json!({
        "secret_store_path": "secrets",
        "providers": [{
            "id": "new",
            "base_url": "https://new.test/v1",
            "api_key": "new credential"
        }]
    });

    let result: Result<(), ConfigError> = with_file_transaction(|transaction| {
        save_configuration_in_transaction(&incoming, Some(&path), &vault, transaction)?;
        Err(FilesystemError::ManagedFileUnavailable.into())
    });
    assert!(result.is_err(), "injected enclosing operation must fail");
    assert_eq!(
        fs::read(&path).expect("rolled-back configuration"),
        original_config
    );
    assert_eq!(
        fs::read(&previous_secret).expect("restored previous secret"),
        original_secret
    );
    assert!(
        !new_secret.exists(),
        "new secret must be removed on rollback"
    );
}

#[test]
fn save_uses_config_path_environment_without_reloading_vault() {
    let _lock = ENV_LOCK.lock().expect("environment lock");
    let directory = tempdir().expect("temporary directory");
    let root = root(&directory);
    let configured = root.join("configured/config.json");
    let vault =
        VaultStore::from_sources(Some(TEST_KEY), &root.join("unused.key")).expect("vault store");
    let original = std::env::var_os(CONFIG_PATH_ENV);
    unsafe {
        std::env::set_var(CONFIG_PATH_ENV, &configured);
    }
    let saved = save_configuration(&json!({}), None, &vault).expect("save configured path");
    assert_eq!(saved, configured);
    assert_eq!(config_path(), configured);
    unsafe {
        match original {
            Some(value) => std::env::set_var(CONFIG_PATH_ENV, value),
            None => std::env::remove_var(CONFIG_PATH_ENV),
        }
    }
    assert!(configured.is_file());
}

#[test]
fn config_save_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let directory = tempdir().expect("temporary directory");
    let root = root(&directory);
    let rust_path = root.join("rust/config.json");
    let python_path = root.join("python/config.json");
    let vault =
        VaultStore::from_sources(Some(TEST_KEY), &root.join("unused.key")).expect("vault store");
    let input = json!({
        "port": 5100,
        "secret_store_path": "secrets",
        "providers": [{
            "id": "deepseek",
            "base_url": "https://api.deepseek.com/v1/chat/completions",
            "api_key": "synthetic ünicode credential"
        }]
    });
    save_configuration(&input, Some(&rust_path), &vault).expect("Rust save");

    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repository root")
        .to_path_buf();
    let script = r#"
import json
import sys
from pathlib import Path
from easy_multi_provider.config import save
from easy_multi_provider.vault import read_encrypted_text

path = Path(sys.argv[1])
value = {
    "port": 5100,
    "secret_store_path": "secrets",
    "providers": [{
        "id": "deepseek",
        "base_url": "https://api.deepseek.com/v1/chat/completions",
        "api_key": "synthetic ünicode credential",
    }],
}
save(value, path)
saved = json.loads(path.read_text(encoding="utf-8"))
secret = Path(saved["providers"][0]["api_key_file"])
saved["providers"][0]["api_key_file"] = secret.name
print(json.dumps({"config": saved, "secret": read_encrypted_text(secret)}, ensure_ascii=False))
"#;
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(&python_path)
        .current_dir(repository)
        .env("EASY_MULTI_PROVIDER_MASTER_KEY", TEST_KEY)
        .output()
        .expect("run Python save oracle");
    assert!(
        output.status.success(),
        "Python oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let python_projection: Value =
        serde_json::from_slice(&output.stdout).expect("Python oracle JSON");

    let mut rust_config: Value =
        serde_json::from_slice(&fs::read(&rust_path).expect("Rust config bytes"))
            .expect("Rust config JSON");
    let rust_secret = PathBuf::from(
        rust_config["providers"][0]["api_key_file"]
            .as_str()
            .expect("Rust secret path"),
    );
    rust_config["providers"][0]["api_key_file"] = Value::String(
        rust_secret
            .file_name()
            .expect("Rust secret filename")
            .to_string_lossy()
            .into_owned(),
    );
    let rust_projection = json!({
        "config": rust_config,
        "secret": vault
            .read_encrypted_text(&rust_secret)
            .expect("Rust secret plaintext")
            .as_str()
    });
    assert_eq!(rust_projection, python_projection);
}
