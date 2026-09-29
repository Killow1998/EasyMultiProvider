use emp_state::{
    MigrationError, VaultStore, account_auth_path, encode_migration, import_migration_bundle,
    load_configuration, normalize_configuration, save_configuration,
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
    assert!(summary.overwritten_providers.is_empty());

    let (_, summary) = import_migration_bundle(&imported, &migration, PASSWORD, &path, &vault)
        .expect("re-import migration");
    assert_eq!(summary.overwritten_providers, ["deepseek"]);
}

#[test]
fn import_preserves_or_replaces_global_context_display_by_field_presence() {
    let directory = tempdir().expect("temporary directory");
    let root = root(&directory);
    let path = root.join("target/config.json");
    let vault = vault(&root);
    let current = normalize_configuration(Some(&json!({"catalog_show_context": false})))
        .expect("current configuration");
    save_configuration(&current, Some(&path), &vault).expect("save current configuration");
    let current = load_configuration(Some(&path)).expect("load current configuration");

    let old_bundle = bundle(&payload(json!({}), json!([]), json!({})));
    let (preserved, _) = import_migration_bundle(&current, &old_bundle, PASSWORD, &path, &vault)
        .expect("import old bundle");
    assert_eq!(preserved["catalog_show_context"], false);

    let explicit_bundle = bundle(&payload(
        json!({"catalog_show_context": true}),
        json!([]),
        json!({}),
    ));
    let (replaced, _) =
        import_migration_bundle(&preserved, &explicit_bundle, PASSWORD, &path, &vault)
            .expect("import explicit preference");
    assert_eq!(replaced["catalog_show_context"], true);

    let explicit_hidden_bundle = bundle(&payload(
        json!({"catalog_show_context": false}),
        json!([]),
        json!({}),
    ));
    let (hidden, _) =
        import_migration_bundle(&replaced, &explicit_hidden_bundle, PASSWORD, &path, &vault)
            .expect("import explicit hidden preference");
    assert_eq!(hidden["catalog_show_context"], false);
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
