use emp_state::{
    ExportGroups, MIGRATION_MAGIC, MigrationError, VaultStore, account_auth_path, decode_migration,
    export_migration_bundle_with_summary, load_configuration, normalize_configuration,
    save_configuration, select_export_config,
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

fn vault(root: &Path) -> VaultStore {
    VaultStore::from_sources(Some(TEST_KEY), &root.join("unused.key")).expect("vault store")
}

fn selected(groups: &[&str]) -> ExportGroups {
    ExportGroups::from_list(groups).expect("valid export groups")
}

fn ids(value: &Value, field: &str) -> Vec<String> {
    value[field]
        .as_array()
        .expect("array field")
        .iter()
        .map(|item| item["id"].as_str().expect("record id").to_owned())
        .collect()
}

#[test]
fn category_selection_matches_python_dependencies() {
    let config = normalize_configuration(Some(&json!({
        "native_catalog_path": "missing-catalog.json",
        "accounts": [{"id": "other", "prefix": "other"}],
        "providers": [
            {"id": "external", "base_url": "https://example.test/v1"},
            {"id": "bridge", "base_url": "https://native.test/v1", "auth_mode": "forward", "protocol": "responses"}
        ],
        "models": [
            {"id": "external/flash", "provider": "external", "family_id": "gemini"},
            {"id": "bridge/local", "provider": "bridge", "upstream_id": "gpt-x"}
        ],
        "native_hidden_models": ["gpt-hidden"],
        "catalog_presentations": {
            "gpt-x": {"catalog_alias": "Native model"},
            "other/gpt-x": {"catalog_alias": "Other model"},
            "external/flash": {"catalog_alias": "External model"},
            "bridge/local": {"catalog_alias": "Native bridge"}
        },
        "catalog_family_presentations": {
            "gpt-x": {"show_context": false},
            "gemini": {"show_context": true}
        }
    })))
    .expect("normalized selection config");

    let cases = [
        (
            vec!["native"],
            vec!["bridge"],
            Vec::<&str>::new(),
            vec!["bridge/local"],
            vec!["bridge/local", "gpt-x"],
            vec!["gpt-x"],
            vec!["gpt-hidden"],
        ),
        (
            vec!["subscriptions"],
            Vec::<&str>::new(),
            vec!["other"],
            Vec::<&str>::new(),
            vec!["other/gpt-x"],
            vec!["gpt-x"],
            Vec::<&str>::new(),
        ),
        (
            vec!["external"],
            vec!["external"],
            Vec::<&str>::new(),
            vec!["external/flash"],
            vec!["external/flash"],
            vec!["gemini"],
            Vec::<&str>::new(),
        ),
        (
            vec!["native", "subscriptions"],
            vec!["bridge"],
            vec!["other"],
            vec!["bridge/local"],
            vec!["bridge/local", "gpt-x", "other/gpt-x"],
            vec!["gpt-x"],
            vec!["gpt-hidden"],
        ),
        (
            vec!["native", "external"],
            vec!["external", "bridge"],
            Vec::<&str>::new(),
            vec!["external/flash", "bridge/local"],
            vec!["bridge/local", "external/flash", "gpt-x"],
            vec!["gemini", "gpt-x"],
            vec!["gpt-hidden"],
        ),
        (
            vec!["subscriptions", "external"],
            vec!["external"],
            vec!["other"],
            vec!["external/flash"],
            vec!["external/flash", "other/gpt-x"],
            vec!["gemini", "gpt-x"],
            Vec::<&str>::new(),
        ),
        (
            vec!["native", "subscriptions", "external"],
            vec!["external", "bridge"],
            vec!["other"],
            vec!["external/flash", "bridge/local"],
            vec!["bridge/local", "external/flash", "gpt-x", "other/gpt-x"],
            vec!["gemini", "gpt-x"],
            vec!["gpt-hidden"],
        ),
    ];
    for (groups, providers, accounts, models, routes, families, hidden) in cases {
        let result =
            select_export_config(&config, Some(&selected(&groups))).expect("select export config");
        assert_eq!(ids(&result, "providers"), providers, "groups: {groups:?}");
        assert_eq!(ids(&result, "accounts"), accounts, "groups: {groups:?}");
        assert_eq!(ids(&result, "models"), models, "groups: {groups:?}");
        assert_eq!(
            result["catalog_presentations"]
                .as_object()
                .expect("presentations")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            routes,
            "groups: {groups:?}"
        );
        assert_eq!(
            result["catalog_family_presentations"]
                .as_object()
                .expect("family presentations")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            families,
            "groups: {groups:?}"
        );
        assert_eq!(result["native_hidden_models"], json!(hidden));
    }

    assert_eq!(
        ExportGroups::from_list(&[]).expect_err("empty selection"),
        MigrationError::InvalidExportGroup
    );
    assert_eq!(
        ExportGroups::from_list(&["unknown"]).expect_err("unknown selection"),
        MigrationError::InvalidExportGroup
    );
}

#[test]
fn export_encrypts_credentials_and_uses_portable_paths() {
    let directory = tempdir().expect("temporary directory");
    let root = root(&directory);
    let path = root.join("source/config.json");
    let vault = vault(&root);
    let mut config = normalize_configuration(Some(&json!({
        "account_store_path": "private/accounts",
        "secret_store_path": "private/secrets",
        "providers": [{
            "id": "deepseek",
            "base_url": "https://api.deepseek.com/v1",
            "api_key": "synthetic provider secret"
        }],
        "models": [{"id": "deepseek/chat", "provider": "deepseek"}],
        "accounts": [{"id": "subscription", "prefix": "subscription"}]
    })))
    .expect("normalized source");
    let auth_path = account_auth_path(&config, "subscription", &path).expect("account path");
    config["accounts"][0]["auth_file"] = Value::String(auth_path.to_string_lossy().into_owned());
    vault
        .write_encrypted_json(
            &auth_path,
            &json!({"tokens": {"access_token": "synthetic account secret"}}),
        )
        .expect("write account secret");
    save_configuration(&config, Some(&path), &vault).expect("save source config");
    let config = load_configuration(Some(&path)).expect("load source config");
    let native_auth = root.join("native-auth.json");
    fs::write(
        &native_auth,
        b"\xef\xbb\xbf{\"tokens\":{\"access_token\":\"synthetic native secret\"}}",
    )
    .expect("write BOM native auth");

    let (bundle, summary) =
        export_migration_bundle_with_summary(&config, PASSWORD, &vault, None, Some(&native_auth))
            .expect("export migration bundle");
    assert!(bundle.starts_with(MIGRATION_MAGIC));
    for secret in [
        b"synthetic provider secret".as_slice(),
        b"synthetic account secret".as_slice(),
        b"synthetic native secret".as_slice(),
    ] {
        assert!(
            !bundle.windows(secret.len()).any(|window| window == secret),
            "migration envelope must not expose plaintext"
        );
    }
    assert_eq!(summary.accounts, 2);
    assert_eq!(summary.providers, 1);
    assert_eq!(summary.models, 1);
    assert_eq!(summary.groups, ["external", "native", "subscriptions"]);
    assert!(summary.native_login_included);
    assert!(!summary.native_login_missing);

    let plaintext = decode_migration(PASSWORD, &bundle).expect("decrypt exported bundle");
    let payload: Value = serde_json::from_slice(&plaintext).expect("export payload JSON");
    assert_eq!(payload["config"]["host"], "127.0.0.1");
    assert_eq!(
        payload["config"]["native_catalog_path"],
        "~/.codex/models_cache.json"
    );
    assert_eq!(payload["config"]["account_store_path"], "state/accounts");
    assert_eq!(payload["config"]["secret_store_path"], "state/secrets");
    assert!(
        payload["config"]["providers"][0]
            .get("api_key_file")
            .is_none()
    );
    assert_eq!(payload["config"]["providers"][0]["api_key"], "");
    assert!(payload["config"]["accounts"][0].get("auth_file").is_none());
    assert_eq!(
        payload["provider_keys"]["deepseek"],
        "synthetic provider secret"
    );
    assert_eq!(
        payload["accounts"][0]["auth"]["tokens"]["access_token"],
        "synthetic account secret"
    );
    assert_eq!(
        payload["accounts"][1]["auth"]["tokens"]["access_token"],
        "synthetic native secret"
    );
}

#[test]
fn omitted_categories_do_not_read_unselected_credentials() {
    let directory = tempdir().expect("temporary directory");
    let root = root(&directory);
    let vault = vault(&root);
    let config = normalize_configuration(Some(&json!({
        "accounts": [{"id": "missing", "prefix": "missing", "auth_file": "unavailable-auth.json"}],
        "providers": [{"id": "external", "base_url": "https://example.test/v1", "api_key_file": "unavailable-key"}]
    })))
    .expect("normalized unavailable config");
    let native = selected(&["native"]);
    let (bundle, summary) =
        export_migration_bundle_with_summary(&config, PASSWORD, &vault, Some(&native), None)
            .expect("native-only export");
    let payload: Value = serde_json::from_slice(
        &decode_migration(PASSWORD, &bundle).expect("decrypt native-only export"),
    )
    .expect("native-only payload");
    assert_eq!(payload["accounts"], json!([]));
    assert_eq!(payload["provider_keys"], json!({}));
    assert_eq!(summary.groups, ["native"]);
    assert!(summary.native_login_missing);
}

#[test]
fn migration_export_matches_live_python_oracle_when_configured() {
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
import json
import base64
import sys
from pathlib import Path
from easy_multi_provider.accounts import import_account
from easy_multi_provider.config import load, normalize, save
from easy_multi_provider import migration as migration_module
from easy_multi_provider.migration import export_bundle_with_summary

path = Path(sys.argv[1])
config = normalize({
    "providers": [{"id": "deepseek", "base_url": "https://api.deepseek.com/v1", "api_key": "synthetic provider secret"}],
    "models": [{"id": "deepseek/chat", "provider": "deepseek", "family_id": "deepseek"}],
    "accounts": [{"id": "subscription", "prefix": "subscription"}],
    "catalog_presentations": {"deepseek/chat": {"catalog_alias": "DeepSeek"}, "subscription/gpt": {"catalog_alias": "Subscription"}},
    "catalog_family_presentations": {"deepseek": {"show_context": False}, "gpt": {"show_context": True}},
})
save(config, path)
config = load(path)
config["accounts"] = [import_account(
    config,
    {"id": "subscription", "prefix": "subscription"},
    {"tokens": {"access_token": "synthetic account secret"}},
    path,
)]
save(config, path)
bundle, summary = export_bundle_with_summary(load(path), path, "migration-pass", ["subscriptions", "external"])
envelope = json.loads(bundle[len(migration_module.MAGIC):].decode("utf-8"))
salt = base64.urlsafe_b64decode(envelope["salt"])
ciphertext = base64.urlsafe_b64decode(envelope["payload"])
payload = json.loads(migration_module._fernet("migration-pass", salt).decrypt(ciphertext).decode("utf-8"))
print(json.dumps({"payload": payload, "summary": summary}, ensure_ascii=False))
"#;
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(&python_path)
        .current_dir(repository)
        .env("EASY_MULTI_PROVIDER_MASTER_KEY", TEST_KEY)
        .output()
        .expect("run Python export oracle");
    assert!(
        output.status.success(),
        "Python oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("Python oracle JSON");

    let vault = vault(&root);
    let mut config = normalize_configuration(Some(&json!({
        "providers": [{"id": "deepseek", "base_url": "https://api.deepseek.com/v1", "api_key": "synthetic provider secret"}],
        "models": [{"id": "deepseek/chat", "provider": "deepseek", "family_id": "deepseek"}],
        "accounts": [{"id": "subscription", "prefix": "subscription"}],
        "catalog_presentations": {"deepseek/chat": {"catalog_alias": "DeepSeek"}, "subscription/gpt": {"catalog_alias": "Subscription"}},
        "catalog_family_presentations": {"deepseek": {"show_context": false}, "gpt": {"show_context": true}}
    })))
    .expect("normalized Rust export config");
    let auth_path = account_auth_path(&config, "subscription", &rust_path).expect("Rust auth path");
    config["accounts"][0]["auth_file"] = Value::String(auth_path.to_string_lossy().into_owned());
    vault
        .write_encrypted_json(
            &auth_path,
            &json!({"tokens": {"access_token": "synthetic account secret"}}),
        )
        .expect("write Rust account auth");
    save_configuration(&config, Some(&rust_path), &vault).expect("save Rust export config");
    let config = load_configuration(Some(&rust_path)).expect("load Rust export config");
    let groups = selected(&["subscriptions", "external"]);
    let (bundle, summary) =
        export_migration_bundle_with_summary(&config, PASSWORD, &vault, Some(&groups), None)
            .expect("Rust migration export");
    let payload: Value =
        serde_json::from_slice(&decode_migration(PASSWORD, &bundle).expect("decrypt Rust export"))
            .expect("Rust export payload");
    let summary = json!({
        "accounts": summary.accounts,
        "providers": summary.providers,
        "models": summary.models,
        "groups": summary.groups,
        "native_login_included": summary.native_login_included,
        "native_login_missing": summary.native_login_missing
    });
    assert_eq!(payload, oracle["payload"]);
    assert_eq!(summary, oracle["summary"]);
}
