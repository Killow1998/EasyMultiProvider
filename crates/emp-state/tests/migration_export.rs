use emp_state::{
    ExportGroups, MIGRATION_MAGIC, MigrationError, VaultStore, account_auth_path, decode_migration,
    export_migration_bundle_with_summary, load_configuration, normalize_configuration,
    save_configuration, select_export_config,
};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
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
fn category_selection_matches_migration_groups() {
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
        "catalog_show_context": false,
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
        assert_eq!(result["catalog_show_context"], false);
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
