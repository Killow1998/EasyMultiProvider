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
    let mut saved: SavedModels = serde_json::from_slice(&bytes).ok()?;
    if saved.identity != identity(provider) {
        return None;
    }
    // An application upgrade must also update previews from older discovery
    // caches, without refetching upstream or discarding advertised restrictions.
    if provider["execution_backend"] == "claude_cli" {
        saved.models = emp_state::official_registry::enrich_discovered_models(
            provider.as_object()?,
            saved.models,
        );
    }
    Some(saved)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::ServerHandle;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn cached_claude_list_gains_new_capabilities_without_refetch_or_disk_rewrite() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let config_path = root.join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&json!({
                "native_catalog_path":root.join("codex/models_cache.json"),
                "providers":[{"id":"claude","base_url":"https://example.invalid",
                    "protocol":"anthropic_messages","execution_backend":"claude_cli",
                    "api_key":"fixture-secret"}]
            }))
            .unwrap(),
        )
        .unwrap();
        let server = ServerHandle::start_with_config_options(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &config_path,
            "codex",
            root.join("codex/auth.json"),
        )
        .unwrap();
        let config = server.state.backend.configuration.read().unwrap().clone();
        let provider = provider(&server.state, &config, "claude").unwrap();
        let old_models = vec![
            json!({"upstream_id":"claude-haiku-5-5"}),
            json!({"upstream_id":"claude-opus-4-8","reasoning_levels":["low"],
                "capability_sources":{"reasoning_levels":{"source":"advertised"}}}),
        ];
        let written = save(&server.state, &provider, old_models).unwrap();
        let file_path = path(&server.state, &provider);
        let before = std::fs::read(&file_path).unwrap();
        let loaded = load(&server.state, &provider).unwrap();
        assert_eq!(
            loaded.models[0]["reasoning_levels"],
            json!(["low", "medium", "high", "xhigh", "max"])
        );
        assert_eq!(
            loaded.models[0]["input_modalities"],
            json!(["text", "image"])
        );
        assert_eq!(loaded.models[1]["reasoning_levels"], json!(["low"]));
        assert_eq!(loaded.updated_at, written.updated_at);
        assert_eq!(std::fs::read(&file_path).unwrap(), before);
        server.shutdown().unwrap();
    }
}
