//! Last fetched provider lists. Reads and selections never contact upstream.
use crate::app::ServerState;
use emp_state::{atomic_write_private_state, provider_api_key, read_file_limited};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
pub(super) struct SavedModels {
    identity: String,
    pub(super) updated_at: String,
    pub(super) models: Vec<Value>,
}

pub(super) fn provider(state: &ServerState, config: &Value, id: &str) -> Option<Value> {
    let mut provider = config["providers"]
        .as_array()?
        .iter()
        .find(|p| p["id"] == id && p["enabled"] != false)?
        .clone();
    provider["api_key"] = json!(provider_api_key(
        &provider,
        &state.backend.configuration.vault
    ));
    Some(provider)
}

fn identity(provider: &Value) -> String {
    // Connection identity only; renaming a service keeps its saved model list.
    // Credentials are used in memory, and only the digest is stored on disk.
    let fields: Vec<_> = [
        "id",
        "base_url",
        "protocol",
        "auth_mode",
        "api_key",
        "anthropic_version",
        "execution_backend",
    ]
    .iter()
    .map(|key| &provider[*key])
    .collect();
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&fields).expect("provider identity JSON"))
    )
}

fn path(state: &ServerState, provider: &Value) -> PathBuf {
    let id = provider["id"].as_str().unwrap_or_default();
    let name = format!("{:x}.json", Sha256::digest(id.as_bytes()));
    state
        .backend
        .configuration
        .config_path
        .parent()
        .expect("config directory")
        .join("provider-model-lists")
        .join(name)
}

pub(super) fn load(state: &ServerState, provider: &Value) -> Option<SavedModels> {
    let path = path(state, provider);
    if !std::fs::symlink_metadata(&path).ok()?.file_type().is_file() {
        return None;
    }
    let bytes = read_file_limited(&path, emp_state::MAX_TRANSACTION_FILE_BYTES).ok()?;
    let saved: SavedModels = serde_json::from_slice(&bytes).ok()?;
    (saved.identity == identity(provider)).then_some(saved)
}

pub(super) fn save(
    state: &ServerState,
    provider: &Value,
    models: Vec<Value>,
) -> Result<SavedModels, ()> {
    let saved = SavedModels {
        identity: identity(provider),
        updated_at: emp_state::observed_at_now(),
        models,
    };
    let bytes = serde_json::to_vec(&saved).map_err(|_| ())?;
    atomic_write_private_state(&path(state, provider), &bytes).map_err(|_| ())?;
    Ok(saved)
}

pub(super) fn unchanged(state: &ServerState, config: &Value, expected: &Value) -> bool {
    provider(state, config, expected["id"].as_str().unwrap_or_default())
        .is_some_and(|current| identity(&current) == identity(expected))
}
