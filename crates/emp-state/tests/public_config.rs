use emp_state::{normalize_configuration, public_configuration_with_file_status};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

fn normalized_fixture(secret_file: &Path) -> Value {
    let mut config = normalize_configuration(Some(&json!({
        "accounts": [{
            "id": "egg",
            "name": "Egg",
            "prefix": "egg",
            "auth_file": "/missing/account-auth.json.enc",
            "credential_status": "valid",
            "hidden_models": ["gpt-hidden"],
            "model_context_windows": {"gpt-context": 128000},
            "quota": {"credit": 1818}
        }],
        "providers": [
            {
                "id": "inline",
                "name": "Inline",
                "base_url": "https://api.example.com/v1",
                "api_key": "secret-inline"
            },
            {
                "id": "managed",
                "name": "Managed",
                "base_url": "https://managed.example.com/v1"
            }
        ]
    })))
    .expect("normalized fixture");
    config["providers"][1]["api_key_file"] =
        Value::String(secret_file.to_string_lossy().into_owned());
    config["providers"][1]["future_safe_field"] = json!({"kept": true});
    config["future_safe_top_level"] = json!([1, 2, 3]);
    config
}

fn regular_file(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

#[test]
fn browser_projection_redacts_credentials_and_preserves_safe_state() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path().canonicalize().expect("canonical root");
    let secret_file = root.join("managed.key");
    std::fs::write(&secret_file, b"never expose this").expect("secret fixture");
    let config = normalized_fixture(&secret_file);
    let duplicates = BTreeMap::from([("egg".to_owned(), "当前 Codex 登录".to_owned())]);

    let public = public_configuration_with_file_status(&config, &duplicates, regular_file)
        .expect("public configuration");
    let encoded = serde_json::to_string(&public).expect("public JSON");
    assert!(!encoded.contains("secret-inline"));
    assert!(!encoded.contains("managed.key"));
    assert!(!encoded.contains("auth_file"));
    assert_eq!(public["providers"][0]["api_key"], "••••••••");
    assert_eq!(public["providers"][0]["api_key_set"], true);
    assert_eq!(public["providers"][1]["api_key"], "");
    assert_eq!(public["providers"][1]["api_key_set"], true);
    assert_eq!(
        public["providers"][1]["future_safe_field"],
        json!({"kept": true})
    );
    assert_eq!(public["future_safe_top_level"], json!([1, 2, 3]));
    assert_eq!(public["accounts"][0]["credential_set"], true);
    assert_eq!(public["accounts"][0]["credential_status"], "valid");
    assert_eq!(public["accounts"][0]["duplicate"], true);
    assert_eq!(public["accounts"][0]["duplicate_of"], "当前 Codex 登录");
    assert_eq!(config["providers"][0]["api_key"], "secret-inline");
    assert_eq!(
        config["accounts"][0]["auth_file"],
        "/missing/account-auth.json.enc"
    );
}

#[cfg(unix)]
#[test]
fn browser_projection_does_not_count_a_symlink_as_a_managed_secret() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path().canonicalize().expect("canonical root");
    let target = root.join("target.key");
    let link = root.join("link.key");
    std::fs::write(&target, b"secret").expect("target");
    symlink(&target, &link).expect("symlink");
    let config = normalized_fixture(&link);
    let public = public_configuration_with_file_status(&config, &BTreeMap::new(), regular_file)
        .expect("public configuration");
    assert_eq!(public["providers"][1]["api_key_set"], false);
}
