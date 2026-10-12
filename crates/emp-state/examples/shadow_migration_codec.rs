//! Developer preview helper: reuse the production migration codec over stdin.
//! Export/preview read the selected local state; no mutations or network calls.
use base64::{Engine, engine::general_purpose::STANDARD};
use emp_state::migration::{MAX_BUNDLE_BYTES, decode_migration, encode_migration};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::PathBuf;

fn local_state(
    value: &Value,
) -> Result<(Value, PathBuf, emp_state::filesystem::VaultStore), String> {
    let path = PathBuf::from(
        value["config_path"]
            .as_str()
            .ok_or("Configuration path required")?,
    );
    let default_key = path
        .parent()
        .ok_or("Configuration directory required")?
        .join("state/master.key");
    let key = std::env::var_os(emp_state::filesystem::MASTER_KEY_FILE_ENV)
        .map(PathBuf::from)
        .unwrap_or(default_key);
    let environment_key = std::env::var(emp_state::filesystem::MASTER_KEY_ENV).ok();
    // Preview must never create a production key or state directory.
    if environment_key
        .as_deref()
        .is_none_or(|key| key.trim().is_empty())
        && !key.is_file()
    {
        return Err("Existing EMP master key required".into());
    }
    let vault = emp_state::filesystem::VaultStore::from_sources(environment_key.as_deref(), &key)
        .map_err(|error| error.to_string())?;
    let bytes = emp_state::filesystem::read_file_limited(&path, MAX_BUNDLE_BYTES)
        .map_err(|_| "Configuration unavailable")?;
    let config = serde_json::from_slice(&bytes).map_err(|_| "Invalid configuration")?;
    Ok((config, path, vault))
}

fn run() -> Result<Value, String> {
    let mut input = String::new();
    std::io::stdin()
        .take((MAX_BUNDLE_BYTES * 2 + 1) as u64)
        .read_to_string(&mut input)
        .map_err(|_| "Invalid request")?;
    if input.len() > MAX_BUNDLE_BYTES * 2 {
        return Err("Request too large".into());
    }
    let value: Value = serde_json::from_str(&input).map_err(|_| "Invalid JSON")?;
    let password = value["password"].as_str().unwrap_or("");
    match value["operation"].as_str() {
        Some("decode") => {
            let bundle = STANDARD
                .decode(value["bundle"].as_str().ok_or("Bundle required")?)
                .map_err(|_| "Invalid bundle encoding")?;
            let plaintext =
                decode_migration(password, &bundle).map_err(|error| error.to_string())?;
            let payload: Value =
                serde_json::from_slice(&plaintext).map_err(|_| "Invalid package")?;
            if payload["schema"] != "easy-multi-provider-migration" || payload["version"] != 1 {
                return Err("Unsupported package".into());
            }
            Ok(json!({"payload":payload}))
        }
        Some("encode") => {
            let payload = serde_json::to_vec(&value["payload"]).map_err(|_| "Invalid package")?;
            let mut salt = [0; 16];
            getrandom::getrandom(&mut salt).map_err(|_| "Random source unavailable")?;
            let bundle =
                encode_migration(password, &salt, &payload).map_err(|error| error.to_string())?;
            Ok(json!({"bundle":STANDARD.encode(bundle)}))
        }
        Some("export") => {
            let (mut config, _, vault) = local_state(&value)?;
            let ids = value["services"].as_array().ok_or("Select services")?;
            let selected = |kind: &str, id: &Value| {
                ids.iter()
                    .any(|item| item["kind"] == kind && item["id"] == *id)
            };
            for (field, kind) in [("accounts", "account"), ("providers", "provider")] {
                if let Some(items) = config[field].as_array_mut() {
                    items.retain(|item| selected(kind, &item["id"]));
                }
            }
            let providers = config["providers"].as_array().cloned().unwrap_or_default();
            if let Some(models) = config["models"].as_array_mut() {
                models.retain(|model| providers.iter().any(|p| p["id"] == model["provider"]));
            }
            let mut groups = Vec::new();
            if selected("account", &json!("@native")) {
                groups.push("native");
            }
            if config["accounts"].as_array().is_some_and(|a| !a.is_empty()) {
                groups.push("subscriptions");
            }
            if !providers.is_empty() {
                groups.push("external");
            }
            let groups = emp_state::migration::ExportGroups::from_list(&groups)
                .map_err(|error| error.to_string())?;
            let native = PathBuf::from(
                value["native_auth_path"]
                    .as_str()
                    .ok_or("Native auth path required")?,
            );
            let (payload, _) = emp_state::migration::prepare_migration_export(
                &config,
                &vault,
                Some(&groups),
                Some(&native),
            )
            .map_err(|error| error.to_string())?;
            Ok(json!({"payload":payload}))
        }
        Some("preview") => {
            let (stored, path, vault) = local_state(&value)?;
            let mut config = value
                .get("current_config")
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or(stored.clone());
            // Keep credential paths owned by the trusted file, never browser input.
            for (field, credential) in [("accounts", "auth_file"), ("providers", "api_key_file")] {
                if let Some(records) = config[field].as_array_mut() {
                    for record in records {
                        let trusted = stored[field]
                            .as_array()
                            .and_then(|items| items.iter().find(|item| item["id"] == record["id"]));
                        record[credential] = trusted
                            .and_then(|item| item.get(credential))
                            .cloned()
                            .unwrap_or(json!(""));
                    }
                }
            }
            let preview_keys: std::collections::BTreeMap<String, String> = serde_json::from_value(
                value
                    .get("preview_provider_keys")
                    .cloned()
                    .unwrap_or(json!({})),
            )
            .map_err(|_| "Invalid preview provider identities")?;
            if let Some(providers) = config["providers"].as_array_mut() {
                for provider in providers {
                    if let Some(key) = preview_keys.get(provider["id"].as_str().unwrap_or("")) {
                        provider["api_key"] = json!(key);
                    }
                }
            }
            let preview_auth =
                serde_json::from_value(value.get("preview_auth").cloned().unwrap_or(json!({})))
                    .map_err(|_| "Invalid preview identities")?;
            let (result, auth, summary) = emp_state::migration::preview_migration_import(
                &config,
                &value["payload"],
                &path,
                &vault,
                &preview_auth,
            )
            .map_err(|error| error.to_string())?;
            Ok(
                json!({"config":result,"imported_auth":auth,"provider_id_map":summary.provider_id_map}),
            )
        }
        _ => Err("Unknown operation".into()),
    }
}

fn main() {
    let result = match run() {
        Ok(value) => value,
        Err(message) => json!({"error":{"message":message}}),
    };
    let _ = serde_json::to_writer(std::io::stdout().lock(), &result);
    let _ = std::io::stdout().write_all(b"\n");
}
