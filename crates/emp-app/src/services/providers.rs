//! Services providers.
use std::path::PathBuf;
use std::sync::Mutex;

use crate::app::ServerState;
use emp_core::ResolvedRoute;
use emp_state::VaultStore;
use emp_state::load_configuration;
use emp_state::provider_api_key;
use emp_state::remember_resolved_protocol;
use emp_state::save_configuration;
use serde_json::Value;

pub(crate) fn hydrate_provider_keys(config: &mut Value, vault: &VaultStore) {
    let Some(providers) = config.get_mut("providers").and_then(Value::as_array_mut) else {
        return;
    };
    for provider in providers {
        let key = provider_api_key(provider, vault);
        if let Some(provider) = provider.as_object_mut() {
            provider.insert("api_key".to_owned(), Value::String(key));
        }
    }
}

pub(crate) fn persist_protocol_observation(state: &ServerState, route: &ResolvedRoute) {
    let Ok(mut config) = state.backend.configuration.config.lock() else {
        return;
    };
    let Ok(Some(updated)) = remember_resolved_protocol(
        &config,
        &route.provider_id,
        &route.requested_model,
        route.protocol.as_config_str(),
    ) else {
        return;
    };
    if save_configuration(
        &updated,
        Some(&state.backend.configuration.config_path),
        &state.backend.configuration.vault,
    )
    .is_err()
    {
        return;
    }
    if let Ok(reloaded) = load_configuration(Some(&state.backend.configuration.config_path)) {
        *config = reloaded;
    }
}

pub(crate) struct ConfigurationState {
    pub(crate) config: Mutex<Value>,
    pub(crate) discovery_lock: Mutex<()>,
    pub(crate) config_path: PathBuf,
    pub(crate) vault: VaultStore,
}
