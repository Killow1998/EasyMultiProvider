use super::*;
use emp_state::{ConfigError, FileTransaction};
use std::path::Path;

pub(super) fn merge_subscription_routes(config: &mut Value, previous: &str, current: &str) {
    let accounts = config["accounts"]
        .as_array_mut()
        .expect("normalized accounts");
    let Some(old) = accounts
        .iter()
        .find(|a| a["id"] == previous && a["workspace_shared"] == true)
    else {
        return;
    };
    let aliases = old["route_names"]
        .as_array()
        .into_iter()
        .flatten()
        .chain([&old["name"], &old["prefix"]])
        .cloned()
        .collect::<Vec<_>>();
    let row = accounts
        .iter_mut()
        .find(|a| a["id"] == current)
        .expect("matched local account");
    let mut names = row["route_names"].as_array().cloned().unwrap_or_default();
    for alias in aliases {
        if alias != row["name"] && alias != row["prefix"] && !names.contains(&alias) {
            names.push(alias);
        }
    }
    if !names.is_empty() {
        row["route_names"] = Value::Array(names);
    }
    if config["subscription_search"]["account_id"] == previous {
        config["subscription_search"]["account_id"] = json!(current);
    }
}

pub(super) fn retire_subscriptions(
    state: &ServerState,
    config: &mut Value,
    ids: &BTreeSet<String>,
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for id in ids {
        let path =
            emp_state::account_auth_path(config, id, &state.backend.configuration.config_path)
                .map_err(|_| Failure::unavailable())?;
        let accounts = config["accounts"]
            .as_array_mut()
            .expect("normalized accounts");
        let Some(index) = accounts
            .iter()
            .position(|a| a["id"] == *id && subscriptions::owns_credential(a, &path))
        else {
            continue;
        };
        if accounts[index]["retained_identity"].is_string() {
            let row = &mut accounts[index];
            row["auth_file"] = json!("");
            row["credential_status"] = json!("invalid");
            row["quota"] = Value::Null;
            row.as_object_mut()
                .expect("account")
                .remove("workspace_shared");
        } else {
            accounts.remove(index);
        }
        paths.push(path.with_file_name("config.toml"));
        paths.push(path);
    }
    Ok(unreferenced_files(config, paths))
}

pub(super) fn remove_legacy_providers(
    config: &mut Value,
    imported: &BTreeMap<String, Imported>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut removed = BTreeSet::new();
    config["providers"]
        .as_array_mut()
        .expect("normalized providers")
        .retain(|entry| {
            let Some(id) = entry["id"].as_str().filter(|id| {
                imported
                    .get(*id)
                    .is_some_and(|record| record.expires_at.is_none())
            }) else {
                return true;
            };
            if let Some(path) = entry["api_key_file"]
                .as_str()
                .filter(|path| !path.is_empty())
            {
                paths.push(PathBuf::from(path));
            }
            removed.insert(id.to_owned());
            false
        });
    config["models"]
        .as_array_mut()
        .expect("normalized models")
        .retain(|model| {
            !model["provider"]
                .as_str()
                .is_some_and(|id| removed.contains(id))
        });
    if config["decision_provider"]
        .as_str()
        .is_some_and(|id| removed.contains(id))
    {
        config["decision_provider"] = json!("");
    }
    unreferenced_files(config, paths)
}

fn unreferenced_files(config: &Value, mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let retained: BTreeSet<_> = [("accounts", "auth_file"), ("providers", "api_key_file")]
        .into_iter()
        .flat_map(|(kind, field)| {
            config[kind]
                .as_array()
                .expect("normalized services")
                .iter()
                .filter_map(move |entry| entry[field].as_str().map(PathBuf::from))
        })
        .collect();
    paths.retain(|path| {
        !retained.contains(path)
            && (path.file_name().and_then(|name| name.to_str()) != Some("config.toml")
                || !retained.iter().any(|auth| auth.parent() == path.parent()))
    });
    paths
}

pub(super) fn remove_private_file(
    files: &mut FileTransaction,
    path: &Path,
) -> std::result::Result<(), ConfigError> {
    files.remember(path)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(emp_state::FilesystemError::ManagedFileUnavailable.into()),
    }
}
