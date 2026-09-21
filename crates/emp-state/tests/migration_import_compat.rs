use base64::{Engine, engine::general_purpose::STANDARD};
use emp_state::{
    MigrationError, VaultStore, account_auth_path, encode_migration, import_migration_bundle,
    load_configuration, normalize_configuration, save_configuration,
};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

const TEST_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
const PASSWORD: &str = "migration-pass";

fn root(directory: &tempfile::TempDir) -> PathBuf {
    fs::canonicalize(directory.path()).expect("canonical temporary directory")
}

fn bundle(payload: &Value) -> Vec<u8> {
    encode_migration(
        PASSWORD,
        &[7_u8; 16],
        &serde_json::to_vec(payload).expect("payload JSON"),
    )
    .expect("migration bundle")
}

fn payload(config: Value, accounts: Value, provider_keys: Value) -> Value {
    json!({
        "schema": "easy-multi-provider-migration",
        "version": 1,
        "config": config,
        "accounts": accounts,
        "provider_keys": provider_keys,
    })
}

fn vault(root: &Path) -> VaultStore {
    VaultStore::from_sources(Some(TEST_KEY), &root.join("unused.key")).expect("vault store")
}

fn normalize_secret_path(config: &mut Value) -> String {
    let provider = &mut config["providers"][1];
    let path = PathBuf::from(
        provider["api_key_file"]
            .as_str()
            .expect("provider secret path"),
    );
    provider["api_key_file"] = Value::String(
        path.file_name()
            .expect("provider secret filename")
            .to_string_lossy()
            .into_owned(),
    );
    path.to_string_lossy().into_owned()
}

#[test]
fn import_merges_without_deleting_local_entries_and_reencrypts_provider_keys() {
    let directory = tempdir().expect("temporary directory");
    let root = root(&directory);
    let path = root.join("target/config.json");
    let vault = vault(&root);
    let current = normalize_configuration(Some(&json!({
        "port": 4299,
        "providers": [{"id": "local", "base_url": "https://local.test/v1"}],
        "models": [{"id": "local/model", "provider": "local"}],
        "catalog_presentations": {"local/model": {"catalog_alias": "Local"}},
        "catalog_family_presentations": {"local-family": {"catalog_alias": "Local family"}},
        "native_hidden_models": ["native-local"]
    })))
    .expect("current configuration");
    save_configuration(&current, Some(&path), &vault).expect("save current configuration");
    let current = load_configuration(Some(&path)).expect("load current configuration");
    let source = json!({
        "providers": [{"id": "deepseek", "base_url": "https://api.deepseek.com/v1"}],
        "models": [{"id": "deepseek/chat", "provider": "deepseek"}],
        "catalog_presentations": {"deepseek/chat": {"catalog_alias": "Imported"}},
        "catalog_family_presentations": {"deepseek-family": {"catalog_alias": "Imported family"}},
        "native_hidden_models": ["native-imported"]
    });
    let migration = bundle(&payload(
        source,
        json!([]),
        json!({"deepseek": "synthetic provider credential"}),
    ));

    let (imported, summary) =
        import_migration_bundle(&current, &migration, PASSWORD, &path, &vault)
            .expect("import migration");
    assert_eq!(summary.accounts, 0);
    assert_eq!(summary.providers, 1);
    assert_eq!(summary.models, 1);
    assert_eq!(summary.renamed_accounts, 0);
    assert_eq!(imported["port"], 4299);
    assert_eq!(
        imported["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .map(|provider| provider["id"].as_str().expect("provider id"))
            .collect::<Vec<_>>(),
        ["local", "deepseek"]
    );
    assert_eq!(
        imported["models"]
            .as_array()
            .expect("models")
            .iter()
            .map(|model| model["id"].as_str().expect("model id"))
            .collect::<Vec<_>>(),
        ["local/model", "deepseek/chat"]
    );
    assert_eq!(
        imported["native_hidden_models"],
        json!(["native-imported", "native-local"])
    );
    let secret = PathBuf::from(
        imported["providers"][1]["api_key_file"]
            .as_str()
            .expect("imported secret path"),
    );
    assert_eq!(
        vault
            .read_encrypted_text(&secret)
            .expect("imported provider credential")
            .as_str(),
        "synthetic provider credential"
    );
}

#[test]
fn import_updates_the_same_account_and_renames_a_different_identity() {
    let directory = tempdir().expect("temporary directory");
    let root = root(&directory);
    let path = root.join("target/config.json");
    let vault = vault(&root);
    let mut current = normalize_configuration(Some(&json!({
        "account_store_path": "accounts",
        "accounts": [{"id": "shared", "prefix": "local", "name": "Old"}],
        "catalog_presentations": {"local/model": {"catalog_alias": "Local"}}
    })))
    .expect("current configuration");
    let auth_path = account_auth_path(&current, "shared", &path).expect("account auth path");
    current["accounts"][0]["auth_file"] = Value::String(auth_path.to_string_lossy().into_owned());
    vault
        .write_encrypted_json(
            &auth_path,
            &json!({"tokens": {"account_id": "account-A", "access_token": "OLD"}}),
        )
        .expect("write current account");
    save_configuration(&current, Some(&path), &vault).expect("save current configuration");
    let current = load_configuration(Some(&path)).expect("load current configuration");

    let same = payload(
        json!({
            "account_store_path": "accounts",
            "accounts": [{"id": "shared", "prefix": "route", "name": "Imported"}],
            "catalog_presentations": {"route/model": {"catalog_alias": "Imported"}}
        }),
        json!([{
            "metadata": {"id": "shared", "prefix": "route", "name": "Imported"},
            "auth": {"tokens": {"account_id": "account-A", "access_token": "NEW"}}
        }]),
        json!({}),
    );
    let (updated, summary) =
        import_migration_bundle(&current, &bundle(&same), PASSWORD, &path, &vault)
            .expect("update same account");
    assert_eq!(summary.renamed_accounts, 0);
    assert_eq!(updated["accounts"].as_array().expect("accounts").len(), 1);
    assert_eq!(updated["accounts"][0]["id"], "shared");
    assert_eq!(updated["accounts"][0]["prefix"], "local");
    assert_eq!(
        updated["catalog_presentations"]["local/model"]["catalog_alias"],
        "Imported"
    );
    assert_eq!(
        vault
            .read_encrypted_json(&auth_path)
            .expect("updated account auth")["tokens"]["access_token"],
        "NEW"
    );

    let different = payload(
        json!({
            "account_store_path": "accounts",
            "accounts": [{"id": "shared", "prefix": "route", "name": "Different"}],
            "catalog_presentations": {"route/model": {"catalog_alias": "Different"}}
        }),
        json!([{
            "metadata": {"id": "shared", "prefix": "route", "name": "Different"},
            "auth": {"tokens": {"account_id": "account-B", "access_token": "OTHER"}}
        }]),
        json!({}),
    );
    let (renamed, summary) =
        import_migration_bundle(&updated, &bundle(&different), PASSWORD, &path, &vault)
            .expect("import different account");
    assert_eq!(summary.renamed_accounts, 1);
    assert_eq!(
        renamed["accounts"]
            .as_array()
            .expect("accounts")
            .iter()
            .map(|account| account["id"].as_str().expect("account id"))
            .collect::<Vec<_>>(),
        ["shared", "shared-2"]
    );
    assert_eq!(
        renamed["catalog_presentations"]["shared-2/model"]["catalog_alias"],
        "Different"
    );
}

#[test]
fn import_validation_rejects_payload_confusion_before_writing() {
    let directory = tempdir().expect("temporary directory");
    let root = root(&directory);
    let path = root.join("target/config.json");
    let vault = vault(&root);
    let current = normalize_configuration(None).expect("default configuration");

    let unknown_key = payload(
        json!({"providers": []}),
        json!([]),
        json!({"unknown": "secret"}),
    );
    assert_eq!(
        import_migration_bundle(&current, &bundle(&unknown_key), PASSWORD, &path, &vault)
            .expect_err("unknown provider key"),
        MigrationError::UnknownProviderKey
    );
    let duplicate_accounts = payload(
        json!({"accounts": [{"id": "same", "prefix": "same"}]}),
        json!([
            {"metadata": {"id": "same", "prefix": "same"}, "auth": {"access_token": "A"}},
            {"metadata": {"id": "same", "prefix": "same"}, "auth": {"access_token": "B"}}
        ]),
        json!({}),
    );
    assert_eq!(
        import_migration_bundle(
            &current,
            &bundle(&duplicate_accounts),
            PASSWORD,
            &path,
            &vault
        )
        .expect_err("duplicate account IDs"),
        MigrationError::DuplicateAccountIds
    );
    assert!(
        !path.exists(),
        "invalid imports must not create configuration"
    );
}

#[test]
fn failed_import_rolls_back_new_account_and_provider_credentials() {
    let directory = tempdir().expect("temporary directory");
    let root = root(&directory);
    let path = root.join("target/config.json");
    fs::create_dir_all(&path).expect("make configuration path a directory");
    let vault = vault(&root);
    let current = normalize_configuration(None).expect("default configuration");
    let source = json!({
        "providers": [{"id": "deepseek", "base_url": "https://api.deepseek.com/v1"}],
        "accounts": [{"id": "imported", "prefix": "imported"}]
    });
    let migration = bundle(&payload(
        source,
        json!([{
            "metadata": {"id": "imported", "prefix": "imported"},
            "auth": {"access_token": "synthetic account credential"}
        }]),
        json!({"deepseek": "synthetic provider credential"}),
    ));

    assert_eq!(
        import_migration_bundle(&current, &migration, PASSWORD, &path, &vault)
            .expect_err("directory configuration path must fail"),
        MigrationError::StateUpdateFailed
    );
    assert!(path.is_dir(), "pre-existing directory must remain");
    assert!(
        !root
            .join("target/state/accounts/imported/auth.json.enc")
            .exists(),
        "account credential must roll back"
    );
    assert!(
        !root.join("target/state/secrets/deepseek.key.enc").exists(),
        "provider credential must roll back"
    );
}

#[test]
fn migration_import_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let directory = tempdir().expect("temporary directory");
    let root = root(&directory);
    let python_path = root.join("python/config.json");
    let rust_path = root.join("rust/config.json");
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repository root")
        .to_path_buf();
    let script = r#"
import base64
import json
import sys
from pathlib import Path
from easy_multi_provider.accounts import import_account, load_auth
from easy_multi_provider.config import api_key, load, normalize, save
from easy_multi_provider.migration import export_bundle, import_bundle

path = Path(sys.argv[1])
source_path = path.parent / "source.json"
source = normalize({
    "providers": [{"id": "deepseek", "base_url": "https://api.deepseek.com/v1", "api_key": "synthetic provider credential"}],
    "models": [{"id": "deepseek/chat", "provider": "deepseek"}],
    "catalog_presentations": {"deepseek/chat": {"catalog_alias": "Imported"}},
    "native_hidden_models": ["native-imported"],
})
save(source, source_path)
source = load(source_path)
source["accounts"] = [import_account(
    source,
    {"id": "shared", "prefix": "route", "name": "Imported account"},
    {"tokens": {"account_id": "same-account", "access_token": "NEW"}},
    source_path,
)]
save(source, source_path)
bundle = export_bundle(load(source_path), source_path, "migration-pass")
target = normalize({
    "port": 4299,
    "providers": [{"id": "local", "base_url": "https://local.test/v1"}],
    "models": [{"id": "local/model", "provider": "local"}],
    "accounts": [{"id": "shared", "prefix": "current-account", "name": "Local account"}],
    "catalog_presentations": {"current-account/model": {"catalog_alias": "Local"}},
    "native_hidden_models": ["native-local"],
})
save(target, path)
target = load(path)
target["accounts"] = [import_account(
    target,
    {"id": "shared", "prefix": "current-account", "name": "Local account"},
    {"tokens": {"account_id": "same-account", "access_token": "OLD"}},
    path,
)]
save(target, path)
result, summary = import_bundle(load(path), bundle, "migration-pass", path)
secret = Path(result["providers"][1]["api_key_file"])
secret_value = api_key(result["providers"][1])
account_secret = load_auth(result["accounts"][0])["tokens"]["access_token"]
account_path = Path(result["accounts"][0]["auth_file"])
result["providers"][1]["api_key_file"] = secret.name
result["accounts"][0]["auth_file"] = account_path.parent.name + "/" + account_path.name
print(json.dumps({"bundle": base64.b64encode(bundle).decode(), "config": result,
                  "secret": secret_value, "account_secret": account_secret,
                  "summary": summary}, ensure_ascii=False))
"#;
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(&python_path)
        .current_dir(repository)
        .env("EASY_MULTI_PROVIDER_MASTER_KEY", TEST_KEY)
        .output()
        .expect("run Python migration oracle");
    assert!(
        output.status.success(),
        "Python oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("Python oracle JSON");
    let bundle = STANDARD
        .decode(oracle["bundle"].as_str().expect("oracle bundle"))
        .expect("oracle bundle base64");

    let vault = vault(&root);
    let mut current = normalize_configuration(Some(&json!({
        "port": 4299,
        "providers": [{"id": "local", "base_url": "https://local.test/v1"}],
        "models": [{"id": "local/model", "provider": "local"}],
        "accounts": [{"id": "shared", "prefix": "current-account", "name": "Local account"}],
        "catalog_presentations": {"current-account/model": {"catalog_alias": "Local"}},
        "native_hidden_models": ["native-local"]
    })))
    .expect("Rust current configuration");
    let current_auth_path =
        account_auth_path(&current, "shared", &rust_path).expect("Rust current auth path");
    current["accounts"][0]["auth_file"] =
        Value::String(current_auth_path.to_string_lossy().into_owned());
    vault
        .write_encrypted_json(
            &current_auth_path,
            &json!({"tokens": {"account_id": "same-account", "access_token": "OLD"}}),
        )
        .expect("write Rust current account");
    save_configuration(&current, Some(&rust_path), &vault).expect("save Rust target");
    let current = load_configuration(Some(&rust_path)).expect("load Rust target");
    let (mut imported, summary) =
        import_migration_bundle(&current, &bundle, PASSWORD, &rust_path, &vault)
            .expect("Rust migration import");
    let secret_path = normalize_secret_path(&mut imported);
    let imported_auth_path = PathBuf::from(
        imported["accounts"][0]["auth_file"]
            .as_str()
            .expect("Rust imported auth path"),
    );
    imported["accounts"][0]["auth_file"] = Value::String(format!(
        "{}/{}",
        imported_auth_path
            .parent()
            .and_then(Path::file_name)
            .expect("Rust account directory")
            .to_string_lossy(),
        imported_auth_path
            .file_name()
            .expect("Rust auth filename")
            .to_string_lossy()
    ));
    let rust_projection = json!({
        "config": imported,
        "secret": vault
            .read_encrypted_text(Path::new(&secret_path))
            .expect("Rust imported secret")
            .as_str(),
        "account_secret": vault
            .read_encrypted_json(&imported_auth_path)
            .expect("Rust imported account auth")["tokens"]["access_token"],
        "summary": {
            "accounts": summary.accounts,
            "providers": summary.providers,
            "models": summary.models
        }
    });
    assert_eq!(rust_projection["config"], oracle["config"]);
    assert_eq!(rust_projection["secret"], oracle["secret"]);
    assert_eq!(rust_projection["summary"], oracle["summary"]);
}
