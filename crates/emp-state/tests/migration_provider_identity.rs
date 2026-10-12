use emp_state::migration::preview_migration_import;
use emp_state::{VaultStore, normalize_configuration};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempfile::tempdir;

fn package(url: &str, key: &str) -> Value {
    json!({"schema":"easy-multi-provider-migration","version":1,
        "config":{"providers":[{"id":"api","name":"Imported","base_url":url}],
        "models":[{"id":"api/model","provider":"api","upstream_id":"model","display_name":"New", "input_modalities":["text","image"],"supports_images":true}],
        "catalog_presentations":{"api/model":{"catalog_alias":"Imported alias"}}},
        "accounts":[],"provider_keys":{"api":key}})
}

#[test]
fn provider_id_collision_requires_verified_url_and_nonempty_key() {
    let root = tempdir().unwrap();
    let path = root.path().join("config.json");
    let vault = VaultStore::from_sources(
        Some("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="),
        &root.path().join("unused"),
    )
    .unwrap();
    let key_path = root.path().join("key.enc");
    vault
        .write_encrypted_text(&key_path, "original-key")
        .unwrap();
    let before = std::fs::read(&key_path).unwrap();
    let current = normalize_configuration(Some(&json!({
        "providers":[{"id":"api","name":"Original","base_url":"https://fixture.invalid/v1","api_key_file":key_path}],
        "models":[{"id":"api/model","provider":"api","upstream_id":"model","display_name":"Old"}]
    }))).unwrap();
    for (url, key, merge) in [
        ("https://fixture.invalid/v1", "original-key", true),
        ("https://fixture.invalid/v1", "different-key", false),
        ("https://different.invalid/v1", "original-key", false),
        ("https://fixture.invalid/v1", "", false),
    ] {
        let (result, _, summary) = preview_migration_import(
            &current,
            &package(url, key),
            &path,
            &vault,
            &BTreeMap::new(),
        )
        .unwrap();
        let target = if merge { "api" } else { "api-2" };
        assert_eq!(summary.provider_id_map["api"], target);
        assert_eq!(
            result["providers"].as_array().unwrap().len(),
            if merge { 1 } else { 2 }
        );
        assert_eq!(
            summary.overwritten_providers,
            if merge { vec!["api"] } else { vec![] }
        );
        let imported = result["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["provider"] == target)
            .unwrap();
        assert_eq!(imported["id"], format!("{target}/model"));
        assert_eq!(imported["input_modalities"], json!(["text", "image"]));
        assert_eq!(
            result["catalog_presentations"][format!("{target}/model")]["catalog_alias"],
            "Imported alias"
        );
        if !merge {
            assert_eq!(result["providers"][0], current["providers"][0]);
            assert_eq!(result["models"][0], current["models"][0]);
            let (again, _, _) = preview_migration_import(
                &result,
                &package(url, key),
                &path,
                &vault,
                &BTreeMap::new(),
            )
            .unwrap();
            assert_eq!(
                again["providers"].as_array().unwrap().len(),
                if key.is_empty() { 3 } else { 2 }
            );
        }
        assert_eq!(std::fs::read(&key_path).unwrap(), before);
        assert!(!path.exists());
    }
    std::fs::write(&key_path, b"unreadable credential").unwrap();
    let (unknown, _, _) = preview_migration_import(
        &current,
        &package("https://fixture.invalid/v1", "original-key"),
        &path,
        &vault,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(unknown["providers"].as_array().unwrap().len(), 2);
}

#[test]
fn verified_connection_merges_even_when_local_ids_differ() {
    let root = tempdir().unwrap();
    let path = root.path().join("config.json");
    let vault = VaultStore::from_sources(
        Some("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="),
        &root.path().join("unused"),
    )
    .unwrap();
    let key = root.path().join("key.enc");
    vault.write_encrypted_text(&key, "same-key").unwrap();
    let current=normalize_configuration(Some(&json!({"providers":[{"id":"original","name":"Current","base_url":"https://fixture.invalid/v1","api_key_file":key}],"models":[]}))).unwrap();
    let (merged, _, summary) = preview_migration_import(
        &current,
        &package("https://fixture.invalid/v1", "same-key"),
        &path,
        &vault,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(summary.provider_id_map["api"], "original");
    assert_eq!(merged["providers"].as_array().unwrap().len(), 1);
    assert_eq!(merged["models"][0]["id"], "original/model");
    let (different, _, _) = preview_migration_import(
        &current,
        &package("https://fixture.invalid/v1", "other-key"),
        &path,
        &vault,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(different["providers"].as_array().unwrap().len(), 2);
    assert_eq!(different["providers"][0], current["providers"][0]);
    let mut duplicate = package("https://fixture.invalid/v1", "same-key");
    let mut provider = duplicate["config"]["providers"][0].clone();
    provider["id"] = json!("second");
    duplicate["config"]["providers"]
        .as_array_mut()
        .unwrap()
        .push(provider);
    duplicate["provider_keys"]["second"] = json!("same-key");
    let empty = normalize_configuration(None).unwrap();
    let (merged, _, summary) =
        preview_migration_import(&empty, &duplicate, &path, &vault, &BTreeMap::new()).unwrap();
    assert_eq!(merged["providers"].as_array().unwrap().len(), 1);
    assert_eq!(summary.provider_id_map["second"], "api");
}
